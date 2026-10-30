//! `mdb_load`: load databases from the dump format written by `mdb_dump`,
//! or from plain text pairs of lines with `-T`
//! (`libraries/liblmdb/mdb_load.c`; the format is described in
//! [`crate::dumpfmt`]).
//!
//! ```text
//! mdb_load [-V] [-a] [-f input] [-n] [-s name] [-N] [-T] dbpath
//! ```
//!
//! The input is read exactly as C reads it: same header keywords and
//! messages, same line numbers in messages, records written through one
//! cursor and committed every 100 records, `mdb_env_sync` after each
//! database. Deliberate differences from C 0.9.36:
//!
//! * The header flags of the *first* database (`dupsort=1`, …) are used;
//!   C drops them (`KNOWN_C_BUGS.md` #1).
//! * `format=` applies to the database whose header it is in; C keeps
//!   `print` mode for all later databases once one header selected it.
//! * A header line without `=` is "unexpected format" (fatal); C searches
//!   its whole 4 KiB line buffer, so leftovers of an earlier, longer line
//!   can make it print "unrecognized keyword ignored" instead.
//! * Lines are read whole, whatever their length, and NUL bytes in them are
//!   data (C's `fgets`/`strlen` logic mangles both cases).
//! * After loading the main DB with `-a`, its default comparator is
//!   restored for the next database (C keeps "always greater", so named
//!   databases after it cannot be found in the main DB).

use std::fs::File;
use std::io::{self, BufRead, BufReader};

use rlmdb::{
    CursorId, CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, RwTxn, Transaction,
    WriteFlags,
};

use crate::cfmt::{atoi, cstr, err_code, io_errno, scan_unsigned, strerror};
use crate::common::{create_db, path_of, report_failed, Tool, EXIT_FAILURE};
use crate::dump::DB_FLAG_NAMES;
use crate::dumpfmt::decode_line_into;
use crate::getopt::{GetOpt, Opt};
use crate::output::eprint_bytes;

const SYNOPSIS: &str = "[-V] [-a] [-f input] [-n] [-s name] [-N] [-T] dbpath";

/// Records per write transaction (C commits every 100 puts).
const BATCH: usize = 100;

/// What a dump header says (the parts of `mdb_load.c`'s globals that
/// `readhdr` sets).
#[derive(Debug, Default)]
struct Header {
    /// `format=print` (C's `mode & PRINT`).
    print: bool,
    /// `database=`: the database to load into (`-s` gives the default).
    subname: Option<Vec<u8>>,
    /// The flag lines (`dupsort=1`, …).
    flags: DatabaseFlags,
    /// `mapaddr=` (only whether it is non-zero matters).
    map_address: u64,
    /// `mapsize=`.
    map_size: u64,
    /// `maxreaders=`.
    max_readers: u32,
}

/// The input stream with C's line counter and end-of-input flag.
struct Input {
    reader: Box<dyn BufRead>,
    /// `lineno`: lines read so far.
    lineno: usize,
    /// `Eof`: no more databases follow (end of input or a fatal format
    /// error).
    eof: bool,
    /// `-T`: plain text, no headers and no leading spaces.
    plain: bool,
    prog: Vec<u8>,
    /// Line buffer, reused.
    line: Vec<u8>,
}

/// Why [`Input::read_line`] returned no value.
#[derive(Debug, PartialEq, Eq)]
enum NoValue {
    /// `DATA=END`, the end of input, or malformed input (then `eof` is set
    /// and a message was printed).
    End,
}

impl Input {
    /// `fgetc`: one byte, `None` at the end of input. Read errors count as
    /// the end of input, as in C.
    fn getc(&mut self) -> Option<u8> {
        let c = *self.reader.fill_buf().ok()?.first()?;
        self.reader.consume(1);
        Some(c)
    }

    /// `fgets` of a whole line (with its newline, if any) into `line`;
    /// `false` if nothing could be read.
    fn gets(&mut self, line: &mut Vec<u8>) -> bool {
        line.clear();
        matches!(self.reader.read_until(b'\n', line), Ok(n) if n > 0)
    }

    /// Writes `"<prog>: line <n>: <msg>"` to standard error.
    fn message(&self, msg: &[u8]) {
        eprint_bytes(&[
            &self.prog,
            format!(": line {}: ", self.lineno).as_bytes(),
            msg,
        ]);
    }

    /// `badend()`: malformed or truncated input ends the load.
    fn bad_end(&mut self) -> NoValue {
        self.eof = true;
        self.message(b"unexpected end of input\n");
        NoValue::End
    }

