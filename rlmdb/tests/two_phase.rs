//! LMDB 1.0's two-phase commit through the Rust API, in both formats:
//! [`RwTxn::prepare`] and [`PreparedTxn`] (`mdb_txn_prepare`), and
//! [`Environment::rollback`] (`mdb_env_rollback`).
//!
//! The file between the phases is the file of a commit stopped before its
//! meta page write: checked byte for byte here (data pages as after the
//! commit, meta pages as before it), and after a process that dies prepared
//! with C 0.9's page audit and writer. A prepared commit leaves the bytes a
//! plain commit leaves (`write_differential.rs` against C 0.9,
//! `interop/tests/write_differential10.rs` against C 1.0, which also
//! compares the rollback's meta page with C 1.0's).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use rlmdb::format::Meta;
use rlmdb::{
    DatabaseFlags, Dbi, Environment, EnvironmentBuilder, EnvironmentFlags, Error, Format, MdbError,
    Transaction, WriteFlags,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-2pc-")
        .tempdir_in(common::scratch_root())
        .unwrap()
}

/// The formats (and 1.0 page sizes) the tests run in.
fn formats() -> Vec<(Format, Option<usize>)> {
    vec![
        (Format::V09, None),
        (Format::V10, Some(1024)),
        (Format::V10, Some(4096)),
    ]
}

fn builder(
    format: Format,
    page_size: Option<usize>,
    flags: EnvironmentFlags,
) -> EnvironmentBuilder {
    let mut b = Environment::builder();
    b.format(format).max_dbs(8).map_size(64 << 20).flags(flags);
    if let Some(ps) = page_size {
        b.page_size(ps);
    }
    b
}

fn open(dir: &Path, format: Format, page_size: Option<usize>) -> Environment {
    builder(format, page_size, EnvironmentFlags::empty())
        .open(dir)
        .unwrap()
}

fn put(env: &Environment, key: &[u8], value: &[u8]) -> u64 {
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, key, value, WriteFlags::empty()).unwrap();
    let id = txn.id();
    txn.commit().unwrap();
    id
}

fn get(env: &Environment, key: &[u8]) -> Option<Vec<u8>> {
    let txn = env.begin_ro_txn().unwrap();
    txn.get(Dbi::MAIN, key).unwrap().map(<[u8]>::to_vec)
}

fn last_txnid(env: &Environment) -> u64 {
    env.info().unwrap().last_txnid as u64
}

fn cant_rollback(r: rlmdb::Result<()>) -> bool {
    matches!(r, Err(Error::Mdb(MdbError::CantRollback)))
}

fn data_file(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("data.mdb")).unwrap()
}

/// The meta page `i` of a data file.
fn meta(file: &[u8], format: Format, ps: usize, i: usize) -> Meta {
    let off = i * ps + if format == Format::V10 { 24 } else { 16 };
    Meta::read(&file[off..]).unwrap()
}

#[test]
fn prepare_then_commit() {
    for (format, ps) in formats() {
        for flags in [
            EnvironmentFlags::empty(),
            EnvironmentFlags::NO_SYNC,
            EnvironmentFlags::NO_META_SYNC,
        ] {
            let dir = tmp();
            let env = builder(format, ps, flags).open(dir.path()).unwrap();
            put(&env, b"a", b"1");
            let mut txn = env.begin_rw_txn().unwrap();
            txn.put(Dbi::MAIN, b"b", b"2", WriteFlags::empty()).unwrap();
            let db = txn.create_db(Some("named"), DatabaseFlags::CREATE).unwrap();
            txn.put(db, b"x", b"y", WriteFlags::empty()).unwrap();
            let prepared = txn.prepare().unwrap();
            assert_eq!(prepared.id(), 2);
            // Not visible before the commit.
            assert_eq!(get(&env, b"b"), None);
            assert_eq!(last_txnid(&env), 1);
            prepared.commit().unwrap();
            assert_eq!(last_txnid(&env), 2);
            assert_eq!(get(&env, b"b").as_deref(), Some(&b"2"[..]));
            // The handle of the database created in it stays valid.
            let txn = env.begin_ro_txn().unwrap();
            assert_eq!(txn.get(db, b"x").unwrap(), Some(&b"y"[..]));
            drop(txn);
            // And the next writer builds on it.
            assert_eq!(put(&env, b"c", b"3"), 3);
            drop(env);
            let env = builder(format, ps, flags).open(dir.path()).unwrap();
            assert_eq!(get(&env, b"b").as_deref(), Some(&b"2"[..]));
            assert_eq!(env.open_db(Some("named")).unwrap(), db);
        }
    }
}

