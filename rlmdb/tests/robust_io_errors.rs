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
//! * So does a sync that fails after the meta page reached the file (on
//!   macOS the `F_FULLFSYNC` after the write; C returns the error and
//!   keeps the page, `KNOWN_C_BUGS.md` #41): the old txnid is written back
//!   over the page, so a plain copy that another handle or process takes
//!   before the re-open shows the last good snapshot too.
//! * A failed spill write fails the operation; the transaction can only be
//!   aborted (`MDB_BAD_TXN`).
//! * `copy_to_fd` into a broken pipe (or `/dev/full` on Linux) reports the
//!   error and changes nothing.
//!
//! The write errors are made by replacing the environment's descriptors
//! with read-only ones (`dup2`), which makes `pwrite` fail with `EBADF`,
//! and putting the originals back afterwards. A failed sync after a meta
//! page write that worked is injected with
//! `Environment::fail_next_meta_sync`, and on macOS also made real: a
//! write-only `/dev/null` takes the write and refuses the `F_FULLFSYNC`.

mod common;

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Replaces descriptor `fd` with a read-only descriptor of `/dev/null` (or
/// another file) until dropped, then puts the original back.
struct BrokenFd {
    fd: i32,
    saved: i32,
}

impl BrokenFd {
    fn new(fd: i32) -> BrokenFd {
        BrokenFd::with(fd, &File::open("/dev/null").unwrap())
    }

    /// Replaces `fd` with a duplicate of `file`.
    fn with(fd: i32, file: &File) -> BrokenFd {
        // SAFETY: plain descriptor calls on descriptors we own or that the
        // environment owns; `fd` stays open throughout (dup2 replaces it
        // atomically), so the environment's `File` stays valid.
        unsafe {
            let saved = libc::dup(fd);
            assert!(saved >= 0);
            assert_eq!(libc::dup2(file.as_raw_fd(), fd), fd);
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

/// `mm_txnid` of meta pages 0 and 1 in the data file (as the file holds
/// them, not as this process's map shows them).
fn file_txnids(dir: &Path, format: Format, ps: usize) -> [u64; 2] {
    let metas = meta_bytes(dir, ps);
    [0, 1].map(|i| {
        rlmdb::format::Meta::read_page_with_layout(&metas[i * ps..(i + 1) * ps], format.layout())
            .unwrap()
            .txnid
    })
}

/// Checks that the newest snapshot holds the keys `keys` (numbers, in
/// order) of [`fill`], with their values, and no others.
fn assert_keys(env: &Environment, keys: impl IntoIterator<Item = u32>) {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    for i in keys {
        let (k, v) = c.next().unwrap().expect("a key is missing");
        assert_eq!(k, format!("key{i:06}").as_bytes());
        assert_eq!(v, &[i as u8; 300][..]);
    }
    assert!(c.next().unwrap().is_none(), "unexpected keys");
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

/// The environment variables that give [`copy_process`] its job: the
/// environment, its format and the directory to copy it to.
const COPY_SRC: &str = "RLMDB_TEST_COPY_SRC";
const COPY_FORMAT: &str = "RLMDB_TEST_COPY_FORMAT";
const COPY_DST: &str = "RLMDB_TEST_COPY_DST";

/// Run by [`copy_in_another_process`] as a second process: a plain copy of
/// an environment that the test keeps open (with a lock file, a process
/// may open an environment only once).
#[test]
#[ignore = "helper process of the other tests in this file"]
fn copy_process() {
    let Ok(src) = std::env::var(COPY_SRC) else {
        return;
    };
    let format = match std::env::var(COPY_FORMAT).unwrap().as_str() {
        "1.0" => Format::V10,
        _ => Format::V09,
    };
    let dst = PathBuf::from(std::env::var(COPY_DST).unwrap());
    let env = open(Path::new(&src), EnvironmentFlags::empty(), format);
    env.copy_to_path(dst, false).unwrap();
}

fn copy_in_another_process(src: &Path, format: Format, dst: &Path) {
    let tag = if format == Format::V10 { "1.0" } else { "0.9" };
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "copy_process",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(COPY_SRC, src)
        .env(COPY_FORMAT, tag)
        .env(COPY_DST, dst)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "copy process failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A sync that fails after the meta page reached the file (on macOS the
/// `F_FULLFSYNC` after the write; injected here, after the write, on every
/// platform) is a failed meta page write: `MDB_FATAL_ERROR`, and the old
/// txnid goes back into the page, so the file holds no commit newer than
/// the published one. (It used to keep the page: a plain copy by another
/// process pinned the published snapshot, cut the data at its last page
/// and kept the newer meta page, which named pages the copy lacks.)
/// Without a lock file, the txnid the file's handles share stays where it
/// was, and another handle goes on reading, copying and writing.
#[test]
fn robust_failed_meta_sync_is_fatal() {
    for format in FORMATS {
        for flags in [EnvironmentFlags::empty(), EnvironmentFlags::NO_LOCK] {
            failed_meta_sync(format, flags);
        }
    }
}

fn failed_meta_sync(format: Format, flags: EnvironmentFlags) {
    let what = format!("{format:?} {flags:?}");
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), flags, format);
    let ps = env.page_size();
    fill(&env, 0, 300);
    let txnid = env.info().unwrap().last_txnid as u64;
    let end = std::fs::metadata(dir.path().join("data.mdb"))
        .unwrap()
        .len();
    // The page the commit writes, and the old txnid it holds.
    let slot = ((txnid + 1) & 1) as usize;
    let old = file_txnids(dir.path(), format, ps)[slot];
    assert!(old < txnid, "{what}");
    let other = flags
        .contains(EnvironmentFlags::NO_LOCK)
        .then(|| open(dir.path(), flags, format));

    // A commit whose pages go past the snapshot's last page.
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 300..2300u32 {
        txn.put(
            Dbi::MAIN,
            format!("key{i:06}").as_bytes(),
            &[i as u8; 300],
            WriteFlags::empty(),
        )
        .unwrap();
    }
    env.fail_next_meta_sync(libc::EIO);
    match txn.commit().unwrap_err() {
        rlmdb::Error::Io(e) => assert_eq!(e.raw_os_error(), Some(libc::EIO), "{what}"),
        e => panic!("{what}: {e:?}"),
    }
    assert!(
        std::fs::metadata(dir.path().join("data.mdb"))
            .unwrap()
            .len()
            > end,
        "{what}: the commit wrote no new pages"
    );
    // MDB_FATAL_ERROR: every transaction fails with MDB_PANIC from now on.
    assert_eq!(mdb(env.begin_ro_txn().unwrap_err()), Some(MdbError::Panic));
    assert_eq!(mdb(env.begin_rw_txn().unwrap_err()), Some(MdbError::Panic));
    // The page that was written has the old txnid again.
    let mut ids = file_txnids(dir.path(), format, ps);
    assert_eq!(ids[slot], old, "{what}");
    assert_eq!(ids[slot ^ 1], txnid, "{what}");

    // A plain copy taken now, by another handle (without a lock file) or
    // another process: its newest meta page is the published snapshot,
    // whose pages it holds.
    let copy = tempfile::tempdir().unwrap();
    match &other {
        Some(other) => {
            assert_eq!(other.begin_ro_txn().unwrap().id(), txnid, "{what}");
            other.copy_to_path(copy.path(), false).unwrap();
        }
        None => copy_in_another_process(dir.path(), format, copy.path()),
    }
    {
        let copied = open(copy.path(), EnvironmentFlags::empty(), format);
        assert_eq!(copied.info().unwrap().last_txnid as u64, txnid, "{what}");
        assert_keys(&copied, 0..300);
    }
    // So is C's, byte for byte (C 0.9's; the C 1.0 tools are `interop`'s).
    if format == Format::V09 && flags.is_empty() {
        if let Some(c) = common::c_tools() {
            let src = dir.path().to_str().unwrap();
            let theirs = c.run("mdb_copy", &[src], None).stdout;
            assert!(theirs == std::fs::read(copy.path().join("data.mdb")).unwrap());
        }
    }

    // The other handle writes on, from the published snapshot.
    let mut keys: Vec<u32> = (0..300).collect();
    if let Some(other) = &other {
        fill(other, 5000, 10);
        keys.extend(5000..5010);
        assert_keys(other, keys.iter().copied());
        ids = file_txnids(dir.path(), format, ps);
        assert_eq!(ids[slot], txnid + 1, "{what}");
    }
    let last = ids[0].max(ids[1]);
    drop(other);
    drop(env);
    audit(dir.path(), format);
    let env = open(dir.path(), flags, format);
    assert_eq!(env.info().unwrap().last_txnid as u64, last, "{what}");
    assert_keys(&env, keys.iter().copied());
    fill(&env, 6000, 10);
    keys.extend(6000..6010);
    assert_keys(&env, keys);
}

/// The real `F_FULLFSYNC` after the meta page write fails: the meta
/// descriptor is replaced by a write-only `/dev/null`, which takes the
/// write and refuses the sync (`ENODEV`). (So the new page goes nowhere
/// and the file holds the old txnid either way; the injected failure
/// above covers the state.)
#[cfg(any(target_os = "macos", target_os = "ios"))]
#[test]
fn robust_failed_full_fsync_is_fatal() {
    let null = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap();
    if null.sync_data().is_ok() {
        // `sync_data` is F_FULLFSYNC on Apple systems.
        eprintln!("skipped: F_FULLFSYNC works on /dev/null here");
        return;
    }
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), EnvironmentFlags::empty(), format);
        fill(&env, 0, 300);
        let txnid = env.info().unwrap().last_txnid;
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"lost", b"value", WriteFlags::empty())
            .unwrap();
        {
            let _broken = BrokenFd::with(meta_fd(&dir.path().join("data.mdb")), &null);
            let err = txn.commit().unwrap_err();
            assert!(matches!(err, rlmdb::Error::Io(_)), "{err:?}");
        }
        assert_eq!(mdb(env.begin_ro_txn().unwrap_err()), Some(MdbError::Panic));
        assert_eq!(mdb(env.begin_rw_txn().unwrap_err()), Some(MdbError::Panic));
        drop(env);
        audit(dir.path(), format);
        let env = open(dir.path(), EnvironmentFlags::empty(), format);
        assert_eq!(env.info().unwrap().last_txnid, txnid);
        assert_keys(&env, 0..300);
        fill(&env, 300, 10);
        assert_keys(&env, 0..310);
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
