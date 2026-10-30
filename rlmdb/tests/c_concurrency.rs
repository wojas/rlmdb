//! Rust and C processes sharing one environment at the same time: the lock
//! file (reader table, shared mutexes or named semaphores, file locks) must
//! work in both directions, and MVCC must hold across implementations.

mod common;

use std::path::Path;

use common::*;
use rlmdb::{Dbi, Environment, EnvironmentFlags, Error, MdbError, Transaction};

/// Parses `mdb_stat -r` output into (pid, txnid or None) pairs.
fn parse_readers(text: &str) -> Vec<(i32, Option<u64>)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() != 3 {
                return None;
            }
            let pid = f[0].parse().ok()?;
            u64::from_str_radix(f[1], 16).ok()?;
            let txnid = if f[2] == "-" {
                None
            } else {
                Some(f[2].parse().ok()?)
            };
            Some((pid, txnid))
        })
        .collect()
}

fn me() -> i32 {
    std::process::id() as i32
}

#[test]
fn rust_creates_environment_c_uses_it() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("testdb");
    std::fs::create_dir(&path).unwrap();

    // Rust is the first opener: it creates data.mdb (two meta pages) and
    // initialises the lock file.
    let env = Environment::builder()
        .map_size(10 << 20)
        .open(&path)
        .unwrap();
    let txn = env.begin_ro_txn().unwrap();
    eprintln!("Rust-created environment, page size {}", env.page_size());

    // C reads the environment while Rust holds it open.
    let stat = parse_stat(&c.stat_text(&path, &["-e"]));
    assert_eq!(stat.env["Page size"], env.page_size().to_string());
    assert_eq!(stat.env["Map size"], (10u64 << 20).to_string());
    assert_eq!(stat.env["Number of pages used"], "2");
    assert_eq!(stat.env["Last transaction ID"], "0");
    assert_eq!(stat.env["Max readers"], "126");
    assert_eq!(stat.env["Number of readers used"], "1");
    assert_eq!(stat.dbs["Main DB"]["Entries"], 0);

    // Our reader slot is visible to C, with our pid and snapshot.
    let readers = parse_readers(&c.stat_text(&path, &["-r"]));
    assert_eq!(readers, vec![(me(), Some(0))]);
    // ... and C's stale-reader check sees us alive (we hold the pid lock).
    let text = c.stat_text(&path, &["-rr"]);
    assert!(text.contains("  0 stale readers cleared."), "{text}");

    // A C writer commits into the Rust-created files while we hold an old
    // snapshot.
    c.run("mtest", &[], Some(dir.path()));
    assert_eq!(txn.id(), 0);
    assert_eq!(walk_forward(&txn, Dbi::MAIN), vec![]);
    let txn2 = env.begin_ro_txn().unwrap();
    assert!(txn2.id() > 0);
    // (mtest seeds rand() with the time; with few values its deletes can
    // leave the DB empty, which is a valid outcome to compare too.)
    let expected = c.dump(&path, None);
    assert_eq!(walk_forward(&txn2, Dbi::MAIN), expected);
    drop((txn, txn2));
    verify_db(&env, Dbi::MAIN, &expected);
    // Our slots are gone from C's view after the txns end.
    assert_eq!(parse_readers(&c.stat_text(&path, &["-r"])), vec![]);
}

#[test]
fn rust_creates_no_sub_dir_environment() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flat.db");
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_SUB_DIR)
        .open(&path)
        .unwrap();
    let _txn = env.begin_ro_txn().unwrap();
    let text = c.stat_text(&path, &["-n", "-e", "-r"]);
    let stat = parse_stat(&text);
    assert_eq!(stat.env["Number of pages used"], "2");
    assert_eq!(parse_readers(&text), vec![(me(), Some(0))]);
}

