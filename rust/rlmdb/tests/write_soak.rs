//! Soak test of the write path (ignored by default):
//!
//! ```sh
//! SOAK_SECS=120 cargo test -p rlmdb --release --test write_soak -- --ignored --nocapture
//! ```
//!
//! Random transactions on a plain DB (values from 1 byte to several
//! overflow pages) and a DUPSORT DB (few to many duplicates per key, so
//! sub-pages grow, turn into sub-DBs and shrink again): puts, overwrites,
//! deletes through the API and through cursors, nested transactions (up to
//! three deep, committed or aborted), a scratch DB that is emptied
//! (`clear_db`) or deleted and created again (`drop_db`), and aborted
//! transactions, for `SOAK_SECS` seconds (default 20). A model of the
//! expected contents is kept; every so often the C page audit (`cpeer
//! check-freelist`) and a full comparison with the model run, and at the
//! end `mdb_dump` must agree with the model too.
//!
//! With `SOAK_SPILL=1` the run starts with transactions that dirty more
//! pages than a transaction can hold in memory (`MDB_IDL_UM_MAX`), so they
//! spill: half a GiB of data with 4 KiB pages, 2 GiB with 16 KiB pages.
//! (Spilling under a lowered limit, with nested transactions, is soaked by
//! the crate's own `core::spill::tests::soak` unit test.)

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::Rng;
use rlmdb::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, RwTxn, Transaction, WriteFlags};

type Plain = BTreeMap<Vec<u8>, Vec<u8>>;
type Dups = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

/// The expected contents of the three databases.
#[derive(Clone, Default)]
struct Model {
    plain: Plain,
    dups: Dups,
    scratch: Plain,
}

#[derive(Default)]
struct Counts {
    commits: u64,
    aborts: u64,
    nested: u64,
    drops: u64,
    ops: u64,
}

fn value(rng: &mut Rng, t: u64) -> Vec<u8> {
    let len = match rng.below(40) {
        0 => 20_000 + rng.below(60_000),
        1..=2 => 2_000 + rng.below(8_000),
        3..=8 => 100 + rng.below(1000),
        _ => 1 + rng.below(60),
    };
    (0..len).map(|i| (i as u64 ^ t) as u8).collect()
}

fn check(env: &Environment, plain_db: Dbi, dups_db: Dbi, m: &Model) {
    let (plain, dups) = (&m.plain, &m.dups);
    let txn = env.begin_ro_txn().unwrap();
    match txn.open_db(Some("scratch")) {
        Ok(db) => {
            let walk = common::walk_forward(&txn, db);
            assert!(walk.into_iter().eq(m.scratch.clone()), "scratch DB differs");
        }
        Err(e) if e.not_found() => assert!(m.scratch.is_empty()),
        Err(e) => panic!("{e:?}"),
    }
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
        .map_size(if std::env::var("SOAK_SPILL").is_ok_and(|v| v == "1") {
            6 << 30
        } else {
            1 << 30
        })
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
    let mut model = Model::default();
    let start = Instant::now();
    let mut n = Counts::default();
    if std::env::var("SOAK_SPILL").is_ok_and(|v| v == "1") {
        spill_txns(&env, plain_db, &mut model);
        check(&env, plain_db, dups_db, &model);
        let out = c.peer(&["check-freelist", &path]);
        assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    }
    let mut t = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        t += 1;
        let mut txn = env.begin_rw_txn().unwrap();
        let mut m2 = model.clone();
        run_txn(&mut txn, &mut rng, t, plain_db, dups_db, &mut m2, 0, &mut n);
        if rng.below(10) == 0 {
            txn.abort();
            n.aborts += 1;
        } else {
            txn.commit().unwrap();
            model = m2;
            n.commits += 1;
        }
        if t % 250 == 0 {
            check(&env, plain_db, dups_db, &model);
            let out = c.peer(&["check-freelist", &path]);
            assert_eq!(
                out.code, 0,
                "after txn {t}:\n{}\n{}",
                out.stdout, out.stderr
            );
        }
    }
    check(&env, plain_db, dups_db, &model);
    let out = c.peer(&["check-freelist", &path]);
    assert_eq!(out.code, 0, "{}\n{}", out.stdout, out.stderr);
    let dump = c.dump(dir.path(), Some("plain"));
    assert!(
        dump.into_iter().eq(model.plain.clone()),
        "mdb_dump of plain differs"
    );
    let info = env.info().unwrap();
    eprintln!(
        "soak: {} commits, {} aborts, {} nested, {} drops, {} ops in {:?}; {} pages, {} free; {} plain keys, {} dup keys",
        n.commits,
        n.aborts,
        n.nested,
        n.drops,
        n.ops,
        start.elapsed(),
        info.last_pgno + 1,
        env.freelist().unwrap(),
        model.plain.len(),
        model.dups.len()
    );
}

