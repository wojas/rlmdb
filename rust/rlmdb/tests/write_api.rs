//! Behaviour of the write API: error codes as C returns them, abort,
//! read-your-writes, handles of created databases, map size handling.

mod common;

use rlmdb::consts::*;
use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error, MdbError, Transaction,
    WriteFlags,
};

fn env_at(path: &std::path::Path, map: usize) -> Environment {
    Environment::builder()
        .map_size(map)
        .max_dbs(4)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

fn mdb(e: Error) -> MdbError {
    match e {
        Error::Mdb(m) => m,
        other => panic!("not an LMDB error: {other:?}"),
    }
}

fn errno(e: Error) -> i32 {
    e.to_err_code()
}

#[test]
fn value_size_limits() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let plain = txn.create_db(Some("plain"), DatabaseFlags::CREATE).unwrap();
    let dups = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    let fixed = txn
        .create_db(
            Some("fixed"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    let e = WriteFlags::empty();
    assert_eq!(
        mdb(txn.put(plain, b"", b"v", e).unwrap_err()),
        MdbError::BadValSize
    );
    assert_eq!(
        mdb(txn.put(plain, &[1; 512], b"v", e).unwrap_err()),
        MdbError::BadValSize
    );
    txn.put(plain, &[1; 511], b"v", e).unwrap();
    // Plain data may be empty and large.
    txn.put(plain, b"empty", b"", e).unwrap();
    txn.put(plain, b"big", &vec![7; 1 << 20], e).unwrap();
    // DUPSORT data are keys of the sub-DB: 1..=511 bytes.
    assert_eq!(
        mdb(txn.put(dups, b"k", b"", e).unwrap_err()),
        MdbError::BadValSize
    );
    assert_eq!(
        mdb(txn.put(dups, b"k", &[2; 512], e).unwrap_err()),
        MdbError::BadValSize
    );
    txn.put(dups, b"k", &[2; 511], e).unwrap();
    // DUPFIXED: all items of a key have one size (C would corrupt).
    txn.put(fixed, b"k", b"abcd", e).unwrap();
    assert_eq!(
        mdb(txn.put(fixed, b"k", b"abcde", e).unwrap_err()),
        MdbError::BadValSize
    );
    txn.put(fixed, b"k", b"abce", e).unwrap();
    assert_eq!(
        mdb(txn.put(fixed, b"k", b"ab", e).unwrap_err()),
        MdbError::BadValSize
    );
    // The transaction is still usable after these.
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(plain, b"empty").unwrap(), Some(&b""[..]));
    assert_eq!(txn.get(plain, b"big").unwrap().unwrap().len(), 1 << 20);
    assert_eq!(txn.stat(fixed).unwrap().entries, 2);
}

#[test]
fn keyexist_and_append_rules() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    txn.put(db, b"b", b"1", WriteFlags::empty()).unwrap();
    assert_eq!(
        mdb(txn
            .put(db, b"b", b"2", WriteFlags::NO_OVERWRITE)
            .unwrap_err()),
        MdbError::KeyExist
    );
    assert_eq!(txn.get(db, b"b").unwrap(), Some(&b"1"[..]));
    // APPEND: the key must sort after the last one.
    assert_eq!(
        mdb(txn.put(db, b"a", b"x", WriteFlags::APPEND).unwrap_err()),
        MdbError::KeyExist
    );
    assert_eq!(
        mdb(txn.put(db, b"b", b"x", WriteFlags::APPEND).unwrap_err()),
        MdbError::KeyExist
    );
    txn.put(db, b"c", b"x", WriteFlags::APPEND).unwrap();
    // Flags mdb_put does not take.
    assert_eq!(
        errno(txn.put(db, b"c", b"x", WriteFlags::CURRENT).unwrap_err()),
        libc::EINVAL
    );
    assert_eq!(
        errno(txn.put(db, b"c", b"x", WriteFlags::MULTIPLE).unwrap_err()),
        libc::EINVAL
    );
    txn.commit().unwrap();
}

