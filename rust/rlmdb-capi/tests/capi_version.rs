//! `tests/c/capi_version.c` checks `include/rlmdb.h` (`rlmdb_version`,
//! `rlmdb_version_string`, `rlmdb_has_capability`) and `mdb_version`
//! against the Rust library, built as C and as C++. There is no C oracle:
//! C LMDB has no `rlmdb.h`; the expectations come from
//! [`rlmdb::version_info`].

mod common;

use std::path::Path;
use std::process::Command;

use common::*;

fn run_capi_version(exe: &Path) -> String {
    let v = rlmdb::version_info();
    let out = Command::new(exe)
        .arg(env!("CARGO_PKG_VERSION"))
        .arg(v.to_string())
        .arg(v.mdb_version_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- {} ---\n{stdout}{stderr}", exe.display());
    assert!(
        out.status.success() && stderr.is_empty(),
        "{} failed ({})",
        exe.display(),
        out.status
    );
    stdout
}

#[test]
fn rlmdb_h_version_functions() {
    let exe = builder().test_program("capi_version", Lib::Rust);
    let out = run_capi_version(&exe);
    assert!(out.starts_with("capi_version: "), "{out}");
    assert!(out.trim_end().ends_with(" 0 failures"), "{out}");
}

#[test]
fn rlmdb_h_from_cxx() {
    let exe = builder().cxx_test_program("capi_version");
    let out = run_capi_version(&exe);
    assert!(out.starts_with("capi_version (C++): "), "{out}");
    assert!(out.trim_end().ends_with(" 0 failures"), "{out}");
}
