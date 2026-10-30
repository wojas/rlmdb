//! Multi-step interop scenarios for the write path: many Rust transactions
//! (overwrites, deletes, aborts) audited by the C page walker, C and Rust
//! writing the same environment in turn, and a C reader pinning a snapshot
//! while Rust commits.

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use common::dataset::Dataset;
use common::{CTools, Rng};
use rlmdb::consts::*;
use rlmdb::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, Transaction, WriteFlags};

fn open_env(path: &Path) -> Environment {
    Environment::builder()
        .map_size(256 << 20)
        .max_dbs(64)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

fn peer_args(cmd: &str, path: &Path, ds: &Dataset) -> Vec<String> {
    let mut a = vec![cmd.to_owned(), path.to_str().unwrap().to_owned()];
    a.extend(ds.cmd_args());
    a
}

fn peer_ok(c: &CTools, args: &[String]) -> common::PeerOutput {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    c.peer_ok(&a)
}

fn check_freelist(c: &CTools, path: &Path) {
    let out = c.peer(&["check-freelist", path.to_str().unwrap()]);
    assert_eq!(
        out.code, 0,
        "check-freelist:\n{}\n{}",
        out.stdout, out.stderr
    );
    assert!(out.stdout.contains("pages_leaked=0"), "{}", out.stdout);
}

fn fill(env: &Environment, ds: &Dataset) -> Dbi {
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(ds.db.as_deref(), ds.db_flags() | DatabaseFlags::CREATE)
        .unwrap();
    for i in 0..ds.count {
        let key = ds.key(i);
        for d in 0..ds.ndups(i) {
            txn.put(db, &key, &ds.value(i, d), WriteFlags::empty())
                .unwrap();
        }
    }
    txn.commit().unwrap();
    db
}

/// Compares the whole DB with a model (plain DB: key -> value).
fn assert_model(env: &Environment, db: Dbi, model: &BTreeMap<Vec<u8>, Vec<u8>>) {
    let txn = env.begin_ro_txn().unwrap();
    let walk = common::walk_forward(&txn, db);
    let expect: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(walk.len(), expect.len());
    assert!(walk == expect, "content differs from model");
    assert_eq!(txn.stat(db).unwrap().entries, model.len());
}

/// Random overwrites (same and other sizes, some overflow), deletes and
/// aborted transactions on a plain DB; the free list must stay exact.
#[test]
fn many_txns_overwrites_deletes_aborts() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(Some("churn"), DatabaseFlags::CREATE).unwrap();
    txn.commit().unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng(0x1234_5678);
    for t in 0..400 {
        let mut txn = env.begin_rw_txn().unwrap();
        let mut shadow = model.clone();
        let ops = 1 + rng.below(40);
        for _ in 0..ops {
            let k = format!("k{:05}", rng.below(3000)).into_bytes();
            match rng.below(10) {
                0..=5 => {
                    let len = match rng.below(20) {
                        0 => 3000 + rng.below(30000),
                        1..=3 => 200 + rng.below(800),
                        _ => 1 + rng.below(60),
                    };
                    let v: Vec<u8> = (0..len).map(|i| (i as u8) ^ (t as u8)).collect();
                    txn.put(db, &k, &v, WriteFlags::empty()).unwrap();
                    shadow.insert(k, v);
                }
                6..=8 => {
                    let found = txn.del(db, &k, None).unwrap();
                    assert_eq!(found, shadow.remove(&k).is_some());
                }
                _ => {
                    // Same-size overwrite through a cursor (MDB_CURRENT).
                    let mut cur = txn.cursor_mut(db).unwrap();
                    if let Some((ck, cv)) = cur.set_range(&k).unwrap() {
                        let (ck, len) = (ck.to_vec(), cv.len());
                        let v = vec![b'c'; len];
                        cur.put(&ck, &v, WriteFlags::CURRENT).unwrap();
                        shadow.insert(ck, v);
                    }
                }
            }
        }
        if t % 10 == 9 {
            txn.abort();
        } else {
            txn.commit().unwrap();
            model = shadow;
        }
        if t % 100 == 99 {
            check_freelist(c, dir.path());
        }
    }
    assert_model(&env, db, &model);
    check_freelist(c, dir.path());
    // mdb_dump agrees.
    let dump = c.dump(dir.path(), Some("churn"));
    let expect: Vec<_> = model.into_iter().collect();
    assert!(dump == expect, "mdb_dump differs");
}