    /// `readhdr()`: reads header lines up to `HEADER=END` (or the end of
    /// input) into `hdr`. `Err(())` after a fatal message.
    fn read_header(&mut self, hdr: &mut Header) -> Result<(), ()> {
        hdr.flags = DatabaseFlags::empty();
        hdr.print = false;
        let mut line = Vec::new();
        while self.gets(&mut line) {
            self.lineno += 1;
            if let Some(rest) = line.strip_prefix(b"VERSION=") {
                let version = atoi(rest);
                if version > 3 {
                    self.message(format!("unsupported VERSION {version}\n").as_bytes());
                    return Err(());
                }
            } else if line.starts_with(b"HEADER=END") {
                break;
            } else if let Some(rest) = line.strip_prefix(b"format=") {
                if rest.starts_with(b"print") {
                    hdr.print = true;
                } else if !rest.starts_with(b"bytevalue") {
                    // C prints the rest of the line including its newline.
                    self.message(&[b"unsupported FORMAT ", cstr(rest), b"\n"].concat());
                    return Err(());
                }
            } else if let Some(rest) = line.strip_prefix(b"database=") {
                hdr.subname = Some(cstr(strip_newline(rest)).to_vec());
            } else if let Some(rest) = line.strip_prefix(b"type=") {
                if !rest.starts_with(b"btree") {
                    self.message(&[b"unsupported type ", cstr(rest), b"\n"].concat());
                    return Err(());
                }
            } else if let Some(rest) = line.strip_prefix(b"mapaddr=") {
                hdr.map_address = self.number(rest, 16, "mapaddr")?;
            } else if let Some(rest) = line.strip_prefix(b"mapsize=") {
                hdr.map_size = self.number(rest, 10, "mapsize")?;
            } else if let Some(rest) = line.strip_prefix(b"maxreaders=") {
                // sscanf("%u") stores the low 32 bits.
                hdr.max_readers = self.number(rest, 10, "maxreaders")? as u32;
            } else if let Some((flag, _)) = DB_FLAG_NAMES.iter().find(|(_, name)| {
                line.starts_with(name.as_bytes()) && line.get(name.len()) == Some(&b'=')
            }) {
                // Any value sets the flag, as in C.
                hdr.flags |= *flag;
            } else {
                match line.iter().position(|&b| b == b'=') {
                    None => {
                        self.message(b"unexpected format\n");
                        return Err(());
                    }
                    Some(i) => self.message(
                        &[b"unrecognized keyword ignored: ", cstr(&line[..i]), b"\n"].concat(),
                    ),
                }
            }
        }
        Ok(())
    }

    /// `sscanf(rest, "%zu"/"%p")` of a header value, with C's message for
    /// an invalid one.
    fn number(&self, rest: &[u8], radix: u32, what: &str) -> Result<u64, ()> {
        let text = cstr(strip_newline(rest));
        scan_unsigned(text, radix).ok_or_else(|| {
            self.message(&[format!("invalid {what} ").as_bytes(), text, b"\n"].concat());
        })
    }

    /// `readline()`: the next key or data item into `value`.
    ///
    /// In dump mode a data line starts with a space; `DATA=END` ends the
    /// database. Any other line, a line without newline or a malformed
    /// encoding ends the whole load with "unexpected end of input"; the
    /// end of input ends it silently.
    fn read_value(&mut self, value: &mut Vec<u8>, print: bool) -> Result<(), NoValue> {
        let mut line = std::mem::take(&mut self.line);
        let rc = self.read_value_with(&mut line, value, print);
        self.line = line;
        rc
    }

    fn read_value_with(
        &mut self,
        line: &mut Vec<u8>,
        value: &mut Vec<u8>,
        print: bool,
    ) -> Result<(), NoValue> {
        if !self.plain {
            let Some(c) = self.getc() else {
                self.eof = true;
                return Err(NoValue::End);
            };
            if c != b' ' {
                self.lineno += 1;
                // The rest of the line (for an empty line: the next one,
                // since the newline was consumed above, as in C).
                if !self.gets(line) {
                    return Err(self.bad_end());
                }
                if c == b'D' && line.starts_with(b"ATA=END") {
                    return Err(NoValue::End);
                }
                return Err(self.bad_end());
            }
        }
        if !self.gets(line) {
            self.eof = true;
            return Err(NoValue::End);
        }
        self.lineno += 1;
        // C: strlen() == 0, i.e. a NUL right after the leading space.
        if line.first() == Some(&0) {
            return Err(self.bad_end());
        }
        // The last line of the input has no newline.
        if line.pop() != Some(b'\n') {
            return Err(self.bad_end());
        }
        if decode_line_into(line, print, value) {
            Ok(())
        } else {
            Err(self.bad_end())
        }
    }
}

