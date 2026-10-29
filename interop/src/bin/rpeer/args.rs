//! Command line, exit codes, environment setup and small helpers (the
//! counterpart of `c/cpeer.c`).
//!
//! ```text
//! rpeer <command> <positional args...> [options...]
//! ```
//!
//! Options start with `--` and may appear anywhere after the command;
//! everything else (including a lone `-`, the main DB) is positional.

use std::fmt;
use std::time::Instant;

use lmdb_core::consts::MDB_REVERSEKEY;
use lmdb_core::consts::{
    MDB_DUPFIXED, MDB_DUPSORT, MDB_INTEGERDUP, MDB_INTEGERKEY, MDB_REVERSEDUP,
};
use lmdb_core::{Environment, EnvironmentFlags};

use crate::dataset::DataSet;

// ---- Exit codes and failures ----------------------------------------------------

/// A data or invariant check failed.
pub const EXIT_MISMATCH: u8 = 1;
/// Bad command line.
pub const EXIT_USAGE: u8 = 2;
/// An LMDB call failed unexpectedly.
pub const EXIT_LMDB: u8 = 3;
/// The command exists in the protocol but rpeer does not implement it yet
/// (it needs a part of `lmdb_core` that does not exist yet).
pub const EXIT_NOT_IMPLEMENTED: u8 = 4;

/// Why a command stopped early: the process exit status plus a message for
/// stderr (`rpeer: <message>`), like C's `die()`.
///
/// Commands return `Result<(), Fail>` instead of exiting on the spot, so
/// that every `Environment` and transaction is dropped (reader slots
/// cleared, files closed) before the process exits.
#[derive(Debug)]
pub struct Fail {
    /// Exit status.
    pub code: u8,
    /// Message, or `None` if the command already printed its diagnostics.
    pub message: Option<String>,
}

impl Fail {
    /// Exit with `code` and `message`.
    pub fn new(code: u8, message: impl Into<String>) -> Fail {
        Fail {
            code,
            message: Some(message.into()),
        }
    }

    /// [`EXIT_MISMATCH`].
    pub fn mismatch(message: impl Into<String>) -> Fail {
        Fail::new(EXIT_MISMATCH, message)
    }

    /// [`EXIT_USAGE`].
    pub fn usage(message: impl Into<String>) -> Fail {
        Fail::new(EXIT_USAGE, message)
    }

    /// [`EXIT_LMDB`]: `<what> failed: <code> <mdb_strerror text>`, the
    /// message C's `check_rc()` prints.
    pub fn lmdb(what: &str, err: &lmdb_core::Error) -> Fail {
        Fail::new(
            EXIT_LMDB,
            format!("{what} failed: {} {err}", err.to_err_code()),
        )
    }

    /// [`EXIT_NOT_IMPLEMENTED`]: `command` needs a part of `lmdb_core` that
    /// does not exist yet. (Every command is implemented now; this stays
    /// for protocol additions that run ahead of the library.)
    #[allow(dead_code)]
    pub fn not_implemented(command: &str) -> Fail {
        Fail::new(
            EXIT_NOT_IMPLEMENTED,
            format!("NOT-IMPLEMENTED {command}: not implemented in rpeer yet"),
        )
    }

    /// Exit with `code`; the diagnostics are already on stderr.
    pub fn quiet(code: u8) -> Fail {
        Fail {
            code,
            message: None,
        }
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(m) => f.write_str(m),
            None => write!(f, "exit status {}", self.code),
        }
    }
}

impl From<std::io::Error> for Fail {
    /// Writing the output failed (e.g. the harness closed the pipe).
    fn from(e: std::io::Error) -> Fail {
        Fail::new(EXIT_LMDB, format!("I/O error: {e}"))
    }
}

/// The result of a command.
pub type CmdResult<T = ()> = Result<T, Fail>;

/// `CHECK(call)`: turn an LMDB error into an [`EXIT_LMDB`] failure naming
/// the call.
pub trait Check<T> {
    /// `Ok` passes through; an error becomes [`Fail::lmdb`]`(what, ..)`.
    fn check(self, what: &str) -> CmdResult<T>;
}

impl<T> Check<T> for lmdb_core::Result<T> {
    fn check(self, what: &str) -> CmdResult<T> {
        self.map_err(|e| Fail::lmdb(what, &e))
    }
}

/// `CHECK` for cursor operations: `Ok(None)` is `MDB_NOTFOUND`, which is a
/// failure where C uses `CHECK(mdb_cursor_get(...))`.
pub fn found<T>(r: lmdb_core::Result<Option<T>>, what: &str) -> CmdResult<T> {
    match r {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(Fail::lmdb(what, &lmdb_core::MdbError::NotFound.into())),
        Err(e) => Err(Fail::lmdb(what, &e)),
    }
}

// ---- Options -----------------------------------------------------------------------

/// Most positional arguments any command takes (`MAX_POS`).
const MAX_POS: usize = 8;

