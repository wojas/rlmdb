//! The core API additions of stage 3 (C2) that the C API, tools and
//! interop crates build on: byte-string DB names, comparators set through a
//! transaction, cursor state queries, `Info::map_address`,
//! `Environment::has_lock_file`, read-only and `NO_LOCK` environments.

use std::cmp::Ordering;

use rlmdb::{
    Comparator, CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Transaction,
    WriteFlags,
};

fn open(dir: &std::path::Path, flags: EnvironmentFlags) -> Environment {
    Environment::builder()
        .map_size(16 << 20)
        .max_dbs(4)
        .flags(flags)
        .open(dir)
        .unwrap()
}

fn reverse(a: &[u8], b: &[u8]) -> Ordering {
    b.cmp(a)
}

#[test]
fn comparators_set_through_a_transaction_apply_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), EnvironmentFlags::NO_SYNC);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("r"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap();
    // C's usual sequence: mdb_dbi_open, mdb_set_compare, then use.
    txn.set_compare(db, Some(reverse)).unwrap();
    txn.set_dupsort(db, Some(reverse)).unwrap();
    let (k, d) = txn.comparators(db).unwrap();
    assert!(matches!(k, Comparator::Custom(_)));
    assert!(matches!(d, Some(Comparator::Custom(_))));
    for key in [b"a", b"c", b"b"] {
        for data in [b"1", b"3", b"2"] {
            txn.put(db, key, data, WriteFlags::empty()).unwrap();
        }
    }
    let mut c = txn.cursor(db).unwrap();
    let mut got = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        got.push(format!("{}{}", k[0] as char, v[0] as char));
    }
    assert_eq!(got, ["c3", "c2", "c1", "b3", "b2", "b1", "a3", "a2", "a1"]);
    txn.commit().unwrap();
    // The setting belongs to the handle: later transactions use it too.
    let txn = env.begin_ro_txn().unwrap();
    assert!(matches!(
        txn.comparators(db).unwrap().0,
        Comparator::Custom(_)
    ));
    // Back to the default through a read transaction.
    txn.set_compare(db, None).unwrap();
    assert!(!matches!(
        txn.comparators(db).unwrap().0,
        Comparator::Custom(_)
    ));
    assert!(
        matches!(txn.comparators(Dbi::FREE).unwrap().0, Comparator::Long),
        "the free DB orders its keys with mdb_cmp_long"
    );
    assert_eq!(
        txn.set_compare(Dbi::FREE, Some(reverse))
            .unwrap_err()
            .to_err_code(),
        libc::EINVAL
    );
}

#[test]
fn byte_names() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), EnvironmentFlags::NO_SYNC);
    let mut txn = env.begin_rw_txn().unwrap();
    let a = txn
        .create_db_bytes(Some(b"caf\xe9"), DatabaseFlags::CREATE)
        .unwrap();
    txn.put(a, b"k", b"v", WriteFlags::empty()).unwrap();
    assert_eq!(txn.open_db_bytes(Some(b"caf\xe9")).unwrap(), a);
    assert_eq!(txn.open_db_bytes(None).unwrap(), Dbi::MAIN);
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let a2 = txn.open_db_bytes(Some(b"caf\xe9")).unwrap();
    assert_eq!(txn.get(a2, b"k").unwrap(), Some(&b"v"[..]));
    // The main DB lists the name as its key.
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    assert_eq!(c.first().unwrap().unwrap().0, b"caf\xe9");
}

#[test]
fn cursor_state_queries() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), EnvironmentFlags::NO_SYNC);
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap();
    txn.put(db, b"one", b"x", WriteFlags::empty()).unwrap();
    txn.put(db, b"two", b"x", WriteFlags::empty()).unwrap();
    txn.put(db, b"two", b"y", WriteFlags::empty()).unwrap();
    // Handle-based cursor of the write transaction.
    let id = txn.open_cursor(db).unwrap();
    assert!(!txn.cursor_is_initialized(id).unwrap());
    assert!(!txn.cursor_current_has_dups(id).unwrap());
    txn.cursor_get(id, Some(b"one"), None, CursorOp::Set)
        .unwrap();
    assert!(txn.cursor_is_initialized(id).unwrap());
    assert!(!txn.cursor_current_has_dups(id).unwrap());
    txn.cursor_get(id, None, None, CursorOp::Next).unwrap();
    assert!(txn.cursor_current_has_dups(id).unwrap());
    txn.close_cursor(id);
    txn.commit().unwrap();

    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    assert!(!c.is_initialized());
    assert!(!c.current_has_dups());
    c.set_key(b"two").unwrap().unwrap();
    assert!(c.is_initialized() && c.current_has_dups());
    c.set_key(b"one").unwrap().unwrap();
    assert!(c.is_initialized() && !c.current_has_dups());
    // A failed search leaves C's cursor initialised, off any item.
    assert!(c.set_key(b"zzz").unwrap().is_none());
    assert!(!c.current_has_dups());
}

#[test]
fn info_and_lock_files() {
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open(dir.path(), EnvironmentFlags::NO_SYNC);
        assert!(env.has_lock_file());
        assert_eq!(env.info().unwrap().map_address, 0);
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"a", b"b", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
    }
    // A write transaction in a read-only environment: EACCES.
    {
        let env = open(dir.path(), EnvironmentFlags::READ_ONLY);
        assert_eq!(env.begin_rw_txn().unwrap_err().to_err_code(), libc::EACCES);
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(Dbi::MAIN, b"a").unwrap(), Some(&b"b"[..]));
    }
    // NO_LOCK: no lock file is created or used; reads and writes work.
    let dir2 = tempfile::tempdir().unwrap();
    {
        let env = open(
            dir2.path(),
            EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC,
        );
        assert!(!env.has_lock_file());
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"k", b"v", WriteFlags::empty()).unwrap();
        let mut child = txn.begin_nested_txn().unwrap();
        child
            .put(Dbi::MAIN, b"k2", b"v2", WriteFlags::empty())
            .unwrap();
        child.commit().unwrap();
        txn.commit().unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(Dbi::MAIN, b"k2").unwrap(), Some(&b"v2"[..]));
        assert!(env.reader_list().unwrap().is_empty());
    }
    assert!(!dir2.path().join("lock.mdb").exists());
    // PREV_SNAPSHOT is an LMDB 1.0 flag; 0.9 refuses it like C.
    let e = Environment::builder()
        .flags(EnvironmentFlags::PREV_SNAPSHOT)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(e.to_err_code(), libc::EINVAL);
}

#[test]
fn close_db_while_a_write_txn_changed_the_db() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), EnvironmentFlags::NO_SYNC);
    let db = env.create_db(Some("x"), DatabaseFlags::CREATE).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(db, b"k", b"v", WriteFlags::empty()).unwrap();
    // As in C: closing the handle of a database a live write transaction
    // modified makes its commit fail (MDB_BAD_DBI), and nothing changes.
    env.close_db(db);
    let e = txn.commit().unwrap_err();
    assert_eq!(e.to_err_code(), rlmdb::MdbError::BadDbi.to_err_code());
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("x")).unwrap();
    assert_eq!(txn.get(db, b"k").unwrap(), None);
}