#[test]
fn c_creates_environment_rust_attaches() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("testdb");
    std::fs::create_dir(&path).unwrap();
    // C first: creates the files and the shared locks, holds a read txn.
    let holder = c.holder(&path, &["-r"]);
    assert_eq!(holder.txnid, Some(0));

    // Rust attaches to C's lock table (on macOS: opens C's semaphores by
    // the names stored in the lock file).
    let env = Environment::builder().max_dbs(4).open(&path).unwrap();
    let readers = env.reader_list().unwrap();
    assert!(
        readers
            .iter()
            .any(|r| r.pid == holder.pid && r.txnid == Some(0)),
        "{readers:?}"
    );
    let txn = env.begin_ro_txn().unwrap();
    let mut readers = env.reader_list().unwrap();
    readers.sort_by_key(|r| r.pid);
    assert_eq!(readers.len(), 2);
    assert_eq!(txn.id(), 0);
    // C sees both readers.
    let mut c_readers = parse_readers(&c.stat_text(&path, &["-r"]));
    c_readers.sort();
    let mut want = vec![(holder.pid, Some(0)), (me(), Some(0))];
    want.sort();
    assert_eq!(c_readers, want);

    // A C writer commits (it needs the writer lock C created) while both
    // readers pin snapshot 0.
    c.run("mtest2", &[], Some(dir.path()));
    let mut holder = holder;
    let renewed = holder.renew();
    assert!(renewed > 0);
    let txn2 = env.begin_ro_txn().unwrap();
    assert_eq!(txn2.id(), renewed);
    let db = txn2.open_db(Some("id1")).unwrap();
    assert_eq!(walk_forward(&txn2, db), c.dump(&path, Some("id1")));
    drop((txn, txn2));
    holder.quit();
    assert!(env.reader_list().unwrap().is_empty());
}

#[test]
fn rust_reader_check_clears_dead_c_reader() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("testdb");
    std::fs::create_dir(&path).unwrap();
    let env = Environment::builder().open(&path).unwrap();
    let holder = c.holder(&path, &["-r"]);
    let pid = holder.pid;
    assert!(env.reader_list().unwrap().iter().any(|r| r.pid == pid));
    // A live reader is never cleared.
    assert_eq!(env.reader_check().unwrap(), 0);
    // Kill it without cleanup: its slot stays, but its pid lock is gone.
    holder.kill();
    assert!(env.reader_list().unwrap().iter().any(|r| r.pid == pid));
    assert_eq!(env.reader_check().unwrap(), 1);
    assert!(!env.reader_list().unwrap().iter().any(|r| r.pid == pid));
    assert_eq!(parse_readers(&c.stat_text(&path, &["-r"])), vec![]);
}

