//! Both data-file formats in one process (`DUAL_FORMAT.md` §4.5): an LMDB
//! 0.9 and an LMDB 1.0 environment (of another page size) open at the same
//! time, written by a thread each while other threads read both and one
//! holds a snapshot of each. Nothing may leak from one environment into the
//! other: each keeps its format, page size, key size limit and lock-file
//! version, its readers see consistent snapshots, and its contents are what
//! its writer committed, also after reopening without a format request.
//!
//! The same with C peers on each environment from other processes is
//! `interop/tests/mixed_formats.rs` (`rpeer formats-loop`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rlmdb::format::lockfile::{LOCK_V09, LOCK_V10};
use rlmdb::{
    sniff_format, DatabaseFlags, Environment, EnvironmentFlags, Format, RoTxn, RwTxn, Transaction,
    WriteFlags,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-mixed-")
        .tempdir()
        .unwrap()
}

/// What a writer committed: `data` key -> value, `dups` key -> values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Model {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
    dups: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
}

/// Transactions per writer.
const TXNS: u32 = 300;

fn key(i: u32) -> Vec<u8> {
    format!("k{:05}", i % 700).into_bytes()
}

/// A value of `len` bytes that names `i`.
fn value(i: u32, len: usize) -> Vec<u8> {
    let mut v = format!("{i}:").into_bytes();
    v.resize(len, b'a' + (i % 26) as u8);
    v
}

/// One transaction's writes to `txn` and `m`: a put (every 17th an
/// overflow value of two pages and more), a delete, a duplicate, and the
/// `#count` and `#txn` counters.
fn ops(txn: &mut RwTxn<'_>, m: &mut Model, i: u32, page_size: usize) {
    let data = txn.create_db(Some("data"), DatabaseFlags::CREATE).unwrap();
    let dups = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    let len = if i % 17 == 0 {
        2 * page_size + i as usize
    } else {
        8 + (i as usize * 31) % 120
    };
    let (k, v) = (key(i.wrapping_mul(7919)), value(i, len));
    txn.put(data, &k, &v, WriteFlags::empty()).unwrap();
    m.data.insert(k, v);
    let gone = key(i.wrapping_mul(104_729));
    assert_eq!(
        txn.del(data, &gone, None).unwrap(),
        m.data.remove(&gone).is_some()
    );
    let (dk, dv) = (format!("d{:02}", i % 50).into_bytes(), i.to_be_bytes());
    txn.put(dups, &dk, &dv, WriteFlags::empty()).unwrap();
    m.dups.entry(dk).or_default().insert(dv.to_vec());
    let main = txn.open_db(None).unwrap();
    txn.put(
        main,
        b"#count",
        m.data.len().to_string().as_bytes(),
        WriteFlags::empty(),
    )
    .unwrap();
    // A nested transaction's writes count as its parent's.
    let t = (i % 1_000_000).to_string();
    txn.put(main, b"#txn", t.as_bytes(), WriteFlags::empty())
        .unwrap();
}

/// The writer of one environment holding `model`: [`TXNS`] transactions,
/// every 9th aborted, every 5th with a nested transaction on top
/// (committed or aborted). Returns what the environment holds then.
fn writer(env: &Environment, mut model: Model) -> Model {
    for i in 1..=TXNS {
        let mut next = model.clone();
        let mut txn = env.begin_rw_txn().unwrap();
        ops(&mut txn, &mut next, i, env.page_size());
        if i % 5 == 0 {
            let mut scratch = next.clone();
            let mut child = txn.begin_nested_txn().unwrap();
            ops(&mut child, &mut scratch, i + 1_000_000, env.page_size());
            if i % 10 == 0 {
                child.commit().unwrap();
                next = scratch;
            } else {
                child.abort();
            }
        }
        if i % 9 == 0 {
            txn.abort();
        } else {
            txn.commit().unwrap();
            model = next;
        }
    }
    model
}

fn counter(txn: &RoTxn<'_>, name: &[u8]) -> u64 {
    let main = txn.open_db(None).unwrap();
    txn.get(main, name)
        .unwrap()
        .map_or(0, |v| std::str::from_utf8(v).unwrap().parse().unwrap())
}

/// One snapshot of `env`: `#count` equals the keys of `data`, every value
/// names a transaction no later than `#txn`; returns `#txn`.
fn check_snapshot(env: &Environment) -> u64 {
    let txn = env.begin_ro_txn().unwrap();
    let last = counter(&txn, b"#txn");
    let Ok(data) = txn.open_db(Some("data")) else {
        return last; // nothing committed yet
    };
    let mut cur = txn.cursor(data).unwrap();
    let mut n = 0u64;
    while let Some((_, v)) = cur.next().unwrap() {
        let i: u64 = std::str::from_utf8(v.split(|&b| b == b':').next().unwrap())
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            i % 1_000_000 <= last,
            "a value of txn {i} in snapshot {last}"
        );
        n += 1;
    }
    assert_eq!(n, counter(&txn, b"#count"), "{:?}", env.format());
    last
}

