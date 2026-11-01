//! Environment behaviour that needs no C peer: creating a new environment,
//! reading an empty one, handle validation, transaction lifecycle, reader
//! table bookkeeping and the process-wide "already open" registry.

mod common;

use rlmdb::consts::MDB_MAXKEYSIZE;
use rlmdb::{
    CursorOp, Dbi, Environment, EnvironmentFlags, Error, Format, MdbError, Transaction, WriteFlags,
};

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
    // Two meta pages, nothing else (on Windows a 0.9 file is as long as its
    // 1 MiB map, as C 0.9 makes it there).
    let data_len = std::fs::metadata(dir.path().join("data.mdb"))
        .unwrap()
        .len();
    let want = if cfg!(windows) {
        1 << 20
    } else {
        2 * psize as u64
    };
    assert_eq!(data_len, want);
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

/// Without a lock file there are no record locks to lose: any number of
/// `NO_LOCK` handles may open one environment (they share the in-process
/// writer lock and reader table). A handle with a lock file and a `NO_LOCK`
/// handle never share one, in either order.
#[test]
fn no_lock_handles_may_share_an_environment() {
    let dir = tmp();
    let open = |flags| Environment::builder().flags(flags).open(dir.path());
    let no_lock = EnvironmentFlags::NO_LOCK;
    let a = open(no_lock).unwrap();
    let b = open(no_lock).unwrap();
    let c = open(no_lock | EnvironmentFlags::READ_ONLY).unwrap();
    assert!(matches!(
        open(EnvironmentFlags::empty()),
        Err(Error::EnvAlreadyOpened)
    ));
    assert!(!dir.path().join("lock.mdb").exists());
    let txn = c.begin_ro_txn().unwrap();
    assert_eq!(a.in_memory_readers(), Some(1));
    drop(txn);
    drop((a, b, c));
    let env = open(EnvironmentFlags::empty()).unwrap();
    for flags in [no_lock, no_lock | EnvironmentFlags::READ_ONLY] {
        assert!(matches!(open(flags), Err(Error::EnvAlreadyOpened)));
    }
    drop(env);
}

/// Set (to an environment directory) in the environment of a
/// [`reader_list_peer`] process.
const PEER_VAR: &str = "RLMDB_RUST_ENV_READER_PEER";

/// Not a test: the body of the peer process that
/// [`second_open_under_another_name_leaves_the_first_alone`] starts (this
/// binary run again with [`PEER_VAR`] set). It opens the environment
/// read-only and prints its reader table as `PEER <pid>:<txnid> ...`.
#[test]
#[ignore = "helper process of second_open_under_another_name_leaves_the_first_alone"]
fn reader_list_peer() {
    let Ok(dir) = std::env::var(PEER_VAR) else {
        return;
    };
    let env = Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir)
        .unwrap();
    let mut line = String::from("PEER");
    for r in env.reader_list().unwrap() {
        line.push_str(&format!(" {}:{:?}", r.pid, r.txnid));
    }
    println!("{line}");
}

/// The reader table of the environment `dir` as another process sees it.
fn peer_reader_list(dir: &std::path::Path) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "reader_list_peer",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PEER_VAR, dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "peer failed: {stdout}");
    // On the line of libtest's "test reader_list_peer ... ".
    let at = stdout
        .find("PEER")
        .unwrap_or_else(|| panic!("peer: {stdout}"));
    stdout[at..].lines().next().unwrap().to_owned()
}

