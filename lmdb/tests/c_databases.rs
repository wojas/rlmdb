//! Databases written by the C library, read by Rust.
//!
//! Each test generates an environment with a C program (`mtest*` or
//! `mdb_load`), then checks the Rust view against the C view: every pair in
//! `mdb_dump` order via cursor walks in both directions, `get`, the `SET*`,
//! dup and `NODUP` operations, statistics against `mdb_stat`, and random
//! cursor-operation sequences against C's `mdb_cursor_get` (positions after
//! failures included).

mod common;

use std::path::Path;

use common::{Pair, *};
use lmdb::{DatabaseFlags, Dbi, Environment, Transaction};

/// Compares env info, main/named DB stats and the free list with
/// `mdb_stat -e -f -a`.
fn check_stats(c: &CTools, path: &Path, env: &Environment, names: &[&str]) {
    let stat = parse_stat(&c.stat_text(path, &["-e", "-f", "-a"]));
    let info = env.info().unwrap();
    assert_eq!(stat.env["Map size"], info.map_size.to_string());
    assert_eq!(stat.env["Page size"], env.page_size().to_string());
    assert_eq!(
        stat.env["Number of pages used"],
        (info.last_pgno + 1).to_string()
    );
    assert_eq!(stat.env["Last transaction ID"], info.last_txnid.to_string());
    assert_eq!(stat.env["Max readers"], info.max_readers.to_string());
    assert_eq!(stat.free_pages, Some(env.freelist().unwrap() as u64));

    let txn = env.begin_ro_txn().unwrap();
    assert_stat_matches("Main DB", &env.stat().unwrap(), &stat.dbs["Main DB"]);
    assert_stat_matches(
        "Main DB (txn)",
        &txn.stat(Dbi::MAIN).unwrap(),
        &stat.dbs["Main DB"],
    );
    assert_stat_matches(
        "free DB",
        &txn.stat(Dbi::FREE).unwrap(),
        &stat.dbs["(free)"],
    );
    for name in names {
        let db = txn.open_db(Some(name)).unwrap();
        assert_stat_matches(name, &txn.stat(db).unwrap(), &stat.dbs[*name]);
    }
    // -a lists exactly the named DBs we expect.
    let mut listed: Vec<&str> = stat
        .dbs
        .keys()
        .map(String::as_str)
        .filter(|n| *n != "Main DB" && *n != "(free)")
        .collect();
    listed.sort();
    let mut want = names.to_vec();
    want.sort();
    assert_eq!(listed, want);
}

/// Full check of one database against `mdb_dump`.
fn check_db(
    c: &CTools,
    path: &Path,
    env: &Environment,
    name: Option<&str>,
    seed: u64,
) -> Vec<Pair> {
    let expected = c.dump(path, name);
    let db = env.open_db(name).unwrap();
    verify_db(env, db, &expected);
    differential(c, path, env, name, &expected, seed, 4);
    expected
}

fn mtest_env(c: &CTools, prog: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = c.mtest(prog, dir.path());
    if std::env::var_os("LMDB_KEEP_TMP").is_some() {
        // Debugging aid: keep a copy of the generated environment.
        let keep = std::env::temp_dir().join(format!("lmdb-keep-{prog}-{}", std::process::id()));
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::copy(path.join("data.mdb"), keep.join("data.mdb")).unwrap();
        eprintln!("kept {prog} environment in {}", keep.display());
    }
    (dir, path)
}

#[test]
fn mtest_main_db() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest");
    let env = open_ro(&path, 0);
    eprintln!("mtest: page size {}", env.page_size());
    // (mtest deletes a random share of its random keys; the result may be
    // empty, which is fine.)
    let pairs = check_db(c, &path, &env, None, 1);
    check_stats(c, &path, &env, &[]);
    // mtest's keys are 4-byte prefixes of "%03x %d foo bar".
    assert!(pairs.iter().all(|(k, v)| k.len() == 4 && v.len() == 32));
}

#[test]
fn mtest2_named_db() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest2");
    let env = open_ro(&path, 4);
    check_db(c, &path, &env, Some("id1"), 2);
    check_stats(c, &path, &env, &["id1"]);
    let txn = env.begin_ro_txn().unwrap();
    // The main DB holds the F_SUBDATA node of "id1": get must not hand out
    // the record as data... it is the named DB's MDB_db, 48 bytes.
    assert_eq!(
        txn.get(Dbi::MAIN, b"id1").unwrap().map(<[u8]>::len),
        Some(48)
    );
    assert!(txn.open_db(Some("nope")).unwrap_err().not_found());
}