#[test]
fn prepare_then_abort_or_drop() {
    for (format, ps) in formats() {
        for drop_it in [false, true] {
            let dir = tmp();
            let env = open(dir.path(), format, ps);
            put(&env, b"a", b"1");
            let before = data_file(dir.path());
            let mut txn = env.begin_rw_txn().unwrap();
            txn.put(Dbi::MAIN, b"b", b"2", WriteFlags::empty()).unwrap();
            let db = txn.create_db(Some("named"), DatabaseFlags::CREATE).unwrap();
            let prepared = txn.prepare().unwrap();
            if drop_it {
                drop(prepared);
            } else {
                prepared.abort();
            }
            assert_eq!(get(&env, b"b"), None);
            assert_eq!(last_txnid(&env), 1);
            // The created database's handle was closed, as by an abort.
            let txn = env.begin_ro_txn().unwrap();
            assert!(txn.get(db, b"x").is_err());
            drop(txn);
            // The meta pages are as before.
            let ps = env.page_size();
            assert_eq!(data_file(dir.path())[..2 * ps], before[..2 * ps]);
            // The next writer gets the same txnid and the file stays sound.
            assert_eq!(put(&env, b"c", b"3"), 2);
            assert_eq!(get(&env, b"c").as_deref(), Some(&b"3"[..]));
            assert_eq!(get(&env, b"b"), None);
        }
    }
}

/// A read transaction begun before the prepare keeps its snapshot through
/// both phases; new readers see the old snapshot until the commit.
#[test]
fn readers_across_the_phases() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"k", b"old");
        let early = env.begin_ro_txn().unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"k", b"new", WriteFlags::empty())
            .unwrap();
        let prepared = txn.prepare().unwrap();
        let between = env.begin_ro_txn().unwrap();
        assert_eq!(between.id(), 1);
        assert_eq!(between.get(Dbi::MAIN, b"k").unwrap(), Some(&b"old"[..]));
        prepared.commit().unwrap();
        assert_eq!(early.get(Dbi::MAIN, b"k").unwrap(), Some(&b"old"[..]));
        assert_eq!(between.get(Dbi::MAIN, b"k").unwrap(), Some(&b"old"[..]));
        let after = env.begin_ro_txn().unwrap();
        assert_eq!(after.id(), 2);
        assert_eq!(after.get(Dbi::MAIN, b"k").unwrap(), Some(&b"new"[..]));
    }
}

/// A prepared transaction keeps the writer lock: another thread's write
/// transaction waits for its commit; `set_map_size` fails as with an open
/// write transaction.
#[test]
fn prepared_holds_the_writer_lock() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"a", b"1");
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"b", b"2", WriteFlags::empty()).unwrap();
        let prepared = txn.prepare().unwrap();
        assert_eq!(
            env.set_map_size(128 << 20).unwrap_err().to_err_code(),
            libc::EINVAL
        );
        std::thread::scope(|s| {
            let (tx, rx) = mpsc::channel();
            let env = &env;
            s.spawn(move || {
                let txn = env.begin_rw_txn().unwrap();
                tx.send((
                    txn.id(),
                    txn.get(Dbi::MAIN, b"b").unwrap().map(<[u8]>::to_vec),
                ))
                .unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            prepared.commit().unwrap();
            let (id, b) = rx.recv_timeout(Duration::from_secs(30)).unwrap();
            assert_eq!((id, b.as_deref()), (3, Some(&b"2"[..])));
        });
    }
}

