//! Pieces shared by all tools: the version string, the program context
//! (name and output), C-style messages and exit codes, and access to
//! databases by byte name.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use rlmdb::{Dbi, Environment, Error, Format, RoCursor, RwTxn, Stat, Transaction};

use crate::cfmt::{err_code, io_errno, strerror};
use crate::output::{eprint_bytes, Output};

/// What `-V` prints: the string `mdb_version` returns,
/// `LMDB 0.9.85: (rlmdb <version>)`
/// ([`rlmdb::VersionInfo::mdb_version_string`]). The C tools print their
/// library's `MDB_VERSION_STRING` the same way.
pub fn version_string() -> String {
    rlmdb::version_info().mdb_version_string()
}

/// `EXIT_FAILURE`.
pub const EXIT_FAILURE: i32 = 1;

/// The state every tool has: its name (`argv[0]`) and its standard output.
#[derive(Debug)]
pub struct Tool {
    /// `argv[0]`, used in messages as C uses it.
    pub prog: Vec<u8>,
    /// Standard output (`mdb_dump -f` replaces it).
    pub out: Output,
}

impl Tool {
    /// The context for `args` (`args[0]` is the program name).
    pub fn new(args: &[Vec<u8>]) -> Tool {
        Tool {
            prog: args.first().cloned().unwrap_or_default(),
            out: Output::stdout(),
        }
    }

    /// `fprintf(stderr, "usage: %s <synopsis>\n", prog)`; returns the exit
    /// code `EXIT_FAILURE`.
    pub fn usage(&self, synopsis: &str) -> i32 {
        eprint_bytes(&[b"usage: ", &self.prog, b" ", synopsis.as_bytes(), b"\n"]);
        EXIT_FAILURE
    }

    /// The version line for `-V`, as C's `printf("%s\n",
    /// MDB_VERSION_STRING)` (with [`version_string`]); returns exit code 0.
    pub fn version(&mut self) -> i32 {
        self.out.print(format_args!("{}\n", version_string()));
        0
    }

    /// Writes `"<prog>: "` followed by `parts` to standard error.
    pub fn eprint_prog(&self, parts: &[&[u8]]) {
        let mut all: Vec<&[u8]> = vec![&self.prog, b": "];
        all.extend_from_slice(parts);
        eprint_bytes(&all);
    }

    /// Flushes standard output and returns the process exit code. A
    /// failed write is reported (C ignores write errors; a broken pipe is
    /// quietly accepted, as C's tools die of `SIGPIPE` or ignore it).
    pub fn finish(self, code: i32) -> i32 {
        match self.out.finish() {
            Ok(()) => code,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => code,
            Err(e) => {
                let text = strerror(io_errno(&e));
                eprint_bytes(&[&self.prog, b": write error: ", text.as_bytes(), b"\n"]);
                EXIT_FAILURE
            }
        }
    }
}

/// Runs a tool's `run` function on the process arguments and returns the
/// exit code (the body of each binary's `main`).
pub fn run_main(run: fn(&mut Tool, &[Vec<u8>]) -> i32) -> i32 {
    let args: Vec<Vec<u8>> = std::env::args_os().map(arg_bytes).collect();
    let mut tool = Tool::new(&args);
    let code = run(&mut tool, &args);
    tool.finish(code)
}

/// `"<what> failed, error <rc> <mdb_strerror(rc)>\n"` on standard error,
/// the most common message of the C tools.
pub fn report_failed(what: &str, e: &Error) {
    report_failed_code(what, err_code(e));
}

/// As [`report_failed`] for a raw return code.
pub fn report_failed_code(what: &str, rc: i32) {
    eprint_bytes(&[format!("{what} failed, error {rc} {}\n", strerror(rc)).as_bytes()]);
}

/// The bytes of a command line argument: as given on Unix; on Windows,
/// where arguments are UTF-16, their UTF-8 form (an unpaired surrogate
/// becomes U+FFFD).
#[cfg(unix)]
fn arg_bytes(arg: OsString) -> Vec<u8> {
    std::os::unix::ffi::OsStringExt::into_vec(arg)
}

#[cfg(windows)]
fn arg_bytes(arg: OsString) -> Vec<u8> {
    arg.to_string_lossy().into_owned().into_bytes()
}

/// A path from command line bytes.
#[cfg(unix)]
pub fn path_of(bytes: &[u8]) -> &Path {
    Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(bytes))
}

