//! I/O errors during commit, spill and copy leave the environment as C
//! leaves it.
//!
//! * A failed data page write (`mdb_page_flush`) or sync fails the commit:
//!   the transaction is aborted, no meta page is written, `mti_txnid` is
//!   unchanged and the environment stays usable (C does not set
//!   `MDB_FATAL_ERROR` there).
//! * A failed meta page write (`mdb_env_write_meta`) sets
//!   `MDB_FATAL_ERROR`: every later transaction fails with `MDB_PANIC`;
//!   after a re-open the environment shows the last good snapshot.
//! * A failed spill write fails the operation; the transaction can only be
//!   aborted (`MDB_BAD_TXN`).
//! * `copy_to_fd` into a broken pipe (or `/dev/full` on Linux) reports the
//!   error and changes nothing.
//!
//! The write errors are made by replacing the environment's descriptors
//! with read-only ones (`dup2`), which makes `pwrite` fail with `EBADF`,
//! and putting the originals back afterwards.

mod common;

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use rlmdb::{Dbi, Environment, EnvironmentFlags, Format, MdbError, Transaction, WriteFlags};

/// Every test runs on an LMDB 0.9 environment and on an LMDB 1.0 one with
/// 1 KiB pages.
const FORMATS: [Format; 2] = [Format::V09, Format::V10];

fn open(path: &Path, flags: EnvironmentFlags, format: Format) -> Environment {
    let mut b = Environment::builder();
    b.map_size(64 << 20).flags(flags).format(format);
    if format == Format::V10 && !path.join("data.mdb").exists() {
        b.page_size(1024);
    }
    b.open(path).unwrap()
}

fn fill(env: &Environment, from: u32, n: u32) {
    let mut txn = env.begin_rw_txn().unwrap();
    for i in from..from + n {
        txn.put(
            Dbi::MAIN,
            format!("key{i:06}").as_bytes(),
            &[i as u8; 300],
            WriteFlags::empty(),
        )
        .unwrap();
    }
    txn.commit().unwrap();
}

fn count(env: &Environment) -> usize {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut n = 0;
    while c.next().unwrap().is_some() {
        n += 1;
    }
    n
}

fn mdb(e: rlmdb::Error) -> Option<MdbError> {
    match e {
        rlmdb::Error::Mdb(m) => Some(m),
        _ => None,
    }
}

/// Replaces descriptor `fd` with a read-only descriptor of `/dev/null` until
/// dropped, then puts the original back.
struct BrokenFd {
    fd: i32,
    saved: i32,
}

impl BrokenFd {
    fn new(fd: i32) -> BrokenFd {
        let null = File::open("/dev/null").unwrap();
        // SAFETY: plain descriptor calls on descriptors we own or that the
        // environment owns; `fd` stays open throughout (dup2 replaces it
        // atomically), so the environment's `File` stays valid.
        unsafe {
            let saved = libc::dup(fd);
            assert!(saved >= 0);
            assert_eq!(libc::dup2(null.as_raw_fd(), fd), fd);
            BrokenFd { fd, saved }
        }
    }
}

impl Drop for BrokenFd {
    fn drop(&mut self) {
        // SAFETY: as in `new`; `saved` is ours and closed exactly once.
        unsafe {
            assert_eq!(libc::dup2(self.saved, self.fd), self.fd);
            libc::close(self.saved);
        }
    }
}

/// The descriptor the environment writes meta pages through (`me_mfd`): the
/// one of this process that is open write-only on the data file.
fn meta_fd(data: &Path) -> i32 {
    let md = std::fs::metadata(data).unwrap();
    let mut found = Vec::new();
    for fd in 0..4096 {
        // SAFETY: fstat/fcntl on any number are harmless; failures mean
        // "not an open descriptor".
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 {
                continue;
            }
            if st.st_dev as u64 != md.dev() || st.st_ino as u64 != md.ino() {
                continue;
            }
            let fl = libc::fcntl(fd, libc::F_GETFL);
            if fl >= 0 && fl & libc::O_ACCMODE == libc::O_WRONLY {
                found.push(fd);
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "write-only descriptors of {data:?}: {found:?}"
    );
    found[0]
}

fn meta_bytes(dir: &Path, ps: usize) -> Vec<u8> {
    std::fs::read(dir.join("data.mdb")).unwrap()[..2 * ps].to_vec()
}

/// The C page audit; only for 0.9 environments here (the 1.0 one is
/// `cpeer10` in `interop`).
fn audit(dir: &Path, format: Format) {
    if format != Format::V09 {
        return;
    }
    if let Some(c) = common::c_tools() {
        let out = c.peer(&["check-freelist", dir.to_str().unwrap()]);
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    }
}

#[test]
fn robust_failed_page_flush_aborts_the_commit() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), EnvironmentFlags::empty(), format);
        fill(&env, 0, 500);
        let before = meta_bytes(dir.path(), env.page_size());
        let txnid = env.info().unwrap().last_txnid;

        let mut txn = env.begin_rw_txn().unwrap();
        for i in 500..1500u32 {
            txn.put(
                Dbi::MAIN,
                format!("key{i:06}").as_bytes(),
                b"new",
                WriteFlags::empty(),
            )
            .unwrap();
        }
        {
            let _broken = BrokenFd::new(env.data_file().as_raw_fd());
            let err = txn.commit().unwrap_err();
            assert!(matches!(err, rlmdb::Error::Io(_)), "{err:?}");
        }
        // Nothing visible changed, and the environment is not hosed.
        assert_eq!(meta_bytes(dir.path(), env.page_size()), before);
        assert_eq!(env.info().unwrap().last_txnid, txnid);
        assert_eq!(count(&env), 500);
        fill(&env, 500, 100);
        assert_eq!(count(&env), 600);
        drop(env);
        audit(dir.path(), format);
        assert_eq!(
            count(&open(dir.path(), EnvironmentFlags::empty(), format)),
            600
        );
    }
}

