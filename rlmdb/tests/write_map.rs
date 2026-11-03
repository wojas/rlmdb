//! `EnvironmentFlags::WRITE_MAP` (`MDB_WRITEMAP`): rlmdb does not use a
//! writable map, but keeps what the flag changes through the API as C
//! does (`mdb.c` `mdb_txn_begin`, `mdb_env_open`, `mdb_env_set_flags`):
//! nested transactions are refused with `MDB_BAD_TXN` (C 0.9's code), the
//! flag cannot be changed on an open environment, and a read-only open
//! drops it. The files a writer leaves with the flag are byte for byte
//! those it leaves without it, and C reads them (the 0.9 ones here with
//! C 0.9; `rlmdb-capi/tests/capi_writemap.rs` has C 1.0 read 1.0 ones).
//!
//! On OpenBSD, which keeps a file's maps apart from its descriptors, an
//! open for writing with the flag is refused with `ENOTSUP` instead
//! (`rlmdb/src/os/mod.rs`); the tests that write with it run elsewhere.

// The helpers of the tests that write with the flag.
#![cfg_attr(target_os = "openbsd", allow(dead_code, unused_imports))]

mod common;

use std::path::Path;

use rlmdb::{
    DatabaseFlags, Environment, EnvironmentFlags, Error, Format, MdbError, Transaction, WriteFlags,
};

fn try_open(
    path: &Path,
    format: Format,
    page_size: Option<usize>,
    flags: EnvironmentFlags,
) -> rlmdb::Result<Environment> {
    let mut b = Environment::builder();
    b.map_size(16 << 20)
        .max_dbs(4)
        .format(format)
        .flags(flags | EnvironmentFlags::NO_SYNC);
    if let Some(ps) = page_size {
        b.page_size(ps);
    }
    b.open(path)
}

fn open(
    path: &Path,
    format: Format,
    page_size: Option<usize>,
    flags: EnvironmentFlags,
) -> Environment {
    try_open(path, format, page_size, flags).unwrap()
}

fn is_bad_txn(e: &Error) -> bool {
    matches!(e, Error::Mdb(MdbError::BadTxn))
}

fn value(i: usize, len: usize) -> Vec<u8> {
    let mut v = format!("v{i:05}-").into_bytes();
    v.resize(len, b'a' + (i % 26) as u8);
    v
}

/// The same work for both environments: puts (overflow values among
/// them), deletes, a database dropped, a map resize, a transaction that
/// spills (a low dirty limit), an aborted one. With `write_map`, every
/// write transaction also tries a nested one, which must be refused
/// without changing anything.
fn workload(env: &Environment, write_map: bool) {
    let try_child = |txn: &mut rlmdb::RwTxn<'_>| {
        if write_map {
            let e = txn.begin_nested_txn().map(|_| ()).unwrap_err();
            assert!(is_bad_txn(&e), "begin_nested_txn: {e:?}");
        }
    };

    let mut txn = env.begin_rw_txn().unwrap();
    try_child(&mut txn);
    let plain = txn.create_db(Some("plain"), DatabaseFlags::CREATE).unwrap();
    let dups = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    for i in 0..1500 {
        let len = if i % 40 == 0 { 3000 + i } else { 20 + i % 90 };
        txn.put(
            plain,
            format!("k{i:05}").as_bytes(),
            &value(i, len),
            WriteFlags::empty(),
        )
        .unwrap();
    }
    try_child(&mut txn);
    for k in 0..20 {
        for d in 0..30 {
            txn.put(
                dups,
                format!("d{k:02}").as_bytes(),
                format!("x{d:03}").as_bytes(),
                WriteFlags::empty(),
            )
            .unwrap();
        }
    }
    txn.commit().unwrap();

    let mut txn = env.begin_rw_txn().unwrap();
    for i in (0..1500).step_by(3) {
        assert!(txn.del(plain, format!("k{i:05}").as_bytes(), None).unwrap());
    }
    try_child(&mut txn);
    for i in (1..1500).step_by(40) {
        txn.put(
            plain,
            format!("k{i:05}").as_bytes(),
            &value(i, 5000),
            WriteFlags::empty(),
        )
        .unwrap();
    }
    txn.commit().unwrap();

    env.set_map_size(32 << 20).unwrap();

    // Spills (C 1.0 with MDB_WRITEMAP never spills; rlmdb spills as
    // without the flag).
    env.set_dirty_limit(16);
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 1500..4000 {
        txn.put(
            plain,
            format!("k{i:05}").as_bytes(),
            &value(i, 30 + i % 200),
            WriteFlags::empty(),
        )
        .unwrap();
    }
    try_child(&mut txn);
    txn.drop_db(dups).unwrap();
    txn.commit().unwrap();

    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(plain, b"aborted", b"x", WriteFlags::empty())
        .unwrap();
    try_child(&mut txn);
    txn.abort();
}