/// A path from command line bytes, which are UTF-8 on Windows
/// ([`run_main`]); other bytes name no path that exists (U+FFFD).
#[cfg(windows)]
pub fn path_of(bytes: &[u8]) -> &Path {
    Path::new(std::str::from_utf8(bytes).unwrap_or("\u{fffd}"))
}

/// A database name as `mdb_dbi_open` sees it: C names are NUL-terminated,
/// so only the bytes before the first NUL count.
fn c_name(name: &[u8]) -> &[u8] {
    crate::cfmt::cstr(name)
}

/// `mdb_dbi_open(txn, name, 0)` in any transaction: the handle of an
/// existing database (`None` = the main DB).
pub fn open_db<T: Transaction>(txn: &T, name: Option<&[u8]>) -> Result<Dbi, Error> {
    txn.open_db_bytes(name.map(c_name))
}

/// `mdb_dbi_open(txn, name, flags)` in a write transaction (`flags` may
/// contain `MDB_CREATE`).
pub fn create_db(
    txn: &mut RwTxn<'_>,
    name: Option<&[u8]>,
    flags: rlmdb::DatabaseFlags,
) -> Result<Dbi, Error> {
    txn.create_db_bytes(name.map(c_name), flags)
}

// ---------------------------------------------------------------------------
// What differs between the LMDB 0.9 and 1.0 tools
// ---------------------------------------------------------------------------
//
// The tools work on environments of both formats. Where the C tools of the
// two releases print or decide differently, the tools behave like the
// release whose format the environment has: each C release is the oracle
// for its own format.

/// The name `mdb_stat` and `mdb_dump` give `mdb_dbi_open` in their message
/// when opening the database fails: C 0.9's tools call it through its old
/// name `mdb_open`, C 1.0's as `mdb_dbi_open`.
pub fn dbi_open_name(env: &Environment) -> &'static str {
    match env.format() {
        Format::V09 => "mdb_open",
        Format::V10 => "mdb_dbi_open",
    }
}

/// For `-a`: the name of the named database whose record `cursor` (on the
/// main DB) is on, as the C tools of the environment's format find it, or
/// `None` to skip the record. C 0.9 tries every key without a NUL byte
/// (`mdb_dbi_open` then decides). C 1.0 takes the records of named
/// databases (`mdb_cursor_is_db`), whose key is the name and its NUL, and
/// uses the key as a C string.
pub fn db_name_of<'k>(format: Format, cursor: &RoCursor<'_>, key: &'k [u8]) -> Option<&'k [u8]> {
    match format {
        Format::V09 => (!key.contains(&0)).then_some(key),
        Format::V10 => cursor.current_is_db().then(|| c_name(key)),
    }
}

/// `prstat()` of `mdb_stat.c`: the statistics block of a database. C 1.0's
/// starts with the page size (0.9 has that line under `#if 0`).
pub fn print_db_stat(out: &mut Output, format: Format, st: &Stat) {
    if format == Format::V10 {
        out.print(format_args!("  Page size: {}\n", st.page_size));
    }
    out.print(format_args!(
        "  Tree depth: {}\n  Branch pages: {}\n  Leaf pages: {}\n  Overflow pages: {}\n  Entries: {}\n",
        st.depth, st.branch_pages, st.leaf_pages, st.overflow_pages, st.entries
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version string has the shape of the C headers' and their major
    /// and minor version; the patch level is rlmdb's (above the header's,
    /// so that rlmdb can be told apart from C LMDB) and the rlmdb version
    /// stands where C has its release date.
    #[test]
    fn version_matches_lmdb_h() {
        let h = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/lmdb-c/0.9/lmdb.h"),
        )
        .unwrap();
        let field = |name: &str| {
            h.lines()
                .find_map(|l| l.strip_prefix(&format!("#define {name}")))
                .unwrap()
                .trim()
                .trim_matches('"')
                .to_owned()
        };
        let expected = format!(
            "LMDB {}.{}.{}: (rlmdb {})",
            field("MDB_VERSION_MAJOR"),
            field("MDB_VERSION_MINOR"),
            rlmdb::consts::MDB_VERSION_PATCH,
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(version_string(), expected);
        let c_patch: i32 = field("MDB_VERSION_PATCH").parse().unwrap();
        assert!(rlmdb::consts::MDB_VERSION_PATCH > c_patch);
    }
}