#[test]
fn snapshot_stable_while_c_writes() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    std::fs::create_dir(&path).unwrap();
    let env = Environment::builder()
        .map_size(256 << 20)
        .open(&path)
        .unwrap();
    let batch = |round: usize| -> Vec<Pair> {
        (0..3000)
            .map(|i| {
                let k = format!("key-{:05}", (i * 7919 + round * 13) % 20000).into_bytes();
                (
                    k,
                    format!("round-{round}-{i}-{}", "v".repeat(i % 300)).into_bytes(),
                )
            })
            .collect()
    };
    c.load(&path, None, &[], &batch(0));
    let txn = env.begin_ro_txn().unwrap();
    let before = walk_forward(&txn, Dbi::MAIN);
    assert_eq!(before, c.dump(&path, None));
    let id = txn.id();

    // Meanwhile, other Rust threads keep starting fresh read transactions:
    // every snapshot they get must be internally consistent.
    let stop = std::sync::atomic::AtomicBool::new(false);
    /// Stops the reader threads even if the main thread panics.
    struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    std::thread::scope(|s| {
        let _guard = StopOnDrop(&stop);
        let readers: Vec<_> = (0..3)
            .map(|_| {
                s.spawn(|| {
                    let mut n = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let t = env.begin_ro_txn().unwrap();
                        let pairs = walk_forward(&t, Dbi::MAIN);
                        assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0));
                        assert_eq!(pairs.len(), t.stat(Dbi::MAIN).unwrap().entries);
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        // C writers run concurrently, each committing every 100 records.
        // Every commit frees the pages it copied; a writer may reuse them
        // only once no reader (our pinned `txn` included) can still see them.
        for round in 1..=4 {
            c.load(&path, None, &[], &batch(round));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let total: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();
        eprintln!("{total} concurrent Rust snapshots checked during C writes");
    });

    // The old snapshot is unchanged...
    assert_eq!(txn.id(), id);
    assert_eq!(walk_forward(&txn, Dbi::MAIN), before);
    drop(txn);
    // ... and a new one sees the C writers' work.
    let txn2 = env.begin_ro_txn().unwrap();
    assert!(txn2.id() > id);
    let after = c.dump(&path, None);
    assert_eq!(walk_forward(&txn2, Dbi::MAIN), after);
    drop(txn2);
    // Statistics agree with C as well.
    let stat = parse_stat(&c.stat_text(&path, &["-e", "-f"]));
    let info = env.info().unwrap();
    assert_eq!(stat.env["Last transaction ID"], info.last_txnid.to_string());
    assert_eq!(stat.free_pages, Some(env.freelist().unwrap() as u64));
    assert_stat_matches("Main DB", &env.stat().unwrap(), &stat.dbs["Main DB"]);
}

#[test]
fn map_resized_when_c_grows_the_file() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    std::fs::create_dir(&path).unwrap();
    // Rust maps only the default 1 MiB.
    let env = Environment::builder().open(&path).unwrap();
    assert_eq!(env.map_size(), 1 << 20);
    let old = env.begin_ro_txn().unwrap();
    // C (with a 256 MiB map) writes more than 1 MiB.
    let big: Vec<Pair> = (0..64)
        .map(|i| (format!("k{i:02}").into_bytes(), vec![i as u8; 64 * 1024]))
        .collect();
    c.load(&path, None, &[], &big);
    // Our old snapshot is still readable; a new one does not fit our map.
    assert_eq!(walk_forward(&old, Dbi::MAIN), vec![]);
    assert!(matches!(
        env.begin_ro_txn(),
        Err(Error::Mdb(MdbError::MapResized))
    ));
    drop(old);
    drop(env);
    // Reopening picks up the larger map size from the meta page.
    let env = Environment::builder().open(&path).unwrap();
    assert!(env.map_size() > 1 << 20);
    verify_db(&env, Dbi::MAIN, &c.dump(&path, None));
}

#[test]
fn exclusive_opener_resizes_small_lock_file() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    // mtest creates its lock file with room for a single reader.
    let path = c.mtest("mtest", dir.path());
    let small = std::fs::metadata(path.join("lock.mdb")).unwrap().len();
    let env = Environment::builder().open(&path).unwrap();
    assert_eq!(env.max_readers(), 126);
    let big = std::fs::metadata(path.join("lock.mdb")).unwrap().len();
    assert!(big > small, "{small} -> {big}");
    // Many concurrent readers fit now, and C agrees on the table size.
    let txns: Vec<_> = (0..10).map(|_| env.begin_ro_txn().unwrap()).collect();
    let stat = parse_stat(&c.stat_text(&path, &["-e"]));
    assert_eq!(stat.env["Max readers"], "126");
    assert_eq!(stat.env["Number of readers used"], "10");
    drop(txns);
    check_env_info_with(&env, c, &path);
}

fn check_env_info_with(env: &Environment, c: &CTools, path: &Path) {
    let stat = parse_stat(&c.stat_text(path, &["-e"]));
    let info = env.info().unwrap();
    assert_eq!(stat.env["Map size"], info.map_size.to_string());
    assert_eq!(stat.env["Last transaction ID"], info.last_txnid.to_string());
    assert_eq!(
        stat.env["Number of pages used"],
        (info.last_pgno + 1).to_string()
    );
}

#[test]
fn rust_readers_while_c_mtest_writes() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("testdb");
    std::fs::create_dir(&path).unwrap();
    let env = Environment::builder()
        .map_size(10 << 20)
        .open(&path)
        .unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _guard = StopOnDrop(&stop);
        let readers: Vec<_> = (0..4)
            .map(|_| {
                s.spawn(|| {
                    let mut snapshots = std::collections::BTreeSet::new();
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        // Pin a snapshot briefly, read it twice: it must not
                        // change while pinned, even as C commits.
                        let t = env.begin_ro_txn().unwrap();
                        let a = walk_forward(&t, Dbi::MAIN);
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        let b = walk_backward(&t, Dbi::MAIN);
                        assert_eq!(a, b, "snapshot {} changed while pinned", t.id());
                        assert_eq!(a.len(), t.stat(Dbi::MAIN).unwrap().entries);
                        snapshots.insert(t.id());
                    }
                    snapshots.len()
                })
            })
            .collect();
        // mtest: one big insert txn, then ~100 single-delete txns and a
        // cursor-delete txn, all while our readers come and go.
        for _ in 0..3 {
            let out = std::process::Command::new(c.bin("mtest"))
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let seen: usize = readers.into_iter().map(|h| h.join().unwrap()).sum();
        eprintln!("Rust readers saw {seen} distinct snapshots while mtest committed");
    });
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(walk_forward(&txn, Dbi::MAIN), c.dump(&path, None));
}