#[test]
fn mtest3_dupsort() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest3");
    let env = open_ro(&path, 4);
    let pairs = check_db(c, &path, &env, Some("id2"), 3);
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("id2")).unwrap();
    assert_eq!(txn.db_flags(db).unwrap(), DatabaseFlags::DUP_SORT);
    assert!(group(&pairs).iter().any(|(_, vs)| vs.len() > 1), "has dups");
    check_stats(c, &path, &env, &["id2"]);
}

#[test]
fn mtest4_dupfixed() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest4");
    let env = open_ro(&path, 4);
    let pairs = check_db(c, &path, &env, Some("id4"), 4);
    check_stats(c, &path, &env, &["id4"]);
    // GET_MULTIPLE + NEXT_MULTIPLE return all duplicates of a key, page by page.
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("id4")).unwrap();
    for (k, vs) in group(&pairs) {
        let mut cur = txn.cursor(db).unwrap();
        cur.set(&k).unwrap().unwrap();
        let mut all = Vec::new();
        let (mk, first) = cur.get_multiple().unwrap().unwrap();
        assert_eq!(mk, &k[..]);
        all.extend_from_slice(first);
        while let Some((mk, chunk)) = cur.next_multiple().unwrap() {
            assert_eq!(mk, &k[..]);
            all.extend_from_slice(chunk);
        }
        assert_eq!(all, vs.concat(), "multiple of {}", hex(&k));
    }
}

#[test]
fn mtest5_dupsort_cursor_put() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest5");
    let env = open_ro(&path, 4);
    check_db(c, &path, &env, Some("id2"), 5);
    check_stats(c, &path, &env, &["id2"]);
}

#[test]
fn mtest6_integerkey_splits() {
    let c = require_c!();
    let (_dir, path) = mtest_env(c, "mtest6");
    let env = open_ro(&path, 4);
    let pairs = check_db(c, &path, &env, Some("id6"), 6);
    assert_eq!(pairs.len(), 36);
    // Keys are native longs in numeric order.
    let nums: Vec<u64> = pairs
        .iter()
        .map(|(k, _)| u64::from_ne_bytes(k[..].try_into().unwrap()))
        .collect();
    let mut sorted = nums.clone();
    sorted.sort();
    assert_eq!(nums, sorted);
    check_stats(c, &path, &env, &["id6"]);
}

// ---------------------------------------------------------------------------
// Crafted databases (mdb_load)
// ---------------------------------------------------------------------------

fn kv(k: impl Into<Vec<u8>>, v: impl Into<Vec<u8>>) -> Pair {
    (k.into(), v.into())
}

fn sorted_set(mut v: Vec<Pair>) -> Vec<Pair> {
    v.sort();
    v
}

#[test]
fn overflow_values() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    let ps = os_page_size().min(32768);
    let nodemax = lmdb::format::node_max(ps);
    let sizes = [
        0,
        1,
        100,
        nodemax - 8 - 4,     // largest that stays inline with a 4-byte key
        nodemax - 8 - 4 + 1, // smallest that goes to an overflow page
        ps - 16,             // exactly one overflow page
        ps - 15,             // two
        3 * ps + 17,
        70_000,
        1 << 20,
    ];
    let pairs: Vec<Pair> = sizes
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let v: Vec<u8> = (0..n).map(|j| (j * 7 + i) as u8).collect();
            kv(format!("k{i:03}"), v)
        })
        .collect();
    c.load(&path, None, &[], &pairs);
    let env = open_ro(&path, 0);
    let got = check_db(c, &path, &env, None, 7);
    assert_eq!(got, pairs);
    let st = env.stat().unwrap();
    assert!(st.overflow_pages > 0);
    check_stats(c, &path, &env, &[]);
}

