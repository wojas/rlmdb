//! Behaviour of the write API: error codes as C returns them, abort,
//! read-your-writes, handles of created databases, map size handling.

mod common;

use rlmdb::consts::*;
use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error, Format, MdbError,
    Transaction, WriteFlags,
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
    // KNOWN_C_BUGS.md row 15: MDB_CURRENT with a longer duplicate in a full
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

/// `KNOWN_C_BUGS.md` row 56: `MDB_CURRENT` replacing a duplicate in a sub-DB
/// tree by an item that sorts where the old one is. C keeps the new item
/// in the old one's leaf at its index and does not look at the branch keys
/// around the leaf, which need not be its items (deleting a leaf's first
/// item leaves the key above it): the first item of a leaf other than the
/// leftmost, replaced by one of another size that sorts before the branch
/// key above the leaf, and the last item of a leaf other than the
/// rightmost, replaced by one at or after the next leaf's branch key, end
/// up where searches do not look. rlmdb deletes and inserts there.
///
/// Every item of a key whose duplicates span several leaves is replaced in
/// turn by items between it and its neighbours (the same size, and without
/// `MDB_DUPFIXED` shorter and longer), also after deleting the item after
/// it, then an item between the new one and the old ones is put: the
/// cursor is on the new item, `MDB_GET_BOTH` finds it, and the duplicates
/// stay in order. In 0.9 and 1.0 (256-byte and 1 KiB pages), with
/// `MDB_DUPSORT`, `MDB_REVERSEDUP` and `MDB_DUPFIXED` (there also
/// `MDB_MULTIPLE` with one item).
/// `rlmdb/tests/write_differential.rs` and
/// `interop/tests/write_differential10.rs` compare the files with C's.
#[test]
fn current_dup_outside_its_leaf() {
    let shapes = [
        (
            Format::V09,
            None,
            160,
            (common::os_page_size() / 24).min(509),
        ),
        (Format::V10, Some(256), 60, 10),
        (Format::V10, Some(1024), 60, 42),
    ];
    for (format, ps, n, sz) in shapes {
        for flags in [
            DatabaseFlags::DUP_SORT,
            DatabaseFlags::DUP_SORT | DatabaseFlags::REVERSE_DUP,
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        ] {
            current_outside_leaf(format, ps, n, sz, flags);
        }
    }
}