#[test]
fn dupsort_rules() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap();
    txn.put(db, b"k", b"2", WriteFlags::empty()).unwrap();
    // Same pair again: no error, nothing added.
    txn.put(db, b"k", b"2", WriteFlags::empty()).unwrap();
    assert_eq!(txn.stat(db).unwrap().entries, 1);
    assert_eq!(
        mdb(txn
            .put(db, b"k", b"2", WriteFlags::NO_DUP_DATA)
            .unwrap_err()),
        MdbError::KeyExist
    );
    txn.put(db, b"k", b"5", WriteFlags::APPEND_DUP).unwrap();
    assert_eq!(
        mdb(txn.put(db, b"k", b"3", WriteFlags::APPEND_DUP).unwrap_err()),
        MdbError::KeyExist
    );
    txn.put(db, b"k", b"3", WriteFlags::empty()).unwrap();
    assert_eq!(
        mdb(txn
            .put(db, b"k", b"9", WriteFlags::NO_OVERWRITE)
            .unwrap_err()),
        MdbError::KeyExist
    );
    // RESERVE is not for DUPSORT (C misbehaves, lmdb.h forbids it).
    assert_eq!(
        errno(txn.reserve(db, b"k", 4, WriteFlags::empty()).unwrap_err()),
        libc::EINVAL
    );
    // del of one pair, then of the rest.
    assert!(txn.del(db, b"k", Some(b"3")).unwrap());
    assert!(!txn.del(db, b"k", Some(b"3")).unwrap());
    assert_eq!(txn.stat(db).unwrap().entries, 2);
    assert!(txn.del(db, b"k", None).unwrap());
    assert!(!txn.del(db, b"k", None).unwrap());
    assert_eq!(txn.stat(db).unwrap().entries, 0);
    txn.commit().unwrap();
}

#[test]
fn cursor_current_rules() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    for k in [b"a", b"b", b"c"] {
        txn.put(db, k, b"0123", WriteFlags::empty()).unwrap();
    }
    let mut c = txn.cursor_mut(db).unwrap();
    // Not positioned: EINVAL, as C.
    assert_eq!(
        errno(c.put(b"a", b"x", WriteFlags::CURRENT).unwrap_err()),
        libc::EINVAL
    );
    // Past the end after a failed SET: C asserts in mdb_node_del (or
    // corrupts the page); Rust reports MDB_NOTFOUND.
    assert!(c.set_key(b"zz").unwrap().is_none());
    assert_eq!(
        mdb(c.put(b"zz", b"x", WriteFlags::CURRENT).unwrap_err()),
        MdbError::NotFound
    );
    // On an item: same size in place, other size replaces the node.
    c.set_key(b"b").unwrap().unwrap();
    c.put(b"b", b"WXYZ", WriteFlags::CURRENT).unwrap();
    assert_eq!(c.get_current().unwrap(), Some((&b"b"[..], &b"WXYZ"[..])));
    c.put(b"b", b"longer value", WriteFlags::CURRENT).unwrap();
    assert_eq!(
        c.get_current().unwrap(),
        Some((&b"b"[..], &b"longer value"[..]))
    );
    // Delete moves to the next item; NEXT then does not skip it.
    c.del(WriteFlags::empty()).unwrap();
    assert_eq!(c.get_current().unwrap(), Some((&b"c"[..], &b"0123"[..])));
    drop(c);
    assert_eq!(txn.get(db, b"b").unwrap(), None);
    txn.commit().unwrap();
}