#[test]
fn deep_tree_long_keys() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    let mut rng = Rng(0x1234_5678);
    let mut pairs: Vec<Pair> = (0..12_000u32)
        .map(|i| {
            let len = 100 + rng.below(412); // up to MDB_MAXKEYSIZE
            let mut k = format!("{i:08}-").into_bytes();
            k.resize(len, b'a' + (i % 26) as u8);
            kv(k, format!("value-{i}").into_bytes())
        })
        .collect();
    // Load in random order so pages split all over the tree.
    for i in (1..pairs.len()).rev() {
        pairs.swap(i, rng.below(i + 1));
    }
    c.load(&path, None, &[], &pairs);
    let env = open_ro(&path, 0);
    let st = env.stat().unwrap();
    eprintln!(
        "deep tree: depth {} with page size {}",
        st.depth, st.page_size
    );
    assert!(st.depth >= 3, "depth {}", st.depth);
    let got = check_db(c, &path, &env, None, 8);
    assert_eq!(got, sorted_set(pairs));
    check_stats(c, &path, &env, &[]);
}

#[test]
fn integer_keys_and_dups() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    let mut rng = Rng(77);
    // INTEGERKEY, 4-byte keys.
    let ik4: Vec<Pair> = (0..3000)
        .map(|_| kv((rng.next() as u32).to_ne_bytes(), b"v4".to_vec()))
        .collect();
    c.load(&path, Some("ik4"), &["integerkey"], &ik4);
    // INTEGERKEY, 8-byte keys.
    let ik8: Vec<Pair> = (0..3000)
        .map(|i| kv((rng.next() >> (i % 64)).to_ne_bytes(), b"v8".to_vec()))
        .collect();
    c.load(&path, Some("ik8"), &["integerkey"], &ik8);
    // DUPSORT|INTEGERDUP with 4-byte dups (mdb_cmp_cint).
    let mut idup: Vec<Pair> = Vec::new();
    for k in 0..40u32 {
        for _ in 0..(1 + rng.below(60)) {
            idup.push(kv(format!("key{k:02}"), (rng.next() as u32).to_ne_bytes()));
        }
    }
    c.load(&path, Some("idup"), &["dupsort", "integerdup"], &idup);
    // DUPSORT|DUPFIXED|INTEGERDUP with 8-byte dups (mdb_cmp_int -> clong),
    // enough for sub-DBs with several LEAF2 pages.
    let mut ifix: Vec<Pair> = Vec::new();
    for k in 0..6u32 {
        let n = if k < 2 { 5000 } else { 1 + rng.below(30) };
        for _ in 0..n {
            ifix.push(kv(format!("fix{k}"), rng.next().to_ne_bytes()));
        }
    }
    c.load(
        &path,
        Some("ifix"),
        &["dupsort", "dupfixed", "integerdup"],
        &ifix,
    );

    let env = open_ro(&path, 8);
    for (name, seed) in [("ik4", 11), ("ik8", 12), ("idup", 13), ("ifix", 14)] {
        let got = check_db(c, &path, &env, Some(name), seed);
        assert!(!got.is_empty());
    }
    // Sanity of the oracle: C sorted the integer keys numerically.
    let got = c.dump(&path, Some("ik8"));
    let nums: Vec<u64> = got
        .iter()
        .map(|(k, _)| u64::from_ne_bytes(k[..].try_into().unwrap()))
        .collect();
    assert!(nums.windows(2).all(|w| w[0] < w[1]));
    check_stats(c, &path, &env, &["idup", "ifix", "ik4", "ik8"]);

    // GET_MULTIPLE / NEXT_MULTIPLE over sub-DBs with several LEAF2 pages.
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("ifix")).unwrap();
    let expected = c.dump(&path, Some("ifix"));
    for (k, vs) in group(&expected) {
        let mut cur = txn.cursor(db).unwrap();
        cur.set(&k).unwrap().unwrap();
        let mut all = cur.get_multiple().unwrap().unwrap().1.to_vec();
        let mut chunks = 1;
        while let Some((_, chunk)) = cur.next_multiple().unwrap() {
            all.extend_from_slice(chunk);
            chunks += 1;
        }
        assert_eq!(all, vs.concat());
        if vs.len() >= 5000 {
            assert!(chunks > 1, "expected several LEAF2 pages");
        }
    }
}

