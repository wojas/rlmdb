//! `-v` of `mdb_stat`, `mdb_dump` and `mdb_copy` (C 1.0's option,
//! `MDB_PREVSNAPSHOT`) on LMDB 0.9 environments, whose C tools do not have
//! it: the Rust tool with `-v` after a commit says what the C 0.9 tool said
//! before it, and its copies hold what C 0.9's copies from before it hold
//! (a compacting one byte for byte). C 1.0's tools are the oracle on 1.0
//! environments (`interop/tests/tools10.rs`).

mod common;

use std::fs;
use std::path::Path;

use common::*;

/// The runs compared, the environment last.
fn cases(env: &[u8]) -> Vec<(&'static str, Vec<Vec<u8>>)> {
    let with = |opts: &str| {
        let mut v = words(opts);
        v.push(env.to_vec());
        v
    };
    vec![
        ("mdb_stat", with("")),
        ("mdb_stat", with("-a")),
        ("mdb_stat", with("-e")),
        ("mdb_stat", with("-ff")),
        ("mdb_stat", with("-a -fff")),
        ("mdb_stat", with("-s dupfixed")),
        ("mdb_dump", with("")),
        ("mdb_dump", with("-a")),
        ("mdb_dump", with("-l")),
        ("mdb_dump", with("-p -s dupfixed")),
    ]
}

/// A copy of `env` by `imp`'s `mdb_copy` with `opts`, as its data file.
fn copy(imp: Impl, opts: &str, env: &Path, dst: &Path) -> Vec<u8> {
    fs::create_dir_all(dst).unwrap();
    let mut argv = words(opts);
    argv.extend([p(env), p(dst)]);
    let r = run(imp, "mdb_copy", &argv, b"", env.parent().unwrap());
    assert_eq!((r.code, r.stderr_str()), (0, String::new()), "{opts}");
    fs::read(dst.join("data.mdb")).unwrap()
}

#[test]
fn dash_v_shows_the_snapshot_before_the_last_commit() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let e = p(&env);
    let cwd = dir.path();
    let before: Vec<Run> = cases(&e)
        .iter()
        .map(|(tool, argv)| run(Impl::C, tool, argv, b"", cwd))
        .collect();
    let plain = copy(Impl::C, "", &env, &dir.path().join("c-plain"));
    let compact = copy(Impl::C, "-c", &env, &dir.path().join("c-compact"));
    let txnid = |env: &Path| {
        let e = rlmdb::Environment::builder()
            .flags(rlmdb::EnvironmentFlags::READ_ONLY)
            .open(env)
            .unwrap();
        e.info().unwrap().last_txnid
    };
    let last = txnid(&env);

    // The last commit: C's mdb_drop deletes a database.
    let r = run(
        Impl::C,
        "mdb_drop",
        &args(&[b"-d".as_slice(), b"-s", b"dupfixed", &e]),
        b"",
        cwd,
    );
    assert_eq!(r.code, 0, "{}", r.stderr_str());
    assert_eq!(txnid(&env), last + 1);

    for ((tool, argv), c) in cases(&e).into_iter().zip(&before) {
        let mut v = words("-v");
        v.extend(argv.iter().cloned());
        let r = run(Impl::Rust, tool, &v, b"", cwd);
        assert_runs_equal(tool, &v, c, &r);
    }
    // Without -v: the latest snapshot, without the database.
    let r = run(
        Impl::Rust,
        "mdb_stat",
        &words(&format!("-s dupfixed {}", env.display())),
        b"",
        cwd,
    );
    assert_ne!(r.code, 0);

    let rust_compact = copy(Impl::Rust, "-v -c", &env, &dir.path().join("r-compact"));
    assert!(
        rust_compact == compact,
        "mdb_copy -v -c differs from C's copy before"
    );
    // A plain copy has the free pages too, which the last commit may have
    // reused: compared by content only.
    let rust_plain = copy(Impl::Rust, "-v", &env, &dir.path().join("r-plain"));
    assert_eq!(rust_plain.len(), plain.len());
    // C 0.9 reads it as the snapshot before the last commit.
    assert!(
        c_dump_all(&dir.path().join("r-plain"), &[])
            == c_dump_all(&dir.path().join("c-plain"), &[]),
        "mdb_copy -v: C's mdb_dump of the copy differs from C's copy before"
    );
}
