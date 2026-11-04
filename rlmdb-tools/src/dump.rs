//! `mdb_dump`: write databases in the portable dump format that `mdb_load`
//! reads (`tests/lmdb-c/0.9/mdb_dump.c`; the format is described in
//! [`crate::dumpfmt`]).
//!
//! ```text
//! mdb_dump [-V] [-f output] [-i txnid] [-l] [-n] [-p] [-v] [-a|-s subdb] dbpath
//! ```
//!
//! `-v` (C 1.0's option, here for both formats) dumps the previous
//! snapshot (`MDB_PREVSNAPSHOT`), without C 1.0's effect on other
//! processes (`KNOWN_C_BUGS.md` row 68).
//!
//! Output and messages are those of the C tool of the environment's format.
//! The dump format is the same for LMDB 0.9 and 1.0 environments; for a 1.0
//! one the messages are 1.0's (`mdb_dbi_open`), and `-a`/`-l` list the
//! records of named databases (`mdb_cursor_is_db`; 1.0 stores a name with
//! its NUL, which 0.9's tool would skip).
//!
//! `-i txnid` is LMDB 1.0's: instead of the text dump it writes an
//! incremental dump since transaction `txnid`
//! ([`rlmdb::Environment::incr_dump`]: C 1.0's bytes but where C's walk
//! takes stale free pages for pages, `KNOWN_C_BUGS.md` row 72) to
//! standard output or, with
//! `-f`, into a new file (`mdb_env_incr_dump`, which refuses an existing
//! one), as `tests/lmdb-c/1.0/mdb_dump.c` does: the txnid is read as
//! `sscanf("%zu")` reads it (`5x` is 5, `-1` is `SIZE_MAX`), and one that
//! is not a number or is 0 is "invalid txnid" with the C library's
//! `strerror(errno)` of the rejected txnid's parse, 0
//! ([`crate::cfmt::crt_strerror_0`]; C's tool keeps an earlier `-i`'s
//! `ERANGE` there with the BSD C library and on Windows,
//! `KNOWN_C_BUGS.md` row 76); the other options but `-n` are ignored. With `-i` the options follow
//! 1.0's tool, which opens `-f`'s file only after the environment; without
//! it, 0.9's, which reopens standard output on it when it reads the
//! option. With `-v` the incremental dump is one of the previous
//! snapshot (C 1.0's holds the latest commit's meta page and pages,
//! `KNOWN_C_BUGS.md` row 72). An LMDB 0.9 environment cannot be dumped
//! this way:
//! "mdb_env_incr_dump failed, error 45 Operation not supported" (the
//! number is the platform's `ENOTSUP`).
//!
//! C installs handlers for `SIGPIPE`, `SIGHUP`, `SIGINT` and `SIGTERM` that
//! make the dump stop after the current record with "Interrupted system
//! call". The Rust tool reproduces the `SIGPIPE` case (a closed pipe on
//! standard output), which is the common one; the other signals terminate
//! it immediately (installing handlers would need `unsafe`). Write errors
//! other than a closed pipe, which C ignores, are reported. Windows has no
//! `SIGPIPE`: there C writes on to a closed pipe, its writes failing, and
//! exits as if the dump had succeeded; so does the Rust tool.

use std::fs::OpenOptions;

use rlmdb::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, RoTxn, Transaction};

use crate::cfmt::{crt_errno, crt_strerror_0, err_code, io_errno, pointer, scan_size, strerror};
use crate::common::{
    db_name_of, dbi_open_name, open_db, path_of, report_failed, Tool, EXIT_FAILURE,
};
use crate::dumpfmt::encode_line;
use crate::getopt::{GetOpt, Opt};
use crate::output::{eprint_bytes, Output};

const SYNOPSIS: &str = "[-V] [-f output] [-i txnid] [-l] [-n] [-p] [-v] [-a|-s subdb] dbpath";

/// The options `getopt` takes.
const OPTSTRING: &str = "af:i:lnps:vV";

const NOTFOUND: i32 = rlmdb::consts::MDB_NOTFOUND;
const EINTR: i32 = 4;
const EPIPE: i32 = 32;

/// The database flags `mdb_dump` writes as `name=1` header lines, in its
/// order (`dbflags[]` in `mdb_dump.c`; `mdb_load.c` reads the same names).
pub const DB_FLAG_NAMES: [(DatabaseFlags, &str); 6] = [
    (DatabaseFlags::REVERSE_KEY, "reversekey"),
    (DatabaseFlags::DUP_SORT, "dupsort"),
    (DatabaseFlags::INTEGER_KEY, "integerkey"),
    (DatabaseFlags::DUP_FIXED, "dupfixed"),
    (DatabaseFlags::INTEGER_DUP, "integerdup"),
    (DatabaseFlags::REVERSE_DUP, "reversedup"),
];