/// One shape of [`current_dup_outside_its_leaf`]: `n` items of `sz` bytes
/// under one key of a DB with `flags`.
fn current_outside_leaf(
    format: Format,
    ps: Option<usize>,
    n: usize,
    sz: usize,
    flags: DatabaseFlags,
) {
    let shape = format!("{format:?}, page size {ps:?}, flags {flags:?}");
    let dir = tempfile::tempdir().unwrap();
    let mut b = Environment::builder();
    b.format(format)
        .max_dbs(4)
        .map_size(64 << 20)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = ps {
        b.page_size(ps);
    }
    let env = b.open(dir.path()).unwrap();
    let fixed = flags.contains(DatabaseFlags::DUP_FIXED);
    let reverse = flags.contains(DatabaseFlags::REVERSE_DUP);
    // Stored so that the duplicate order is the byte order of the plain
    // item (MDB_REVERSEDUP compares from the end); `dec` undoes it.
    let enc = |s: &[u8]| -> Vec<u8> {
        let mut b = s.to_vec();
        if reverse {
            b.reverse();
        }
        b
    };
    let dec = enc;
    let item = |j: usize| -> Vec<u8> {
        let mut s = format!("b{j:04}").into_bytes();
        s.resize(sz, b'p');
        s
    };
    let cat = |s: &[u8], t: &str| [s, t.as_bytes()].concat();
    let last = |s: &[u8], c: u8| {
        let mut s = s.to_vec();
        *s.last_mut().unwrap() = c;
        s
    };
    let key = b"k";
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), flags | DatabaseFlags::CREATE)
        .unwrap();
    for j in 0..n {
        txn.put(db, key, &enc(&item(j)), WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    let mut count = n;
    // Replaces item j by `nw` (after deleting item `del`, if any), puts
    // `between` (if any) and checks the duplicates; commits if `commit`.
    let mut attempt =
        |j: usize, del: Option<usize>, nw: &[u8], between: Option<&[u8]>, commit: bool| {
            let what = format!(
                "{shape}: item {j} -> {:?}.. of {} bytes (deleted {del:?})",
                String::from_utf8_lossy(&nw[..nw.len().min(6)]),
                nw.len()
            );
            let mut txn = env.begin_rw_txn().unwrap();
            let c = txn.open_cursor(db).unwrap();
            let mut want = count;
            if let Some(del) = del {
                txn.cursor_get(c, Some(key), Some(&enc(&item(del))), CursorOp::GetBoth)
                    .unwrap()
                    .unwrap();
                txn.cursor_del(c, WriteFlags::empty()).unwrap();
                want -= 1;
            }
            txn.cursor_get(c, Some(key), Some(&enc(&item(j))), CursorOp::GetBoth)
                .unwrap()
                .unwrap();
            if fixed && j % 2 == 1 {
                // MDB_MULTIPLE with one item replaces it the same way.
                let stored = txn
                    .cursor_put_multiple(c, key, &enc(nw), nw.len(), WriteFlags::CURRENT)
                    .unwrap();
                assert_eq!(stored, 1, "{what}: items stored");
            } else {
                txn.cursor_put(c, key, &enc(nw), WriteFlags::CURRENT)
                    .unwrap();
            }
            let cur = txn
                .cursor_get(c, None, None, CursorOp::GetCurrent)
                .unwrap()
                .map(|(_, d)| d.to_vec());
            assert_eq!(cur, Some(enc(nw)), "{what}: the cursor");
            let found = txn
                .cursor_get(c, Some(key), Some(&enc(nw)), CursorOp::GetBoth)
                .unwrap()
                .is_some();
            assert!(found, "{what}: MDB_GET_BOTH");
            if let Some(between) = between {
                txn.put(db, key, &enc(between), WriteFlags::empty())
                    .unwrap();
                want += 1;
            }
            let mut got = Vec::new();
            let mut op = CursorOp::Set;
            while let Some((_, d)) = txn
                .cursor_get(c, (op == CursorOp::Set).then_some(&key[..]), None, op)
                .unwrap()
            {
                got.push(dec(d));
                op = CursorOp::NextDup;
            }
            assert_eq!(got.len(), want, "{what}: items");
            assert!(got.is_sorted(), "{what}: not in order");
            txn.close_cursor(c);
            if commit {
                txn.commit().unwrap();
                count = want;
            }
        };
    for j in 1..n - 1 {
        let prev = item(j - 1);
        // Between item j-1 and item j: the same size, and (without
        // MDB_DUPFIXED) shorter and longer.
        attempt(j, None, &last(&item(j), b'o'), None, false);
        if !fixed {
            let (s, b) = (cat(&prev[..5], "q"), cat(&prev[..5], "r"));
            attempt(j, None, &s, Some(&b), false);
            let (s, b) = (cat(&prev, "q"), cat(&prev, "r"));
            attempt(j, None, &s, Some(&b), false);
        }
        // Item j-1 after deleting item j, by items between item j-2 and
        // item j+1, at or after the old item j (the stale branch key where
        // item j was the first of its leaf); then item j put back.
        let next = item(j);
        attempt(j - 1, Some(j), &next, None, false);
        attempt(j - 1, Some(j), &last(&next, b'q'), Some(&next), false);
        if !fixed {
            attempt(j - 1, Some(j), &cat(&next[..5], "q"), Some(&next), false);
            attempt(j - 1, Some(j), &cat(&next, "q"), Some(&next), false);
        }
    }
    // One of each, committed.
    attempt(n / 2, Some(n / 2 + 1), &item(n / 2 + 1), None, true);
    if !fixed {
        attempt(n / 3, None, &cat(&item(n / 3 - 1)[..5], "q"), None, true);
    }
    // A fresh read transaction sees every item, in order.
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    let mut got = Vec::new();
    while let Some((_, d)) = c.next().unwrap() {
        got.push(dec(d));
    }
    assert_eq!(got.len(), count, "{shape}: committed items");
    assert!(got.is_sorted(), "{shape}: committed items not in order");
    for d in &got {
        let found = c
            .get(Some(key), Some(&enc(d)), CursorOp::GetBoth)
            .unwrap()
            .is_some();
        assert!(found, "{shape}: {:?} not found", String::from_utf8_lossy(d));
    }
}

/// `KNOWN_C_BUGS.md` row 58: `MDB_CURRENT` with a key other than the
/// cursor's. C writes the caller's key over the node where it rewrites it
/// (data of another size, or too large for the overflow pages in place),
/// leaving the leaf unsorted, and keeps the old key where it overwrites the
/// data in place; rlmdb keeps the cursor's key for every size. Wrong keys
/// that sort before and after every key, next to the item, equal to a
/// neighbour's and longer than any; data of the same size, larger, smaller,
/// on overflow pages (in place and not) and reserved; then puts of keys
/// between the items, which C would place by the unsorted leaf. Both
/// formats, small 1.0 pages too.
#[test]
fn current_with_another_key() {
    let shapes = [
        (Format::V09, None),
        (Format::V10, None),
        (Format::V10, Some(256)),
        (Format::V10, Some(1024)),
    ];
    for (format, ps) in shapes {
        for flags in [DatabaseFlags::empty(), DatabaseFlags::DUP_SORT] {
            current_other_key(format, ps, flags);
        }
    }
}

/// One shape of [`current_with_another_key`].
fn current_other_key(format: Format, ps: Option<usize>, flags: DatabaseFlags) {
    use std::collections::BTreeMap;
    let shape = format!("{format:?}, page size {ps:?}, flags {flags:?}");
    let dir = tempfile::tempdir().unwrap();
    let mut b = Environment::builder();
    b.format(format)
        .max_dbs(4)
        .map_size(64 << 20)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = ps {
        b.page_size(ps);
    }
    let env = b.open(dir.path()).unwrap();
    let psize = env.stat().unwrap().page_size as usize;
    let maxkey = env.max_key_size();
    let dupsort = flags.contains(DatabaseFlags::DUP_SORT);
    let key = |i: usize| format!("k{i:04}").into_bytes();
    let val = |len: usize, seed: usize| -> Vec<u8> {
        (0..len).map(|b| (b * 7 + seed * 13 + 1) as u8).collect()
    };
    // Enough keys of 8-byte values for several full leaves.
    let n = (4 * psize / 24).max(60);
    let mut model: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), flags | DatabaseFlags::CREATE)
        .unwrap();
    for i in 0..n {
        txn.put(db, &key(i), &val(8, i), WriteFlags::empty())
            .unwrap();
        model.insert(key(i), vec![val(8, i)]);
    }
    // Every seventh key gets duplicates (on a sub-page) in a DUPSORT DB.
    let dup_items = |i: usize| -> Vec<Vec<u8>> {
        [10u8, 20, 30]
            .iter()
            .map(|&d| [&[b'd', d][..], &val(6, i)].concat())
            .collect()
    };
    if dupsort {
        for i in (3..n).step_by(7) {
            txn.del(db, &key(i), None).unwrap();
            for d in dup_items(i) {
                txn.put(db, &key(i), &d, WriteFlags::empty()).unwrap();
            }
            model.insert(key(i), dup_items(i));
        }
    }
    let c = txn.open_cursor(db).unwrap();
    // An empty key fails as in C (the size check comes first).
    txn.cursor_get(c, Some(&key(1)), None, CursorOp::Set)
        .unwrap()
        .unwrap();
    assert_eq!(
        mdb(txn
            .cursor_put(c, b"", &val(8, 0), WriteFlags::CURRENT)
            .unwrap_err()),
        MdbError::BadValSize,
        "{shape}"
    );
    let wrong_keys = |i: usize| -> Vec<Vec<u8>> {
        let mut between = key(i);
        between.push(b'x');
        vec![
            b"a".to_vec(),
            b"zz".to_vec(),
            between,
            key(i + 1),
            key(i - 1),
            vec![b'w'; maxkey],
        ]
    };
    // Data sizes: the same, larger, smaller, then (plain DB) on overflow
    // pages, overwritten there in place, and back inline; for DUPSORT the
    // largest item (it does not fit a node with 256-byte pages,
    // KNOWN_C_BUGS.md row 38).
    let sizes: Vec<usize> = if dupsort {
        vec![8, 40, 2, maxkey, 8]
    } else {
        vec![8, 60, 2, 3 * psize, 3 * psize - 100, 8]
    };
    let mut step = 0;
    let mut replace =
        |txn: &mut rlmdb::RwTxn<'_>, i: usize, wrong: &[u8], data: &[u8], reserve: bool| {
            step += 1;
            let what = format!(
                "{shape}: key {i} by {:?}.. ({} bytes), {} bytes of data, step {step}",
                String::from_utf8_lossy(&wrong[..wrong.len().min(6)]),
                wrong.len(),
                data.len()
            );
            if reserve {
                let buf = txn
                    .cursor_reserve(c, wrong, data.len(), WriteFlags::CURRENT)
                    .unwrap();
                buf.copy_from_slice(data);
            } else {
                txn.cursor_put(c, wrong, data, WriteFlags::CURRENT).unwrap();
            }
            let cur = txn
                .cursor_get(c, None, None, CursorOp::GetCurrent)
                .unwrap()
                .map(|(k, d)| (k.to_vec(), d.to_vec()));
            assert_eq!(cur, Some((key(i), data.to_vec())), "{what}: the cursor");
            what
        };
    for i in (1..n - 1).step_by(n / 9) {
        if model[&key(i)].len() > 1 {
            continue; // duplicates: below
        }
        for wrong in wrong_keys(i) {
            for (s, &len) in sizes.iter().enumerate() {
                let data = val(len, i + s);
                txn.cursor_get(c, Some(&key(i)), None, CursorOp::Set)
                    .unwrap()
                    .unwrap();
                let reserve = !dupsort && s == 0;
                let what = replace(&mut txn, i, &wrong, &data, reserve);
                model.insert(key(i), vec![data.clone()]);
                assert_eq!(
                    txn.get(db, &key(i)).unwrap(),
                    Some(&data[..]),
                    "{what}: get"
                );
                let other = model.get(&wrong).map(|v| v[0].clone());
                assert_eq!(
                    txn.get(db, &wrong).unwrap().map(<[u8]>::to_vec),
                    other,
                    "{what}: get of the other key"
                );
            }
        }
    }
    if dupsort {
        // A duplicate replaced through a wrong key: the key stays, also
        // where the sub-page is rebuilt (an item of another size).
        // The middle one of the three is replaced by items between the
        // other two: of the same size, larger and smaller.
        for i in (3..n).step_by(7).take(4) {
            let news = [
                (&b"a"[..], [&[b'd', 20][..], &val(6, i + 50)].concat()),
                (b"zz", [&[b'd', 21][..], &val(12, i)].concat()),
                (b"k", vec![b'd', 22]),
            ];
            for (wrong, nw) in news {
                let mid = model[&key(i)][1].clone();
                txn.cursor_get(c, Some(&key(i)), Some(&mid), CursorOp::GetBoth)
                    .unwrap()
                    .unwrap();
                let what = replace(&mut txn, i, wrong, &nw, false);
                let dups = model.get_mut(&key(i)).unwrap();
                dups[1] = nw;
                assert!(dups.is_sorted(), "{what}: the test's items");
                assert_eq!(txn.get(db, wrong).unwrap(), None, "{what}: get");
            }
        }
    }
    txn.close_cursor(c);
    txn.commit().unwrap();
    let pairs = |m: &BTreeMap<Vec<u8>, Vec<Vec<u8>>>| -> Vec<common::Pair> {
        m.iter()
            .flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone())))
            .collect()
    };
    common::verify_db(&env, db, &pairs(&model));
    // Keys between the items land in order (C's unsorted leaf would send
    // them elsewhere).
    let mut txn = env.begin_rw_txn().unwrap();
    for i in (0..n).step_by(3) {
        let mut k = key(i);
        k.push(b'm');
        txn.put(db, &k, &val(8, i), WriteFlags::empty()).unwrap();
        model.insert(k, vec![val(8, i)]);
    }
    txn.commit().unwrap();
    common::verify_db(&env, db, &pairs(&model));
    if format == Format::V09 {
        if let Some(c) = common::c_tools() {
            let out = c.peer(&["check-freelist", dir.path().to_str().unwrap()]);
            assert_eq!(out.code, 0, "{shape}: {}\n{}", out.stdout, out.stderr);
        }
    }
}

