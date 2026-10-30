//! The tools of `rlmdb-tools` (`mdb_stat`, `mdb_dump`, `mdb_load`,
//! `mdb_copy`, `mdb_drop`) in one executable, for the tests of this crate:
//! `rtools <tool> <args...>` runs `<tool>` with `argv[0] = <tool>`, so its
//! messages name it as the tool's own binary would. Cargo builds a
//! package's binaries for its integration tests (`CARGO_BIN_EXE_rtools`),
//! not those of other packages, and the C 1.0 tools the Rust ones are
//! compared with are built here.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use rlmdb_tools::common::Tool;
use rlmdb_tools::{copy, drop, dump, load, stat};

fn main() {
    let args: Vec<Vec<u8>> = std::env::args_os()
        .skip(1)
        .map(OsString::into_vec)
        .collect();
    let run: fn(&mut Tool, &[Vec<u8>]) -> i32 = match args.first().map(Vec::as_slice) {
        Some(b"mdb_stat") => stat::run,
        Some(b"mdb_dump") => dump::run,
        Some(b"mdb_load") => load::run,
        Some(b"mdb_copy") => copy::run,
        Some(b"mdb_drop") => drop::run,
        _ => {
            eprintln!("usage: rtools mdb_stat|mdb_dump|mdb_load|mdb_copy|mdb_drop [args...]");
            std::process::exit(2);
        }
    };
    let mut tool = Tool::new(&args);
    let code = run(&mut tool, &args);
    std::process::exit(tool.finish(code));
}
