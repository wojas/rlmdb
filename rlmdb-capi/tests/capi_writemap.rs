//! `tests/c/capi_writemap.c`: `MDB_WRITEMAP` through the C API.
//!
//! rlmdb does not use a writable map, but keeps what the flag changes
//! through the API as C does: `mdb_env_get_flags` reports it,
//! `mdb_env_set_flags` cannot change it, a read-only open drops it, and
//! nested transactions are refused (`MDB_BAD_TXN`, C 0.9's code, also in
//! the LMDB 1.0 stub, where C 1.0 says `EINVAL`).
//!
//! The program runs against C LMDB 0.9 and 1.0 (the oracles) and against
//! the Rust libraries (`librlmdb` with 0.9's and with its own headers, on
//! both formats, and the 1.0 stub); every run must report no failures and
//! print the oracle's trace (`T ...` lines). It writes the same into an
//! environment opened with `MDB_WRITEMAP` and into one opened without,
//! and copies both (`mdb_env_copy2` with `MDB_CP_COMPACT`): the Rust
//! builds must leave the same files with and without the flag, byte for
//! byte, and C must read them all (`mdb_stat`, the C peer's page audit)
//! and dump the oracle's contents from them. (C with the flag extends the
//! data file to the map size; rlmdb does not, see `COMPATIBILITY.md`. The
//! bytes C leaves uninitialised differ between C and rlmdb as they do
//! without the flag, so the files are not compared with C's.)

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

/// The environments the program leaves in its scratch directory.
const ENVS: [&str; 4] = ["writemap", "plain", "writemap-copy", "plain-copy"];

fn run_capi_writemap(lib: Lib, format: &str) -> (String, PathBuf) {
    let b = builder();
    let exe = b.test_program("capi_writemap", lib);
    let scratch = b.scratch(&format!("writemap-{}-{format}", lib.name()));
    let out = Command::new(&exe)
        .arg(&scratch)
        .arg(format)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_writemap ({lib:?}, {format}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_writemap against the {lib:?} library ({format}) failed ({})",
        out.status
    );
    assert!(
        stdout.trim_end().ends_with(" 0 failures"),
        "capi_writemap ({lib:?}, {format}): {stdout}"
    );
    (stdout, scratch)
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

fn data_file(scratch: &Path, env: &str) -> Vec<u8> {
    std::fs::read(scratch.join(env).join("data.mdb")).unwrap()
}

/// `lib`'s C tools and peer read every environment in `scratch`:
/// `mdb_stat -a` and the page audit (`cpeer check-freelist`) succeed, and
/// `mdb_dump` prints what it prints for the oracle's environment (or
/// copy) without the flag (in `oracle`, written by `lib` itself).
fn c_reads_environments(scratch: &Path, oracle: &Path, lib: Lib) {
    let b = builder();
    let mdb_stat = b.tool("mdb_stat", lib);
    let mdb_dump = b.tool("mdb_dump", lib);
    let cpeer = b.cpeer(lib);
    // The main DB (`mdb_dump` alone) and the named ones (`-a`), without
    // the `mapaddr` line (C 1.0 reports the address of its map).
    let dump = |path: &Path| {
        let mut text = String::new();
        for args in [&[][..], &["-a"][..]] {
            let out = run_ok(command_as(&mdb_dump, "mdb_dump").args(args).arg(path));
            for line in String::from_utf8(out.stdout).unwrap().lines() {
                if !line.starts_with("mapaddr=") {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
        text
    };
    for env in ENVS {
        let path = scratch.join(env);
        run_ok(command_as(&mdb_stat, "mdb_stat").arg("-a").arg(&path));
        run_ok(Command::new(&cpeer).arg("check-freelist").arg(&path));
        // (The main DB's dump holds the named DB's record, whose root page
        // a compacting copy moves.)
        let want = if env.ends_with("-copy") {
            "plain-copy"
        } else {
            "plain"
        };
        assert_eq!(
            dump(&path),
            dump(&oracle.join(want)),
            "{lib:?}: mdb_dump {env}"
        );
    }
}

#[test]
fn writemap_limits_match_c() {
    let (c09, c09_scratch) = run_capi_writemap(Lib::C, "0.9");
    let (c10, c10_scratch) = run_capi_writemap(Lib::C10, "1.0");
    assert!(c10.starts_with("library: LMDB 1.0."), "{c10}");
    let t09 = trace(&c09);
    assert!(t09.len() > 30, "{c09}");
    // Both C releases print the same trace (the return codes that differ
    // are checked, not traced).
    assert_eq!(t09, trace(&c10));

    for (lib, format) in [
        (Lib::Rust, "0.9"),
        (Lib::RustApi, "0.9"),
        (Lib::RustApi, "1.0"),
        (Lib::Rust10, "1.0"),
    ] {
        let (rust, scratch) = run_capi_writemap(lib, format);
        assert_eq!(t09, trace(&rust), "{lib:?} {format}");
        for (a, b) in [("writemap", "plain"), ("writemap-copy", "plain-copy")] {
            assert!(
                data_file(&scratch, a) == data_file(&scratch, b),
                "{lib:?} {format}: {a} and {b} differ"
            );
        }
        if format == "1.0" {
            c_reads_environments(&scratch, &c10_scratch, Lib::C10);
        } else {
            c_reads_environments(&scratch, &c09_scratch, Lib::C);
        }
    }
}