/// `mdb_cursor_count` of a cursor on a key after another cursor (or a
/// plain put, or `MDB_MULTIPLE`) put duplicates of that key: converting
/// the key's single item, into its sub-page, growing it, promoting it to a
/// sub-DB, into the sub-DB, splitting the sub-DB's leaf; before and after
/// the cursor's item. The count is the key's (C's is the one before the
/// put, `KNOWN_C_BUGS.md` row 14; `rlmdb-capi/tests/c/capi_write.c` runs
/// the same against C), and the cursor stays on its item. Both formats, 1.0
/// at 256 B and 1 KiB pages too, `MDB_DUPSORT` and `MDB_DUPFIXED`.
#[test]
fn count_after_another_cursors_put() {
    let shapes = [
        (Format::V09, None),
        (Format::V10, None),
        (Format::V10, Some(256)),
        (Format::V10, Some(1024)),
    ];
    for (format, ps) in shapes {
        for flags in [
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
            DatabaseFlags::DUP_SORT,
        ] {
            count_after_put(format, ps, flags);
        }
    }
}

/// One shape of [`count_after_another_cursors_put`].
fn count_after_put(format: Format, ps: Option<usize>, flags: DatabaseFlags) {
    let shape = format!("{format:?}, page size {ps:?}, flags {flags:?}");
    let dir = tempfile::tempdir().unwrap();
    let mut b = Environment::builder();
    b.format(format)
        .max_dbs(200)
        .map_size(64 << 20)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = ps {
        b.page_size(ps);
    }
    let env = b.open(dir.path()).unwrap();
    let fixed = flags.contains(DatabaseFlags::DUP_FIXED);
    let item = |v: u32| v.to_be_bytes();
    let mut txn = env.begin_rw_txn().unwrap();
    let mut seq = 0;
    // A new DB with "a" and "z" around "k", which gets items 10, 20, ...:
    // `n` of them, or (n == 0) until a put adds a leaf page for the
    // `from`-th time (1: "k" moves to a sub-DB; 2: its leaf splits); the
    // DB and the number of items.
    let mut fill = |txn: &mut rlmdb::RwTxn<'_>, n: u32, from: u32| {
        seq += 1;
        let name = format!("count{seq}");
        let db = txn
            .create_db(Some(&name), flags | DatabaseFlags::CREATE)
            .unwrap();
        for k in [b"a", b"z"] {
            txn.put(db, k, &item(1), WriteFlags::empty()).unwrap();
        }
        let mut added = 0;
        for i in 1.. {
            let leaves = txn.stat(db).unwrap().leaf_pages;
            txn.put(db, b"k", &item(10 * i), WriteFlags::empty())
                .unwrap();
            if n == 0 && txn.stat(db).unwrap().leaf_pages != leaves {
                added += 1;
                if added == from {
                    return (db, i);
                }
            }
            if i == n {
                return (db, n);
            }
        }
        unreachable!()
    };
    let (_, s) = fill(&mut txn, 0, 1);
    let (_, t) = fill(&mut txn, 0, 2);
    assert!(s > 4 && t > s, "{shape}: sub-DB at {s}, split at {t}");
    let cases = [
        ("single item, after", 1, 10, 15),
        ("single item, before", 1, 10, 5),
        ("sub-page, after", 3, 20, 25),
        ("sub-page, before", 3, 20, 15),
        ("sub-page grows", 4, 20, 45),
        ("to a sub-DB, after", s - 1, 20, 10 * s + 5),
        ("to a sub-DB, before", s - 1, 20, 5),
        ("sub-DB, after", s + 5, 20, 25),
        ("sub-DB, before", s + 5, 20, 15),
        ("sub-DB leaf splits, after", t - 1, 10, 10 * t + 5),
        ("sub-DB leaf splits, before", t - 1, 10 * (t - 1), 5),
    ];
    for how in 0..if fixed { 3 } else { 2 } {
        for &(what, n, on, v) in &cases {
            let what = format!("{shape}: {what}, put {how}");
            let (db, _) = fill(&mut txn, n, 0);
            let a = txn.open_cursor(db).unwrap();
            let b = txn.open_cursor(db).unwrap();
            txn.cursor_get(a, Some(b"k"), Some(&item(on)), CursorOp::GetBoth)
                .unwrap()
                .unwrap();
            let new: Vec<u32> = if how == 2 {
                vec![v, v + 1, v + 2]
            } else {
                vec![v]
            };
            match how {
                0 => txn
                    .cursor_put(b, b"k", &item(v), WriteFlags::empty())
                    .unwrap(),
                1 => txn.put(db, b"k", &item(v), WriteFlags::empty()).unwrap(),
                _ => {
                    let items: Vec<u8> = new.iter().flat_map(|&x| item(x)).collect();
                    let stored = txn
                        .cursor_put_multiple(b, b"k", &items, 4, WriteFlags::empty())
                        .unwrap();
                    assert_eq!(stored, 3, "{what}");
                }
            }
            let mut all: Vec<u32> = (1..=n).map(|i| 10 * i).chain(new).collect();
            all.sort_unstable();
            assert_eq!(
                txn.cursor_count(a).unwrap(),
                all.len() as u64,
                "{what}: count"
            );
            let dup = |e: Option<(&[u8], &[u8])>| {
                e.map(|(_, d)| u32::from_be_bytes(d.try_into().unwrap()))
            };
            let cur = txn.cursor_get(a, None, None, CursorOp::GetCurrent).unwrap();
            assert_eq!(dup(cur), Some(on), "{what}: current");
            let at = all.iter().position(|&x| x == on).unwrap();
            let next = txn.cursor_get(a, None, None, CursorOp::NextDup).unwrap();
            assert_eq!(dup(next), all.get(at + 1).copied(), "{what}: next dup");
            assert_eq!(
                txn.cursor_count(a).unwrap(),
                all.len() as u64,
                "{what}: count after next dup"
            );
            let prev = txn.cursor_get(a, None, None, CursorOp::PrevDup).unwrap();
            let want = if at + 1 < all.len() {
                Some(on)
            } else {
                at.checked_sub(1).map(|p| all[p])
            };
            assert_eq!(dup(prev), want, "{what}: previous dup");
            txn.close_cursor(a);
            txn.close_cursor(b);
        }
    }
    txn.commit().unwrap();
}