/// Runs [`workload`] with and without `WRITE_MAP` and returns the two
/// environments' directories.
fn both(format: Format, page_size: Option<usize>) -> (tempfile::TempDir, tempfile::TempDir) {
    let (wm, plain) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    {
        let env = open(wm.path(), format, page_size, EnvironmentFlags::WRITE_MAP);
        assert!(env.flags().contains(EnvironmentFlags::WRITE_MAP));
        workload(&env, true);
        let env = open(plain.path(), format, page_size, EnvironmentFlags::empty());
        workload(&env, false);
    }
    let (a, b) = (
        std::fs::read(wm.path().join("data.mdb")).unwrap(),
        std::fs::read(plain.path().join("data.mdb")).unwrap(),
    );
    assert!(
        a == b,
        "{format:?}: the data file written with WRITE_MAP differs from the one without (lengths {} and {})",
        a.len(),
        b.len()
    );
    (wm, plain)
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn write_map_leaves_the_same_files_v09() {
    let (wm, _plain) = both(Format::V09, None);
    let c = require_c!();
    let p = wm.path().to_str().unwrap();
    c.peer_ok(&["check-freelist", p]);
    let env = open(wm.path(), Format::V09, None, EnvironmentFlags::READ_ONLY);
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("plain")).unwrap();
    assert_eq!(
        c.dump(wm.path(), Some("plain")),
        common::walk_forward(&txn, db)
    );
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn write_map_leaves_the_same_files_v10() {
    both(Format::V10, None);
    both(Format::V10, Some(1024));
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn nested_transactions_are_refused_with_write_map() {
    let dir = tempfile::tempdir().unwrap();
    for format in [Format::V09, Format::V10] {
        let path = dir.path().join(format!("{format:?}"));
        std::fs::create_dir(&path).unwrap();
        let env = open(&path, format, None, EnvironmentFlags::WRITE_MAP);
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.put(db, b"a", b"1", WriteFlags::empty()).unwrap();
        let e = txn.begin_nested_txn().map(|_| ()).unwrap_err();
        assert!(is_bad_txn(&e), "{format:?}: {e:?}");
        // The parent is untouched and commits.
        txn.put(db, b"b", b"2", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(db, b"a").unwrap(), Some(&b"1"[..]));
        assert_eq!(txn.get(db, b"b").unwrap(), Some(&b"2"[..]));
        drop(txn);

        // Without the flag, the same environment allows them.
        drop(env);
        let env = open(&path, format, None, EnvironmentFlags::empty());
        let mut txn = env.begin_rw_txn().unwrap();
        let mut child = txn.begin_nested_txn().unwrap();
        child.put(db, b"c", b"3", WriteFlags::empty()).unwrap();
        child.commit().unwrap();
        txn.commit().unwrap();
    }
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn write_map_flag_as_c_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), Format::V09, None, EnvironmentFlags::WRITE_MAP);
    let flags = env.flags();
    assert!(flags.contains(EnvironmentFlags::WRITE_MAP), "{flags:?}");
    // mdb_env_set_flags: MDB_WRITEMAP is not one of the changeable flags.
    for on in [false, true] {
        let e = env.set_flags(EnvironmentFlags::WRITE_MAP, on).unwrap_err();
        assert_eq!(e.to_err_code(), libc::EINVAL, "{e:?}");
    }
    assert_eq!(env.flags(), flags);
    // MAP_ASYNC can be changed (and has no effect: there is no map to
    // flush asynchronously); mdb_env_sync works with it either way.
    env.set_flags(EnvironmentFlags::MAP_ASYNC, true).unwrap();
    assert!(env.flags().contains(EnvironmentFlags::MAP_ASYNC));
    env.sync(false).unwrap();
    env.sync(true).unwrap();
    env.create_db(None, DatabaseFlags::empty()).unwrap();
    drop(env);

    // A read-only open silently drops it, as C does.
    let env = open(
        dir.path(),
        Format::V09,
        None,
        EnvironmentFlags::WRITE_MAP | EnvironmentFlags::READ_ONLY,
    );
    let flags = env.flags();
    assert!(!flags.contains(EnvironmentFlags::WRITE_MAP), "{flags:?}");
    assert!(flags.contains(EnvironmentFlags::READ_ONLY), "{flags:?}");
    env.begin_ro_txn().unwrap();
}

/// OpenBSD: an open for writing with the flag fails with `ENOTSUP` before
/// any file is touched; without it, and read-only with it (the flag
/// dropped, as C does), the environment opens as anywhere else.
#[test]
#[cfg(target_os = "openbsd")]
fn write_map_is_refused_on_openbsd() {
    let dir = tempfile::tempdir().unwrap();
    for format in [Format::V09, Format::V10] {
        let path = dir.path().join(format!("{format:?}"));
        std::fs::create_dir(&path).unwrap();
        let e = try_open(&path, format, None, EnvironmentFlags::WRITE_MAP).unwrap_err();
        assert_eq!(e.to_err_code(), libc::ENOTSUP, "{format:?}: {e:?}");
        let no_lock = EnvironmentFlags::WRITE_MAP | EnvironmentFlags::NO_LOCK;
        let e = try_open(&path, format, None, no_lock).unwrap_err();
        assert_eq!(e.to_err_code(), libc::ENOTSUP, "{format:?}: {e:?}");
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0, "{format:?}");

        let env = open(&path, format, None, EnvironmentFlags::empty());
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.put(db, b"a", b"1", WriteFlags::empty()).unwrap();
        let mut child = txn.begin_nested_txn().unwrap();
        child.put(db, b"b", b"2", WriteFlags::empty()).unwrap();
        child.commit().unwrap();
        txn.commit().unwrap();
        drop(env);

        let read_only = EnvironmentFlags::WRITE_MAP | EnvironmentFlags::READ_ONLY;
        let env = open(&path, format, None, read_only);
        let flags = env.flags();
        assert!(!flags.contains(EnvironmentFlags::WRITE_MAP), "{flags:?}");
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(db, b"b").unwrap(), Some(&b"2"[..]));
    }
}