/// Transactions that spill: overflow values of about one page each, more
/// of them than the dirty list holds, then all of them deleted again.
fn spill_txns(env: &Environment, plain_db: Dbi, m: &mut Model) {
    let ps = env.page_size();
    let n = rlmdb::consts::MDB_IDL_UM_MAX as u32 + 20_000;
    let started = Instant::now();
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 0..n {
        let k = format!("s{i:07}").into_bytes();
        let v = vec![(i % 251) as u8; ps - 100];
        txn.put(plain_db, &k, &v, WriteFlags::empty()).unwrap();
        m.plain.insert(k, v);
    }
    txn.commit().unwrap();
    // Delete them again in two steps (the second large transaction frees
    // what the first one reclaims; the model stays small for the rest).
    for start in [0, 1] {
        let mut txn = env.begin_rw_txn().unwrap();
        for i in (start..n).step_by(2) {
            let k = format!("s{i:07}").into_bytes();
            assert!(txn.del(plain_db, &k, None).unwrap());
            m.plain.remove(&k);
        }
        txn.commit().unwrap();
    }
    eprintln!("soak: spilling transactions took {:?}", started.elapsed());
}

/// The ops of one transaction at nesting depth `depth`, applied to the
/// model `m` too.
#[allow(clippy::too_many_arguments)]
fn run_txn(
    txn: &mut RwTxn<'_>,
    rng: &mut Rng,
    t: u64,
    plain_db: Dbi,
    dups_db: Dbi,
    m: &mut Model,
    depth: usize,
    n: &mut Counts,
) {
    for _ in 0..1 + rng.below(80) {
        n.ops += 1;
        let k = format!("k{:05}", rng.below(4000)).into_bytes();
        let dk = format!("d{:03}", rng.below(150)).into_bytes();
        match rng.below(18) {
            0..=4 => {
                let v = value(rng, t);
                txn.put(plain_db, &k, &v, WriteFlags::empty()).unwrap();
                m.plain.insert(k, v);
            }
            5..=6 => {
                assert_eq!(
                    txn.del(plain_db, &k, None).unwrap(),
                    m.plain.remove(&k).is_some()
                );
            }
            7..=10 => {
                let max = if rng.below(10) == 0 { 400 } else { 8 };
                let cnt = rng.below(max);
                for _ in 0..=cnt {
                    let v = format!("{:06}", rng.below(5000)).into_bytes();
                    txn.put(dups_db, &dk, &v, WriteFlags::empty()).unwrap();
                    m.dups.entry(dk.clone()).or_default().insert(v);
                }
            }
            11 => {
                let found = txn.del(dups_db, &dk, None).unwrap();
                assert_eq!(found, m.dups.remove(&dk).is_some());
            }
            12..=13 => {
                // Delete some duplicates of one key through a cursor.
                let mut cur = txn.cursor_mut(dups_db).unwrap();
                if cur.set_key(&dk).unwrap().is_some() {
                    let cnt = 1 + rng.below(30);
                    for _ in 0..cnt {
                        let Some((ck, cv)) = cur.get_current().unwrap() else {
                            break;
                        };
                        if ck != &dk[..] {
                            break;
                        }
                        let cv = cv.to_vec();
                        cur.del(WriteFlags::empty()).unwrap();
                        let set = m.dups.get_mut(&dk).unwrap();
                        set.remove(&cv);
                        if set.is_empty() {
                            m.dups.remove(&dk);
                            break;
                        }
                    }
                }
            }
            14 => {
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
                    m.plain.insert(ck, v);
                    pos = cur.next().unwrap().map(|(a, b)| (a.to_vec(), b.len()));
                }
            }
            15 => {
                // The scratch DB: fill, empty, or delete it. (The handle is
                // looked up every time: dropping it in a child that aborts
                // closes the handle but keeps the DB.)
                let s = txn
                    .create_db(Some("scratch"), DatabaseFlags::CREATE)
                    .unwrap();
                match rng.below(8) {
                    0 => {
                        txn.clear_db(s).unwrap();
                        m.scratch.clear();
                        n.drops += 1;
                    }
                    1 => {
                        txn.drop_db(s).unwrap();
                        m.scratch.clear();
                        n.drops += 1;
                    }
                    _ => {
                        for _ in 0..rng.below(200) {
                            let sk = format!("s{:04}", rng.below(3000)).into_bytes();
                            let v = value(rng, t);
                            txn.put(s, &sk, &v, WriteFlags::empty()).unwrap();
                            m.scratch.insert(sk, v);
                        }
                    }
                }
            }
            _ if depth < 3 => {
                // A nested transaction, committed or aborted.
                n.nested += 1;
                let mut child = txn.begin_nested_txn().unwrap();
                let mut m2 = m.clone();
                run_txn(&mut child, rng, t, plain_db, dups_db, &mut m2, depth + 1, n);
                if rng.below(3) == 0 {
                    child.abort();
                } else {
                    child.commit().unwrap();
                    *m = m2;
                }
            }
            _ => {}
        }
    }
}