/// The parsed command line (`struct opts`).
#[derive(Debug, Default)]
pub struct Opts {
    /// The command.
    pub cmd: String,
    /// Positional arguments after the command.
    pub pos: Vec<String>,

    // Environment options, valid for every command.
    /// `--nosubdir`: `MDB_NOSUBDIR`.
    pub nosubdir: bool,
    /// `--notls`: `MDB_NOTLS` (the Rust library always behaves like it).
    pub notls: bool,
    /// `--nosync`: `MDB_NOSYNC`.
    pub nosync: bool,
    /// `--nometasync`: `MDB_NOMETASYNC`.
    pub nometasync: bool,
    /// `--mapsize N`; 0 keeps the environment's size.
    pub mapsize: u64,
    /// `--maxdbs N` (default 64).
    pub maxdbs: u32,
    /// `--maxreaders N`; 0 keeps LMDB's default (126).
    pub maxreaders: u32,

    // Data set and command options.
    /// The data set options (`--dupsort`, `--valsize`, ...); the positional
    /// part is filled in by [`Opts::dataset`].
    pub ds: DataSet,
    /// `--per-txn N` (fill/delete).
    pub per_txn: u32,
    /// `--every N` (delete; default 2).
    pub every: u32,
    /// `--append` (fill).
    pub append: bool,
    /// `--reserve` (fill).
    pub reserve: bool,
    /// `--compact` (copy).
    pub compact: bool,
    /// `--all` (dump).
    pub all: bool,
    /// `--until-stdin` (hold-reader).
    pub until_stdin: bool,
    /// `--check` (nested-test).
    pub check: bool,
    /// `--delete` (drop).
    pub delete: bool,
    /// `--keys N` (writer-loop; default 500).
    pub keys: u32,
    /// `--max-commits N` (writer-loop; 0 = no limit).
    pub max_commits: u64,
}

impl Opts {
    /// `parse_args`: split `args` (without the program name) into the
    /// command, positional arguments and options.
    pub fn parse(args: &[String]) -> CmdResult<Opts> {
        let mut o = Opts {
            maxdbs: 64,
            every: 2,
            keys: 500,
            ..Opts::default()
        };
        let mut it = args.iter();
        o.cmd = it.next().cloned().unwrap_or_default();
        while let Some(a) = it.next() {
            if !a.starts_with("--") {
                if o.pos.len() == MAX_POS {
                    return Err(Fail::usage("too many arguments"));
                }
                o.pos.push(a.clone());
                continue;
            }
            let mut value = |what: &str| -> CmdResult<&String> {
                it.next()
                    .ok_or_else(|| Fail::usage(format!("option {what} needs a value")))
            };
            match a.as_str() {
                // environment options
                "--nosubdir" => o.nosubdir = true,
                "--notls" => o.notls = true,
                "--nosync" => o.nosync = true,
                "--nometasync" => o.nometasync = true,
                "--mapsize" => o.mapsize = parse_u64(value(a)?, a)?,
                "--maxdbs" => o.maxdbs = parse_u32(value(a)?, a)?,
                "--maxreaders" => o.maxreaders = parse_u32(value(a)?, a)?,
                // data set options
                "--dupsort" => o.ds.flags |= MDB_DUPSORT,
                "--dupfixed" => o.ds.flags |= MDB_DUPSORT | MDB_DUPFIXED,
                "--intkey" => o.ds.flags |= MDB_INTEGERKEY,
                "--intdup" => o.ds.flags |= MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP,
                "--revkey" => o.ds.flags |= MDB_REVERSEKEY,
                "--revdup" => o.ds.flags |= MDB_DUPSORT | MDB_REVERSEDUP,
                "--valsize" => o.ds.valsize = parse_u32(value(a)?, a)?,
                "--big" => o.ds.big = parse_u32(value(a)?, a)?,
                "--dups" => o.ds.dups = parse_u32(value(a)?, a)?,
                "--deleted-every" => o.ds.deleted_every = parse_u32(value(a)?, a)?,
                // command options
                "--per-txn" => o.per_txn = parse_u32(value(a)?, a)?,
                "--every" => o.every = parse_u32(value(a)?, a)?,
                "--keys" => o.keys = parse_u32(value(a)?, a)?,
                "--max-commits" => o.max_commits = parse_u64(value(a)?, a)?,
                "--append" => o.append = true,
                "--reserve" => o.reserve = true,
                "--compact" => o.compact = true,
                "--all" => o.all = true,
                "--until-stdin" => o.until_stdin = true,
                "--check" => o.check = true,
                "--delete" => o.delete = true,
                _ => return Err(Fail::usage(format!("unknown option {a}"))),
            }
        }
        Ok(o)
    }

    /// `parse_dataset_args`: complete [`Opts::ds`] from the positional
    /// arguments `<db|-> <count> <seed>` starting at `pos[first]`.
    pub fn dataset(&mut self, first: usize) -> CmdResult<DataSet> {
        let arg = |k: usize| self.pos.get(first + k).map(String::as_str);
        let db = arg(0).ok_or_else(|| Fail::usage("missing db"))?;
        self.ds.db = (db != "-").then(|| db.to_owned());
        self.ds.count = parse_u32_opt(arg(1), "count")?;
        self.ds.seed = parse_u32_opt(arg(2), "seed")?;
        self.ds.finish().map_err(Fail::usage)?;
        Ok(self.ds.clone())
    }