/// A process has an environment open with a live read transaction; its
/// files are reachable under other names (hard links here; a bind mount
/// looks the same). Opening those fails with `EBUSY` before the lock file
/// is set up again (the exclusive lock would be granted within the process
/// and the reader table re-initialised) and without closing a descriptor of
/// the lock file (which would drop the process's record locks on it). The
/// first handle keeps its reader slot, its record locks (another process
/// attaches to the table instead of re-initialising it), the value its
/// reader holds and the txnid of its next writer. Both files under another
/// name, then the lock file alone (`core::registry` tests every branch of
/// the open).
#[test]
fn second_open_under_another_name_leaves_the_first_alone() {
    let dir = tmp();
    let [a, b, c] = ["a", "b", "c"].map(|name| {
        let path = dir.path().join(name);
        std::fs::create_dir(&path).unwrap();
        path
    });
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_SYNC)
        .open(&a)
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, b"key", &[11; 32000], WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
    let reader = env.begin_ro_txn().unwrap();
    let held = reader.get(Dbi::MAIN, b"key").unwrap().unwrap();
    let before = held.to_vec();
    let pid = std::process::id() as i32;
    let table = format!("PEER {pid}:Some(1)");
    assert_eq!(peer_reader_list(&a), table);

    if let Err(e) = std::fs::hard_link(a.join("data.mdb"), b.join("data.mdb")) {
        eprintln!("NOTE: skipped: the file system refuses hard links: {e}");
        return;
    }
    std::fs::hard_link(a.join("lock.mdb"), b.join("lock.mdb")).unwrap();
    std::fs::hard_link(a.join("lock.mdb"), c.join("lock.mdb")).unwrap();
    for alias in [&b, &c] {
        assert!(
            matches!(
                Environment::builder().open(alias),
                Err(Error::EnvAlreadyOpened)
            ),
            "{alias:?}"
        );
        let readers = env.reader_list().unwrap();
        assert_eq!(readers.len(), 1, "{alias:?}");
        assert_eq!((readers[0].pid, readers[0].txnid), (pid, Some(1)));
        assert_eq!(env.info().unwrap().num_readers, 1);
        assert_eq!(peer_reader_list(&a), table, "{alias:?}");
        assert_eq!(held, &before[..]);
    }
    assert!(!c.join("data.mdb").exists());

    let mut txn = env.begin_rw_txn().unwrap();
    assert_eq!(txn.id(), 2);
    txn.put(Dbi::MAIN, b"key", &[22; 32000], WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(held, &before[..]);
}

/// A database for the handle tests below: `n` keys, value = key repeated.
fn fill(dir: &std::path::Path, n: u32) {
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC)
        .open(dir)
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 0..n {
        let k = i.to_be_bytes();
        txn.put(Dbi::MAIN, &k, &k.repeat(20), WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
}

/// The pattern of a program with several embedded interpreters on threads
/// of one process, each opening the environment with its own handle,
/// read-only and without a lock file, all at the same time.
#[test]
fn no_lock_read_only_handles_on_threads() {
    let dir = tmp();
    fill(dir.path(), 500);
    let threads = 8;
    let opened = std::sync::Barrier::new(threads);
    let reading = std::sync::Barrier::new(threads);
    let checked = std::sync::Barrier::new(threads);
    std::thread::scope(|s| {
        for t in 0..threads {
            let (dir, opened, reading, checked) = (dir.path(), &opened, &reading, &checked);
            s.spawn(move || {
                for cycle in 0..5 {
                    let env = Environment::builder()
                        .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
                        .open(dir)
                        .unwrap();
                    if cycle == 0 {
                        opened.wait();
                    }
                    let txn = env.begin_ro_txn().unwrap();
                    if cycle == 0 {
                        // Every handle's reader is in the one table.
                        reading.wait();
                        assert_eq!(env.in_memory_readers(), Some(threads));
                        checked.wait();
                    }
                    let mut c = txn.cursor(Dbi::MAIN).unwrap();
                    let mut n = 0u32;
                    while let Some((k, v)) = c.next().unwrap() {
                        assert_eq!(k, n.to_be_bytes());
                        assert_eq!(v, k.repeat(20), "thread {t}");
                        n += 1;
                    }
                    assert_eq!(n, 500);
                }
            });
        }
    });
    // Everything was released: an open with a lock file works again.
    Environment::builder().open(dir.path()).unwrap();
}

/// Sets the flag when dropped (also when unwinding).
struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Write transactions of different `NO_LOCK` handles of one file are
/// serialised, and readers of any handle keep their values while writers
/// of others commit; handles open and close all the while.
#[test]
fn no_lock_handles_serialise_writers_and_keep_readers() {
    let dir = tmp();
    // (Readers keep pages from reuse while the file grows.)
    let open = || {
        Environment::builder()
            .map_size(64 << 20)
            .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC)
            .open(dir.path())
            .unwrap()
    };
    let count = |txn: &dyn Transaction| -> u64 {
        txn.get(Dbi::MAIN, b"n")
            .unwrap()
            .map_or(0, |v| u64::from_le_bytes(v[..8].try_into().unwrap()))
    };
    let (writers, rounds) = (3, 60);
    // (Read-only handles need the data file.)
    drop(open());
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        // The readers stop once the writers are done, also if one fails.
        let _stop = StopOnDrop(&done);
        let mut handles = Vec::new();
        for _ in 0..writers {
            handles.push(s.spawn(|| {
                // Each writer thread with its own handle.
                let env = open();
                for _ in 0..rounds {
                    let mut txn = env.begin_rw_txn().unwrap();
                    let n = count(&txn);
                    // A value of a page or more, so that commits free and
                    // reuse pages.
                    let mut v = vec![(n % 251) as u8; 5000];
                    v[..8].copy_from_slice(&(n + 1).to_le_bytes());
                    let mut child = txn.begin_nested_txn().unwrap();
                    child.put(Dbi::MAIN, b"n", &v, WriteFlags::empty()).unwrap();
                    child.commit().unwrap();
                    std::thread::yield_now();
                    txn.commit().unwrap();
                }
            }));
        }
        for _ in 0..3 {
            s.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    // A fresh handle per snapshot, read-only.
                    let env = Environment::builder()
                        .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::READ_ONLY)
                        .open(dir.path())
                        .unwrap();
                    let txn = env.begin_ro_txn().unwrap();
                    let held = txn.get(Dbi::MAIN, b"n").unwrap();
                    let copy = held.map(<[u8]>::to_vec);
                    for _ in 0..20 {
                        std::thread::yield_now();
                    }
                    assert_eq!(held.map(<[u8]>::to_vec), copy, "a value changed");
                    if let Some(v) = held {
                        let n = u64::from_le_bytes(v[..8].try_into().unwrap());
                        assert!(v[8..].iter().all(|&b| u64::from(b) == (n - 1) % 251));
                    }
                }
            });
        }
        for h in handles {
            h.join().unwrap();
        }
    });
    let env = open();
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(count(&txn), writers * rounds);
    assert_eq!(txn.id(), writers * rounds);
}

