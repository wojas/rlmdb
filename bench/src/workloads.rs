//! The Rust side of every workload: line-by-line twins of the groups in
//! `c/cbench.c`, on `lmdb_core`'s byte-level API. Each group runs one
//! warm-up iteration and `reps` timed ones per workload and prints
//! `RESULT <name> <ns>/<ops> ...` lines (see `main.rs`).

use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lmdb_core::{
    DatabaseFlags, Dbi, Environment, EnvironmentFlags, Result, RwTxn, Transaction, WriteFlags,
};

use crate::data::*;

const MAPSIZE: usize = 4 << 30;
const BATCH: usize = 10_000;
const OVF_BATCH: usize = 1_000;
const DUPS: usize = 10;
const READERS: usize = 8;
const MIXED_READERS: usize = 4;
const MIXED_BATCH: usize = 100;

/// Parameters of a worker run (the command line of `cbench`).
pub struct Params<'a> {
    pub dir: &'a Path,
    pub n: usize,
    pub reps: usize,
    pub sync_commits: usize,
}

/// Timed samples of one workload.
#[derive(Default)]
struct Samples(Vec<(u64, u64)>);

impl Samples {
    /// Records iteration `it` (0 is the warm-up, not recorded).
    fn record(&mut self, it: usize, elapsed: Duration, ops: usize) {
        if it > 0 {
            self.0.push((elapsed.as_nanos() as u64, ops as u64));
        }
    }

    fn report(&self, name: &str) {
        let pairs: Vec<String> = self
            .0
            .iter()
            .map(|(ns, ops)| format!("{ns}/{ops}"))
            .collect();
        println!("RESULT {name} {}", pairs.join(" "));
    }
}

/// Runs workload group `group`.
pub fn run(group: &str, p: &Params<'_>) -> Result<()> {
    let pool = pool();
    match group {
        "seq_put" => put_group("seq_put_append", p, &pool, true),
        "rand_put" => put_group("rand_put", p, &pool, false),
        "read" => read_group(p, &pool),
        "overwrite" => overwrite_group(p, &pool),
        "dupsort" => dupsort_group(p, &pool),
        "overflow" => overflow_group(p, &pool),
        "delete" => delete_group(p, &pool),
        "small_txn" => small_txn_group(p, &pool),
        "mixed" => mixed_group(p, &pool),
        "datacheck" => {
            println!("DATA {:016x}", datacheck(p.n));
            return Ok(());
        }
        _ => {
            eprintln!("lmdb-bench: unknown group {group}");
            std::process::exit(2);
        }
    }?;
    reset_dir(p.dir);
    let _ = std::fs::remove_dir(p.dir);
    Ok(())
}