/// A transaction without changes ends at the prepare, as an empty commit
/// does (C too): the writer lock is free, and the commit does nothing.
#[test]
fn empty_prepare() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"a", b"1");
        let before = data_file(dir.path());
        let txn = env.begin_rw_txn().unwrap();
        let prepared = txn.prepare().unwrap();
        assert_eq!(prepared.id(), 2);
        // Another writer can begin (and finish) meanwhile.
        std::thread::scope(|s| {
            s.spawn(|| assert_eq!(put(&env, b"b", b"2"), 2))
                .join()
                .unwrap();
        });
        prepared.commit().unwrap();
        assert_eq!(last_txnid(&env), 2);
        assert_ne!(data_file(dir.path()), before);
        // Abort and drop are no-ops too.
        env.begin_rw_txn().unwrap().prepare().unwrap().abort();
        drop(env.begin_rw_txn().unwrap().prepare().unwrap());
        assert_eq!(last_txnid(&env), 2);
    }
}

/// A nested transaction cannot be prepared: `EINVAL`, and it is aborted;
/// its parent goes on.
#[test]
fn nested_prepare_is_refused() {
    let dir = tmp();
    let env = open(dir.path(), Format::V09, None);
    let mut parent = env.begin_rw_txn().unwrap();
    parent
        .put(Dbi::MAIN, b"p", b"1", WriteFlags::empty())
        .unwrap();
    let mut child = parent.begin_nested_txn().unwrap();
    child
        .put(Dbi::MAIN, b"c", b"1", WriteFlags::empty())
        .unwrap();
    assert_eq!(child.prepare().unwrap_err().to_err_code(), libc::EINVAL);
    assert_eq!(parent.get(Dbi::MAIN, b"c").unwrap(), None);
    parent.prepare().unwrap().commit().unwrap();
    assert_eq!(get(&env, b"p").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&env, b"c"), None);
}

/// A failed transaction (`MDB_TXN_ERROR`) fails its prepare with
/// `MDB_BAD_TXN`, as its commit does.
#[test]
fn prepare_of_a_failed_transaction() {
    let dir = tmp();
    let env = Environment::builder()
        .map_size(1 << 20)
        .open(dir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let mut i = 0u32;
    let err = loop {
        if let Err(e) = txn.put(
            Dbi::MAIN,
            &i.to_be_bytes(),
            &[0u8; 400],
            WriteFlags::empty(),
        ) {
            break e;
        }
        i += 1;
    };
    assert!(matches!(err, Error::Mdb(MdbError::MapFull)), "{err:?}");
    assert!(matches!(
        txn.prepare().unwrap_err(),
        Error::Mdb(MdbError::BadTxn)
    ));
    assert_eq!(put(&env, b"a", b"1"), 1);
}

/// The file after the prepare holds the data and free-list pages the
/// commit leaves, and the meta pages from before the transaction.
#[test]
fn the_file_between_the_phases() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        let ps = env.page_size();
        for i in 0..200u32 {
            put(&env, &i.to_be_bytes(), &[i as u8; 100]);
        }
        let before = data_file(dir.path());
        let mut txn = env.begin_rw_txn().unwrap();
        for i in (0..200u32).step_by(3) {
            txn.del(Dbi::MAIN, &i.to_be_bytes(), None).unwrap();
        }
        for i in 1000..1100u32 {
            txn.put(Dbi::MAIN, &i.to_be_bytes(), &[7; 300], WriteFlags::empty())
                .unwrap();
        }
        let id = txn.id();
        let prepared = txn.prepare().unwrap();
        let between = data_file(dir.path());
        prepared.commit().unwrap();
        let after = data_file(dir.path());
        assert_eq!(
            between[..2 * ps],
            before[..2 * ps],
            "{format:?}: meta pages"
        );
        assert_eq!(between.len(), after.len(), "{format:?}: file length");
        assert_eq!(between[2 * ps..], after[2 * ps..], "{format:?}: data pages");
        let slot = (id & 1) as usize;
        assert_eq!(
            between[(1 - slot) * ps..(2 - slot) * ps],
            after[(1 - slot) * ps..(2 - slot) * ps],
            "{format:?}: the other meta page"
        );
        assert_eq!(meta(&after, format, ps, slot).txnid, id);
    }
}