/// Command line options.
#[derive(Debug, Default)]
struct Options {
    all_dbs: u32,
    list: bool,
    no_sub_dir: bool,
    print: bool,
    /// `-v`: `MDB_PREVSNAPSHOT`.
    prev_snapshot: bool,
    subname: Option<Vec<u8>>,
    /// `-i`: the incremental dump since this txnid (never 0).
    txnid: Option<u64>,
    /// `-f` with `-i`: the file for the incremental dump.
    outname: Option<Vec<u8>>,
}

/// The `main` of `mdb_dump`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    if args.len() < 2 {
        return tool.usage(SYNOPSIS);
    }
    let mut o = Options::default();
    // With -i anywhere, -f names the file of the incremental dump (C 1.0
    // opens it after the environment); without, C 0.9's tool reopens
    // standard output on it at once.
    let incremental = {
        let mut scan = GetOpt::new(args, OPTSTRING);
        let mut found = false;
        while let Some(opt) = scan.next() {
            found |= matches!(opt, Opt::Arg(b'i', _));
        }
        found
    };
    let mut getopt = GetOpt::new(args, OPTSTRING);
    while let Some(opt) = getopt.next() {
        match opt {
            Opt::Flag(b'V') => return tool.version(),
            Opt::Flag(c @ (b'l' | b'a')) => {
                if c == b'l' {
                    o.list = true;
                }
                if o.subname.is_some() {
                    return tool.usage(SYNOPSIS);
                }
                o.all_dbs += 1;
            }
            Opt::Arg(b'f', path) if incremental => o.outname = Some(path),
            Opt::Arg(b'i', arg) => match scan_size(&arg) {
                Some(txnid) if txnid != 0 => o.txnid = Some(txnid),
                _ => {
                    // The parse of this txnid set no errno (one that
                    // overflows is SIZE_MAX, a valid txnid).
                    let text = crt_strerror_0();
                    tool.eprint_prog(&[&arg, b": invalid txnid: ", text.as_bytes(), b"\n"]);
                    return EXIT_FAILURE;
                }
            },
            Opt::Arg(b'f', path) => {
                // freopen(optarg, "w", stdout)
                let mut opts = OpenOptions::new();
                opts.write(true).create(true).truncate(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o666);
                match opts.open(path_of(&path)) {
                    Ok(file) => tool.out = Output::file(file),
                    Err(e) => {
                        let text = strerror(crt_errno(&e));
                        tool.eprint_prog(&[&path, b": reopen: ", text.as_bytes(), b"\n"]);
                        return EXIT_FAILURE;
                    }
                }
            }
            Opt::Flag(b'n') => o.no_sub_dir = true,
            Opt::Flag(b'p') => o.print = true,
            Opt::Flag(b'v') => o.prev_snapshot = true,
            Opt::Arg(b's', name) => {
                if o.all_dbs > 0 {
                    return tool.usage(SYNOPSIS);
                }
                o.subname = Some(name);
            }
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

    let mut builder = Environment::builder();
    if o.all_dbs > 0 || o.subname.is_some() {
        builder.max_dbs(2);
    }
    let mut flags = EnvironmentFlags::READ_ONLY;
    if o.no_sub_dir {
        flags |= EnvironmentFlags::NO_SUB_DIR;
    }
    if o.prev_snapshot {
        flags |= EnvironmentFlags::PREV_SNAPSHOT;
    }
    builder.flags(flags);
    let env = match builder.open_with_permissions(path_of(envname), 0o664) {
        Ok(env) => env,
        Err(e) => {
            report_failed("mdb_env_open", &e);
            return EXIT_FAILURE;
        }
    };
    if let Some(txnid) = o.txnid {
        let rc = match &o.outname {
            Some(name) => env.incr_dump_to_path(path_of(name), txnid),
            None => incr_dump_to_stdout(&env, txnid),
        };
        return match rc {
            Ok(()) => 0,
            Err(e) => {
                report_failed("mdb_env_incr_dump", &e);
                EXIT_FAILURE
            }
        };
    }
    let txn = match env.begin_ro_txn() {
        Ok(t) => t,
        Err(e) => {
            report_failed("mdb_txn_begin", &e);
            return EXIT_FAILURE;
        }
    };
    let dbi = match open_db(&txn, o.subname.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            report_failed(dbi_open_name(&env), &e);
            return EXIT_FAILURE;
        }
    };

    let rc = if o.all_dbs > 0 {
        dump_all(tool, &env, &txn, dbi, &o, envname)
    } else {
        dump_db(
            &mut tool.out,
            &env,
            &txn,
            dbi,
            o.subname.as_deref(),
            o.print,
        )
    };
    if rc != 0 && rc != NOTFOUND {
        let text = strerror(rc);
        tool.eprint_prog(&[envname, b": ", text.as_bytes(), b"\n"]);
    }
    if rc != 0 {
        EXIT_FAILURE
    } else {
        0
    }
}