#[test]
fn current_dup_that_does_not_fit_its_subpage() {
    // KNOWN_C_BUGS.md #15: MDB_CURRENT with a longer duplicate in a full
    // sub-page. lmdb.h says this is a delete followed by an insert.
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap();
    for v in [&b"a1"[..], b"b2", b"c3"] {
        txn.put(db, b"k", v, WriteFlags::empty()).unwrap();
    }
    let mut c = txn.cursor_mut(db).unwrap();
    c.get(Some(b"k"), Some(b"b2"), CursorOp::GetBoth)
        .unwrap()
        .unwrap();
    let long = [b'b'; 300];
    c.put(b"k", &long, WriteFlags::CURRENT).unwrap();
    assert_eq!(c.get_current().unwrap(), Some((&b"k"[..], &long[..])));
    assert_eq!(c.count().unwrap(), 3);
    drop(c);
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    let mut got = Vec::new();
    while let Some((_, v)) = c.next().unwrap() {
        got.push(v.to_vec());
    }
    assert_eq!(got, vec![b"a1".to_vec(), long.to_vec(), b"c3".to_vec()]);
    assert_eq!(txn.stat(db).unwrap().entries, 3);
}

#[test]
fn read_your_writes_and_abort() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    txn.put(db, b"keep", b"1", WriteFlags::empty()).unwrap();
    txn.commit().unwrap();
    let id = env.info().unwrap().last_txnid;

    let mut txn = env.begin_rw_txn().unwrap();
    assert_eq!(txn.id(), id as u64 + 1);
    let named = txn.create_db(Some("temp"), DatabaseFlags::CREATE).unwrap();
    for i in 0..2000u32 {
        txn.put(named, &i.to_be_bytes(), b"x", WriteFlags::empty())
            .unwrap();
    }
    txn.put(db, b"gone", b"2", WriteFlags::empty()).unwrap();
    // The transaction sees its own changes, through get and cursors.
    assert_eq!(txn.get(db, b"gone").unwrap(), Some(&b"2"[..]));
    assert_eq!(txn.stat(named).unwrap().entries, 2000);
    {
        let mut c = txn.cursor(named).unwrap();
        assert_eq!(c.last().unwrap().unwrap().0, &1999u32.to_be_bytes()[..]);
    }
    // A reader does not.
    let r = env.begin_ro_txn().unwrap();
    assert_eq!(r.get(db, b"gone").unwrap(), None);
    drop(r);
    // The free DB cannot be opened for writing (EINVAL, as C).
    assert_eq!(errno(txn.cursor(Dbi::FREE).unwrap_err()), libc::EINVAL);
    txn.abort();

    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(db, b"gone").unwrap(), None);
    assert_eq!(txn.get(db, b"keep").unwrap(), Some(&b"1"[..]));
    assert_eq!(
        mdb(txn.open_db(Some("temp")).unwrap_err()),
        MdbError::NotFound
    );
    // The handle of the aborted DB is closed.
    assert!(txn.get(named, b"x").is_err());
    drop(txn);
    assert_eq!(env.info().unwrap().last_txnid, id);
    // An empty commit writes nothing.
    let txn = env.begin_rw_txn().unwrap();
    txn.commit().unwrap();
    assert_eq!(env.info().unwrap().last_txnid, id);
}

