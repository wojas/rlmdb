//! `mdb_copy`: copy an environment, optionally compacting it
//! (`libraries/liblmdb/mdb_copy.c`).
//!
//! ```text
//! mdb_copy [-V] [-c] [-n] srcpath [dstpath]
//! ```
//!
//! Without `dstpath` the copy (the data file only) is written to standard
//! output. Options are parsed by hand, as in C: only leading arguments that
//! are exactly `-V`, `-c` or `-n` are options; any other argument starting
//! with `-` makes the command line invalid.
//!
//! C installs no-op handlers for `SIGPIPE`, `SIGHUP`, `SIGINT` and
//! `SIGTERM`, so those signals do not interrupt it (a closed pipe is a
//! write error). The Rust tool ignores `SIGPIPE` too (the Rust runtime
//! does), but the other signals terminate it (installing handlers would
//! need `unsafe`).

use std::os::fd::AsFd;
use std::path::Path;

use rlmdb::{Environment, EnvironmentFlags, Error};

use crate::cfmt::{err_code, strerror};
use crate::common::{path_of, Tool, EXIT_FAILURE};
use crate::output::eprint_bytes;

/// The `main` of `mdb_copy`; returns the exit code.
pub fn run(tool: &mut Tool, args: &[Vec<u8>]) -> i32 {
    let mut flags = EnvironmentFlags::READ_ONLY;
    let mut compact = false;
    // C: for (; argc > 1 && argv[1][0] == '-'; argc--, argv++)
    let mut first = 1;
    let mut valid = true;
    while args.len() - first > 0 && args[first].first() == Some(&b'-') {
        match args[first].as_slice() {
            b"-n" => flags |= EnvironmentFlags::NO_SUB_DIR,
            b"-c" => compact = true,
            b"-V" => return tool.version(),
            _ => {
                valid = false;
                break;
            }
        }
        first += 1;
    }
    let rest = &args[first.min(args.len())..];
    if !valid || rest.is_empty() || rest.len() > 2 {
        return tool.usage("[-V] [-c] [-n] srcpath [dstpath]");
    }

    let mut act = "opening environment";
    let mut builder = Environment::builder();
    builder.flags(flags);
    let result = builder
        .open_with_permissions(path_of(&rest[0]), 0o600)
        .and_then(|env| {
            act = "copying";
            match rest.get(1) {
                Some(dst) => copy_to_path(&env, path_of(dst), compact),
                None => copy_to_stdout(&env, compact),
            }
        });
    if let Err(e) = result {
        let rc = err_code(&e);
        eprint_bytes(&[
            &tool.prog,
            format!(": {act} failed, error {rc} ({})\n", strerror(rc)).as_bytes(),
        ]);
        return EXIT_FAILURE;
    }
    0
}

/// `mdb_env_copy2(env, path, flags)`: `path` is the directory for the
/// copy's `data.mdb` (with `-n`, the file itself); it must not exist yet.
fn copy_to_path(env: &Environment, path: &Path, compact: bool) -> Result<(), Error> {
    env.copy_to_path(path, compact)
}

/// `mdb_env_copyfd2(env, 1, flags)`: the copy goes to standard output.
fn copy_to_stdout(env: &Environment, compact: bool) -> Result<(), Error> {
    env.copy_to_fd(std::io::stdout().as_fd(), compact)
}
