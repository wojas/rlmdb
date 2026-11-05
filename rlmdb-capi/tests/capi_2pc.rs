//! `tests/c/capi_2pc.c`: LMDB 1.0's two-phase commit (`mdb_txn_prepare`,
//! `mdb_env_rollback`) through the C API.
//!
//! The program runs against C LMDB 1.0 (the oracle, with 1.0's `lmdb.h`),
//! against the LMDB 1.0 stub with the same header, and against
//! `librlmdb` with rlmdb's headers, on 1.0 environments and (rlmdb only:
//! C 0.9 has no two-phase commit) on 0.9 ones. All must report no
//! failures and print the same trace (`T ...` lines): the return codes on
//! a prepared transaction, what readers see between and after the phases,
//! what rollbacks return and leave. The 1.0 environments the Rust builds
//! leave must be the oracle's byte for byte, the files copied between the
//! two phases (`prepared.mdb`) included (but for one environment in
//! which the Rust builds commit more); C 1.0's `mdb_stat` reads them, and
//! C 0.9's reads the 0.9 ones, whose rolled-back meta pages are C 1.0's
//! but for the format's version.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use rlmdb::format::Meta;

fn run_capi_2pc(lib: Lib, format: &str) -> (String, PathBuf) {
    let b = builder();
    let exe = b.test_program("capi_2pc", lib);
    let scratch = b.scratch(&format!("2pc-{}-{format}", lib.name()));
    let out = Command::new(&exe)
        .arg(&scratch)
        .arg(format)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_2pc ({lib:?}, {format}) ---\n{stdout}{stderr}");
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_2pc against the {lib:?} library ({format}) failed ({})",
        out.status
    );
    assert!(
        stdout.trim_end().ends_with(" 0 failures"),
        "capi_2pc ({lib:?}, {format}): {stdout}"
    );
    (stdout, scratch)
}

/// The `T ...` lines.
fn trace(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("T ")).collect()
}

/// The environments (directories with a data file) in `scratch`.
fn environments(scratch: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(scratch)
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("data.mdb").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `mdb_stat -a` of `lib`'s tools must succeed on every environment.
fn c_reads_environments(scratch: &Path, lib: Lib) {
    let mdb_stat = builder().tool("mdb_stat", lib);
    for name in environments(scratch) {
        run_ok(
            command_as(&mdb_stat, "mdb_stat")
                .arg("-a")
                .arg(scratch.join(name)),
        );
    }
}

/// The meta pages (as decoded `MDB_meta`s) of the data file in `dir`, of
/// the format whose meta page starts `offset` bytes into the page.
fn metas(dir: &Path, offset: usize) -> [Meta; 2] {
    let file = std::fs::read(dir.join("data.mdb")).unwrap();
    let first = Meta::read(&file[offset..]).unwrap();
    let ps = first.page_size() as usize;
    [first, Meta::read(&file[ps + offset..]).unwrap()]
}

/// The rollbacks of a 0.9 environment (rlmdb, `rust`) left the meta pages
/// C 1.0 left in its 1.0 environment (`c10`): the rolled-back page the
/// same in every field but the format's version, the other page with the
/// same txnid (its trees are the format's).
fn same_rollback_metas(c10: &Path, rust: &Path) {
    for name in ["rollback-odd", "rollback-nosync", "rollback-nometasync"] {
        let (c, r) = (metas(&c10.join(name), 24), metas(&rust.join(name), 16));
        for i in 0..2 {
            assert_eq!(c[i].txnid, r[i].txnid, "{name}: meta page {i}");
        }
        // The page of the larger txnid holds the previous snapshot.
        let rolled = usize::from(c[0].txnid > c[1].txnid);
        let (mut cm, rm) = (c[rolled], r[rolled]);
        assert_eq!(cm.last_pgno, u64::MAX, "{name}");
        assert_eq!(cm.dbs[1], Default::default(), "{name}");
        cm.version = rm.version;
        assert_eq!(cm, rm, "{name}: the rolled-back meta page {rolled}");
    }
}

#[test]
fn two_phase_commit_matches_c_1_0() {
    let (c, c_scratch) = run_capi_2pc(Lib::C10, "1.0");
    assert!(c.starts_with("library: LMDB 1.0."), "{c}");
    let tc = trace(&c);
    assert!(tc.len() > 80, "{c}");
    let c_envs = environments(&c_scratch);
    assert!(c_envs.len() >= 9, "{c_envs:?}");
    for (lib, format) in [
        (Lib::Rust10, "1.0"),
        (Lib::RustApi, "1.0"),
        (Lib::RustApi, "0.9"),
    ] {
        let (rust, scratch) = run_capi_2pc(lib, format);
        let tr = trace(&rust);
        for (i, (a, b)) in tc.iter().zip(&tr).enumerate() {
            assert_eq!(
                a, b,
                "trace line {i}: C 1.0 (left) and {lib:?} {format} (right) differ"
            );
        }
        assert_eq!(
            tc.len(),
            tr.len(),
            "trace lengths differ ({lib:?} {format})"
        );
        assert_eq!(environments(&scratch), c_envs);
        if format == "0.9" {
            c_reads_environments(&scratch, Lib::C);
            same_rollback_metas(&c_scratch, &scratch);
            continue;
        }
        c_reads_environments(&scratch, Lib::C10);
        // The same files as C 1.0's, also between the phases ("ends" has
        // more commits in the Rust builds).
        for name in c_envs.iter().filter(|n| *n != "ends") {
            for file in ["data.mdb", "prepared.mdb"] {
                let (a, b) = (
                    c_scratch.join(name).join(file),
                    scratch.join(name).join(file),
                );
                if !a.exists() {
                    assert!(!b.exists(), "{lib:?}: {name}/{file}");
                    continue;
                }
                let (a, b) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
                assert!(
                    a == b,
                    "{lib:?}: {name}/{file} differs from C 1.0's (lengths {} and {})",
                    a.len(),
                    b.len()
                );
            }
        }
    }
}
