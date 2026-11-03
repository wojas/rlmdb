//! `tests/c/capi_prevsnapshot.c`: `MDB_PREVSNAPSHOT` through the C API.
//!
//! The program runs against C LMDB 1.0 (the oracle), against the LMDB 1.0
//! stub and against `librlmdb` with rlmdb's headers (LMDB 1.0
//! environments), and against `librlmdb` with LMDB 0.9's `lmdb.h` (LMDB
//! 0.9 environments). All must report no failures and print the same
//! trace (`T ...` lines); where C 1.0 has a bug rlmdb does not port
//! (`KNOWN_C_BUGS.md` rows 67 and 69) the program checks each build's own
//! result. The environment a commit on the previous snapshot left, and a
//! compacting copy of a previous snapshot, are byte for byte C 1.0's; the
//! C peer of each format audits every environment the Rust builds left
//! (`check-freelist`).
//!
//! A second run of the program opens an environment that this test
//! process holds open: a writer on the previous snapshot gets
//! `MDB_ENV_BUSY`, a read-only handle reads the previous snapshot, and
//! (rlmdb) leaves the lock file alone, so the holder's next commit builds
//! on the latest snapshot. C 1.0's read-only handle publishes the previous
//! snapshot in the lock file, and the holder's next commit overwrites the
//! latest one (row 68); the test checks that too.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use rlmdb::{Dbi, Environment, EnvironmentFlags, Format, Transaction, WriteFlags};

/// The builds, and the format of the environments each makes.
const LIBS: [(Lib, Format); 4] = [
    (Lib::C10, Format::V10),
    (Lib::Rust10, Format::V10),
    (Lib::RustApi, Format::V10),
    (Lib::Rust, Format::V09),
];

fn run(lib: Lib, args: &[&Path]) -> String {
    let exe = builder().test_program("capi_prevsnapshot", lib);
    let out = Command::new(&exe).args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_prevsnapshot ({lib:?}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_prevsnapshot against the {lib:?} library failed ({})",
        out.status
    );
    assert!(
        stdout.trim_end().ends_with(" 0 failures"),
        "capi_prevsnapshot ({lib:?}): {stdout}"
    );
    stdout
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

fn assert_same_trace(c: &str, rust: &str, lib: Lib) {
    let (tc, tr) = (trace(c), trace(rust));
    for (i, (a, b)) in tc.iter().zip(&tr).enumerate() {
        assert_eq!(
            a, b,
            "trace line {i}: C 1.0 (left) and {lib:?} (right) differ"
        );
    }
    assert_eq!(tc.len(), tr.len(), "trace lengths differ ({lib:?})");
}

/// The C peer of `format` audits the environment `env` (no page lost or
/// shared) and C's `mdb_stat` reads it.
fn audit(env: &Path, format: Format) {
    let lib = match format {
        Format::V09 => Lib::C,
        Format::V10 => Lib::C10,
    };
    run_ok(
        Command::new(builder().cpeer(lib))
            .arg("check-freelist")
            .arg(env),
    );
    let mdb_stat = builder().tool("mdb_stat", lib);
    run_ok(command_as(&mdb_stat, "mdb_stat").arg("-a").arg(env));
}

fn data_file(scratch: &Path, env: &str) -> Vec<u8> {
    fs::read(scratch.join(env).join("data.mdb")).unwrap()
}

#[test]
fn prev_snapshot_matches_c_1_0() {
    let mut c = None;
    for (lib, format) in LIBS {
        let scratch = builder().scratch(&format!("prevsnapshot-{}", lib.name()));
        let out = run(lib, &[&scratch]);
        let Some((c_out, c_scratch)) = &c else {
            c = Some((out, scratch));
            continue;
        };
        assert_same_trace(c_out, &out, lib);
        for env in [
            "new",
            "prev-commit",
            "prev-two",
            "copy-src",
            "copy-compact",
            "nolock",
        ] {
            audit(&scratch.join(env), format);
        }
        if format == Format::V10 {
            for env in ["prev-commit", "copy-compact"] {
                assert!(
                    data_file(c_scratch, env) == data_file(&scratch, env),
                    "{env}: {lib:?} left another file than C 1.0"
                );
            }
        }
    }
}

/// An environment in a new directory of `scratch` after two commits,
/// `a=1` and `b=2`, open in this process.
fn held_env(scratch: &Path, format: Format) -> (PathBuf, Environment) {
    let dir = scratch.join("held");
    fs::create_dir_all(&dir).unwrap();
    let env = Environment::builder()
        .format(format)
        .map_size(8 << 20)
        .flags(EnvironmentFlags::NO_TLS)
        .open(&dir)
        .unwrap();
    for (k, v) in [("a", "1"), ("b", "2")] {
        put(&env, k, v);
    }
    (dir, env)
}

fn put(env: &Environment, k: &str, v: &str) {
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, k.as_bytes(), v.as_bytes(), WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
}

fn keys(env: &Environment) -> String {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut s = Vec::new();
    while let Some((k, _)) = c.next().unwrap() {
        s.push(String::from_utf8_lossy(k).into_owned());
    }
    format!("{} @{}", s.join(" "), txn.id())
}

#[test]
fn prev_snapshot_next_to_another_process() {
    let mut c = None;
    for (lib, format) in LIBS {
        let scratch = builder().scratch(&format!("prevsnapshot-shared-{}", lib.name()));
        let (dir, env) = held_env(&scratch, format);
        let out = run(lib, &[&scratch, Path::new("shared"), &dir]);
        put(&env, "c", "3");
        if lib == Lib::C10 {
            // C 1.0's read-only handle published txnid 1 in the lock
            // file: this commit began from snapshot 1 and overwrote 2.
            assert_eq!(keys(&env), "a c @2");
            c = Some(out);
            continue;
        }
        assert_same_trace(c.as_deref().unwrap(), &out, lib);
        assert_eq!(keys(&env), "a b c @3", "{lib:?}");
        drop(env);
        audit(&dir, format);
    }
}
