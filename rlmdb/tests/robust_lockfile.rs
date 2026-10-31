//! Lock-file robustness: an environment whose lock file holds garbage that
//! another process left there.
//!
//! `robust_mutation.rs` damages the data file. The lock file is the other
//! input a process does not control: every process using the environment
//! writes it, and only an exclusive opener re-initialises it. These tests
//! keep the environment open in a second process (this test binary run
//! again as a holder, see [`lock_holder_process`]) so that our open is not
//! exclusive and has to use the table as found, then damage the header
//! (`mti_txnid`, `mti_numreaders`, on macOS the semaphore names or hash)
//! and the reader slots (pids including our own, negative and huge ones,
//! transaction ids, thread ids, random bytes) before the open and between
//! operations, and open, read, renew, write, list and check readers.
//!
//! The requirement is the one of the data-file tests: no panic and no
//! endless loop; errors are fine. Not damaged: the process-shared pthread
//! mutexes of the Linux layouts (corrupt mutex state hangs or crashes
//! glibc, for C processes just the same) and reader slots this process
//! owns while it uses them (that is a peer breaking the protocol under a
//! live snapshot, see `os::DataMap`). Lies in `mti_txnid` can make the
//! data file inconsistent (C's writer trusts that field as ours does); the
//! last step of each case re-opens it exclusively and reads and writes it,
//! again requiring only errors, not panics.
//!
//! Knobs: `ROBUST_LOCK_CASES=N` cases per format (default 25),
//! `ROBUST_LOCK_SEED=<09|10>:<seed>` to run one case,
//! `ROBUST_LOCK_TIMEOUT=S` watchdog per case (default 60).

use std::fs::{File, OpenOptions};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rlmdb::format::lockfile::{
    LockLayout, LockObjects, MNAME_LEN, READER_PID, READER_SIZE, READER_TID, READER_TXNID,
    TXNINFO_FORMAT, TXNINFO_MAGIC,
};
use rlmdb::{Dbi, Environment, EnvironmentFlags, Error, Format, MdbError, Transaction, WriteFlags};

/// `read_exact_at`/`write_all_at` on Windows, where `FileExt` only has
/// `seek_read`/`seek_write`. (The damage never touches byte 0, the only
/// byte the processes lock, which Windows would refuse to write.)
#[cfg(windows)]
trait FileExt {
    fn read_exact_at(&self, buf: &mut [u8], off: u64) -> std::io::Result<()>;
    fn write_all_at(&self, buf: &[u8], off: u64) -> std::io::Result<()>;
}

#[cfg(windows)]
impl FileExt for File {
    fn read_exact_at(&self, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(self, buf, off)? {
                0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_write(self, buf, off)? {
                0 => return Err(std::io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// Set (to `<dir>|<09|10>`) in the environment of a holder process.
const HOLDER_VAR: &str = "ROBUST_LOCK_HOLDER";
/// Name of the file a holder creates once it holds the environment.
const READY: &str = "holder-ready";
const MAX_READERS: u32 = 8;

/// How far the cases got (both formats together), to see that the damage
/// still leaves something to exercise.
static OPENED: AtomicUsize = AtomicUsize::new(0);
static READS: AtomicUsize = AtomicUsize::new(0);
static COMMITS: AtomicUsize = AtomicUsize::new(0);

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len())]
    }
}

fn format_tag(format: Format) -> &'static str {
    match format {
        Format::V09 => "09",
        Format::V10 => "10",
    }
}

fn open_env(dir: &Path, format: Format) -> rlmdb::Result<Environment> {
    Environment::builder()
        .map_size(16 << 20)
        .max_readers(MAX_READERS)
        .format(format)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
}

fn key(i: u64) -> Vec<u8> {
    format!("key{i:08}").into_bytes()
}

