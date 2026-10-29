//! Environment behaviour that needs no C peer: creating a new environment,
//! reading an empty one, handle validation, transaction lifecycle, reader
//! table bookkeeping and the process-wide "already open" registry.

mod common;

use lmdb::consts::MDB_MAXKEYSIZE;
use lmdb::{CursorOp, Dbi, Environment, EnvironmentFlags, Error, MdbError, Transaction};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn new_environment_files_and_meta() {
    let dir = tmp();
    let env = Environment::builder().open(dir.path()).unwrap();
    let psize = env.page_size();
    eprintln!("new environment page size: {psize}");
    assert_eq!(psize, common::os_page_size().min(32768));
    // Two meta pages, nothing else.
    let data_len = std::fs::metadata(dir.path().join("data.mdb"))
        .unwrap()
        .len();
    assert_eq!(data_len, 2 * psize as u64);
    assert!(dir.path().join("lock.mdb").exists());

    let info = env.info().unwrap();
    assert_eq!(info.map_size, 1 << 20, "default map size");
    assert_eq!(info.last_pgno, 1);
    assert_eq!(info.last_txnid, 0);
    assert_eq!(info.max_readers, 126);
    let stat = env.stat().unwrap();
    assert_eq!(stat.entries, 0);
    assert_eq!(stat.depth, 0);
    assert_eq!(stat.page_size as usize, psize);
    assert_eq!(env.max_key_size(), MDB_MAXKEYSIZE);
    assert_eq!(env.freelist().unwrap(), 0);

    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), 0);
    assert_eq!(txn.get(Dbi::MAIN, b"x").unwrap(), None);
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    assert_eq!(c.first().unwrap(), None);
    assert_eq!(c.last().unwrap(), None);
    assert_eq!(c.next().unwrap(), None);
    assert_eq!(c.prev().unwrap(), None);
    assert_eq!(c.set_range(b"a").unwrap(), None);
    // Empty key: BAD_VALSIZE, as in C.
    assert!(matches!(
        txn.get(Dbi::MAIN, b""),
        Err(Error::Mdb(MdbError::BadValSize))
    ));
    // No named DBs in an empty main DB.
    assert!(matches!(
        txn.open_db(Some("nope")),
        Err(Error::Mdb(MdbError::DbsFull))
    ));
}

#[test]
fn reopen_with_configured_map_size() {
    let dir = tmp();
    drop(Environment::builder().open(dir.path()).unwrap());
    let env = Environment::builder()
        .map_size(10 << 20)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.info().unwrap().map_size, 10 << 20);
    drop(env);
    // Not configured: the size recorded in the file (still the default,
    // nothing was committed with the larger size).
    let env = Environment::builder().open(dir.path()).unwrap();
    assert_eq!(env.info().unwrap().map_size, 1 << 20);
}

#[test]
fn no_sub_dir_paths() {
    let dir = tmp();
    let path = dir.path().join("db");
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_SUB_DIR)
        .open(&path)
        .unwrap();
    assert!(path.is_file());
    assert!(dir.path().join("db-lock").is_file());
    // NOSUBDIR is a persistent env flag in the free DB record of a new file.
    assert!(env.flags().contains(EnvironmentFlags::NO_SUB_DIR));
}

#[test]
fn already_opened_and_reopen_after_drop() {
    let dir = tmp();
    let env = Environment::builder().open(dir.path()).unwrap();
    assert!(matches!(
        Environment::builder().open(dir.path()),
        Err(Error::EnvAlreadyOpened)
    ));
    // Same environment through a different spelling of the path.
    let alt = dir.path().join(".");
    assert!(matches!(
        Environment::builder().open(&alt),
        Err(Error::EnvAlreadyOpened)
    ));
    drop(env);
    let env = Environment::builder().open(dir.path()).unwrap();
    drop(env);
}

#[test]
fn read_only_open_requires_existing_files() {
    let dir = tmp();
    let err = Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::ENOENT);
    drop(Environment::builder().open(dir.path()).unwrap());
    let env = Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir.path())
        .unwrap();
    assert!(matches!(env.sync(true), Err(e) if e.to_err_code() == libc::EACCES));
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), 0);
}

#[test]
fn invalid_flags_and_options() {
    let dir = tmp();
    let err = Environment::builder()
        .flags(EnvironmentFlags::PREV_SNAPSHOT)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    let err = Environment::builder()
        .max_readers(0)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    let env = Environment::builder().open(dir.path()).unwrap();
    assert!(env.set_flags(EnvironmentFlags::READ_ONLY, true).is_err());
    env.set_flags(EnvironmentFlags::NO_SYNC, true).unwrap();
    assert!(env.flags().contains(EnvironmentFlags::NO_SYNC));
    env.set_flags(EnvironmentFlags::NO_SYNC, false).unwrap();
    assert!(!env.flags().contains(EnvironmentFlags::NO_SYNC));
    env.sync(true).unwrap();
}

#[test]
fn not_an_lmdb_file() {
    let dir = tmp();
    std::fs::write(dir.path().join("data.mdb"), vec![0x5a; 8192]).unwrap();
    let err = Environment::builder().open(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Mdb(MdbError::Invalid)), "{err:?}");
}