fn reset_dir(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the environment directory");
    for f in ["data.mdb", "lock.mdb"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
}

fn open_env(dir: &Path, flags: EnvironmentFlags) -> Result<Environment> {
    Environment::builder()
        .map_size(MAPSIZE)
        .max_dbs(4)
        .max_readers(126)
        .flags(flags)
        .open(dir)
}

fn open_dbi(env: &Environment, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi> {
    let mut txn = env.begin_rw_txn()?;
    let db = txn.create_db(name, flags | DatabaseFlags::CREATE)?;
    txn.commit()?;
    Ok(db)
}

/// `put_keys` of cbench.c: puts `keys[order[j]]` (or `keys[j]`) for
/// `j < n`, `vlen`-byte values at `val_off(i + vsalt)`, committing every
/// `batch` puts.
#[allow(clippy::too_many_arguments)]
fn put_keys(
    env: &Environment,
    db: Dbi,
    pool: &[u8],
    keys: &[[u8; KEYLEN]],
    order: Option<&[u32]>,
    n: usize,
    vlen: usize,
    vsalt: u64,
    flags: WriteFlags,
    batch: usize,
) -> Result<()> {
    let mut txn: Option<RwTxn<'_>> = None;
    for j in 0..n {
        let i = order.map_or(j, |o| o[j] as usize);
        let t = match &mut txn {
            Some(t) => t,
            None => txn.insert(env.begin_rw_txn()?),
        };
        t.put(db, &keys[i], value(pool, i as u64 + vsalt, vlen), flags)?;
        if (j + 1) % batch == 0 {
            if let Some(t) = txn.take() {
                t.commit()?;
            }
        }
    }
    if let Some(t) = txn {
        t.commit()?;
    }
    Ok(())
}

/// A fresh environment holding the `rand_put` data set.
fn build_rand(dir: &Path, pool: &[u8], keys: &[[u8; KEYLEN]]) -> Result<(Environment, Dbi)> {
    reset_dir(dir);
    let env = open_env(dir, EnvironmentFlags::NO_SYNC)?;
    let db = open_dbi(&env, None, DatabaseFlags::empty())?;
    put_keys(
        &env,
        db,
        pool,
        keys,
        None,
        keys.len(),
        VALLEN,
        0,
        WriteFlags::empty(),
        BATCH,
    )?;
    Ok((env, db))
}

fn put_group(name: &str, p: &Params<'_>, pool: &[u8], seq: bool) -> Result<()> {
    let keys = if seq { seq_keys(p.n) } else { rand_keys(p.n) };
    let flags = if seq {
        WriteFlags::APPEND
    } else {
        WriteFlags::empty()
    };
    let mut s = Samples::default();
    for it in 0..=p.reps {
        reset_dir(p.dir);
        let env = open_env(p.dir, EnvironmentFlags::NO_SYNC)?;
        let db = open_dbi(&env, None, DatabaseFlags::empty())?;
        let t0 = Instant::now();
        put_keys(&env, db, pool, &keys, None, p.n, VALLEN, 0, flags, BATCH)?;
        s.record(it, t0.elapsed(), p.n);
    }
    s.report(name);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn get_loop(
    env: &Environment,
    db: Dbi,
    keys: &[[u8; KEYLEN]],
    idx: &[u32],
    touch_end: bool,
    reps: usize,
    s: &mut Samples,
) -> Result<()> {
    for it in 0..=reps {
        let txn = env.begin_ro_txn()?;
        let mut sum = 0u64;
        let t0 = Instant::now();
        for &i in idx {
            let v = txn.get(db, &keys[i as usize])?.expect("key present");
            sum += v.len() as u64 + u64::from(v[0]);
            if touch_end {
                sum += u64::from(v[v.len() - 1]);
            }
        }
        s.record(it, t0.elapsed(), idx.len());
        black_box(sum);
    }
    Ok(())
}

fn scan(env: &Environment, db: Dbi, forward: bool, reps: usize, s: &mut Samples) -> Result<()> {
    for it in 0..=reps {
        let txn = env.begin_ro_txn()?;
        let (mut cnt, mut sum) = (0usize, 0u64);
        let t0 = Instant::now();
        let mut cur = txn.cursor(db)?;
        loop {
            let e = if forward { cur.next()? } else { cur.prev()? };
            let Some((k, v)) = e else { break };
            cnt += 1;
            sum += u64::from(k[0]) + v.len() as u64;
        }
        s.record(it, t0.elapsed(), cnt);
        black_box(sum);
    }
    Ok(())
}

fn read_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    let keys = rand_keys(p.n);
    let idx = rand_idx(S_GET, p.n, p.n);
    let (env, db) = build_rand(p.dir, pool, &keys)?;

    let mut s = Samples::default();
    get_loop(&env, db, &keys, &idx, false, p.reps, &mut s)?;
    s.report("rand_get");
    let mut s = Samples::default();
    scan(&env, db, true, p.reps, &mut s)?;
    s.report("scan_fwd");
    let mut s = Samples::default();
    scan(&env, db, false, p.reps, &mut s)?;
    s.report("scan_bwd");

    let ridx: Vec<Vec<u32>> = (0..READERS as u64)
        .map(|t| rand_idx(S_READER + (t << 48), p.n, p.n))
        .collect();
    let mut s = Samples::default();
    for it in 0..=p.reps {
        let start = AtomicBool::new(false);
        let elapsed = std::thread::scope(|sc| -> Result<Duration> {
            let handles: Vec<_> = ridx
                .iter()
                .map(|idx| {
                    let (env, keys, start) = (&env, &keys, &start);
                    sc.spawn(move || -> Result<()> {
                        let txn = env.begin_ro_txn()?;
                        while !start.load(Ordering::SeqCst) {
                            std::hint::spin_loop();
                        }
                        let mut sum = 0u64;
                        for &i in idx {
                            let v = txn.get(db, &keys[i as usize])?.expect("key present");
                            sum += v.len() as u64 + u64::from(v[0]);
                        }
                        black_box(sum);
                        Ok(())
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_millis(10));
            let t0 = Instant::now();
            start.store(true, Ordering::SeqCst);
            for h in handles {
                h.join().expect("reader thread")?;
            }
            Ok(t0.elapsed())
        })?;
        s.record(it, elapsed, READERS * p.n);
    }
    s.report("readers8_get");
    Ok(())
}

fn overwrite_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    let keys = rand_keys(p.n);
    let order = perm(S_OVERWRITE, p.n);
    let (env, db) = build_rand(p.dir, pool, &keys)?;
    let mut s = Samples::default();
    for it in 0..=p.reps {
        let t0 = Instant::now();
        put_keys(
            &env,
            db,
            pool,
            &keys,
            Some(&order),
            p.n,
            VALLEN,
            it as u64 + 1,
            WriteFlags::empty(),
            BATCH,
        )?;
        s.record(it, t0.elapsed(), p.n);
    }
    s.report("rand_overwrite");
    Ok(())
}

fn dupsort_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    let nk = p.n / DUPS;
    let keys = rand_keys(nk);
    let gi = rand_idx(S_DUP_GET, p.n, nk);
    let gj = rand_idx(S_DUP_GET ^ 1, p.n, DUPS);
    let order = perm(S_DUP_SCAN, nk);
    let mut buf = [0u8; VALLEN];
    let mut env_db = None;

    let mut s = Samples::default();
    for it in 0..=p.reps {
        drop(env_db.take());
        reset_dir(p.dir);
        let env = open_env(p.dir, EnvironmentFlags::NO_SYNC)?;
        let db = open_dbi(&env, Some("dups"), DatabaseFlags::DUP_SORT)?;
        let t0 = Instant::now();
        put_dups(&env, db, pool, &keys, &mut buf)?;
        s.record(it, t0.elapsed(), nk * DUPS);
        env_db = Some((env, db));
    }
    s.report("dup_put");
    let (env, db) = env_db.expect("reps >= 1");

    let mut s = Samples::default();
    for it in 0..=p.reps {
        let txn = env.begin_ro_txn()?;
        let mut cur = txn.cursor(db)?;
        let mut sum = 0u64;
        let t0 = Instant::now();
        for (&i, &d) in gi.iter().zip(&gj) {
            dup_data(pool, u64::from(i), u64::from(d), &mut buf);
            let (_, v) = cur
                .get_both(&keys[i as usize], &buf)?
                .expect("pair present");
            sum += v.len() as u64;
        }
        s.record(it, t0.elapsed(), p.n);
        black_box(sum);
    }
    s.report("dup_get_both");

    let mut s = Samples::default();
    for it in 0..=p.reps {
        let txn = env.begin_ro_txn()?;
        let mut cur = txn.cursor(db)?;
        let (mut cnt, mut sum) = (0usize, 0u64);
        let t0 = Instant::now();
        for &i in &order {
            let (_, mut v) = cur.set(&keys[i as usize])?.expect("key present");
            loop {
                cnt += 1;
                sum += u64::from(v[0]);
                match cur.next_dup()? {
                    Some((_, nv)) => v = nv,
                    None => break,
                }
            }
        }
        s.record(it, t0.elapsed(), cnt);
        black_box(sum);
    }
    s.report("dup_next_dup");
    Ok(())
}

/// The puts of `dup_put`: `DUPS` duplicates per key, key-major, committing
/// every `BATCH` puts.
fn put_dups(
    env: &Environment,
    db: Dbi,
    pool: &[u8],
    keys: &[[u8; KEYLEN]],
    buf: &mut [u8; VALLEN],
) -> Result<()> {
    let mut txn: Option<RwTxn<'_>> = None;
    let mut puts = 0usize;
    for (i, key) in keys.iter().enumerate() {
        for d in 0..DUPS as u64 {
            let t = match &mut txn {
                Some(t) => t,
                None => txn.insert(env.begin_rw_txn()?),
            };
            dup_data(pool, i as u64, d, buf);
            t.put(db, key, &buf[..], WriteFlags::empty())?;
            puts += 1;
            if puts % BATCH == 0 {
                if let Some(t) = txn.take() {
                    t.commit()?;
                }
            }
        }
    }
    if let Some(t) = txn {
        t.commit()?;
    }
    Ok(())
}

fn overflow_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    for (vlen, div, put_name, get_name) in [
        (4096, 20, "put_4k", "get_4k"),
        (65536, 200, "put_64k", "get_64k"),
    ] {
        let cnt = p.n / div;
        let keys = rand_keys(cnt);
        let idx = rand_idx(S_OVF_GET, p.n, cnt);
        let mut env_db = None;
        let mut s = Samples::default();
        for it in 0..=p.reps {
            drop(env_db.take());
            reset_dir(p.dir);
            let env = open_env(p.dir, EnvironmentFlags::NO_SYNC)?;
            let db = open_dbi(&env, None, DatabaseFlags::empty())?;
            let t0 = Instant::now();
            put_keys(
                &env,
                db,
                pool,
                &keys,
                None,
                cnt,
                vlen,
                0,
                WriteFlags::empty(),
                OVF_BATCH,
            )?;
            s.record(it, t0.elapsed(), cnt);
            env_db = Some((env, db));
        }
        s.report(put_name);
        let (env, db) = env_db.expect("reps >= 1");
        let mut s = Samples::default();
        get_loop(&env, db, &keys, &idx, true, p.reps, &mut s)?;
        s.report(get_name);
    }
    Ok(())
}

fn delete_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    let keys = rand_keys(p.n);
    let order = perm(S_DELETE, p.n);
    let mut s = Samples::default();
    for it in 0..=p.reps {
        let (env, db) = build_rand(p.dir, pool, &keys)?;
        let t0 = Instant::now();
        let mut txn: Option<RwTxn<'_>> = None;
        for (j, &i) in order.iter().enumerate() {
            let t = match &mut txn {
                Some(t) => t,
                None => txn.insert(env.begin_rw_txn()?),
            };
            assert!(t.del(db, &keys[i as usize], None)?, "key present");
            if (j + 1) % BATCH == 0 {
                if let Some(t) = txn.take() {
                    t.commit()?;
                }
            }
        }
        if let Some(t) = txn {
            t.commit()?;
        }
        s.record(it, t0.elapsed(), p.n);
    }
    s.report("del_rand");
    Ok(())
}

fn small_txn_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    for (name, flags) in [
        ("txn_nosync", EnvironmentFlags::NO_SYNC),
        ("txn_sync", EnvironmentFlags::empty()),
    ] {
        let cnt = if flags.is_empty() {
            p.sync_commits
        } else {
            p.n / 20
        };
        let keys = rand_keys(cnt);
        let mut s = Samples::default();
        for it in 0..=p.reps {
            reset_dir(p.dir);
            let env = open_env(p.dir, flags)?;
            let db = open_dbi(&env, None, DatabaseFlags::empty())?;
            let t0 = Instant::now();
            for (j, key) in keys.iter().enumerate() {
                let mut txn = env.begin_rw_txn()?;
                txn.put(db, key, value(pool, j as u64, VALLEN), WriteFlags::empty())?;
                txn.commit()?;
            }
            s.record(it, t0.elapsed(), cnt);
        }
        s.report(name);
    }
    Ok(())
}