/// Moves after a failed exact search (`KNOWN_C_BUGS.md` row 59). `MDB_SET`
/// of "b" leaves the cursor on the node of "c", whose duplicates the test
/// varies, with the sub-cursor reset. The next `MDB_NEXT`, `MDB_PREV`,
/// `MDB_NEXT_DUP`, `MDB_PREV_DUP` or `MDB_NEXT_MULTIPLE` enters those
/// duplicates; C enters them with the record its sub-cursor last read,
/// which here is stale. Either another cursor added duplicates since then
/// (C counts too few; after a sub-DB's root split it walks the old root,
/// and a sub-page the put moved crashes it), or it is another key's
/// record ("e": C walks its duplicates), or there is none (the cursor
/// never was on duplicates, row 6: C asserts); a sub-cursor that a delete
/// marked (`C_DEL`, row 29: "b" was deleted under the cursor, moving it
/// onto "c") keeps the mark, and C returns the first duplicate twice. rlmdb
/// sets the sub-cursor up from the node, so the walk sees every duplicate
/// of "c" once, in order, the
/// count is right, and the walk ends on the key next to "c" ("a" or "e")
/// as C's walk does with a right record (the cases "the record is right"
/// have one: there rlmdb and C agree). The
/// count and `MDB_GET_CURRENT` right after the failed search stay C's
/// `EINVAL`. Write transactions in both formats, 1.0 at 256 B and 1 KiB
/// pages too, `MDB_DUPSORT` and `MDB_DUPFIXED`; read transactions for the
/// stale records that need no other cursor.
#[test]
fn moves_after_a_failed_search() {
    let shapes = [
        (Format::V09, None),
        (Format::V10, None),
        (Format::V10, Some(256)),
        (Format::V10, Some(1024)),
    ];
    for (format, ps) in shapes {
        for flags in [
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
            DatabaseFlags::DUP_SORT,
        ] {
            moves_after_failed_search(format, ps, flags);
        }
    }
}

