//! `tests/c/capi_nolock.c`: read transactions in an `MDB_NOLOCK`
//! environment through the C API. It runs against the C library (the
//! oracle for what `mdb_env_info`, `mdb_reader_list` and `mdb_reader_check`
//! report without a lock file, and the demonstration that C reuses pages
//! of a live read transaction there) and against the Rust library, which
//! keeps them (`KNOWN_C_BUGS.md` #35). Both must report no failures and
//! print the same trace, and the environments they leave behind must pass
//! the C peer's page audit (`check-freelist`).

mod common;

use std::process::Command;

use common::*;

fn run_capi_nolock(lib: Lib) -> String {
    let b = builder();
    let exe = b.test_program("capi_nolock", lib);
    let scratch = b.scratch(&format!("nolock-{}", lib.name()));
    let out = Command::new(&exe).arg(&scratch).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_nolock ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_nolock against the {lib:?} library failed ({})",
        out.status
    );
    run_ok(
        Command::new(b.cpeer(Lib::C))
            .arg("check-freelist")
            .arg(scratch.join("nolock")),
    );
    stdout
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

#[test]
fn nolock_read_transactions_through_the_c_api() {
    let c = run_capi_nolock(Lib::C);
    let rust = run_capi_nolock(Lib::Rust);
    assert_eq!(trace(&c), trace(&rust));
    assert!(!trace(&rust).is_empty());
}