/// Rust writes, C writes more into the same environment, Rust reads all of
/// it back and writes again; C verifies everything at the end.
#[test]
fn rust_and_c_take_turns() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let env = open_env(path);
    let r1 = Dataset::new(Some("r1"), 3000, 31)
        .with_flags(MDB_DUPSORT)
        .dups(20)
        .finish();
    let r2 = Dataset::new(Some("r2"), 400, 32).big(9000).finish();
    fill(&env, &r1);
    fill(&env, &r2);
    check_freelist(c, path);

    // C writes into the same environment while we keep it open.
    let c1 = Dataset::new(Some("c1"), 2000, 33)
        .with_flags(MDB_DUPFIXED)
        .dups(30)
        .valsize(8)
        .finish();
    let mut a = peer_args("fill", path, &c1);
    a.extend(["--per-txn".to_owned(), "300".to_owned()]);
    peer_ok(c, &a);
    let c2 = Dataset::new(Some("r2"), 400, 32).big(9000).finish();
    let mut a = peer_args("fill", path, &c2); // idempotent rewrite of r2
    a.push("--reserve".to_owned());
    peer_ok(c, &a);

    // Rust reads what C wrote.
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("c1")).unwrap();
    let mut n = 0;
    for i in 0..c1.count {
        let k = c1.key(i);
        let mut cur = txn.cursor(db).unwrap();
        cur.set(&k).unwrap().expect("key written by C");
        assert_eq!(cur.count().unwrap(), u64::from(c1.ndups(i)));
        n += c1.ndups(i) as usize;
    }
    assert_eq!(txn.stat(db).unwrap().entries, n);
    drop(txn);

    // Rust writes again: replace C's DB content (deleting C-written
    // sub-DBs and sub-pages), and rewrite r1.
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.open_db(Some("c1")).unwrap();
    for i in 0..c1.count {
        assert!(txn.del(db, &c1.key(i), None).unwrap());
    }
    assert_eq!(txn.stat(db).unwrap().entries, 0);
    txn.commit().unwrap();
    check_freelist(c, path);
    let mut txn = env.begin_rw_txn().unwrap();
    let extra = Dataset::new(Some("c1"), 2000, 33)
        .with_flags(MDB_DUPFIXED)
        .dups(45)
        .valsize(8)
        .finish();
    for i in 0..extra.count {
        let k = extra.key(i);
        for d in 0..extra.ndups(i) {
            txn.put(db, &k, &extra.value(i, d), WriteFlags::empty())
                .unwrap();
        }
    }
    txn.commit().unwrap();
    fill(&env, &r1);
    check_freelist(c, path);
    for ds in [&r1, &r2, &extra] {
        let out = peer_ok(c, &peer_args("verify", path, ds));
        assert!(out.stdout.starts_with("verified"), "{}", out.stdout);
    }
    // And C deletes half, Rust verifies the rest.
    let mut a = peer_args("delete", path, &r1);
    a.extend(["--every".to_owned(), "2".to_owned()]);
    peer_ok(c, &a);
    let mut a = peer_args("verify", path, &r1);
    a.extend(["--deleted-every".to_owned(), "2".to_owned()]);
    peer_ok(c, &a);
    // Rust deletes C's DB content key by key and puts it back.
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.open_db(Some("c1")).unwrap();
    for i in (0..extra.count).step_by(3) {
        assert!(txn.del(db, &extra.key(i), None).unwrap());
    }
    txn.commit().unwrap();
    check_freelist(c, path);
    let mut txn = env.begin_rw_txn().unwrap();
    for i in (0..extra.count).step_by(3) {
        let k = extra.key(i);
        for d in 0..extra.ndups(i) {
            txn.put(db, &k, &extra.value(i, d), WriteFlags::empty())
                .unwrap();
        }
    }
    txn.commit().unwrap();
    peer_ok(c, &peer_args("verify", path, &extra));
    check_freelist(c, path);
}