/// Without a lock file, a read transaction reads the snapshot whose txnid
/// it pins, also when it begins while commits replace the meta page it
/// copies: one writer stores each transaction's txnid as the value of
/// `counter`, and readers on other threads check it against `RoTxn::id`.
/// Readers that pinned a newer snapshot than they read would let the
/// writer reuse their pages, so some readers also hold their value across
/// many commits and check that it does not change. Both formats, with
/// readers in the writer's handle and in another handle of the file.
#[test]
fn no_lock_readers_pin_the_snapshot_they_read() {
    for format in [Format::V09, Format::V10] {
        for two_handles in [false, true] {
            no_lock_snapshot_matches_its_id(format, two_handles);
        }
    }
}

fn no_lock_snapshot_matches_its_id(format: Format, two_handles: bool) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let dir = tmp();
    let open = || {
        Environment::builder()
            .format(format)
            // 1 GiB is reserved, not written: a reader descheduled under load while a
            // fast release-mode writer runs 4000 commits must not fill the map.
            .map_size(1 << 30)
            .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC)
            .open(dir.path())
            .unwrap()
    };
    let writer_env = open();
    let other = two_handles.then(open);
    let reader_env = other.as_ref().unwrap_or(&writer_env);
    // The value of `counter` in transaction `id`: the id, then a filler
    // that changes with it.
    let value = |id: u64| -> Vec<u8> {
        let mut v = vec![id as u8; 64];
        v[..8].copy_from_slice(&id.to_le_bytes());
        v
    };
    let check = |txn: &rlmdb::RoTxn<'_>, v: &[u8], what: &str| {
        let id = u64::from_le_bytes(v[..8].try_into().unwrap());
        assert_eq!(
            (txn.id(), v),
            (id, &value(id)[..]),
            "{format:?}, two handles {two_handles}: {what}"
        );
    };
    let (commits, held_commits) = (4000, 20);
    let committed = AtomicU64::new(0);
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let _stop = StopOnDrop(&done);
        let (committed, done) = (&committed, &done);
        for t in 0..6 {
            s.spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    let txn = reader_env.begin_ro_txn().unwrap();
                    let Some(held) = txn.get(Dbi::MAIN, b"counter").unwrap() else {
                        assert_eq!(txn.id(), 0);
                        continue;
                    };
                    check(&txn, held, "snapshot and value differ");
                    if t % 2 == 1 {
                        // Hold the value while the writer commits.
                        let copy = held.to_vec();
                        let start = committed.load(Ordering::SeqCst);
                        while committed.load(Ordering::SeqCst) < start + held_commits
                            && !done.load(Ordering::SeqCst)
                        {
                            std::thread::yield_now();
                        }
                        assert_eq!(held, &copy[..], "a held value changed");
                        check(&txn, held, "a held value changed");
                    }
                }
            });
        }
        for _ in 0..commits {
            let mut txn = writer_env.begin_rw_txn().unwrap();
            let id = txn.id();
            txn.put(Dbi::MAIN, b"counter", &value(id), WriteFlags::empty())
                .unwrap();
            txn.commit().unwrap();
            committed.store(id, Ordering::SeqCst);
        }
    });
    let txn = reader_env.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), commits);
    let v = txn.get(Dbi::MAIN, b"counter").unwrap().unwrap();
    check(&txn, v, "end");
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
fn data_file_format() {
    let dir = tmp();
    // Without a request, and on request, a new environment is LMDB 0.9.
    let env = Environment::builder().open(dir.path()).unwrap();
    assert_eq!(env.format(), Format::V09);
    drop(env);
    let env = Environment::builder()
        .format(Format::V09)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.format(), Format::V09);
    assert_eq!(env.max_key_size(), MDB_MAXKEYSIZE);
    drop(env);
    // The file keeps its format: asking for the other one is refused before
    // the lock file is touched, and 0.9 has no page size setting.
    let lock_before = std::fs::read(dir.path().join("lock.mdb")).unwrap();
    let err = Environment::builder()
        .format(Format::V10)
        .open(dir.path())
        .unwrap_err();
    assert!(
        matches!(err, Error::Mdb(MdbError::VersionMismatch)),
        "{err:?}"
    );
    let err = Environment::builder()
        .page_size(4096)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    assert_eq!(
        std::fs::read(dir.path().join("lock.mdb")).unwrap(),
        lock_before
    );
    assert_eq!(
        rlmdb::sniff_format(dir.path(), EnvironmentFlags::empty()).unwrap(),
        Some(Format::V09)
    );
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
        rlmdb::format_reader_list(&env.reader_list().unwrap()),
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
    assert_eq!(env.in_memory_readers(), Some(0));
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.reader_slot(), None);
    // Tracked in memory; the lock-file views report what C does.
    assert_eq!(env.in_memory_readers(), Some(1));
    assert!(env.reader_list().unwrap().is_empty());
    assert_eq!(env.info().unwrap().num_readers, 0);
    assert_eq!(env.reader_check().unwrap(), 0);
    let inactive = txn.reset();
    assert_eq!(env.in_memory_readers(), Some(0));
    let txn = inactive.renew().unwrap();
    assert_eq!(env.in_memory_readers(), Some(1));
    // A transaction that lives on in another thread unregisters when it
    // is dropped there.
    std::thread::scope(|s| {
        s.spawn(move || drop(txn));
    });
    assert_eq!(env.in_memory_readers(), Some(0));
    assert!(!dir.path().join("lock.mdb").exists());

    // With a lock file the table is the lock file's.
    drop(env);
    let env = Environment::builder().open(dir.path()).unwrap();
    let _txn = env.begin_ro_txn().unwrap();
    assert_eq!(env.in_memory_readers(), None);
    assert_eq!(env.reader_list().unwrap().len(), 1);
}

