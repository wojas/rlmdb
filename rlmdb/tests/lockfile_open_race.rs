//! Opening an environment while its last user closes it, on the platforms
//! whose lock objects the last user removes: POSIX named semaphores (macOS;
//! OpenBSD, NetBSD and DragonFly with 0.9 lock files) and SysV semaphore
//! sets (those three BSDs with 1.0 lock files).
//!
//! The last user takes byte 0 of the lock file exclusively and removes the
//! objects (`mdb_env_close0`). An opener whose exclusive probe failed
//! because that process still held its lock waits for a shared lock, gets
//! it once the closer has gone, and finds the lock file naming objects that
//! no longer exist. C fails the open there (`KNOWN_C_BUGS.md` #47); rlmdb
//! goes back to the exclusive probe (`LockRegion::open`).
//!
//! Several processes (this test binary run again as workers, see
//! [`open_close_worker`]) open the environment, begin a read transaction
//! (which takes the reader lock) and close it, over and over; every
//! [`WRITE_EVERY`]th time they increment a counter in a write transaction
//! instead. (Short opens leave moments when nobody has the environment
//! open, which the race needs.) Every open must succeed, and the counter
//! must end at the number of increments: if two processes ever used two
//! different sets of semaphores, their write transactions could overlap
//! and lose increments. (Without the second probe, every worker failed
//! with `ENOENT` within a few dozen opens on macOS.)
//!
//! Knob: `OPEN_RACE_ITERATIONS=N` opens per worker (default 1000).

#![cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rlmdb::{Dbi, Environment, EnvironmentFlags, Format, Transaction, WriteFlags};

/// Set (to `<dir>|<09|10>|<iterations>`) in the environment of a worker.
const WORKER_VAR: &str = "OPEN_RACE_WORKER";
/// Worker processes per format.
const WORKERS: usize = 4;
/// How long the workers of one format may take before the test fails.
const TIMEOUT: Duration = Duration::from_secs(300);
const COUNTER: &[u8] = b"counter";
/// Every this many opens, starting with the first, a worker writes.
const WRITE_EVERY: u64 = 8;

fn format_tag(format: Format) -> &'static str {
    match format {
        Format::V09 => "09",
        Format::V10 => "10",
    }
}

fn open_env(dir: &Path, format: Format) -> rlmdb::Result<Environment> {
    Environment::builder()
        .map_size(16 << 20)
        .format(format)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
}

fn counter(env: &Environment) -> u64 {
    let txn = env.begin_ro_txn().unwrap();
    let v = txn.get(Dbi::MAIN, COUNTER).unwrap().expect("counter");
    u64::from_ne_bytes(v.try_into().expect("counter size"))
}

/// One iteration of a worker: open, read or increment the counter, close.
fn iteration(dir: &Path, format: Format, write: bool) -> rlmdb::Result<()> {
    let env = open_env(dir, format)?;
    if !write {
        return env.begin_ro_txn().map(drop);
    }
    let mut txn = env.begin_rw_txn()?;
    let n = match txn.get(Dbi::MAIN, COUNTER)? {
        Some(v) => u64::from_ne_bytes(v.try_into().expect("counter size")),
        None => 0,
    };
    txn.put(
        Dbi::MAIN,
        COUNTER,
        &(n + 1).to_ne_bytes(),
        WriteFlags::empty(),
    )?;
    txn.commit()
}

/// Not a test: the body of a worker process (this binary run again with
/// [`WORKER_VAR`] set). It opens and closes the environment the given
/// number of times, reading or incrementing the counter in between, and
/// exits with status 2 and the error on stderr at the first failure.
#[test]
#[ignore = "helper process of the other tests in this file"]
fn open_close_worker() {
    let Ok(spec) = std::env::var(WORKER_VAR) else {
        return;
    };
    let mut parts = spec.rsplitn(3, '|');
    let iterations: u64 = parts.next().unwrap().parse().unwrap();
    let format = match parts.next().unwrap() {
        "10" => Format::V10,
        _ => Format::V09,
    };
    let dir = PathBuf::from(parts.next().unwrap());
    for i in 0..iterations {
        if let Err(e) = iteration(&dir, format, i % WRITE_EVERY == 0) {
            eprintln!("pid {} iteration {i}: {e}", std::process::id());
            std::process::exit(2);
        }
    }
}

fn iterations() -> u64 {
    std::env::var("OPEN_RACE_ITERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

fn spawn_worker(dir: &Path, format: Format, iterations: u64) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            "open_close_worker",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(
            WORKER_VAR,
            format!("{}|{}|{iterations}", dir.display(), format_tag(format)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn race(format: Format) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let iterations = iterations();
    drop(open_env(&path, format).unwrap());

    let mut children: Vec<Option<Child>> = (0..WORKERS)
        .map(|_| Some(spawn_worker(&path, format, iterations)))
        .collect();
    let deadline = Instant::now() + TIMEOUT;
    let mut failures = Vec::new();
    while children.iter().any(Option::is_some) {
        for slot in children.iter_mut() {
            let Some(child) = slot.as_mut() else {
                continue;
            };
            if child.try_wait().unwrap().is_some() {
                let out = slot.take().unwrap().wait_with_output().unwrap();
                if !out.status.success() {
                    failures.push(format!(
                        "{}: {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
            }
        }
        if Instant::now() > deadline {
            for child in children.iter_mut().flatten() {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("workers still running after {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(failures.is_empty(), "workers failed: {failures:#?}");

    let env = open_env(&path, format).unwrap();
    let writes = iterations.div_ceil(WRITE_EVERY);
    assert_eq!(counter(&env), WORKERS as u64 * writes);
}

#[test]
fn open_close_race_v09() {
    race(Format::V09);
}

#[test]
fn open_close_race_v10() {
    race(Format::V10);
}