/// What the cursor was on before the failed search, in
/// [`moves_after_a_failed_search`].
#[derive(Clone, Copy, Debug)]
enum Before {
    /// "c" itself.
    Same,
    /// "e", with this many duplicates of 8 bytes (those of "c" have 4).
    Other(u32),
    /// Nothing: a new cursor.
    Fresh,
    /// "b", which `mdb_del` then deleted, moving the cursor onto "c".
    Deleted,
}

/// A cursor of [`moves_after_a_failed_search`], in a write or a read
/// transaction; results are copied out.
trait Moves {
    fn get(
        &mut self,
        key: Option<&[u8]>,
        op: CursorOp,
    ) -> rlmdb::Result<Option<(Vec<u8>, Vec<u8>)>>;
    fn count(&mut self) -> rlmdb::Result<u64>;
}

impl Moves for (&mut rlmdb::RwTxn<'_>, rlmdb::CursorId) {
    fn get(
        &mut self,
        key: Option<&[u8]>,
        op: CursorOp,
    ) -> rlmdb::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let (txn, c) = self;
        let e = txn.cursor_get(*c, key, None, op)?;
        Ok(e.map(|(k, d)| (k.to_vec(), d.to_vec())))
    }

    fn count(&mut self) -> rlmdb::Result<u64> {
        self.0.cursor_count(self.1)
    }
}

