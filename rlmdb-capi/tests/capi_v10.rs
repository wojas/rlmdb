//! `tests/c/capi_v10.c`: the 13 functions LMDB 1.0's `lmdb.h` declares
//! and 0.9's does not, on LMDB 1.0 environments.
//!
//! The program runs against C LMDB 1.0 (the oracle, with 1.0's `lmdb.h`),
//! against the LMDB 1.0 stub with the same header (a program built for C
//! 1.0), and against `librlmdb` with rlmdb's `lmdb.h` and `rlmdb.h`. All
//! must report no failures and print the same trace (`T ...` lines) of
//! what `mdb_env_set_pagesize`, `mdb_txn_flags` and `mdb_cursor_is_db`
//! do; the Rust builds also check that the unimplemented functions return
//! `ENOTSUP` after C 1.0's argument checks and change nothing. C 1.0's
//! `mdb_stat` then reads every environment the Rust builds left.

mod common;

use std::path::Path;
use std::process::Command;

use common::*;

fn run_capi_v10(lib: Lib) -> (String, std::path::PathBuf) {
    let b = builder();
    let exe = b.test_program("capi_v10", lib);
    let scratch = b.scratch(&format!("v10-{}", lib.name()));
    let out = Command::new(&exe).arg(&scratch).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_v10 ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_v10 against the {lib:?} library failed ({})",
        out.status
    );
    assert!(
        stdout.trim_end().ends_with(" 0 failures"),
        "capi_v10 ({lib:?}): {stdout}"
    );
    (stdout, scratch)
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

/// C 1.0's `mdb_stat -a` of every environment in `scratch` must succeed.
fn c10_reads_environments(scratch: &Path) {
    let mdb_stat = builder().tool("mdb_stat", Lib::C10);
    let mut n = 0;
    for e in std::fs::read_dir(scratch).unwrap().flatten() {
        if e.path().join("data.mdb").exists() {
            run_ok(command_as(&mdb_stat, "mdb_stat").arg("-a").arg(e.path()));
            n += 1;
        }
    }
    assert!(n >= 8, "only {n} environments in {}", scratch.display());
}

#[test]
fn lmdb_1_0_functions_match_c_1_0() {
    let (c, _) = run_capi_v10(Lib::C10);
    assert!(c.starts_with("library: LMDB 1.0."), "{c}");
    let tc = trace(&c);
    assert!(tc.len() > 50, "{c}");
    for (lib, version) in [(Lib::Rust10, "1.0.85"), (Lib::Rust, "0.9.85")] {
        let (rust, scratch) = run_capi_v10(lib);
        assert!(
            rust.starts_with(&format!("library: LMDB {version}\n")),
            "{rust}"
        );
        let tr = trace(&rust);
        for (i, (a, b)) in tc.iter().zip(&tr).enumerate() {
            assert_eq!(
                a, b,
                "trace line {i}: C 1.0 (left) and {lib:?} (right) differ"
            );
        }
        assert_eq!(tc.len(), tr.len(), "trace lengths differ ({lib:?})");
        c10_reads_environments(&scratch);
    }
}
