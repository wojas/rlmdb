//! `include/rlmdb.h` after either `lmdb.h`: `tests/c/capi_headers.c`,
//! compiled with warnings as errors as C and as C++, once with rlmdb's
//! `lmdb.h` (LMDB 0.9.37's) and linked against `librlmdb`, once with LMDB
//! 1.0's own `lmdb.h` (`tests/lmdb-c/1.0`) in front and linked against
//! the LMDB 1.0 stub, as a program written for C LMDB 1.0 that also uses
//! rlmdb's functions is built. The types `rlmdb.h` adds for the 1.0
//! functions must then not be defined twice, and its declarations must
//! agree with 1.0's. No C oracle: C LMDB has no `rlmdb.h`.

mod common;

use std::path::PathBuf;
use std::process::Command;

use common::*;

/// Builds `capi_headers` with LMDB 1.0's `lmdb.h` (`v10`) or rlmdb's, as
/// C or C++, runs it and checks its summary. Skipped, with a note, where
/// there is no C++ compiler.
fn check(v10: bool, cpp: bool) {
    let b = builder();
    let mut includes: Vec<PathBuf> = Vec::new();
    if v10 {
        includes.push(c10_src());
    }
    includes.push(crate_dir().join("include"));
    let name = format!(
        "capi_headers{}{}",
        if v10 { "10" } else { "09" },
        if cpp { "_cxx" } else { "" }
    );
    let source = test_c_dir().join("capi_headers.c");
    let Some(obj) = b.strict_object(&name, &source, &includes, cpp) else {
        return;
    };
    let lib = if v10 { Lib::Rust10 } else { Lib::Rust };
    let exe = b.program(&name, &[obj], lib);
    let out = run_ok(&mut Command::new(&exe));
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    let header = if v10 {
        "LMDB 1.0.3 header"
    } else {
        "LMDB 0.9.37 header"
    };
    let language = if cpp { "C++" } else { "C" };
    assert!(
        stdout.starts_with(&format!("capi_headers ({header}, {language}): ")),
        "{stdout}"
    );
    assert!(stdout.trim_end().ends_with(" 0 failures"), "{stdout}");
}

#[test]
fn rlmdb_h_with_rlmdb_lmdb_h() {
    check(false, false);
}

#[test]
fn rlmdb_h_after_lmdb_1_0_h() {
    check(true, false);
}

#[test]
fn rlmdb_h_with_rlmdb_lmdb_h_from_cxx() {
    check(false, true);
}

#[test]
fn rlmdb_h_after_lmdb_1_0_h_from_cxx() {
    check(true, true);
}
