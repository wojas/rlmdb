//! Soak test of the write path (ignored by default):
//!
//! ```sh
//! SOAK_SECS=120 cargo test -p lmdb --release --test write_soak -- --ignored --nocapture
//! ```
//!
//! Random transactions on a plain DB (values from 1 byte to several
//! overflow pages) and a DUPSORT DB (few to many duplicates per key, so
//! sub-pages grow, turn into sub-DBs and shrink again): puts, overwrites,
//! deletes through the API and through cursors, and aborted transactions,
//! for `SOAK_SECS` seconds (default 20). A model of the expected contents
//! is kept; every so often the C page audit (`cpeer check-freelist`) and a
//! full comparison with the model run, and at the end `mdb_dump` must agree
//! with the model too.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::Rng;
use lmdb_core::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, Transaction, WriteFlags};

type Plain = BTreeMap<Vec<u8>, Vec<u8>>;
type Dups = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

fn value(rng: &mut Rng, t: u64) -> Vec<u8> {
    let len = match rng.below(40) {
        0 => 20_000 + rng.below(60_000),
        1..=2 => 2_000 + rng.below(8_000),
        3..=8 => 100 + rng.below(1000),
        _ => 1 + rng.below(60),
    };
    (0..len).map(|i| (i as u64 ^ t) as u8).collect()
}

fn check(env: &Environment, plain_db: Dbi, dups_db: Dbi, plain: &Plain, dups: &Dups) {
    let txn = env.begin_ro_txn().unwrap();
    let walk = common::walk_forward(&txn, plain_db);
    assert_eq!(walk.len(), plain.len());
    assert!(walk
        .iter()
        .zip(plain)
        .all(|((k, v), (mk, mv))| k == mk && v == mv));
    let walk = common::walk_forward(&txn, dups_db);
    let expect: Vec<(Vec<u8>, Vec<u8>)> = dups
        .iter()
        .flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone())))
        .collect();
    assert_eq!(walk.len(), expect.len());
    assert!(walk == expect, "dups DB differs from model");
    assert_eq!(txn.stat(dups_db).unwrap().entries, expect.len());
}

#[test]
#[ignore]
fn soak() {
    let c = require_c!();
    let secs: u64 = std::env::var("SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let seed: u64 = std::env::var("SOAK_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    let env = Environment::builder()
        .map_size(1 << 30)
        .max_dbs(4)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let plain_db = txn.create_db(Some("plain"), DatabaseFlags::CREATE).unwrap();
    let dups_db = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    txn.commit().unwrap();

    let mut rng = Rng(seed | 1);
    let mut plain = Plain::new();
    let mut dups = Dups::new();
    let start = Instant::now();
    let (mut commits, mut aborts, mut ops) = (0u64, 0u64, 0u64);
    let mut t = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        t += 1;
        let mut txn = env.begin_rw_txn().unwrap();
        let (mut p2, mut d2) = (plain.clone(), dups.clone());
        for _ in 0..1 + rng.below(80) {
            ops += 1;
            let k = format!("k{:05}", rng.below(4000)).into_bytes();
            let dk = format!("d{:03}", rng.below(150)).into_bytes();
            match rng.below(16) {
                0..=4 => {
                    let v = value(&mut rng, t);
                    txn.put(plain_db, &k, &v, WriteFlags::empty()).unwrap();
                    p2.insert(k, v);
                }
                5..=6 => {
                    assert_eq!(
                        txn.del(plain_db, &k, None).unwrap(),
                        p2.remove(&k).is_some()
                    );
                }
                7..=10 => {
                    let max = if rng.below(10) == 0 { 400 } else { 8 };
                    let n = rng.below(max);
                    for _ in 0..=n {
                        let v = format!("{:06}", rng.below(5000)).into_bytes();
                        txn.put(dups_db, &dk, &v, WriteFlags::empty()).unwrap();
                        d2.entry(dk.clone()).or_default().insert(v);
                    }
                }
                11 => {
                    let found = txn.del(dups_db, &dk, None).unwrap();
                    assert_eq!(found, d2.remove(&dk).is_some());
                }
                12..=13 => {
                    // Delete some duplicates of one key through a cursor.
                    let mut cur = txn.cursor_mut(dups_db).unwrap();
                    if cur.set_key(&dk).unwrap().is_some() {
                        let n = 1 + rng.below(30);
                        for _ in 0..n {
                            let Some((ck, cv)) = cur.get_current().unwrap() else {
                                break;
                            };
                            if ck != &dk[..] {
                                break;
                            }
                            let cv = cv.to_vec();
                            cur.del(WriteFlags::empty()).unwrap();
                            let set = d2.get_mut(&dk).unwrap();
                            set.remove(&cv);
                            if set.is_empty() {
                                d2.remove(&dk);
                                break;
                            }
                        }
                    }
                }
                _ => {
                    // Walk and rewrite a stretch of plain values in place.
                    let mut cur = txn.cursor_mut(plain_db).unwrap();
                    let mut pos = cur
                        .set_range(&k)
                        .unwrap()
                        .map(|(a, b)| (a.to_vec(), b.len()));
                    for _ in 0..rng.below(20) {
                        let Some((ck, len)) = pos else { break };
                        let v = vec![b'w'; len];
                        cur.put(&ck, &v, WriteFlags::CURRENT).unwrap();
                        p2.insert(ck, v);
                        pos = cur.next().unwrap().map(|(a, b)| (a.to_vec(), b.len()));
                    }
                }
            }
        }
        if rng.below(10) == 0 {
            txn.abort();
            aborts += 1;
        } else {
            txn.commit().unwrap();
            plain = p2;
            dups = d2;
            commits += 1;
        }
        if t % 250 == 0 {
            check(&env, plain_db, dups_db, &plain, &dups);
            let out = c.peer(&["check-freelist", &path]);
            assert_eq!(
                out.code, 0,
                "after txn {t}:\n{}\n{}",
                out.stdout, out.stderr
            );
        }
    }
    check(&env, plain_db, dups_db, &plain, &dups);
    let out = c.peer(&["check-freelist", &path]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let dump = c.dump(dir.path(), Some("plain"));
    assert!(
        dump.into_iter().eq(plain.clone()),
        "mdb_dump of plain differs"
    );
    let info = env.info().unwrap();
    eprintln!(
        "soak: {commits} commits, {aborts} aborts, {ops} ops in {:?}; {} pages, {} free; {} plain keys, {} dup keys",
        start.elapsed(),
        info.last_pgno + 1,
        env.freelist().unwrap(),
        plain.len(),
        dups.len()
    );
}