#[test]
fn rollback_of_the_last_commit() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"a", b"1");
        put(&env, b"b", b"2");
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"c", b"3", WriteFlags::empty()).unwrap();
        txn.put(Dbi::MAIN, b"a", b"changed", WriteFlags::empty())
            .unwrap();
        txn.del(Dbi::MAIN, b"b", None).unwrap();
        let db = txn.create_db(Some("named"), DatabaseFlags::CREATE).unwrap();
        txn.put(db, b"x", b"y", WriteFlags::empty()).unwrap();
        let prepared = txn.prepare().unwrap();
        let id = prepared.id();
        assert_eq!(id, 3);
        prepared.commit().unwrap();
        let committed = data_file(dir.path());
        env.rollback(id).unwrap();
        assert_eq!(last_txnid(&env), 2);
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
        assert_eq!(get(&env, b"b").as_deref(), Some(&b"2"[..]));
        assert_eq!(get(&env, b"c"), None);
        // The meta page written: C's (the module documentation of rollback).
        let page = env.page_size();
        let file = data_file(dir.path());
        let m = meta(&file, format, page, 1);
        let old = meta(&committed, format, page, 1);
        assert_eq!(m.txnid, 1, "txnid & 1");
        assert_eq!(m.last_pgno, u64::MAX);
        assert_eq!(m.map_size, 64 << 20);
        assert_eq!(
            (m.dbs[0].pad, m.dbs[0].flags),
            (old.dbs[0].pad, old.dbs[0].flags)
        );
        let mut zero = m.dbs[0];
        (zero.pad, zero.flags) = (0, 0);
        assert_eq!(zero, Default::default());
        assert_eq!(m.dbs[1], Default::default());
        assert_eq!((m.magic, m.version), (old.magic, old.version));
        assert_eq!(file[..page], committed[..page], "the other meta page");
        assert_eq!(file[2 * page..], committed[2 * page..], "the data pages");
        // The handle of the rolled-back database names nothing now.
        let txn = env.begin_ro_txn().unwrap();
        assert!(txn.get(db, b"x").is_err());
        drop(txn);
        // The next writer builds on snapshot 2 and gets txnid 3 again.
        let mut txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), 3);
        txn.put(Dbi::MAIN, b"d", b"4", WriteFlags::empty()).unwrap();
        // As in C, opening the name returns the open handle, whose
        // database is gone: MDB_BAD_DBI (mdb_page_search's stale record).
        assert_eq!(
            txn.create_db(Some("named"), DatabaseFlags::CREATE).unwrap(),
            db
        );
        assert!(matches!(
            txn.put(db, b"x", b"z", WriteFlags::empty()),
            Err(Error::Mdb(MdbError::BadDbi))
        ));
        txn.commit().unwrap();
        // Closed, the name can be created again.
        env.close_db(db);
        let mut txn = env.begin_rw_txn().unwrap();
        let db2 = txn.create_db(Some("named"), DatabaseFlags::CREATE).unwrap();
        txn.put(db2, b"x", b"z", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        assert_eq!(get(&env, b"d").as_deref(), Some(&b"4"[..]));
        assert_eq!(get(&env, b"c"), None);
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(db2, b"x").unwrap(), Some(&b"z"[..]));
        drop(txn);
        drop(env);
        let env = open(dir.path(), format, ps);
        assert_eq!(last_txnid(&env), 4);
        assert_eq!(get(&env, b"d").as_deref(), Some(&b"4"[..]));
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
    }
}