#[test]
fn database_handles() {
    let dir = tempfile::tempdir().unwrap();
    let env = Environment::builder()
        .max_dbs(2)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    assert_eq!(
        mdb(txn
            .create_db(Some("x"), DatabaseFlags::empty())
            .unwrap_err()),
        MdbError::NotFound
    );
    let a = txn.create_db(Some("a"), DatabaseFlags::CREATE).unwrap();
    let b = txn
        .create_db(
            Some("b"),
            DatabaseFlags::CREATE | DatabaseFlags::INTEGER_KEY,
        )
        .unwrap();
    assert_eq!(
        mdb(txn.create_db(Some("c"), DatabaseFlags::CREATE).unwrap_err()),
        MdbError::DbsFull
    );
    // Same name: same handle.
    assert_eq!(txn.create_db(Some("a"), DatabaseFlags::CREATE).unwrap(), a);
    assert_eq!(txn.db_flags(b).unwrap(), DatabaseFlags::INTEGER_KEY);
    // A DB record in the main DB cannot be overwritten with plain data.
    assert_eq!(
        mdb(txn
            .put(Dbi::MAIN, b"a", b"data", WriteFlags::empty())
            .unwrap_err()),
        MdbError::Incompatible
    );
    txn.put(b, &7u64.to_ne_bytes(), b"seven", WriteFlags::empty())
        .unwrap();
    // Handles of created DBs are not valid for other transactions yet.
    let r = env.begin_ro_txn().unwrap();
    assert!(r.get(b, &7u64.to_ne_bytes()).is_err());
    drop(r);
    txn.commit().unwrap();
    let r = env.begin_ro_txn().unwrap();
    assert_eq!(r.get(b, &7u64.to_ne_bytes()).unwrap(), Some(&b"seven"[..]));
    assert_eq!(r.open_db(Some("a")).unwrap(), a);
    drop(r);
    // Environment::create_db and unknown flags.
    assert_eq!(env.create_db(Some("a"), DatabaseFlags::empty()).unwrap(), a);
    let mut txn = env.begin_rw_txn().unwrap();
    assert_eq!(
        errno(
            txn.create_db(None, DatabaseFlags::from_bits_retain(0x8000_0000))
                .unwrap_err()
        ),
        libc::EINVAL
    );
}

#[test]
fn main_db_flags_block_named_dbs() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    txn.create_db(None, DatabaseFlags::DUP_SORT).unwrap();
    assert_eq!(
        mdb(txn.create_db(Some("n"), DatabaseFlags::CREATE).unwrap_err()),
        MdbError::Incompatible
    );
    assert_eq!(
        mdb(txn
            .create_db(Some("n"), DatabaseFlags::empty())
            .unwrap_err()),
        MdbError::NotFound
    );
    txn.put(Dbi::MAIN, b"k", b"1", WriteFlags::empty()).unwrap();
    txn.put(Dbi::MAIN, b"k", b"2", WriteFlags::empty()).unwrap();
    txn.commit().unwrap();
    let r = env.begin_ro_txn().unwrap();
    assert_eq!(r.db_flags(Dbi::MAIN).unwrap(), DatabaseFlags::DUP_SORT);
    assert_eq!(r.stat(Dbi::MAIN).unwrap().entries, 2);
}

#[test]
fn read_only_environment() {
    let dir = tempfile::tempdir().unwrap();
    drop(env_at(dir.path(), 1 << 20));
    let env = Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir.path())
        .unwrap();
    assert_eq!(errno(env.begin_rw_txn().unwrap_err()), libc::EACCES);
}

#[test]
fn map_full_poisons_the_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 1 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    let mut i = 0u32;
    let err = loop {
        match txn.put(db, &i.to_be_bytes(), &[0u8; 900], WriteFlags::empty()) {
            Ok(()) => i += 1,
            Err(e) => break e,
        }
    };
    assert_eq!(mdb(err), MdbError::MapFull);
    // Like C, the transaction is unusable now.
    assert_eq!(
        mdb(txn.put(db, b"x", b"y", WriteFlags::empty()).unwrap_err()),
        MdbError::BadTxn
    );
    assert_eq!(mdb(txn.get(db, b"x").unwrap_err()), MdbError::BadTxn);
    assert_eq!(mdb(txn.commit().unwrap_err()), MdbError::BadTxn);
    // Nothing was committed, and the next transaction works.
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.stat(db).unwrap().entries, 0);
}