/// The value of a header line without its trailing newline.
fn strip_newline(rest: &[u8]) -> &[u8] {
    rest.strip_suffix(b"\n").unwrap_or(rest)
}

/// The comparator of `-a`: every new item sorts after all existing ones,
/// so records are stored in input order (`greater()` in `mdb_load.c`).
fn greater(_a: &[u8], _b: &[u8]) -> std::cmp::Ordering {
    std::cmp::Ordering::Greater
}

/// Command line options.
#[derive(Debug, Default)]
struct Options {
    /// `-a`.
    append: bool,
    /// `-f input`.
    input: Option<File>,
    /// `-n`.
    no_sub_dir: bool,
    /// `-s name`.
    subname: Option<Vec<u8>>,
    /// `-N`: `MDB_NOOVERWRITE | MDB_NODUPDATA`, existing records are kept.
    no_overwrite: bool,
    /// `-T`.
    plain: bool,
}

/// The `main` of `mdb_load`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    if args.len() < 2 {
        return tool.usage(SYNOPSIS);
    }
    let mut o = Options::default();
    let mut getopt = GetOpt::new(args, "af:ns:NQTV");
    while let Some(opt) = getopt.next() {
        match opt {
            Opt::Flag(b'V') => return tool.version(),
            Opt::Flag(b'a') => o.append = true,
            Opt::Arg(b'f', path) => match File::open(path_of(&path)) {
                // freopen(optarg, "r", stdin)
                Ok(f) => o.input = Some(f),
                Err(e) => {
                    let text = strerror(io_errno(&e));
                    tool.eprint_prog(&[&path, b": reopen: ", text.as_bytes(), b"\n"]);
                    return EXIT_FAILURE;
                }
            },
            Opt::Flag(b'n') => o.no_sub_dir = true,
            Opt::Arg(b's', name) => o.subname = Some(name),
            Opt::Flag(b'N') => o.no_overwrite = true,
            // -Q: MDB_NOSYNC, which mdb_load always uses anyway.
            Opt::Flag(b'Q') => {}
            Opt::Flag(b'T') => o.plain = true,
            Opt::Invalid(msg) => {
                eprint_bytes(&[&msg]);
                return tool.usage(SYNOPSIS);
            }
            _ => return tool.usage(SYNOPSIS),
        }
    }
    let operands = getopt.operands();
    if operands.len() != 1 {
        return tool.usage(SYNOPSIS);
    }
    let envname = &operands[0];

    let reader: Box<dyn BufRead> = match o.input.take() {
        Some(f) => Box::new(BufReader::with_capacity(64 * 1024, f)),
        None => Box::new(BufReader::with_capacity(64 * 1024, io::stdin())),
    };
    let mut input = Input {
        reader,
        lineno: 0,
        eof: false,
        plain: o.plain,
        prog: tool.prog.clone(),
        line: Vec::new(),
    };
    let mut hdr = Header {
        subname: o.subname.take(),
        // -T: plain text is in the print encoding.
        print: o.plain,
        ..Header::default()
    };
    if !o.plain && input.read_header(&mut hdr).is_err() {
        return EXIT_FAILURE;
    }

    let mut flags = EnvironmentFlags::NO_SYNC;
    if o.no_sub_dir {
        flags |= EnvironmentFlags::NO_SUB_DIR;
    }
    if hdr.map_address != 0 {
        flags |= EnvironmentFlags::FIXED_MAP;
    }
    let mut builder = Environment::builder();
    builder.max_dbs(2).flags(flags);
    if hdr.max_readers != 0 {
        builder.max_readers(hdr.max_readers);
    }
    if hdr.map_size != 0 {
        builder.map_size(usize::try_from(hdr.map_size).unwrap_or(usize::MAX));
    }
    let env = match builder.open_with_permissions(path_of(envname), 0o664) {
        Ok(env) => env,
        Err(e) => {
            report_failed("mdb_env_open", &e);
            return EXIT_FAILURE;
        }
    };

    let mut loader = Loader {
        env: &env,
        input,
        append: o.append,
        put_flags: if o.no_overwrite {
            WriteFlags::NO_OVERWRITE | WriteFlags::NO_DUP_DATA
        } else {
            WriteFlags::empty()
        },
    };
    let mut first = true;
    while !loader.input.eof {
        if first {
            // The first header was read before opening the environment.
            // C resets its flags here (KNOWN_C_BUGS.md #1); we keep them.
            first = false;
        } else if !o.plain && loader.input.read_header(&mut hdr).is_err() {
            return EXIT_FAILURE;
        }
        if loader.load_db(&hdr).is_err() {
            return EXIT_FAILURE;
        }
    }
    0
}