/// `MDB_CANT_ROLLBACK`: a wrong txnid, no previous snapshot (a new
/// environment, its first commit), a second rollback in a row (for an
/// even and for an odd txnid: C 1.0 lets the latter through, row 62).
#[test]
fn what_cannot_be_rolled_back() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        assert!(cant_rollback(env.rollback(0)));
        assert!(cant_rollback(env.rollback(1)));
        put(&env, b"a", b"1");
        assert!(cant_rollback(env.rollback(1)), "no snapshot before 1");
        put(&env, b"b", b"2");
        assert!(cant_rollback(env.rollback(1)));
        assert!(cant_rollback(env.rollback(3)));
        // Even: the meta page of 2 gets txnid 0.
        env.rollback(2).unwrap();
        assert!(cant_rollback(env.rollback(1)), "twice in a row");
        assert!(cant_rollback(env.rollback(2)));
        put(&env, b"b", b"2");
        put(&env, b"c", b"3");
        // Odd: the meta page of 3 gets txnid 1, which C takes for a
        // snapshot.
        env.rollback(3).unwrap();
        assert!(cant_rollback(env.rollback(2)), "twice in a row");
        assert_eq!(last_txnid(&env), 2);
        assert_eq!(get(&env, b"b").as_deref(), Some(&b"2"[..]));
        assert_eq!(put(&env, b"c", b"3"), 3);
        env.rollback(3).unwrap();
        assert_eq!(put(&env, b"c", b"3"), 3);
    }
}

/// A reader of the snapshot being rolled back stops the rollback (not in
/// C, row 63); readers of older snapshots do not.
#[test]
fn rollback_with_readers() {
    for (format, ps) in formats() {
        for no_lock in [false, true] {
            let dir = tmp();
            let flags = if no_lock {
                EnvironmentFlags::NO_LOCK
            } else {
                EnvironmentFlags::empty()
            };
            let env = builder(format, ps, flags).open(dir.path()).unwrap();
            put(&env, b"a", b"1");
            let old = env.begin_ro_txn().unwrap();
            put(&env, b"a", b"2");
            let pinning = env.begin_ro_txn().unwrap();
            assert_eq!(pinning.id(), 2);
            assert!(cant_rollback(env.rollback(2)));
            // Nothing changed: new readers still see 2.
            assert_eq!(get(&env, b"a").as_deref(), Some(&b"2"[..]));
            assert_eq!(last_txnid(&env), 2);
            // A reset transaction pins nothing.
            let inactive = pinning.reset();
            env.rollback(2).unwrap();
            assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
            assert_eq!(old.get(Dbi::MAIN, b"a").unwrap(), Some(&b"1"[..]));
            let renewed = inactive.renew().unwrap();
            assert_eq!(renewed.id(), 1);
        }
    }
}

/// A C 0.9 process reading the snapshot stops the rollback too. (The map
/// is the holder's 10 MiB: on Windows C 0.9 sets the data file to its map
/// size at the open, which fails while this process maps more,
/// `KNOWN_C_BUGS.md` row 57.)
#[test]
fn rollback_with_a_c_reader() {
    let c = require_c!();
    let dir = tmp();
    let env = builder(Format::V09, None, EnvironmentFlags::empty())
        .map_size(10 << 20)
        .open(dir.path())
        .unwrap();
    put(&env, b"a", b"1");
    put(&env, b"a", b"2");
    let holder = c.holder(dir.path(), &["-r"]);
    assert_eq!(holder.txnid, Some(2));
    assert!(cant_rollback(env.rollback(2)));
    holder.quit();
    env.rollback(2).unwrap();
}