#[test]
fn set_map_size_and_resize() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 1 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    for i in 0..300u32 {
        txn.put(db, &i.to_be_bytes(), &[1u8; 900], WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    // A reader on the old map keeps its slices while the map is replaced.
    let r = env.begin_ro_txn().unwrap();
    let held = r.get(db, &5u32.to_be_bytes()).unwrap().unwrap();
    // Not while a write transaction is active (EINVAL, as C).
    {
        let w = env.begin_rw_txn().unwrap();
        assert_eq!(errno(env.set_map_size(64 << 20).unwrap_err()), libc::EINVAL);
        drop(w);
    }
    env.set_map_size(64 << 20).unwrap();
    assert_eq!(env.map_size(), 64 << 20);
    assert_eq!(held, &[1u8; 900][..]);
    drop(r);
    // Now much more fits; the new size is recorded in the meta page.
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 0..20_000u32 {
        txn.put(db, &i.to_be_bytes(), &[2u8; 900], WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    drop(env);
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.map_size(), 64 << 20);
    assert_eq!(env.stat().unwrap().entries, 20_000);
}

#[test]
fn map_resized_by_another_process() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 1 << 20);
    let txn = env.begin_rw_txn().unwrap();
    txn.commit().unwrap();
    // C grows the database beyond our map.
    c.peer_ok(&[
        "fill",
        dir.path().to_str().unwrap(),
        "-",
        "4000",
        "7",
        "--mapsize",
        "67108864",
        "--valsize",
        "500",
    ]);
    assert_eq!(mdb(env.begin_rw_txn().unwrap_err()), MdbError::MapResized);
    assert_eq!(mdb(env.begin_ro_txn().unwrap_err()), MdbError::MapResized);
    // mdb_env_set_mapsize(env, 0) adopts the size in the file.
    env.set_map_size(0).unwrap();
    assert_eq!(env.map_size(), 64 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, b"rust", b"was here", WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
    let out = c.peer(&["check-freelist", dir.path().to_str().unwrap()]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
}

#[test]
fn reserve_and_multiple() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(Some("r"), DatabaseFlags::CREATE).unwrap();
    txn.reserve(db, b"small", 5, WriteFlags::empty())
        .unwrap()
        .copy_from_slice(b"hello");
    let big = txn
        .reserve(db, b"big", 100_000, WriteFlags::empty())
        .unwrap();
    big.fill(9);
    // Overwrite of an overflow value with one of the same page count in
    // the same transaction happens in place.
    txn.reserve(db, b"big", 99_000, WriteFlags::empty())
        .unwrap()
        .fill(8);
    let fixed = txn
        .create_db(
            Some("f"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    let items: Vec<u8> = (0..1000u32).flat_map(|i| i.to_be_bytes()).collect();
    {
        let mut c = txn.cursor_mut(fixed).unwrap();
        assert_eq!(
            c.put_multiple(b"k", &items, 4, WriteFlags::empty())
                .unwrap(),
            1000
        );
        assert_eq!(c.count().unwrap(), 1000);
    }
    {
        let mut c = txn.cursor_mut(db).unwrap();
        assert_eq!(
            mdb(c
                .put_multiple(b"k", &items, 4, WriteFlags::empty())
                .unwrap_err()),
            MdbError::Incompatible
        );
    }
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(db, b"small").unwrap(), Some(&b"hello"[..]));
    assert_eq!(txn.get(db, b"big").unwrap(), Some(&vec![8u8; 99_000][..]));
    // All items back, a page at a time, in order.
    let mut c = txn.cursor(fixed).unwrap();
    c.set(b"k").unwrap().unwrap();
    let mut got = c.get_multiple().unwrap().unwrap().1.to_vec();
    while let Some((_, v)) = c.next_multiple().unwrap() {
        got.extend_from_slice(v);
    }
    assert_eq!(got, items);
    assert_eq!(txn.stat(fixed).unwrap().entries, 1000);
    assert_eq!(
        u32::from(txn.db_flags(fixed).unwrap().bits() as u16) & MDB_DUPFIXED,
        MDB_DUPFIXED
    );
}

#[test]
fn dupfixed_without_dupsort_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_at(dir.path(), 16 << 20);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("f"), DatabaseFlags::CREATE | DatabaseFlags::DUP_FIXED)
        .unwrap();
    assert_eq!(
        mdb(txn.put(db, b"k", b"v", WriteFlags::empty()).unwrap_err()),
        MdbError::Incompatible
    );
    // The transaction is still fine.
    txn.commit().unwrap();
}