/// A C reader pins a snapshot while Rust deletes and rewrites everything in
/// many transactions. The reader still sees the old data (so Rust did not
/// reuse its pages); afterwards the pages are reused and accounted for.
#[test]
fn c_reader_pins_snapshot() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let env = open_env(path);
    let old = Dataset::new(Some("m"), 2000, 41).big(7000).finish();
    fill(&env, &old);

    let mut args = vec![
        "hold-reader".to_owned(),
        path.to_str().unwrap().to_owned(),
        "120".to_owned(),
        "--until-stdin".to_owned(),
    ];
    args.extend(old.cmd_args());
    let mut child = Command::new(c.cpeer())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert!(line.starts_with("READY"), "{line}");
    let held: u64 = line
        .split_whitespace()
        .find_map(|t| t.strip_prefix("txnid="))
        .unwrap()
        .parse()
        .unwrap();

    // Delete everything, then write a new data set, in many transactions.
    let db = env.open_db(Some("m")).unwrap();
    for chunk in (0..old.count).collect::<Vec<_>>().chunks(100) {
        let mut txn = env.begin_rw_txn().unwrap();
        for &i in chunk {
            assert!(txn.del(db, &old.key(i), None).unwrap());
        }
        txn.commit().unwrap();
    }
    let new = Dataset::new(Some("m"), 2500, 42).big(5000).finish();
    for chunk in (0..new.count).collect::<Vec<_>>().chunks(50) {
        let mut txn = env.begin_rw_txn().unwrap();
        for &i in chunk {
            txn.put(db, &new.key(i), &new.value(i, 0), WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    // The reader is still listed with its old snapshot.
    let readers = env.reader_list().unwrap();
    assert!(readers.iter().any(|r| r.txnid == Some(held)), "{readers:?}");
    let size_while_held = env.info().unwrap().last_pgno;

    // Release: the holder verifies the old data in its snapshot.
    child.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut out, &mut rest).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "holder failed: {rest}");
    assert!(rest.contains("verified"), "{rest}");

    check_freelist(c, path);
    peer_ok(c, &peer_args("verify", path, &new));

    // Now the old pages are reusable: rewriting everything does not grow
    // the file much.
    for chunk in (0..new.count).collect::<Vec<_>>().chunks(500) {
        let mut txn = env.begin_rw_txn().unwrap();
        for &i in chunk {
            txn.put(db, &new.key(i), &new.value(i, 0), WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    let after = env.info().unwrap().last_pgno;
    assert!(
        after <= size_while_held + size_while_held / 4,
        "file grew from {size_while_held} to {after} pages although pages were free"
    );
    check_freelist(c, path);
    peer_ok(c, &peer_args("verify", path, &new));
}

fn counter(txn: &impl Transaction, db: Dbi, name: &str) -> u64 {
    txn.get(db, name.as_bytes())
        .unwrap()
        .map_or(0, |v| std::str::from_utf8(v).unwrap().parse().unwrap())
}

/// A C writer loop (random puts/deletes, nested and aborted transactions)
/// and a Rust writer loop run at the same time on one environment; the
/// shared writer lock serialises them, so no increment of the shared
/// `#commits` counter is lost, and the result passes the C audit and the C
/// reader's consistency checks.
#[test]
fn concurrent_c_and_rust_writers() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    // Initialise the writer-loop DBs ("0 seconds" commits once).
    c.peer_ok(&[
        "writer-loop",
        &path,
        "w",
        "0",
        "1",
        "--nosync",
        "--mapsize",
        "268435456",
    ]);
    let env = open_env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let mine = txn.create_db(Some("rust"), DatabaseFlags::CREATE).unwrap();
    txn.commit().unwrap();
    let w = env.open_db(Some("w")).unwrap();

    let child = Command::new(c.cpeer())
        .args([
            "writer-loop",
            &path,
            "w",
            "3",
            "77",
            "--nosync",
            "--keys",
            "300",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    let mut rng = Rng(99);
    let mut mine_model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rust_commits = 0u64;
    while start.elapsed() < std::time::Duration::from_secs(3) {
        let mut txn = env.begin_rw_txn().unwrap();
        let version = counter(&txn, w, "#commits") + 1;
        let mut shadow = mine_model.clone();
        for _ in 0..1 + rng.below(10) {
            let k = format!("r{:04}", rng.below(500)).into_bytes();
            if rng.below(4) == 0 {
                txn.del(mine, &k, None).unwrap();
                shadow.remove(&k);
            } else {
                let max = if rng.below(20) == 0 { 30_000 } else { 300 };
                let v = vec![b'r'; 1 + rng.below(max)];
                txn.put(mine, &k, &v, WriteFlags::empty()).unwrap();
                shadow.insert(k, v);
            }
        }
        if rng.below(8) == 0 {
            txn.abort();
            continue;
        }
        txn.put(
            w,
            b"#commits",
            version.to_string().as_bytes(),
            WriteFlags::empty(),
        )
        .unwrap();
        txn.commit().unwrap();
        mine_model = shadow;
        rust_commits += 1;
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let c_commits: u64 = text
        .split_whitespace()
        .find_map(|t| t.strip_prefix("commits="))
        .unwrap()
        .parse()
        .unwrap();
    eprintln!("C commits {c_commits}, Rust commits {rust_commits}");
    assert!(c_commits > 0 && rust_commits > 0);
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(counter(&txn, w, "#commits"), 1 + c_commits + rust_commits);
    drop(txn);
    assert_model(&env, mine, &mine_model);
    check_freelist(c, dir.path());
    // The C reader's invariants (#count, #dups, value formats) hold.
    c.peer_ok(&["reader-loop", &path, "w", "0.5"]);
}

/// Two threads of this process start write transactions: the second waits
/// for the first to end.
#[test]
fn writers_in_one_process_take_turns() {
    let dir = tempfile::tempdir().unwrap();
    let env = std::sync::Arc::new(open_env(dir.path()));
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    txn.put(db, b"n", b"0", WriteFlags::empty()).unwrap();
    txn.commit().unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let env = env.clone();
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let mut txn = env.begin_rw_txn().unwrap();
                    let n: u64 = std::str::from_utf8(txn.get(db, b"n").unwrap().unwrap())
                        .unwrap()
                        .parse()
                        .unwrap();
                    txn.put(
                        db,
                        b"n",
                        (n + 1).to_string().as_bytes(),
                        WriteFlags::empty(),
                    )
                    .unwrap();
                    txn.commit().unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(db, b"n").unwrap(), Some(&b"200"[..]));
}