/// After a rollback C 0.9 reads the previous snapshot, finds every page
/// in place, and writes on it.
#[test]
fn c_09_after_a_rollback() {
    let c = require_c!();
    for odd in [false, true] {
        let dir = tmp();
        let p = dir.path().to_str().unwrap();
        let env = open(dir.path(), Format::V09, None);
        for i in 0..300u32 {
            let mut txn = env.begin_rw_txn().unwrap();
            txn.put(Dbi::MAIN, &i.to_be_bytes(), &[1; 200], WriteFlags::empty())
                .unwrap();
            txn.commit().unwrap();
        }
        if !odd {
            put(&env, b"one more", b"");
        }
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..300u32 {
            txn.put(Dbi::MAIN, &i.to_be_bytes(), &[2; 300], WriteFlags::empty())
                .unwrap();
        }
        let id = txn.id();
        assert_eq!(id % 2 == 1, odd);
        txn.prepare().unwrap().commit().unwrap();
        env.rollback(id).unwrap();
        let dump = c.dump(dir.path(), None);
        assert!(dump
            .iter()
            .all(|(k, v)| k.len() != 4 || v == &vec![1u8; 200]));
        let info = c.stat_text(dir.path(), &["-e"]);
        assert!(
            info.contains(&format!("Last transaction ID: {}", id - 1)),
            "{info}"
        );
        c.peer_ok(&["check-freelist", p]);
        // C writes the next transaction on the previous snapshot.
        let entries = env.stat().unwrap().entries;
        drop(env);
        c.peer_ok(&["fill", p, "-", "50", "7"]);
        c.peer_ok(&["check-freelist", p]);
        let env = open(dir.path(), Format::V09, None);
        assert_eq!(last_txnid(&env), id);
        assert_eq!(env.stat().unwrap().entries, entries + 50);
        assert_eq!(get(&env, &0u32.to_be_bytes()), Some(vec![1; 200]));
    }
}

/// `EACCES` in a read-only environment.
#[test]
fn rollback_read_only() {
    let dir = tmp();
    let env = open(dir.path(), Format::V09, None);
    put(&env, b"a", b"1");
    put(&env, b"a", b"2");
    drop(env);
    let env = Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.rollback(2).unwrap_err().to_err_code(), libc::EACCES);
    assert_eq!(last_txnid(&env), 2);
}

/// A failed meta page write fails the prepared commit (aborted, the
/// environment unusable here) and the rollback (the meta page put back
/// whole: a reopen finds the commit still there).
#[test]
fn failed_meta_writes() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"a", b"1");
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"a", b"2", WriteFlags::empty()).unwrap();
        let prepared = txn.prepare().unwrap();
        env.fail_next_meta_sync(libc::EIO);
        assert_eq!(prepared.commit().unwrap_err().to_err_code(), libc::EIO);
        assert!(matches!(
            env.begin_rw_txn().unwrap_err(),
            Error::Mdb(MdbError::Panic)
        ));
        drop(env);
        let env = open(dir.path(), format, ps);
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
        let id = put(&env, b"a", b"3");
        env.fail_next_meta_sync(libc::EIO);
        assert_eq!(env.rollback(id).unwrap_err().to_err_code(), libc::EIO);
        assert!(matches!(
            env.begin_rw_txn().unwrap_err(),
            Error::Mdb(MdbError::Panic)
        ));
        drop(env);
        let env = open(dir.path(), format, ps);
        assert_eq!(last_txnid(&env), id);
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"3"[..]));
        env.rollback(id).unwrap();
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
    }
}