    /// The environment path (`pos[0]`).
    pub fn env_path(&self) -> &str {
        self.pos.first().map(String::as_str).unwrap_or_default()
    }

    /// `open_env`: open the environment named by `pos[0]` with the global
    /// options, read-only if `read_only`.
    pub fn open_env(&self, read_only: bool) -> CmdResult<Environment> {
        let mut flags = EnvironmentFlags::empty();
        flags.set(EnvironmentFlags::READ_ONLY, read_only);
        flags.set(EnvironmentFlags::NO_SUB_DIR, self.nosubdir);
        // Accepted for symmetry with C; read transactions of the Rust
        // library are never bound to a thread anyway.
        flags.set(EnvironmentFlags::NO_TLS, self.notls);
        flags.set(EnvironmentFlags::NO_SYNC, self.nosync);
        flags.set(EnvironmentFlags::NO_META_SYNC, self.nometasync);

        let mut builder = Environment::builder();
        builder.max_dbs(self.maxdbs).flags(flags);
        if self.mapsize != 0 {
            let size = usize::try_from(self.mapsize)
                .map_err(|_| Fail::usage(format!("--mapsize {} too large", self.mapsize)))?;
            builder.map_size(size);
        }
        if self.maxreaders != 0 {
            builder.max_readers(self.maxreaders);
        }
        let path = self.env_path();
        builder
            .open_with_permissions(path, 0o664)
            .check(&format!("mdb_env_open({path})"))
    }
}

// ---- Number parsing ------------------------------------------------------------------

/// `parse_u64`: like `strtoull(s, &end, 0)` with the whole string consumed:
/// decimal, `0x` hex or leading-`0` octal.
pub fn parse_u64(s: &str, what: &str) -> CmdResult<u64> {
    if s.is_empty() {
        return Err(Fail::usage(format!("missing {what}")));
    }
    let (digits, radix) = if let Some(hex) = s.strip_prefix("0x").or(s.strip_prefix("0X")) {
        (hex, 16)
    } else if s.len() > 1 && s.starts_with('0') {
        (&s[1..], 8)
    } else {
        (s, 10)
    };
    u64::from_str_radix(digits, radix)
        .ok()
        .filter(|_| !digits.starts_with('+'))
        .ok_or_else(|| Fail::usage(format!("bad {what}: '{s}'")))
}

/// `parse_u32`: [`parse_u64`] limited to `u32`.
pub fn parse_u32(s: &str, what: &str) -> CmdResult<u32> {
    let v = parse_u64(s, what)?;
    u32::try_from(v).map_err(|_| Fail::usage(format!("{what} out of range: '{s}'")))
}

/// [`parse_u32`] of an optional argument (`missing <what>` if absent).
pub fn parse_u32_opt(s: Option<&str>, what: &str) -> CmdResult<u32> {
    parse_u32(s.unwrap_or_default(), what)
}

/// `parse_seconds`: a non-negative number of seconds (`strtod`).
pub fn parse_seconds(s: Option<&str>) -> CmdResult<f64> {
    let s = s.ok_or_else(|| Fail::usage("missing seconds"))?;
    match s.parse::<f64>() {
        Ok(v) if v >= 0.0 => Ok(v),
        _ => Err(Fail::usage(format!("bad seconds: '{s}'"))),
    }
}

/// The value of a counter as C's `strtoull(buf, NULL, 10)` reads it:
/// leading whitespace, an optional `+`, then decimal digits up to the
/// first non-digit (0 if there are none; saturating on overflow).
pub fn strtoull10(bytes: &[u8]) -> u64 {
    let s = bytes.trim_ascii_start();
    let s = s.strip_prefix(b"+").unwrap_or(s);
    s.iter()
        .take_while(|b| b.is_ascii_digit())
        .fold(0u64, |n, b| {
            n.saturating_mul(10).saturating_add(u64::from(b - b'0'))
        })
}

// ---- Time --------------------------------------------------------------------------

/// `now_seconds`: seconds on a monotonic clock since the first call.
pub fn now_seconds() -> f64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// `sleep_seconds`.
pub fn sleep_seconds(s: f64) {
    if s > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(s));
    }
}

// ---- Output helpers ------------------------------------------------------------------

/// `hex_encode`: lower-case hex digits of `data`.
pub fn hex(data: &[u8]) -> String {
    use fmt::Write;
    let mut s = String::with_capacity(2 * data.len());
    for b in data {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `short_hex`: up to 40 bytes as hex, `...` if longer (error messages).
pub fn short_hex(data: &[u8]) -> String {
    let mut s = hex(&data[..data.len().min(40)]);
    if data.len() > 40 {
        s.push_str("...");
    }
    s
}

/// The bytes of a key as text for messages (C prints them with `%.*s`).
pub fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