/// The state of a load: the environment, the input and the put options.
struct Loader<'env> {
    env: &'env Environment,
    input: Input,
    append: bool,
    put_flags: WriteFlags,
}

/// A transaction and its cursor, renewed every [`BATCH`] records.
struct Batch<'env> {
    txn: RwTxn<'env>,
    cursor: CursorId,
}

impl<'env> Loader<'env> {
    /// One iteration of `mdb_load.c`'s main loop: load the records that
    /// follow a header into `hdr.subname`. `Err(())` after a message.
    fn load_db(&mut self, hdr: &Header) -> Result<(), ()> {
        let env = self.env;
        let mut txn = begin(env)?;
        let dbi = match create_db(
            &mut txn,
            hdr.subname.as_deref(),
            hdr.flags | DatabaseFlags::CREATE,
        ) {
            Ok(d) => d,
            Err(e) => {
                report_failed("mdb_dbi_open", &e);
                return Err(());
            }
        };
        let dupsort = hdr.flags.contains(DatabaseFlags::DUP_SORT);
        if self.append {
            // As C: records go in input order, without comparing.
            let _ = txn.set_compare(dbi, Some(greater));
            if dupsort {
                let _ = txn.set_dupsort(dbi, Some(greater));
            }
        }
        let mut batch = open_cursor(txn, dbi)?;

        // The previous key with -a on DUPSORT (C's `prevk`).
        let mut prev_key: Vec<u8> = Vec::new();
        let mut key = Vec::new();
        let mut data = Vec::new();
        let mut count = 0usize;
        loop {
            if self.input.read_value(&mut key, hdr.print).is_err() {
                break;
            }
            if self.input.read_value(&mut data, hdr.print).is_err() {
                self.input.message(b"failed to read key value\n");
                return Err(());
            }
            let mut flags = self.put_flags;
            if self.append {
                if dupsort && prev_key == key {
                    // Another duplicate of the key the cursor is on.
                    flags |= WriteFlags::CURRENT | WriteFlags::APPEND_DUP;
                } else {
                    flags |= WriteFlags::APPEND;
                    if dupsort {
                        prev_key.clone_from(&key);
                    }
                }
            }
            match batch.txn.cursor_put(batch.cursor, &key, &data, flags) {
                Ok(()) => {}
                Err(rlmdb::Error::Mdb(rlmdb::MdbError::KeyExist)) if !self.put_flags.is_empty() => {
                }
                Err(e) => {
                    let rc = err_code(&e);
                    self.input.message(
                        format!("mdb_cursor_put failed, error {rc} {}\n", strerror(rc)).as_bytes(),
                    );
                    return Err(());
                }
            }
            count += 1;
            if count == BATCH {
                self.commit(batch.txn)?;
                let txn = begin(env)?;
                batch = open_cursor(txn, dbi)?;
                if self.append {
                    if let Ok(Some((k, _))) =
                        batch
                            .txn
                            .cursor_get(batch.cursor, None, None, CursorOp::Last)
                    {
                        prev_key = k.to_vec();
                    }
                }
                count = 0;
            }
        }
        self.commit(batch.txn)?;
        // mdb_load always runs with MDB_NOSYNC and syncs once per database.
        if let Err(e) = env.sync(true) {
            report_failed("mdb_env_sync", &e);
            return Err(());
        }
        if self.append && dbi == Dbi::MAIN {
            let _ = env.set_compare(dbi, None);
            let _ = env.set_dupsort(dbi, None);
        }
        env.close_db(dbi);
        Ok(())
    }

    /// `mdb_txn_commit` with C's message.
    fn commit(&self, txn: RwTxn<'_>) -> Result<(), ()> {
        txn.commit().map_err(|e| {
            let text = strerror(err_code(&e));
            self.input
                .message(format!("txn_commit: {text}\n").as_bytes());
        })
    }
}

/// `mdb_txn_begin` with C's message.
fn begin(env: &Environment) -> Result<RwTxn<'_>, ()> {
    env.begin_rw_txn()
        .map_err(|e| report_failed("mdb_txn_begin", &e))
}

/// `mdb_cursor_open` with C's message.
fn open_cursor(mut txn: RwTxn<'_>, dbi: Dbi) -> Result<Batch<'_>, ()> {
    match txn.open_cursor(dbi) {
        Ok(cursor) => Ok(Batch { txn, cursor }),
        Err(e) => {
            report_failed("mdb_cursor_open", &e);
            Err(())
        }
    }
}