impl Moves for rlmdb::RoCursor<'_> {
    fn get(
        &mut self,
        key: Option<&[u8]>,
        op: CursorOp,
    ) -> rlmdb::Result<Option<(Vec<u8>, Vec<u8>)>> {
        let e = rlmdb::RoCursor::get(self, key, None, op)?;
        Ok(e.map(|(k, d)| (k.to_vec(), d.to_vec())))
    }

    fn count(&mut self) -> rlmdb::Result<u64> {
        rlmdb::RoCursor::count(self)
    }
}

/// Puts "a" and "c" (items `10, 20, ..., 10 n`, 4 bytes) and "e" (`m`
/// items `0, 1, ...` of 8 bytes if `m > 0`, else one of 4 bytes) into a new
/// database "t".
fn put_ace(txn: &mut rlmdb::RwTxn<'_>, flags: DatabaseFlags, n: u32, m: u32) -> Dbi {
    let db = txn
        .create_db(Some("t"), flags | DatabaseFlags::CREATE)
        .unwrap();
    let e = WriteFlags::empty();
    txn.put(db, b"a", &1u32.to_be_bytes(), e).unwrap();
    for i in 1..=n {
        txn.put(db, b"c", &(10 * i).to_be_bytes(), e).unwrap();
    }
    if m == 0 {
        txn.put(db, b"e", &1u32.to_be_bytes(), e).unwrap();
    }
    for i in 0..u64::from(m) {
        txn.put(db, b"e", &i.to_be_bytes(), e).unwrap();
    }
    db
}

