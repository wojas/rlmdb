//! The headers: `include/lmdb.h` (LMDB 1.0's) and `include/rlmdb.h`
//! after it, `rlmdb.h` alone, and `rlmdb.h` after LMDB 0.9's `lmdb.h`
//! (`tests/lmdb-c/0.9`, first on the include path), as a program written
//! for LMDB 0.9 that uses rlmdb's functions is built: `tests/c/capi_headers.c`,
//! compiled with warnings as errors as C and as C++ each way, and linked
//! against `librlmdb`. `rlmdb.h` needs nothing either `lmdb.h` lacks and
//! defines nothing either defines; with `include` alone, a program has
//! LMDB 1.0's types, constants and the 13 functions 1.0 adds. No C
//! oracle: C LMDB has no `rlmdb.h`.

mod common;

use std::process::Command;

use common::*;

/// The headers `capi_headers` is built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Headers {
    /// `include`: LMDB 1.0's `lmdb.h`, then `rlmdb.h`.
    Api,
    /// `include`, the program includes `rlmdb.h` only (`RLMDB_H_ONLY`).
    RlmdbHOnly,
    /// LMDB 0.9's `lmdb.h` (`tests/lmdb-c/0.9` first), then `rlmdb.h`.
    Lmdb09,
}

/// Builds `capi_headers` with `headers`, as C or C++, runs it and checks
/// its summary. Skipped, with a note, where there is no C++ compiler.
fn check(headers: Headers, cpp: bool) {
    let b = builder();
    let lib = match headers {
        Headers::Api | Headers::RlmdbHOnly => Lib::RustApi,
        Headers::Lmdb09 => Lib::Rust,
    };
    let mut defines = vec!["LMDB_RUST", "LMDB_LIBRLMDB"];
    if headers == Headers::RlmdbHOnly {
        defines.push("RLMDB_H_ONLY");
    }
    let name = format!("capi_headers-{headers:?}{}", if cpp { "_cxx" } else { "" });
    let source = test_c_dir().join("capi_headers.c");
    let Some(obj) = b.strict_object(&name, &source, &lib.includes(), &defines, cpp) else {
        return;
    };
    let exe = b.program(&name, &[obj], lib);
    let out = run_ok(&mut Command::new(&exe));
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!("{stdout}");
    // The releases the two formats match are those of the two headers.
    let v = rlmdb::version_info();
    let (ma, mi, pa) = if headers == Headers::Lmdb09 {
        v.v09
    } else {
        v.v10
    };
    let language = if cpp { "C++" } else { "C" };
    assert!(
        stdout.starts_with(&format!(
            "capi_headers (LMDB {ma}.{mi}.{pa} header, {language}): "
        )),
        "{stdout}"
    );
    assert!(stdout.trim_end().ends_with(" 0 failures"), "{stdout}");
}

#[test]
fn lmdb_1_0_api_with_rlmdb_include() {
    check(Headers::Api, false);
}

#[test]
fn rlmdb_h_alone_includes_lmdb_1_0_h() {
    check(Headers::RlmdbHOnly, false);
}

#[test]
fn rlmdb_h_after_lmdb_0_9_h() {
    check(Headers::Lmdb09, false);
}

#[test]
fn lmdb_1_0_api_with_rlmdb_include_from_cxx() {
    check(Headers::Api, true);
}

#[test]
fn rlmdb_h_alone_includes_lmdb_1_0_h_from_cxx() {
    check(Headers::RlmdbHOnly, true);
}

#[test]
fn rlmdb_h_after_lmdb_0_9_h_from_cxx() {
    check(Headers::Lmdb09, true);
}
