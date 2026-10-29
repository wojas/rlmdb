//! Pieces shared by all tools: the version string, the program context
//! (name and output), C-style messages and exit codes, and access to
//! databases by byte name.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use lmdb_core::{Dbi, Error, RwTxn, Transaction};

use crate::cfmt::{err_code, io_errno, strerror};
use crate::output::{eprint_bytes, Output};

/// `MDB_VERSION_STRING` of LMDB 0.9.36, which `-V` prints.
pub const VERSION_STRING: &str = "LMDB 0.9.36: (Aug 6, 2026)";

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

    /// `printf("%s\n", MDB_VERSION_STRING)` for `-V`; returns exit code 0.
    pub fn version(&mut self) -> i32 {
        self.out.print(format_args!("{VERSION_STRING}\n"));
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
    let args: Vec<Vec<u8>> = std::env::args_os().map(OsString::into_vec).collect();
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

/// A path from command line bytes.
pub fn path_of(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
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
    flags: lmdb_core::DatabaseFlags,
) -> Result<Dbi, Error> {
    txn.create_db_bytes(name.map(c_name), flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version string is the one of the C headers this crate mirrors.
    #[test]
    fn version_matches_lmdb_h() {
        let h = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../libraries/liblmdb/lmdb.h"),
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
            "LMDB {}.{}.{}: ({})",
            field("MDB_VERSION_MAJOR"),
            field("MDB_VERSION_MINOR"),
            field("MDB_VERSION_PATCH"),
            field("MDB_VERSION_DATE")
        );
        assert_eq!(VERSION_STRING, expected);
    }
}
