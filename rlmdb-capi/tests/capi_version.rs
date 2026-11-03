//! `tests/c/capi_version.c` checks `include/rlmdb.h` (`rlmdb_version`,
//! `rlmdb_version_string`) and `mdb_version` against `librlmdb`, built
//! with rlmdb's `lmdb.h` (LMDB 1.0's) and with LMDB 0.9's, and as C++.
//! There is no C oracle: C LMDB has no `rlmdb.h`; the expectations come
//! from [`rlmdb_capi::version`].

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use rlmdb_capi::version::{mdb_version_string, version_info};
use rlmdb_capi::Profile;

fn run_capi_version(exe: &Path) -> String {
    let line = version_info(Profile::Rlmdb).to_string();
    let lmdb_line = mdb_version_string(Profile::Rlmdb);
    // librlmdb's mdb_version returns rlmdb's line.
    assert_eq!(lmdb_line, line);
    let out = Command::new(exe)
        .arg(env!("CARGO_PKG_VERSION"))
        .arg(line)
        .arg(lmdb_line)
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
    let exe = builder().test_program("capi_version", Lib::RustApi);
    let out = run_capi_version(&exe);
    assert!(out.starts_with("capi_version (LMDB 1.0 header): "), "{out}");
    assert!(out.trim_end().ends_with(" 0 failures"), "{out}");
}

#[test]
fn rlmdb_h_version_functions_with_lmdb_0_9_h() {
    let exe = builder().test_program("capi_version", Lib::Rust);
    let out = run_capi_version(&exe);
    assert!(out.starts_with("capi_version (LMDB 0.9 header): "), "{out}");
    assert!(out.trim_end().ends_with(" 0 failures"), "{out}");
}

#[test]
fn rlmdb_h_from_cxx() {
    // Skipped (with a note) where there is no C++ compiler.
    let Some(exe) = builder().cxx_test_program("capi_version", Lib::RustApi) else {
        return;
    };
    let out = run_capi_version(&exe);
    assert!(
        out.starts_with("capi_version (C++, LMDB 1.0 header): "),
        "{out}"
    );
    assert!(out.trim_end().ends_with(" 0 failures"), "{out}");
}
