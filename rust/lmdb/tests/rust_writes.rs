//! Rust writes, C verifies: environments written by the Rust write path
//! are checked with the C tools (`mdb_dump`, `mdb_stat`) and the interop C
//! peer (`cpeer verify`, `cpeer check-freelist`), then written further by C
//! and read back by Rust.

mod common;

use std::path::Path;

use common::dataset::Dataset;
use common::{hex, CTools};
use lmdb::consts::*;
use lmdb::{DatabaseFlags, Dbi, Environment, EnvironmentFlags, Transaction, WriteFlags};

fn open_env(path: &Path) -> Environment {
    Environment::builder()
        .map_size(256 << 20)
        .max_dbs(64)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

/// Puts the data set's items in index order (random order for the
/// comparator), committing every `per_txn` keys, like `cpeer fill`.
fn fill(env: &Environment, ds: &Dataset, per_txn: u32) -> Dbi {
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(ds.db.as_deref(), ds.db_flags() | DatabaseFlags::CREATE)
        .unwrap();
    for i in 0..ds.count {
        let key = ds.key(i);
        for d in 0..ds.ndups(i) {
            txn.put(db, &key, &ds.value(i, d), WriteFlags::empty())
                .unwrap_or_else(|e| panic!("put {i}/{d}: {e}"));
        }
        if per_txn > 0 && (i + 1) % per_txn == 0 {
            txn.commit().unwrap();
            txn = env.begin_rw_txn().unwrap();
        }
    }
    txn.commit().unwrap();
    db
}

/// The data set in database order (key, values), as Rust's comparators sort.
fn sorted_model(ds: &Dataset) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let kc = lmdb::Comparator::default_key(ds.flags);
    let mut dc = lmdb::Comparator::default_dup(ds.flags).unwrap_or(lmdb::Comparator::Memn);
    // C compares 8-byte INTEGERDUP|DUPFIXED items with mdb_cmp_clong.
    if dc.is_int() && ds.valsize == 8 {
        dc = lmdb::Comparator::Clong;
    }
    let mut items: Vec<(Vec<u8>, Vec<Vec<u8>>)> = (0..ds.count)
        .map(|i| {
            let mut vals: Vec<Vec<u8>> = (0..ds.ndups(i)).map(|d| ds.value(i, d)).collect();
            vals.sort_by(|a, b| dc.compare(a, b));
            (ds.key(i), vals)
        })
        .collect();
    items.sort_by(|a, b| kc.compare(&a.0, &b.0));
    items
}

/// Fills with MDB_APPEND / MDB_APPENDDUP through a cursor, in database order.
fn fill_append(env: &Environment, ds: &Dataset) -> Dbi {
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(ds.db.as_deref(), ds.db_flags() | DatabaseFlags::CREATE)
        .unwrap();
    {
        let mut c = txn.cursor_mut(db).unwrap();
        for (k, vals) in sorted_model(ds) {
            for (j, v) in vals.iter().enumerate() {
                let f = if j == 0 {
                    WriteFlags::APPEND
                } else {
                    WriteFlags::APPEND_DUP
                };
                c.put(&k, v, f).unwrap();
            }
        }
    }
    txn.commit().unwrap();
    db
}

fn verify_with_c(c: &CTools, path: &Path, ds: &Dataset) {
    let p = path.to_str().unwrap();
    let mut args = vec!["verify".to_owned(), p.to_owned()];
    args.extend(ds.cmd_args());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = c.peer_ok(&args);
    assert!(out.stdout.starts_with("verified"), "{}", out.stdout);
}

fn check_freelist(c: &CTools, path: &Path) -> common::PeerOutput {
    let out = c.peer(&["check-freelist", path.to_str().unwrap()]);
    assert_eq!(
        out.code, 0,
        "check-freelist:\n{}\n{}",
        out.stdout, out.stderr
    );
    assert!(out.stdout.contains("pages_leaked=0"), "{}", out.stdout);
    out
}

