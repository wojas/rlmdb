//! Test 2: `tests/c/capi_read.c` exercises every read-side function of the
//! C API and its error codes. It runs against the C library first (the
//! oracle: the expectations hold for C LMDB 0.9.36) and then against the
//! Rust library; both must report no failures. Known gaps of the Rust
//! implementation are printed by the Rust build (see README.md).

mod common;

use std::process::Command;

use common::*;

fn run_capi_read(lib: Lib) -> String {
    let b = builder();
    let exe = b.test_program("capi_read", lib);
    let fixture = b.scratch(&format!("fixture-{}", lib.name()));
    make_fixture(&fixture);
    let ro = b.scratch(&format!("ro-{}", lib.name()));
    let rw = b.scratch(&format!("rw-{}", lib.name()));
    copy_env(&fixture, &ro);
    copy_env(&fixture, &rw);
    let scratch = b.scratch(&format!("work-{}", lib.name()));
    let out = Command::new(&exe)
        .arg(&ro)
        .arg(&rw)
        .arg(&scratch)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_read ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_read against the {lib:?} library failed ({})",
        out.status
    );
    stdout
}

#[test]
fn c_api_read_side_matches_c() {
    let c = run_capi_read(Lib::C);
    assert!(!c.contains("known gap"), "{c}");
    let rust = run_capi_read(Lib::Rust);
    // Same number of checks on both sides (GAP lines are not checks in the
    // Rust build).
    let summary = |s: &str| s.lines().last().unwrap_or("").to_owned();
    let gaps = rust.lines().filter(|l| l.starts_with("known gap")).count();
    let checks = |s: &str| -> usize {
        summary(s)
            .split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .unwrap()
    };
    assert_eq!(checks(&c), checks(&rust) + gaps, "C:\n{c}\nRust:\n{rust}");
}