/// Two handles without a lock file in two processes: after the other
/// process rolled back, this process's readers take the previous snapshot
/// (they used to keep the txnid they knew, whose meta page then holds no
/// snapshot).
#[test]
fn no_lock_rollback_by_another_process() {
    for (format, ps) in formats() {
        let dir = tmp();
        let env = builder(format, ps, EnvironmentFlags::NO_LOCK)
            .open(dir.path())
            .unwrap();
        put(&env, b"a", b"1");
        let id = put(&env, b"a", b"2");
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"2"[..]));
        helper(Helper::RollbackNoLock, dir.path(), format, ps, id);
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"1"[..]));
        assert_eq!(put(&env, b"a", b"3"), id);
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"3"[..]));
    }
}

/// A process commits `t`; another rolls it back and commits another `t`;
/// the first process's next write transaction builds on the second `t`. (A
/// writer used to begin from the meta page its process wrote last while
/// the lock file named that txnid, which a rollback makes ambiguous.)
#[test]
fn txnid_reused_by_another_process() {
    let c = common::c_tools();
    for (format, ps) in formats() {
        let dir = tmp();
        let env = open(dir.path(), format, ps);
        put(&env, b"a", b"1");
        let id = put(&env, b"a", b"mine");
        helper(Helper::RollbackAndCommit, dir.path(), format, ps, id);
        assert_eq!(last_txnid(&env), id);
        let mut txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), id + 1);
        assert_eq!(txn.get(Dbi::MAIN, b"a").unwrap(), Some(&b"theirs"[..]));
        assert_eq!(txn.get(Dbi::MAIN, b"b").unwrap(), Some(&[9u8; 2000][..]));
        txn.put(Dbi::MAIN, b"c", b"3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        if let (Some(c), Format::V09) = (c, format) {
            c.peer_ok(&["check-freelist", dir.path().to_str().unwrap()]);
        }
        assert_eq!(get(&env, b"a").as_deref(), Some(&b"theirs"[..]));
    }
}

/// A process that dies between the phases (holding the writer lock) leaves
/// the environment as a crash in a commit before its meta page write: the
/// old snapshot, every page of it in place (C 0.9's page audit finds
/// nothing lost), the new pages unreferenced; C and rlmdb write on it.
#[test]
fn a_process_dies_prepared() {
    let c = common::c_tools();
    for (format, ps) in formats() {
        let dir = tmp();
        let p = dir.path().to_str().unwrap().to_owned();
        let env = open(dir.path(), format, ps);
        for i in 0..100u32 {
            put(&env, &i.to_be_bytes(), &[1; 100]);
        }
        let before = data_file(dir.path());
        // Nobody else has the environment open while the helper dies:
        // the next opener resets the lock file (the writer lock of a dead
        // process stays locked otherwise, except with robust mutexes).
        drop(env);
        helper(Helper::DiePrepared, dir.path(), format, ps, 0);
        let between = data_file(dir.path());
        let page = env_page_size(&before, format);
        assert_eq!(between[..2 * page], before[..2 * page], "meta pages");
        assert!(between.len() >= before.len());
        assert_ne!(
            between[2 * page..before.len()],
            before[2 * page..],
            "data pages"
        );
        let env = open(dir.path(), format, ps);
        assert_eq!(last_txnid(&env), 100);
        assert_eq!(get(&env, &1000u32.to_be_bytes()), None);
        assert_eq!(get(&env, &0u32.to_be_bytes()), Some(vec![1; 100]));
        if let (Some(c), Format::V09) = (c, format) {
            c.peer_ok(&["check-freelist", &p]);
            drop(env);
            c.peer_ok(&["fill", &p, "-", "50", "7"]);
            c.peer_ok(&["check-freelist", &p]);
        } else {
            drop(env);
        }
        let env = open(dir.path(), format, ps);
        let id = put(&env, &1000u32.to_be_bytes(), b"after");
        assert_eq!(
            get(&env, &1000u32.to_be_bytes()).as_deref(),
            Some(&b"after"[..])
        );
        assert_eq!(get(&env, &0u32.to_be_bytes()), Some(vec![1; 100]));
        assert!(id > 100);
    }
}