/// Compares `mdb_stat -e -a -f` with Rust's view of every DB.
fn compare_stats(c: &CTools, env: &Environment, path: &Path, names: &[Option<&str>]) {
    let text = c.stat_text(path, &["-e", "-f", "-a"]);
    let stat = common::parse_stat(&text);
    let txn = env.begin_ro_txn().unwrap();
    for name in names {
        let db = txn.open_db(*name).unwrap();
        let label = name.unwrap_or("Main DB");
        let cstat = stat
            .dbs
            .get(label)
            .unwrap_or_else(|| panic!("no {label} in\n{text}"));
        common::assert_stat_matches(label, &txn.stat(db).unwrap(), cstat);
    }
    let free = txn.stat(Dbi::FREE).unwrap();
    common::assert_stat_matches("free", &free, &stat.dbs["(free)"]);
    assert_eq!(stat.free_pages, Some(env.freelist().unwrap() as u64));
}

/// `mdb_dump` of a named DB (or the main DB) equals Rust's walk.
fn compare_dump(c: &CTools, env: &Environment, path: &Path, name: Option<&str>) {
    let dump = c.dump(path, name);
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(name).unwrap();
    let walk = common::walk_forward(&txn, db);
    if name.is_none() {
        // The main DB also holds named DB records; mdb_dump shows them too.
        assert_eq!(dump, walk);
    } else {
        assert_eq!(dump.len(), walk.len(), "dump length");
        assert!(dump == walk, "dump differs");
    }
}

#[test]
fn gen_vectors_match_cpeer() {
    let c = require_c!();
    let sets = [
        Dataset::new(None, 0, 1).finish(),
        Dataset::new(None, 0, 1).with_flags(MDB_INTEGERKEY).finish(),
        Dataset::new(None, 0, 9)
            .with_flags(MDB_DUPFIXED)
            .dups(3)
            .valsize(12)
            .finish(),
        Dataset::new(None, 0, 3)
            .with_flags(MDB_INTEGERDUP)
            .dups(3)
            .finish(),
        Dataset::new(None, 0, 3)
            .with_flags(MDB_INTEGERDUP)
            .dups(7)
            .valsize(4)
            .finish(),
        Dataset::new(None, 0, 5)
            .with_flags(MDB_DUPSORT)
            .dups(9)
            .valsize(100)
            .finish(),
        Dataset::new(None, 0, 6)
            .with_flags(MDB_REVERSEKEY)
            .big(5000)
            .finish(),
        Dataset::new(None, 0, 7)
            .with_flags(MDB_DUPSORT | MDB_REVERSEDUP)
            .finish(),
    ];
    for ds in &sets {
        for i in [0u32, 1, 3, 7, 12, 1000, 65537] {
            let mut args = vec!["gen".to_owned(), ds.seed.to_string(), i.to_string()];
            args.extend(ds.args());
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = c.peer_ok(&args);
            let mut expect = format!("key={}\nndups={}\n", hex(&ds.key(i)), ds.ndups(i));
            for d in 0..ds.ndups(i) {
                expect.push_str(&format!("value{d}={}\n", hex(&ds.value(i, d))));
            }
            let got: String = out
                .stdout
                .split_whitespace()
                .map(|t| format!("{t}\n"))
                .collect();
            assert_eq!(got, expect, "gen {args:?}");
        }
    }
}

#[test]
fn plain_random_order_main_db() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let ds = Dataset::new(None, 5000, 11).finish();
    let env = open_env(dir.path());
    fill(&env, &ds, 0);
    verify_with_c(c, dir.path(), &ds);
    check_freelist(c, dir.path());
    compare_stats(c, &env, dir.path(), &[None]);
    compare_dump(c, &env, dir.path(), None);
}

/// Writes `ds` with Rust (`how`), then checks it with C and Rust.
fn run_variant(ds: Dataset, how: Fill) {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path());
    match how {
        Fill::Put(per_txn) => {
            fill(&env, &ds, per_txn);
        }
        Fill::Append => {
            fill_append(&env, &ds);
        }
        Fill::Reserve => {
            fill_reserve(&env, &ds);
        }
    }
    verify_with_c(c, dir.path(), &ds);
    let names = [ds.db.as_deref()];
    compare_stats(c, &env, dir.path(), &names);
    check_freelist(c, dir.path());
    if ds.db.is_some() {
        compare_dump(c, &env, dir.path(), ds.db.as_deref());
    }
    // Rust reads back what it wrote, every read path.
    let expected: Vec<(Vec<u8>, Vec<u8>)> = sorted_model(&ds)
        .into_iter()
        .flat_map(|(k, vs)| vs.into_iter().map(move |v| (k.clone(), v)))
        .collect();
    let db = env.open_db(ds.db.as_deref()).unwrap();
    common::verify_db(&env, db, &expected);
}