/// Positions `c` as `before` says (for [`Before::Deleted`] the caller
/// deletes "b").
fn position(c: &mut impl Moves, before: Before) {
    let key: &[u8] = match before {
        Before::Same => b"c",
        Before::Other(_) => b"e",
        Before::Deleted => b"b",
        Before::Fresh => return,
    };
    assert!(c.get(Some(key), CursorOp::Set).unwrap().is_some());
}

/// Searches "b" and checks what C's argument checks say about the cursor
/// now; `MDB_GET_CURRENT` only if `current` (C's clears the sub-cursor's
/// `C_DEL` mark, also when it fails).
fn miss_b(c: &mut impl Moves, current: bool, what: &str) {
    assert_eq!(c.get(Some(b"b"), CursorOp::Set).unwrap(), None, "{what}");
    // The sub-cursor is not on an item: C's EINVAL, kept.
    assert_eq!(errno(c.count().unwrap_err()), libc::EINVAL, "{what}: count");
    if current {
        let cur = c.get(None, CursorOp::GetCurrent).unwrap_err();
        assert_eq!(errno(cur), libc::EINVAL, "{what}: current");
    }
}

/// `op` and then the same move while on "c" (`MDB_NEXT_MULTIPLE`: pages of
/// items): every item of `all` once, in order; the count after the first
/// move; the key the walk ends on.
fn walk_c(c: &mut impl Moves, op: CursorOp, all: &[u32], what: &str) {
    let back = matches!(op, CursorOp::Prev | CursorOp::PrevDup);
    let mut seen = Vec::new();
    let mut e = c.get(None, op).unwrap();
    assert_eq!(c.count().unwrap(), all.len() as u64, "{what}: count");
    while let Some((_, d)) = e.as_ref().filter(|(k, _)| k == b"c") {
        assert!(!d.is_empty() && d.len() % 4 == 0, "{what}: {d:?}");
        seen.extend(
            d.chunks(4)
                .map(|x| u32::from_be_bytes(x.try_into().unwrap())),
        );
        e = c.get(None, op).unwrap();
    }
    if back {
        seen.reverse();
    }
    assert_eq!(seen, all, "{what}: walk");
    let end = match op {
        CursorOp::Next => Some(&b"e"[..]),
        CursorOp::Prev => Some(&b"a"[..]),
        _ => None,
    };
    assert_eq!(
        e.as_ref().map(|(k, _)| &k[..]),
        end,
        "{what}: after the walk"
    );
}