/// The page size of a data file, from its first meta page.
fn env_page_size(file: &[u8], format: Format) -> usize {
    meta(file, format, 0, 0).page_size() as usize
}

// --- helper processes ------------------------------------------------------

const HELPER: &str = "RLMDB_TWO_PHASE_HELPER";
const HELPER_ENV: &str = "RLMDB_TWO_PHASE_ENV";
const HELPER_FORMAT: &str = "RLMDB_TWO_PHASE_FORMAT";
const HELPER_TXNID: &str = "RLMDB_TWO_PHASE_TXNID";

#[derive(Clone, Copy, Debug)]
enum Helper {
    /// Prepare a transaction, then abort the process.
    DiePrepared,
    /// Open without a lock file and roll back the txnid given.
    RollbackNoLock,
    /// Roll back the txnid given, then commit other data.
    RollbackAndCommit,
}

fn helper(what: Helper, dir: &Path, format: Format, ps: Option<usize>, txnid: u64) {
    let format = match (format, ps) {
        (Format::V09, _) => "0.9".to_owned(),
        (_, ps) => format!("1.0:{}", ps.unwrap_or(0)),
    };
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "helper_process",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(HELPER, format!("{what:?}"))
        .env(HELPER_ENV, dir)
        .env(HELPER_FORMAT, format)
        .env(HELPER_TXNID, txnid.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    match what {
        Helper::DiePrepared => {
            assert!(!out.status.success(), "{stdout}\n{stderr}");
            assert!(stdout.contains("helper: prepared"), "{stdout}\n{stderr}");
        }
        Helper::RollbackNoLock | Helper::RollbackAndCommit => {
            assert!(out.status.success(), "{stdout}\n{stderr}");
            assert!(stdout.contains("helper: rolled back"), "{stdout}\n{stderr}");
        }
    }
}

#[test]
#[ignore = "helper process of the other tests in this file"]
fn helper_process() {
    let Ok(what) = std::env::var(HELPER) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(HELPER_ENV).unwrap());
    let format = std::env::var(HELPER_FORMAT).unwrap();
    let (format, ps) = match format.split_once(':') {
        Some((_, ps)) => (Format::V10, Some(ps.parse().unwrap()).filter(|&p| p != 0)),
        None => (Format::V09, None),
    };
    let txnid: u64 = std::env::var(HELPER_TXNID).unwrap().parse().unwrap();
    match what.as_str() {
        "DiePrepared" => {
            let env = open(&dir, format, ps);
            let mut txn = env.begin_rw_txn().unwrap();
            for i in 0..100u32 {
                txn.put(Dbi::MAIN, &i.to_be_bytes(), &[2; 150], WriteFlags::empty())
                    .unwrap();
            }
            for i in 1000..1200u32 {
                txn.put(Dbi::MAIN, &i.to_be_bytes(), &[3; 150], WriteFlags::empty())
                    .unwrap();
            }
            let prepared = txn.prepare().unwrap();
            println!("helper: prepared {}", prepared.id());
            use std::io::Write;
            std::io::stdout().flush().unwrap();
            std::process::abort();
        }
        "RollbackNoLock" => {
            let env = builder(format, ps, EnvironmentFlags::NO_LOCK)
                .open(&dir)
                .unwrap();
            env.rollback(txnid).unwrap();
            println!("helper: rolled back {txnid}");
        }
        "RollbackAndCommit" => {
            let env = open(&dir, format, ps);
            env.rollback(txnid).unwrap();
            let mut txn = env.begin_rw_txn().unwrap();
            assert_eq!(txn.id(), txnid);
            txn.put(Dbi::MAIN, b"a", b"theirs", WriteFlags::empty())
                .unwrap();
            txn.put(Dbi::MAIN, b"b", &[9; 2000], WriteFlags::empty())
                .unwrap();
            txn.commit().unwrap();
            println!("helper: rolled back {txnid} and committed it again");
        }
        other => panic!("unknown helper {other}"),
    }
}