/// What `env` holds.
fn read_model(env: &Environment) -> Model {
    let txn = env.begin_ro_txn().unwrap();
    let mut m = Model::default();
    let data = txn.open_db(Some("data")).unwrap();
    let mut cur = txn.cursor(data).unwrap();
    while let Some((k, v)) = cur.next().unwrap() {
        m.data.insert(k.to_vec(), v.to_vec());
    }
    let dups = txn.open_db(Some("dups")).unwrap();
    let mut cur = txn.cursor(dups).unwrap();
    while let Some((k, v)) = cur.next().unwrap() {
        m.dups.entry(k.to_vec()).or_default().insert(v.to_vec());
    }
    m
}

fn open(dir: &Path, format: Option<Format>, page_size: Option<usize>) -> Environment {
    let mut b = Environment::builder();
    b.max_dbs(4).map_size(256 << 20);
    if let Some(f) = format {
        b.format(f);
    }
    if let Some(ps) = page_size {
        b.page_size(ps);
    }
    b.open(dir).unwrap()
}

/// `mti_magic` and `mti_format` of a lock file (read only once no
/// environment of this process has it open: closing another descriptor of
/// the file would drop this process's locks on it).
fn lock_header(dir: &Path) -> (u32, u32) {
    let b = std::fs::read(dir.join("lock.mdb")).unwrap();
    let word = |o: usize| u32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
    (word(0), word(4))
}

#[test]
fn both_formats_in_one_process() {
    let (d09, d10) = (tmp(), tmp());
    let env09 = open(d09.path(), Some(Format::V09), None);
    let env10 = open(d10.path(), Some(Format::V10), Some(1024));
    let envs = [&env09, &env10];
    assert_eq!(env09.format(), Format::V09);
    assert_eq!(env10.format(), Format::V10);
    assert_eq!(env09.max_key_size(), 511);
    assert_eq!(env10.max_key_size(), (((1024 - 24) / 2) & !1) - 2 - 56);
    assert_eq!(env10.page_size(), 1024);

    // A snapshot of each, held across all the writes below.
    let held: Vec<(RoTxn<'_>, Model)> = envs
        .iter()
        .map(|env| {
            let mut txn = env.begin_rw_txn().unwrap();
            let mut m = Model::default();
            ops(&mut txn, &mut m, 0, env.page_size());
            txn.commit().unwrap();
            (env.begin_ro_txn().unwrap(), m)
        })
        .collect();

    let done = AtomicBool::new(false);
    let models = std::thread::scope(|s| {
        let writers: Vec<_> = envs
            .iter()
            .zip(&held)
            .map(|(env, (_, m))| s.spawn(|| writer(env, m.clone())))
            .collect();
        let readers: Vec<_> = (0..2)
            .map(|_| {
                s.spawn(|| {
                    let mut last = [0u64; 2];
                    let mut reads = 0;
                    while !done.load(Ordering::SeqCst) || reads == 0 {
                        for (k, env) in envs.iter().enumerate() {
                            let t = check_snapshot(env);
                            assert!(
                                t >= last[k],
                                "{:?}: #txn {t} after {}",
                                env.format(),
                                last[k]
                            );
                            last[k] = t;
                        }
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();
        let models: Vec<Model> = writers.into_iter().map(|w| w.join().unwrap()).collect();
        done.store(true, Ordering::SeqCst);
        for r in readers {
            assert!(r.join().unwrap() > 0);
        }
        models
    });

    // The held snapshots still show the first transaction only.
    for (txn, m) in &held {
        let data = txn.open_db(Some("data")).unwrap();
        let mut cur = txn.cursor(data).unwrap();
        let mut got = BTreeMap::new();
        while let Some((k, v)) = cur.next().unwrap() {
            got.insert(k.to_vec(), v.to_vec());
        }
        assert_eq!(got, m.data);
    }
    drop(held);
    for (env, m) in envs.iter().zip(&models) {
        assert_eq!(&read_model(env), m, "{:?}", env.format());
    }

    // Compacting copies keep the format.
    let copies = [tmp(), tmp()];
    for (env, dir) in envs.iter().zip(&copies) {
        env.copy_to_path(dir.path(), true).unwrap();
    }
    drop(env09);
    drop(env10);

    for (dir, format, lock) in [
        (&d09, Format::V09, &LOCK_V09),
        (&d10, Format::V10, &LOCK_V10),
    ] {
        assert_eq!(
            sniff_format(dir.path(), EnvironmentFlags::empty()).unwrap(),
            Some(format)
        );
        assert_eq!(lock_header(dir.path()), (0xBEEF_C0DE, lock.lock_format));
    }
    for ((dir, format), m) in [(&d09, Format::V09), (&d10, Format::V10)]
        .into_iter()
        .chain([(&copies[0], Format::V09), (&copies[1], Format::V10)])
        .zip(models.iter().chain(&models))
    {
        // Reopened without a format request: the file decides.
        let env = open(dir.path(), None, None);
        assert_eq!(env.format(), format);
        assert_eq!(env.page_size() == 1024, format == Format::V10);
        assert_eq!(&read_model(&env), m, "{format:?}");
    }
}