/// One shape of [`moves_after_a_failed_search`].
fn moves_after_failed_search(format: Format, ps: Option<usize>, flags: DatabaseFlags) {
    let shape = format!("{format:?}, page size {ps:?}, flags {flags:?}");
    let dir = tempfile::tempdir().unwrap();
    let mut b = Environment::builder();
    b.format(format)
        .max_dbs(4)
        .map_size(64 << 20)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = ps {
        b.page_size(ps);
    }
    let env = b.open(dir.path()).unwrap();
    let fixed = flags.contains(DatabaseFlags::DUP_FIXED);
    // How many items "c" has when it moves to a sub-DB (s) and when that
    // sub-DB's leaf splits (t).
    let (s, t) = {
        let mut txn = env.begin_rw_txn().unwrap();
        let db = put_ace(&mut txn, flags, 0, 0);
        let (mut s, mut i) = (0, 0u32);
        loop {
            i += 1;
            let leaves = txn.stat(db).unwrap().leaf_pages;
            txn.put(db, b"c", &(10 * i).to_be_bytes(), WriteFlags::empty())
                .unwrap();
            if txn.stat(db).unwrap().leaf_pages != leaves {
                if s > 0 {
                    break (s, i);
                }
                s = i;
            }
        }
    };
    assert!(s > 4 && t > s + 1, "{shape}: sub-DB at {s}, split at {t}");
    // A sub-DB of one leaf.
    let sub = s + 1;
    let odd = |n: u32| (0..n).map(|i| 10 * i + 5).collect::<Vec<u32>>();
    // (what, items of "c", the cursor before, items another cursor adds,
    // with MDB_MULTIPLE).
    let mut cases = vec![
        (
            "sub-page, another cursor adds one",
            3,
            Before::Same,
            vec![25],
            false,
        ),
        (
            "sub-page becomes a sub-DB",
            s - 1,
            Before::Same,
            vec![5, 15],
            false,
        ),
        (
            "sub-DB, another cursor adds one",
            sub,
            Before::Same,
            vec![25],
            false,
        ),
        (
            "sub-DB's root splits",
            sub,
            Before::Same,
            odd(t - s + 1),
            false,
        ),
        ("another key's sub-page", 3, Before::Other(2), vec![], false),
        (
            "another key's sub-DB",
            3,
            Before::Other(s + 1),
            vec![],
            false,
        ),
        (
            "a sub-DB, another key's sub-page",
            sub,
            Before::Other(2),
            vec![],
            false,
        ),
        (
            "a sub-page, never on duplicates",
            3,
            Before::Fresh,
            vec![],
            false,
        ),
        (
            "a sub-DB, never on duplicates",
            sub,
            Before::Fresh,
            vec![],
            false,
        ),
        (
            "sub-page, a delete marked it",
            3,
            Before::Deleted,
            vec![],
            false,
        ),
        (
            "sub-DB, a delete marked it",
            sub,
            Before::Deleted,
            vec![],
            false,
        ),
        (
            "sub-page, the record is right",
            3,
            Before::Same,
            vec![],
            false,
        ),
        (
            "sub-DB, the record is right",
            sub,
            Before::Same,
            vec![],
            false,
        ),
    ];
    let mut ops = vec![
        CursorOp::Next,
        CursorOp::Prev,
        CursorOp::NextDup,
        CursorOp::PrevDup,
    ];
    if fixed {
        cases.push((
            "sub-page, MDB_MULTIPLE of two",
            3,
            Before::Same,
            vec![15, 25],
            true,
        ));
        cases.push((
            "sub-DB's root splits, MDB_MULTIPLE",
            sub,
            Before::Same,
            odd(t - s + 1),
            true,
        ));
        ops.push(CursorOp::NextMultiple);
    }
    for (what, n, before, add, multiple) in &cases {
        let m = match before {
            Before::Other(m) => *m,
            _ => 0,
        };
        let mut all: Vec<u32> = (1..=*n)
            .map(|i| 10 * i)
            .chain(add.iter().copied())
            .collect();
        all.sort_unstable();
        // Each move in a nested transaction (aborted) of one that holds the
        // data, so that the data is put once.
        let mut txn = env.begin_rw_txn().unwrap();
        let db = put_ace(&mut txn, flags, *n, m);
        let deleted = matches!(before, Before::Deleted);
        if deleted {
            txn.put(db, b"b", &1u32.to_be_bytes(), WriteFlags::empty())
                .unwrap();
        }
        for &op in &ops {
            let what = format!("{shape}: {what}, {op:?}");
            let mut child = txn.begin_nested_txn().unwrap();
            let c0 = child.open_cursor(db).unwrap();
            let c1 = child.open_cursor(db).unwrap();
            position(&mut (&mut child, c0), *before);
            if deleted {
                assert!(child.del(db, b"b", None).unwrap(), "{what}");
            }
            miss_b(&mut (&mut child, c0), !deleted, &what);
            if *multiple {
                let items: Vec<u8> = add.iter().flat_map(|v| v.to_be_bytes()).collect();
                let stored = child
                    .cursor_put_multiple(c1, b"c", &items, 4, WriteFlags::empty())
                    .unwrap();
                assert_eq!(stored, add.len(), "{what}");
            } else {
                for v in add {
                    child
                        .cursor_put(c1, b"c", &v.to_be_bytes(), WriteFlags::empty())
                        .unwrap();
                }
            }
            walk_c(&mut (&mut child, c0), op, &all, &what);
            // MDB_PREV_MULTIPLE does not enter the duplicates: C's
            // MDB_NOTFOUND.
            if fixed && op == CursorOp::Next {
                let what = format!("{what}, then MDB_PREV_MULTIPLE");
                let c2 = child.open_cursor(db).unwrap();
                if !deleted {
                    position(&mut (&mut child, c2), *before);
                }
                miss_b(&mut (&mut child, c2), true, &what);
                let got = child.cursor_get(c2, None, None, CursorOp::PrevMultiple);
                assert!(matches!(got, Ok(None)), "{what}: {got:?}");
            }
        }
        if !add.is_empty() || deleted {
            continue;
        }
        // The stale records that need no other cursor, in read
        // transactions.
        txn.commit().unwrap();
        for &op in &ops {
            let what = format!("{shape}: {what}, {op:?}, read transaction");
            let r = env.begin_ro_txn().unwrap();
            let mut c = r.cursor(db).unwrap();
            position(&mut c, *before);
            miss_b(&mut c, true, &what);
            walk_c(&mut c, op, &all, &what);
        }
        let mut txn = env.begin_rw_txn().unwrap();
        txn.drop_db(db).unwrap();
        txn.commit().unwrap();
    }
}