#[test]
fn handle_validation() {
    let dir = tmp();
    let env = Environment::builder().open(dir.path()).unwrap();
    let txn = env.begin_ro_txn().unwrap();
    // Unknown handle: EINVAL as TXN_DBI_EXIST fails.
    let err = txn.get(Dbi::from_raw(7), b"k").unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    // FREE_DBI: no get, no flags (not DB_USRVALID), but cursor and stat work.
    assert_eq!(
        txn.get(Dbi::FREE, b"k").unwrap_err().to_err_code(),
        libc::EINVAL
    );
    assert!(txn.db_flags(Dbi::FREE).is_err());
    assert!(txn.cursor(Dbi::FREE).is_ok());
    assert_eq!(txn.stat(Dbi::FREE).unwrap().entries, 0);
    assert_eq!(txn.open_db(None).unwrap(), Dbi::MAIN);
    assert!(txn.db_flags(Dbi::MAIN).unwrap().is_empty());
}

#[test]
fn cursor_op_preconditions() {
    let dir = tmp();
    let env = Environment::builder().open(dir.path()).unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    // GET_CURRENT on an unpositioned cursor: EINVAL.
    assert_eq!(c.get_current().unwrap_err().to_err_code(), libc::EINVAL);
    // Dup operations on a non-DUPSORT DB: MDB_INCOMPATIBLE (or EINVAL first
    // for ops that need a position, as in C).
    assert!(matches!(
        c.get(Some(b"k"), Some(b"v"), CursorOp::GetBoth),
        Err(Error::Mdb(MdbError::Incompatible))
    ));
    assert!(matches!(c.count(), Err(Error::Mdb(MdbError::Incompatible))));
    assert!(matches!(
        c.next_multiple(),
        Err(Error::Mdb(MdbError::Incompatible))
    ));
    assert_eq!(c.first_dup().unwrap_err().to_err_code(), libc::EINVAL);
    // SET without a key: EINVAL.
    assert_eq!(
        c.get(None, None, CursorOp::Set).unwrap_err().to_err_code(),
        libc::EINVAL
    );
}

#[test]
fn reader_slots_lifecycle() {
    let dir = tmp();
    let env = Environment::builder()
        .max_readers(3)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.max_readers(), 3);
    let pid = std::process::id() as i32;
    assert!(env.reader_list().unwrap().is_empty());

    let t1 = env.begin_ro_txn().unwrap();
    let t2 = env.begin_ro_txn().unwrap();
    let t3 = env.begin_ro_txn().unwrap();
    let readers = env.reader_list().unwrap();
    assert_eq!(readers.len(), 3);
    assert!(readers.iter().all(|r| r.pid == pid && r.txnid == Some(0)));
    assert_eq!(env.info().unwrap().num_readers, 3);
    assert!(matches!(
        env.begin_ro_txn(),
        Err(Error::Mdb(MdbError::ReadersFull))
    ));

    // Reset keeps the slot (txnid shows as "-"), renew re-reads.
    let inactive = t2.reset();
    let readers = env.reader_list().unwrap();
    assert_eq!(readers.iter().filter(|r| r.txnid.is_none()).count(), 1);
    assert!(matches!(
        env.begin_ro_txn(),
        Err(Error::Mdb(MdbError::ReadersFull))
    ));
    let t2 = inactive.renew().unwrap();
    assert_eq!(t2.id(), 0);
    drop(t1);
    // A released slot is free for reuse.
    let t4 = env.begin_ro_txn().unwrap();
    assert_eq!(t4.reader_slot(), Some(0));
    drop((t2, t3, t4));
    assert!(env.reader_list().unwrap().is_empty());
    // numreaders is a high-water mark.
    assert_eq!(env.info().unwrap().num_readers, 3);
    assert_eq!(env.reader_check().unwrap(), 0);
    assert_eq!(
        lmdb::format_reader_list(&env.reader_list().unwrap()),
        "(no active readers)\n"
    );
}

#[test]
fn transactions_move_between_threads() {
    let dir = tmp();
    let env = Environment::builder().open(dir.path()).unwrap();
    std::thread::scope(|s| {
        let txn = env.begin_ro_txn().unwrap();
        s.spawn(move || {
            assert_eq!(txn.get(Dbi::MAIN, b"k").unwrap(), None);
            drop(txn);
        })
        .join()
        .unwrap();
        // Several threads reading at once.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    for _ in 0..50 {
                        let t = env.begin_ro_txn().unwrap();
                        assert_eq!(t.id(), 0);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    });
    assert!(env.reader_list().unwrap().is_empty());
}

#[test]
fn no_lock_environment() {
    let dir = tmp();
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_LOCK)
        .open(dir.path())
        .unwrap();
    assert!(!dir.path().join("lock.mdb").exists());
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.reader_slot(), None);
    assert!(env.reader_list().unwrap().is_empty());
    assert_eq!(env.info().unwrap().num_readers, 0);
}

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn auto_traits() {
    assert_send::<Environment>();
    assert_sync::<Environment>();
    assert_send::<lmdb::RoTxn<'static>>();
    assert_send::<lmdb::InactiveTxn<'static>>();
}