/// Without a lock file, write transactions of the process (nested ones
/// included) are still serialised: by the in-process writer lock.
#[test]
fn no_lock_writers_are_serialised() {
    let dir = tmp();
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let count = |txn: &dyn Transaction| -> u64 {
        txn.get(Dbi::MAIN, b"n")
            .unwrap()
            .map_or(0, |v| u64::from_le_bytes(v.try_into().unwrap()))
    };
    let (threads, rounds) = (4, 50);
    std::thread::scope(|s| {
        for t in 0..threads {
            let env = &env;
            s.spawn(move || {
                for r in 0..rounds {
                    let mut txn = env.begin_rw_txn().unwrap();
                    let n = count(&txn);
                    let mut child = txn.begin_nested_txn().unwrap();
                    assert_eq!(count(&child), n);
                    child
                        .put(Dbi::MAIN, b"n", &(n + 1).to_le_bytes(), WriteFlags::empty())
                        .unwrap();
                    child.commit().unwrap();
                    if (t + r) % 7 == 0 {
                        // A nested transaction that aborts changes nothing.
                        let mut child = txn.begin_nested_txn().unwrap();
                        child
                            .put(Dbi::MAIN, b"n", &0u64.to_le_bytes(), WriteFlags::empty())
                            .unwrap();
                        child.abort();
                    }
                    std::thread::yield_now();
                    txn.commit().unwrap();
                }
            });
        }
    });
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(count(&txn), threads * rounds);
    assert_eq!(txn.id(), threads * rounds);
}

fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}

#[test]
fn auto_traits() {
    assert_send::<Environment>();
    assert_sync::<Environment>();
    assert_send::<rlmdb::RoTxn<'static>>();
    assert_send::<rlmdb::InactiveTxn<'static>>();
}