/// `mdb_env_incr_dumpfd(env, MDB_STDOUT, txnid)`: the incremental dump
/// goes to standard output's descriptor (on Windows its handle), not
/// through the text output.
fn incr_dump_to_stdout(env: &Environment, txnid: u64) -> Result<(), rlmdb::Error> {
    #[cfg(unix)]
    let fd = std::os::fd::AsFd::as_fd(&std::io::stdout()).try_clone_to_owned()?;
    #[cfg(windows)]
    let fd = std::os::windows::io::AsHandle::as_handle(&std::io::stdout()).try_clone_to_owned()?;
    env.incr_dump(std::fs::File::from(fd), txnid)
}

/// `-a` / `-l`: dump (or list) every named database of the main DB.
fn dump_all(
    tool: &mut Tool,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    o: &Options,
    envname: &[u8],
) -> i32 {
    let mut cursor = match txn.cursor(dbi) {
        Ok(c) => c,
        Err(e) => {
            report_failed("mdb_cursor_open", &e);
            return err_code(&e);
        }
    };
    let format = env.format();
    let mut count = 0usize;
    let mut rc = loop {
        let key = match cursor.next_nodup() {
            Ok(Some((key, _))) => key,
            Ok(None) => break NOTFOUND,
            Err(e) => break err_code(&e),
        };
        let Some(name) = db_name_of(format, &cursor, key) else {
            continue;
        };
        // Counted even if it turns out not to be a database, as in C.
        count += 1;
        let Ok(db2) = open_db(txn, Some(name)) else {
            continue;
        };
        if o.list {
            tool.out.write(name);
            tool.out.write(b"\n");
        } else {
            let rc = dump_db(&mut tool.out, env, txn, db2, Some(name), o.print);
            if rc != 0 {
                env.close_db(db2);
                break rc;
            }
        }
        env.close_db(db2);
    };
    if count == 0 {
        tool.eprint_prog(&[envname, b" does not contain multiple databases\n"]);
        rc = NOTFOUND;
    } else if rc == NOTFOUND {
        rc = 0;
    }
    rc
}

/// `dumpit()`: the header and all records of one database. Returns C's
/// return code (0 on success).
fn dump_db(
    out: &mut Output,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    name: Option<&[u8]>,
    print: bool,
) -> i32 {
    match dump_db_inner(out, env, txn, dbi, name, print) {
        Ok(()) => 0,
        Err(rc) => rc,
    }
}

fn dump_db_inner(
    out: &mut Output,
    env: &Environment,
    txn: &RoTxn<'_>,
    dbi: Dbi,
    name: Option<&[u8]>,
    print: bool,
) -> Result<(), i32> {
    let code = |e: rlmdb::Error| err_code(&e);
    let flags = txn.db_flags(dbi).map_err(code)?;
    let st = txn.stat(dbi).map_err(code)?;
    let info = env.info().map_err(code)?;

    let mut header = Vec::with_capacity(256);
    header.extend_from_slice(b"VERSION=3\n");
    header.extend_from_slice(if print {
        b"format=print\n"
    } else {
        b"format=bytevalue\n"
    });
    if let Some(name) = name {
        header.extend_from_slice(b"database=");
        header.extend_from_slice(name);
        header.push(b'\n');
    }
    header.extend_from_slice(format!("type=btree\nmapsize={}\n", info.map_size).as_bytes());
    if info.map_address != 0 {
        header.extend_from_slice(
            format!("mapaddr={}\n", pointer(info.map_address as u64)).as_bytes(),
        );
    }
    header.extend_from_slice(format!("maxreaders={}\n", info.max_readers).as_bytes());
    if flags.contains(DatabaseFlags::DUP_SORT) {
        header.extend_from_slice(b"duplicates=1\n");
    }
    for (flag, flag_name) in DB_FLAG_NAMES {
        if flags.contains(flag) {
            header.extend_from_slice(format!("{flag_name}=1\n").as_bytes());
        }
    }
    header.extend_from_slice(format!("db_pagesize={}\nHEADER=END\n", st.page_size).as_bytes());
    out.write(&header);

    let mut cursor = txn.cursor(dbi).map_err(code)?;
    let mut line = Vec::new();
    let rc = loop {
        let (key, data) = match cursor.next() {
            Ok(Some(kv)) => kv,
            Ok(None) => break NOTFOUND,
            Err(e) => break err_code(&e),
        };
        // C checks its signal flag here; a closed pipe raised SIGPIPE
        // (not on Windows, see the module documentation).
        if let Some(e) = out.error() {
            if !(cfg!(windows) && e.kind() == std::io::ErrorKind::BrokenPipe) {
                let errno = io_errno(e);
                out.set_reported();
                break if errno == EPIPE { EINTR } else { errno };
            }
        }
        line.clear();
        encode_line(&mut line, key, print);
        encode_line(&mut line, data, print);
        out.write(&line);
    };
    out.write(b"DATA=END\n");
    if rc == NOTFOUND {
        Ok(())
    } else {
        Err(rc)
    }
}