/// A fresh environment with some data and some free pages.
fn create(dir: &Path, format: Format) {
    let env = open_env(dir, format).unwrap();
    for round in 0..3u64 {
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..200 {
            let v = vec![(i + round) as u8; 50 + (i as usize % 7) * 40];
            txn.put(Dbi::MAIN, &key(i), &v, WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
    }
}

// ---------------------------------------------------------------------------
// The holder process
// ---------------------------------------------------------------------------

/// Not a test: the body of the holder process the other tests start (this
/// binary run again with [`HOLDER_VAR`] set). It opens the environment,
/// begins a read transaction (so it owns a reader slot and its pid lock),
/// signals readiness and holds everything until its stdin is closed. It
/// then ends the transaction and closes the environment over whatever the
/// lock file holds by then; a panic there fails the case that started it.
#[test]
#[ignore = "helper process of the other tests in this file"]
fn lock_holder_process() {
    let Ok(spec) = std::env::var(HOLDER_VAR) else {
        return;
    };
    let (dir, tag) = spec.rsplit_once('|').expect("holder spec");
    let format = if tag == "10" {
        Format::V10
    } else {
        Format::V09
    };
    let dir = PathBuf::from(dir);
    let env = open_env(&dir, format).expect("holder: open");
    let txn = env.begin_ro_txn().expect("holder: read txn");
    std::fs::write(dir.join(READY), std::process::id().to_string()).unwrap();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(txn);
    drop(env);
}

/// A running holder process.
struct Holder {
    child: Option<Child>,
    pid: i32,
}

impl Holder {
    fn start(dir: &Path, format: Format) -> Holder {
        let _ = std::fs::remove_file(dir.join(READY));
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "lock_holder_process",
                "--exact",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(
                HOLDER_VAR,
                format!("{}|{}", dir.display(), format_tag(format)),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut holder = Holder {
            child: Some(child),
            pid: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(s) = std::fs::read_to_string(dir.join(READY)) {
                if let Ok(pid) = s.trim().parse() {
                    holder.pid = pid;
                    return holder;
                }
            }
            if let Some(status) = holder.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("holder exited early: {status}");
            }
            assert!(Instant::now() < deadline, "holder did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Lets the holder close the environment; `true` if it exited cleanly.
    fn finish(mut self) -> bool {
        let mut child = self.child.take().unwrap();
        drop(child.stdin.take());
        child.wait().unwrap().success()
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Damage
// ---------------------------------------------------------------------------

/// The lock file, opened by the test for writing. It stays open until the
/// test's environment is closed: closing any descriptor of a file drops
/// all of the process's `fcntl` locks on it.
struct LockFile {
    file: File,
    layout: &'static LockLayout,
    slots: usize,
}

impl LockFile {
    fn open(dir: &Path, format: Format) -> LockFile {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("lock.mdb"))
            .unwrap();
        let layout = LockLayout::of(format);
        let slots = layout.readers_in_file(file.metadata().unwrap().len() as usize);
        LockFile {
            file,
            layout,
            slots,
        }
    }

    fn put_u32(&self, off: usize, v: u32) {
        self.file
            .write_all_at(&v.to_ne_bytes(), off as u64)
            .unwrap();
    }

    fn put_u64(&self, off: usize, v: u64) {
        self.file
            .write_all_at(&v.to_ne_bytes(), off as u64)
            .unwrap();
    }

    fn get_u64(&self, off: usize) -> u64 {
        let mut b = [0u8; 8];
        self.file.read_exact_at(&mut b, off as u64).unwrap();
        u64::from_ne_bytes(b)
    }

    fn slot_pid(&self, i: usize) -> i32 {
        let mut b = [0u8; 4];
        let off = self.layout.reader_offset(i) + READER_PID;
        self.file.read_exact_at(&mut b, off as u64).unwrap();
        i32::from_ne_bytes(b)
    }

    /// `true` if byte `off` belongs to a pthread mutex (never damaged).
    fn is_mutex_byte(&self, off: usize) -> bool {
        let l = self.layout;
        l.objects == LockObjects::PthreadMutexes
            && ((l.rlock..l.rlock + l.rlock_size).contains(&off)
                || l.wlock.is_some_and(|w| (w..w + MNAME_LEN).contains(&off)))
    }

    /// Applies one random mutation and describes it. `before_open`: the
    /// test's environment is not open yet (the semaphore names or hash may
    /// be damaged, and slots the test owns need not be spared).
    fn mutate(&self, rng: &mut Rng, holder: i32, before_open: bool) -> String {
        let me = std::process::id() as i32;
        let l = self.layout;
        let current = self.get_u64(l.txnid);
        // A slot the test process does not own right now.
        let slot = |rng: &mut Rng| -> Option<usize> {
            let i = rng.below(self.slots);
            (before_open || self.slot_pid(i) != me).then_some(i)
        };
        match rng.below(if before_open { 7 } else { 6 }) {
            0 => {
                let r = rng.next();
                let v = rng.pick(&[
                    0,
                    1,
                    current.wrapping_sub(1),
                    current.wrapping_add(1),
                    current.wrapping_add(2),
                    current.wrapping_add(1000),
                    u64::MAX,
                    u64::MAX - 1,
                    r,
                ]);
                self.put_u64(l.txnid, v);
                format!("mti_txnid {current} -> {v:#x}")
            }
            1 => {
                let n = self.slots as u32;
                let r = rng.next() as u32;
                let v = rng.pick(&[0, 1, n - 1, n, n + 1, u32::MAX, r]);
                self.put_u32(l.num_readers, v);
                format!("mti_numreaders = {v}")
            }
            2 => {
                let Some(i) = slot(rng) else {
                    return "skip (own slot)".into();
                };
                let r = rng.next() as i32;
                let v = rng.pick(&[0, me, holder, -1, i32::MIN, i32::MAX, 1, r]);
                self.put_u32(l.reader_offset(i) + READER_PID, v as u32);
                format!("slot {i} pid = {v}")
            }
            3 => {
                let Some(i) = slot(rng) else {
                    return "skip (own slot)".into();
                };
                let r = rng.next();
                let v = rng.pick(&[0, !0, current, current.wrapping_add(5), 1, r]);
                self.put_u64(l.reader_offset(i) + READER_TXNID, v);
                format!("slot {i} txnid = {v:#x}")
            }
            4 => {
                let Some(i) = slot(rng) else {
                    return "skip (own slot)".into();
                };
                self.put_u64(l.reader_offset(i) + READER_TID, rng.next());
                format!("slot {i} tid")
            }
            5 => {
                let Some(i) = slot(rng) else {
                    return "skip (own slot)".into();
                };
                let mut b = [0u8; READER_SIZE];
                b.iter_mut().for_each(|x| *x = rng.next() as u8);
                // Mostly small pids, so that they are probed for liveness.
                if rng.below(2) == 0 {
                    let pid = 2 + rng.below(1 << 16) as i32;
                    b[READER_PID..READER_PID + 4].copy_from_slice(&pid.to_ne_bytes());
                }
                let off = l.reader_offset(i);
                self.file.write_all_at(&b, off as u64).unwrap();
                format!("slot {i} random bytes")
            }
            _ => {
                // Anywhere in the header but the pthread mutexes: on macOS
                // this damages the semaphore names (version 1) or the hash
                // they are derived from (version 2).
                let off = loop {
                    let off = rng.below(l.readers);
                    if !self.is_mutex_byte(off) && off >= TXNINFO_FORMAT + 4 {
                        break off;
                    }
                };
                let v = rng.next() as u8;
                self.file.write_all_at(&[v], off as u64).unwrap();
                format!("header byte {off} = {v:#x}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Exercising the environment
// ---------------------------------------------------------------------------

fn read_some(env: &Environment, rng: &mut Rng) {
    let Ok(txn) = env.begin_ro_txn() else {
        return;
    };
    READS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut c) = txn.cursor(Dbi::MAIN) {
        for _ in 0..300 {
            match c.next() {
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    }
    for _ in 0..20 {
        let _ = txn.get(Dbi::MAIN, &key(rng.below(400) as u64));
    }
    let _ = txn.stat(Dbi::MAIN);
}

fn write_some(env: &Environment, rng: &mut Rng) {
    let Ok(mut txn) = env.begin_rw_txn() else {
        return;
    };
    for _ in 0..30 {
        let k = key(rng.below(400) as u64);
        let r = if rng.below(4) == 0 {
            txn.del(Dbi::MAIN, &k, None).map(drop)
        } else {
            let v = vec![rng.next() as u8; 20 + rng.below(3000)];
            txn.put(Dbi::MAIN, &k, &v, WriteFlags::empty())
        };
        if r.is_err() {
            return;
        }
    }
    if txn.commit().is_ok() {
        COMMITS.fetch_add(1, Ordering::Relaxed);
    }
}

fn exercise(env: &Environment, lock: &LockFile, rng: &mut Rng, holder: i32, log: &mut Vec<String>) {
    for _ in 0..4 {
        for _ in 0..rng.below(3) {
            log.push(lock.mutate(rng, holder, false));
        }
        match rng.below(6) {
            0 => {
                // Reset, damage, renew.
                if let Ok(txn) = env.begin_ro_txn() {
                    let inactive = txn.reset();
                    log.push(lock.mutate(rng, holder, false));
                    if let Ok(txn) = inactive.renew() {
                        let _ = txn.get(Dbi::MAIN, &key(1));
                    }
                }
            }
            1 => {
                let _ = env.reader_check();
            }
            2 => {
                let _ = env.reader_list();
                let _ = env.info();
            }
            3 | 4 => write_some(env, rng),
            _ => read_some(env, rng),
        }
    }
    // Several read transactions at once (more slots, ReadersFull).
    let txns: Vec<_> = (0..rng.below(MAX_READERS as usize + 2))
        .filter_map(|_| env.begin_ro_txn().ok())
        .collect();
    let _ = env.reader_list();
    drop(txns);
    let _ = env.reader_check();
}

/// One case: returns the mutations applied, and whether the holder
/// exited cleanly.
fn case(format: Format, seed: u64) -> (Vec<String>, bool) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    create(dir, format);
    let holder = Holder::start(dir, format);
    let mut log = Vec::new();
    {
        let lock = LockFile::open(dir, format);
        for _ in 0..1 + rng.below(4) {
            log.push(lock.mutate(&mut rng, holder.pid, true));
        }
        match open_env(dir, format) {
            Ok(env) => {
                OPENED.fetch_add(1, Ordering::Relaxed);
                read_some(&env, &mut rng);
                exercise(&env, &lock, &mut rng, holder.pid, &mut log);
            }
            Err(e) => log.push(format!("open: {e}")),
        }
        // The environment is closed; now the descriptor may go.
    }
    let holder_ok = holder.finish();
    // Alone again: an exclusive open re-initialises the lock file; the
    // data file may be inconsistent after the lies above.
    if let Ok(env) = open_env(dir, format) {
        read_some(&env, &mut rng);
        write_some(&env, &mut rng);
        read_some(&env, &mut rng);
    }
    (log, holder_ok)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn run(format: Format, first_seed: u64) {
    let tag = format_tag(format);
    let seeds: Vec<u64> = match std::env::var("ROBUST_LOCK_SEED") {
        Ok(s) => match s.split_once(':') {
            Some((t, seed)) if t == tag => vec![seed.parse().expect("ROBUST_LOCK_SEED")],
            _ => Vec::new(),
        },
        Err(_) => (0..env_u64("ROBUST_LOCK_CASES", 25))
            .map(|i| first_seed + i)
            .collect(),
    };
    let timeout = Duration::from_secs(env_u64("ROBUST_LOCK_TIMEOUT", 60));
    let mut failures = Vec::new();
    for seed in seeds {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| case(format, seed)));
            let _ = tx.send(r);
        });
        match rx.recv_timeout(timeout) {
            Ok(Ok((_, true))) => {}
            Ok(Ok((log, false))) => failures.push(format!(
                "HOLDER FAILED ROBUST_LOCK_SEED={tag}:{seed}\n    mutations: {log:?}"
            )),
            Ok(Err(_)) => failures.push(format!("PANIC ROBUST_LOCK_SEED={tag}:{seed}")),
            Err(_) => {
                // The case's thread cannot be stopped: report and give up.
                panic!("HANG ROBUST_LOCK_SEED={tag}:{seed}");
            }
        }
    }
    eprintln!(
        "robust_lockfile (so far): opened={} read_txns={} commits={}",
        OPENED.load(Ordering::Relaxed),
        READS.load(Ordering::Relaxed),
        COMMITS.load(Ordering::Relaxed)
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn robust_lockfile_v09() {
    run(Format::V09, 1);
}

#[test]
fn robust_lockfile_v10() {
    run(Format::V10, 1_000_001);
}

/// A table whose magic or format word is not ours, found while another
/// process holds it, is refused (`MDB_INVALID`, `MDB_VERSION_MISMATCH`);
/// a reader count beyond the table is clamped to it.
#[test]
fn robust_lockfile_header_checked() {
    for format in [Format::V09, Format::V10] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create(dir, format);
        let holder = Holder::start(dir, format);
        {
            let lock = LockFile::open(dir, format);
            let layout = lock.layout;
            let other = match format {
                Format::V09 => LockLayout::of(Format::V10),
                Format::V10 => LockLayout::of(Format::V09),
            };

            // Byte 0, the lock byte, cannot be written on Windows while a
            // process holds the environment (locks are mandatory there).
            if cfg!(unix) {
                lock.put_u32(TXNINFO_MAGIC, 0x1234_5678);
                assert!(matches!(
                    open_env(dir, format),
                    Err(Error::Mdb(MdbError::Invalid))
                ));
                lock.put_u32(TXNINFO_MAGIC, rlmdb::consts::MDB_MAGIC);
            }

            lock.put_u32(TXNINFO_FORMAT, other.lock_format);
            assert!(matches!(
                open_env(dir, format),
                Err(Error::Mdb(MdbError::VersionMismatch))
            ));
            lock.put_u32(TXNINFO_FORMAT, layout.lock_format);

            lock.put_u32(layout.num_readers, u32::MAX);
            let env = open_env(dir, format).unwrap();
            assert_eq!(env.info().unwrap().num_readers, u32::MAX);
            // Every slot looks used: the holder's is the only live one.
            let txn = env.begin_ro_txn();
            let n = env.reader_list().unwrap().len();
            assert!(n >= 1 && n <= lock.slots, "{n}");
            drop(txn);
            assert!(env.reader_check().is_ok());
        }
        assert!(holder.finish(), "holder failed");
    }
}