#[test]
fn robust_failed_meta_write_is_fatal() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), EnvironmentFlags::empty(), format);
        fill(&env, 0, 300);
        let txnid = env.info().unwrap().last_txnid;
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"lost", b"value", WriteFlags::empty())
            .unwrap();
        {
            let _broken = BrokenFd::new(meta_fd(&dir.path().join("data.mdb")));
            assert!(txn.commit().is_err());
        }
        // MDB_FATAL_ERROR: every transaction fails with MDB_PANIC from now on.
        assert_eq!(mdb(env.begin_ro_txn().unwrap_err()), Some(MdbError::Panic));
        assert_eq!(mdb(env.begin_rw_txn().unwrap_err()), Some(MdbError::Panic));
        // The writer lock was released: another environment handle (after
        // this one is closed) can write.
        drop(env);
        audit(dir.path(), format);
        let env = open(dir.path(), EnvironmentFlags::empty(), format);
        assert_eq!(env.info().unwrap().last_txnid, txnid);
        assert_eq!(count(&env), 300);
        assert!(env
            .begin_ro_txn()
            .unwrap()
            .get(Dbi::MAIN, b"lost")
            .unwrap()
            .is_none());
        fill(&env, 300, 10);
        assert_eq!(count(&env), 310);
    }
}

#[test]
fn robust_failed_spill_fails_the_transaction() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), EnvironmentFlags::NO_SYNC, format);
        fill(&env, 0, 200);
        env.set_dirty_limit(16);
        let mut txn = env.begin_rw_txn().unwrap();
        let failed = {
            let _broken = BrokenFd::new(env.data_file().as_raw_fd());
            (1000..20_000u32).find_map(|i| {
                txn.put(
                    Dbi::MAIN,
                    format!("key{i:06}").as_bytes(),
                    &[1; 500],
                    WriteFlags::empty(),
                )
                .err()
            })
        };
        let err = failed.expect("a spill must have failed");
        assert!(matches!(err, rlmdb::Error::Io(_)), "{err:?}");
        // The transaction is unusable now (C: MDB_TXN_ERROR).
        assert_eq!(
            mdb(txn
                .put(Dbi::MAIN, b"x", b"y", WriteFlags::empty())
                .unwrap_err()),
            Some(MdbError::BadTxn)
        );
        assert_eq!(mdb(txn.commit().unwrap_err()), Some(MdbError::BadTxn));
        assert_eq!(count(&env), 200);
        fill(&env, 200, 50);
        assert_eq!(count(&env), 250);
        drop(env);
        audit(dir.path(), format);
    }
}

#[test]
fn robust_failed_copy_changes_nothing() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), EnvironmentFlags::NO_SYNC, format);
        fill(&env, 0, 400);
        for compact in [false, true] {
            // A pipe whose reader is gone: EPIPE (SIGPIPE is ignored in Rust
            // programs).
            let mut fds = [0; 2];
            // SAFETY: pipe() fills two descriptors that we then own.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: both descriptors are ours and owned once.
            let (r, w) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
            drop(r);
            let err = env
                .copy_to_fd(std::os::fd::AsFd::as_fd(&w), compact)
                .unwrap_err();
            assert!(matches!(err, rlmdb::Error::Io(_)), "{err:?}");
            #[cfg(target_os = "linux")]
            {
                let full = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap();
                let err = env
                    .copy_to_fd(std::os::fd::AsFd::as_fd(&full), compact)
                    .unwrap_err();
                match err {
                    rlmdb::Error::Io(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOSPC)),
                    e => panic!("{e:?}"),
                }
            }
            // A read-only descriptor: EBADF.
            let ro = File::open(dir.path().join("data.mdb")).unwrap();
            assert!(env
                .copy_to_fd(std::os::fd::AsFd::as_fd(&ro), compact)
                .is_err());
        }
        // The environment is untouched: no reader slot left behind, writes work.
        assert!(env.reader_list().unwrap().iter().all(|r| r.txnid.is_none()));
        fill(&env, 400, 10);
        assert_eq!(count(&env), 410);
        let target = tempfile::tempdir().unwrap();
        env.copy_to_path(target.path(), true).unwrap();
        assert_eq!(
            count(&open(target.path(), EnvironmentFlags::NO_SYNC, format)),
            410
        );
    }
}