#[derive(Clone, Copy)]
enum Fill {
    Put(u32),
    Append,
    Reserve,
}

/// `cpeer fill --reserve`: `MDB_RESERVE`, then write the value.
fn fill_reserve(env: &Environment, ds: &Dataset) -> Dbi {
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(ds.db.as_deref(), ds.db_flags() | DatabaseFlags::CREATE)
        .unwrap();
    for i in 0..ds.count {
        let v = ds.value(i, 0);
        let buf = txn
            .reserve(db, &ds.key(i), v.len(), WriteFlags::empty())
            .unwrap();
        buf.copy_from_slice(&v);
    }
    txn.commit().unwrap();
    db
}

macro_rules! variant {
    ($name:ident, $ds:expr, $how:expr) => {
        #[test]
        fn $name() {
            run_variant($ds.finish(), $how);
        }
    };
}

variant!(
    v_plain_many_txns,
    Dataset::new(None, 3000, 2),
    Fill::Put(97)
);
variant!(v_named, Dataset::new(Some("named"), 4000, 3), Fill::Put(0));
variant!(
    v_intkey,
    Dataset::new(Some("ik"), 4000, 4).with_flags(MDB_INTEGERKEY),
    Fill::Put(0)
);
variant!(
    v_revkey,
    Dataset::new(Some("rk"), 4000, 5).with_flags(MDB_REVERSEKEY),
    Fill::Put(500)
);
variant!(
    v_big_one_page,
    Dataset::new(Some("big1"), 600, 6).big(3000),
    Fill::Put(0)
);
variant!(
    v_big_overflow,
    Dataset::new(Some("big2"), 300, 7).big(20000),
    Fill::Put(50)
);
variant!(
    v_big_multi,
    Dataset::new(Some("big3"), 90, 8).big(70000),
    Fill::Put(0)
);
variant!(
    v_dupsort_few,
    Dataset::new(Some("ds"), 3000, 9).with_flags(MDB_DUPSORT),
    Fill::Put(0)
);
variant!(
    v_dupsort_many,
    Dataset::new(Some("dsm"), 60, 10)
        .with_flags(MDB_DUPSORT)
        .dups(400)
        .valsize(100),
    Fill::Put(0)
);
variant!(
    v_dupsort_max_values,
    Dataset::new(Some("dsx"), 40, 12)
        .with_flags(MDB_DUPSORT)
        .dups(40)
        .valsize(511),
    Fill::Put(7)
);
variant!(
    v_revdup,
    Dataset::new(Some("rd"), 1500, 13)
        .with_flags(MDB_DUPSORT | MDB_REVERSEDUP)
        .dups(30),
    Fill::Put(0)
);
variant!(
    v_dupfixed,
    Dataset::new(Some("df"), 1500, 14)
        .with_flags(MDB_DUPFIXED)
        .dups(50)
        .valsize(12),
    Fill::Put(0)
);
variant!(
    v_dupfixed_many,
    Dataset::new(Some("dfm"), 20, 15)
        .with_flags(MDB_DUPFIXED)
        .dups(3000)
        .valsize(8),
    Fill::Put(0)
);
variant!(
    v_intdup8,
    Dataset::new(Some("id8"), 500, 16)
        .with_flags(MDB_INTEGERDUP)
        .dups(200),
    Fill::Put(0)
);
variant!(
    v_intdup4,
    Dataset::new(Some("id4"), 500, 17)
        .with_flags(MDB_INTEGERDUP)
        .dups(200)
        .valsize(4),
    Fill::Put(100)
);
variant!(v_append, Dataset::new(Some("ap"), 5000, 18), Fill::Append);
variant!(
    v_append_dups,
    Dataset::new(Some("apd"), 400, 19)
        .with_flags(MDB_DUPSORT)
        .dups(100)
        .valsize(40),
    Fill::Append
);
variant!(
    v_append_dupfixed,
    Dataset::new(Some("apf"), 50, 20)
        .with_flags(MDB_DUPFIXED)
        .dups(2000)
        .valsize(16),
    Fill::Append
);
variant!(
    v_append_big,
    Dataset::new(Some("apb"), 200, 21).big(30000),
    Fill::Append
);
variant!(
    v_reserve,
    Dataset::new(Some("rs"), 2000, 22).big(25000),
    Fill::Reserve
);
variant!(v_reserve_main, Dataset::new(None, 2000, 23), Fill::Reserve);