#[test]
fn reverse_key_and_dup() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    let mut rng = Rng(99);
    let words: Vec<Vec<u8>> = (0..500)
        .map(|_| {
            let len = 1 + rng.below(12);
            (0..len).map(|_| b'a' + rng.below(26) as u8).collect()
        })
        .collect();
    let rk: Vec<Pair> = words.iter().map(|w| kv(w.clone(), w.repeat(2))).collect();
    c.load(&path, Some("rk"), &["reversekey"], &rk);
    let mut rd: Vec<Pair> = Vec::new();
    for (i, w) in words.iter().enumerate().take(100) {
        for j in 0..(i % 7 + 1) {
            rd.push(kv(
                format!("k{:02}", i % 13),
                [w.as_slice(), &[b'0' + j as u8]].concat(),
            ));
        }
    }
    c.load(&path, Some("rd"), &["dupsort", "reversedup"], &rd);
    let env = open_ro(&path, 4);
    let got = check_db(c, &path, &env, Some("rk"), 21);
    // Order is by reversed bytes.
    let rev: Vec<Vec<u8>> = got
        .iter()
        .map(|(k, _)| k.iter().rev().copied().collect())
        .collect();
    assert!(rev.windows(2).all(|w| w[0] < w[1]));
    check_db(c, &path, &env, Some("rd"), 22);
    check_stats(c, &path, &env, &["rd", "rk"]);
}

#[test]
fn dupsort_subpages_and_subdbs() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    // Keys with 1, a few, and thousands of duplicates: plain nodes,
    // sub-pages (F_DUPDATA) and sub-DBs (F_DUPDATA|F_SUBDATA).
    let mut pairs: Vec<Pair> = Vec::new();
    for (k, n) in [
        ("a", 1),
        ("b", 3),
        ("c", 40),
        ("d", 4000),
        ("e", 2),
        ("f", 900),
    ] {
        for i in 0..n {
            pairs.push(kv(k, format!("{k}-dup-{i:06}-{}", "x".repeat(i % 50))));
        }
    }
    c.load(&path, Some("dups"), &["dupsort"], &pairs);
    // And a DUPFIXED one with mixed sizes of dup sets.
    let mut fixed: Vec<Pair> = Vec::new();
    for (k, n) in [("p", 1), ("q", 10), ("r", 3000), ("s", 2)] {
        for i in 0..n {
            fixed.push(kv(k, format!("{i:012}")));
        }
    }
    c.load(&path, Some("fixed"), &["dupsort", "dupfixed"], &fixed);
    let env = open_ro(&path, 4);
    let got = check_db(c, &path, &env, Some("dups"), 31);
    assert_eq!(got, sorted_set(pairs));
    let got = check_db(c, &path, &env, Some("fixed"), 32);
    assert_eq!(got, sorted_set(fixed));
    check_stats(c, &path, &env, &["dups", "fixed"]);
}

#[test]
fn many_named_dbs_and_handles() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    for i in 0..5 {
        let pairs: Vec<Pair> = (0..10 * (i + 1))
            .map(|j| kv(format!("k{j:03}"), format!("db{i}")))
            .collect();
        c.load(&path, Some(&format!("db{i}")), &[], &pairs);
    }
    // Room for 3 named DBs only.
    let env = open_ro(&path, 3);
    let txn = env.begin_ro_txn().unwrap();
    let h: Vec<Dbi> = (0..3)
        .map(|i| txn.open_db(Some(&format!("db{i}"))).unwrap())
        .collect();
    assert!(matches!(
        txn.open_db(Some("db3")),
        Err(lmdb::Error::Mdb(lmdb::MdbError::DbsFull))
    ));
    // Same name, same handle.
    assert_eq!(txn.open_db(Some("db1")).unwrap(), h[1]);
    assert_eq!(txn.stat(h[2]).unwrap().entries, 30);
    drop(txn);
    // Closing frees a slot; the old handle becomes invalid.
    env.close_db(h[0]);
    let txn = env.begin_ro_txn().unwrap();
    let err = txn.get(h[0], b"k000").unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    let d3 = txn.open_db(Some("db3")).unwrap();
    assert_eq!(d3, h[0], "slot reused");
    assert_eq!(txn.get(d3, b"k000").unwrap(), Some(&b"db3"[..]));
    // A transaction that cached the handle sees BAD_DBI after a close.
    env.close_db(d3);
    assert!(matches!(
        txn.get(d3, b"k000"),
        Err(lmdb::Error::Mdb(lmdb::MdbError::BadDbi))
    ));
    // Handles opened in a read txn survive it and work in later ones.
    let d4 = env.open_db(Some("db4")).unwrap();
    let txn2 = env.begin_ro_txn().unwrap();
    assert_eq!(txn2.stat(d4).unwrap().entries, 50);
    assert_eq!(txn2.get(h[1], b"k019").unwrap(), Some(&b"db1"[..]));
}
