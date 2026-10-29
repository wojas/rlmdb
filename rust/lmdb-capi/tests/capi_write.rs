//! `tests/c/capi_write.c` exercises every write-side function of the C API
//! and its error codes. It runs against the C library first (the oracle:
//! the expectations hold for C LMDB 0.9.36) and then against the Rust
//! library; both must report no failures, and the traces they print (`T
//! ...` lines: statistics, counts, keys and values they observe) must be
//! identical. Known gaps of the Rust implementation are printed by the
//! Rust build (see README.md). Every environment the two runs leave behind
//! (including the copies) must pass the C peer's page audit
//! (`check-freelist`).

mod common;

use std::process::Command;

use common::*;

fn run_capi_write(lib: Lib) -> String {
    let b = builder();
    let exe = b.test_program("capi_write", lib);
    let scratch = b.scratch(&format!("write-{}", lib.name()));
    let out = run_in(lib, &exe, &scratch);
    check_environments(&scratch);
    out
}

/// The C peer's page audit of every environment in `scratch`.
fn check_environments(scratch: &std::path::Path) {
    let cpeer = builder().cpeer(Lib::C);
    let mut n = 0;
    for e in std::fs::read_dir(scratch).unwrap().flatten() {
        if e.path().join("data.mdb").exists() {
            run_ok(Command::new(&cpeer).arg("check-freelist").arg(e.path()));
            n += 1;
        }
    }
    assert!(n >= 10, "only {n} environments in {}", scratch.display());
}

fn run_in(lib: Lib, exe: &std::path::Path, scratch: &std::path::Path) -> String {
    let out = Command::new(exe).arg(scratch).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_write ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_write against the {lib:?} library failed ({})",
        out.status
    );
    stdout
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

#[test]
fn c_api_write_side_matches_c() {
    let c = run_capi_write(Lib::C);
    assert!(!c.contains("known gap"), "{c}");
    let rust = run_capi_write(Lib::Rust);
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
    let (tc, tr) = (trace(&c), trace(&rust));
    for (i, (a, b)) in tc.iter().zip(&tr).enumerate() {
        assert_eq!(a, b, "trace line {i}: C (left) and Rust (right) differ");
    }
    assert_eq!(tc.len(), tr.len(), "trace lengths differ");
}