fn mixed_group(p: &Params<'_>, pool: &[u8]) -> Result<()> {
    let keys = rand_keys(p.n);
    let commits = p.n / 2000;
    let puts = commits * MIXED_BATCH;
    let idx = rand_idx(S_MIXED, puts, p.n);
    let (env, db) = build_rand(p.dir, pool, &keys)?;
    let (mut sw, mut sr) = (Samples::default(), Samples::default());
    for it in 0..=p.reps {
        let start = AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        let (elapsed, entries) = std::thread::scope(|sc| -> Result<(Duration, usize)> {
            let handles: Vec<_> = (0..MIXED_READERS)
                .map(|_| {
                    let (env, start, stop) = (&env, &start, &stop);
                    sc.spawn(move || -> Result<usize> {
                        while !start.load(Ordering::SeqCst) {
                            std::hint::spin_loop();
                        }
                        let mut entries = 0usize;
                        while !stop.load(Ordering::Relaxed) {
                            let txn = env.begin_ro_txn()?;
                            let mut cur = txn.cursor(db)?;
                            while cur.next()?.is_some() {
                                entries += 1;
                                if entries & 1023 == 0 && stop.load(Ordering::Relaxed) {
                                    break;
                                }
                            }
                        }
                        Ok(entries)
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_millis(10));
            let t0 = Instant::now();
            start.store(true, Ordering::SeqCst);
            let wrote = || -> Result<()> {
                for c in 0..commits {
                    let mut txn = env.begin_rw_txn()?;
                    for &i in &idx[c * MIXED_BATCH..(c + 1) * MIXED_BATCH] {
                        let i = i as usize;
                        let v = value(pool, (i + c + 1) as u64, VALLEN);
                        txn.put(db, &keys[i], v, WriteFlags::empty())?;
                    }
                    txn.commit()?;
                }
                Ok(())
            };
            let rc = wrote();
            let elapsed = t0.elapsed();
            stop.store(true, Ordering::SeqCst);
            let mut entries = 0;
            for h in handles {
                entries += h.join().expect("scanner thread")?;
            }
            rc?;
            Ok((elapsed, entries))
        })?;
        sw.record(it, elapsed, puts);
        sr.record(it, elapsed, entries);
    }
    sw.report("mixed_writer");
    sr.report("mixed_readers");
    Ok(())
}
