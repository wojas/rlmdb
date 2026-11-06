//! The opt-in feature `split-freelist` ([`Feature::SplitFreelist`]): a
//! write transaction that frees more pages than one page of free-list
//! record holds saves them in several records of at most one page each,
//! under the keys `T .. T + n - 1` from its own txnid `T`, and writes its
//! snapshot into both meta pages, under `P = T + (n & !1)` and `P + 1`;
//! [`PreparedTxn::id`](rlmdb::PreparedTxn::id) is `P + 1`, the ID the
//! commit publishes (`docs/split-freelist-details.md`).
//!
//! Checked here, in both formats and at small 1.0 page sizes: the records
//! (keys, sizes, order of the pages), the IDs a transaction reports and
//! publishes, the two meta pages, that every page is used once (in a tree
//! or in one free record), that a reader of an older snapshot keeps its
//! pages, that the next writer reuses the freed pages sooner than without
//! the feature, that the previous snapshot stays intact when a later writer
//! writes pages and aborts, that a split commit cannot be rolled back, a
//! pair a dying process left half written, failed meta writes, `NO_LOCK`
//! (with a stale reader in another process's footsteps), and that nothing
//! changes with the feature off or where one record suffices. The feature is rlmdb's own, so there is no C oracle;
//! the format is C's (a free DB of txnid keys and page lists, two meta
//! pages with consecutive txnids).

mod common;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use rlmdb::format::Meta;
use rlmdb::{
    Dbi, Environment, EnvironmentBuilder, EnvironmentFlags, Feature, Format, MdbError, Transaction,
    WriteFlags,
};

/// The formats and 1.0 page sizes the tests run in (0.9: the OS page
/// size).
fn layouts() -> Vec<(Format, Option<usize>)> {
    vec![
        (Format::V09, None),
        (Format::V10, Some(256)),
        (Format::V10, Some(1024)),
        (Format::V10, Some(4096)),
    ]
}

/// A builder for an environment of `format` (pages of `psize` bytes for
/// 1.0) with the feature on or off.
fn builder(format: Format, psize: Option<usize>, split: bool) -> EnvironmentBuilder {
    let mut b = Environment::builder();
    b.format(format)
        .map_size(256 << 20)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = psize {
        b.page_size(ps);
    }
    if split {
        b.enable(Feature::SplitFreelist);
    }
    b
}

/// `maxfree_1pg`: how many page numbers a record of one page holds.
fn maxfree(env: &Environment) -> usize {
    let hdr = match env.format() {
        Format::V09 => 16,
        _ => 24,
    };
    (env.page_size() - hdr) / 8 - 1
}

/// The key of value `i`.
fn key(i: u32) -> [u8; 4] {
    i.to_be_bytes()
}

/// Value `i` of `len` bytes, telling `i` and `round` apart.
fn value(i: u32, round: u8, len: usize) -> Vec<u8> {
    let mut v = vec![round; len];
    v[..4].copy_from_slice(&i.to_le_bytes());
    v
}

/// The length of a value that takes 4 overflow pages.
fn big(env: &Environment) -> usize {
    3 * env.page_size()
}

/// Stores `count` big values (round `round`) from key `from` on and
/// commits.
fn fill(env: &Environment, from: u32, count: u32, round: u8) {
    let len = big(env);
    let mut txn = env.begin_rw_txn().unwrap();
    for i in from..from + count {
        txn.put(
            Dbi::MAIN,
            &key(i),
            &value(i, round, len),
            WriteFlags::empty(),
        )
        .unwrap();
    }
    txn.commit().unwrap();
}

/// How many big values to free about `records` records' worth of pages.
fn values_for(env: &Environment, records: f64) -> u32 {
    (records * maxfree(env) as f64 / 4.0).ceil() as u32
}

/// The free DB records of the newest snapshot: txnid -> page numbers, as
/// stored (descending).
fn free_records(env: &Environment) -> BTreeMap<u64, Vec<u64>> {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::FREE).unwrap();
    let mut out = BTreeMap::new();
    while let Some((k, v)) = c.next().unwrap() {
        let id = u64::from_ne_bytes(k.try_into().unwrap());
        let words: Vec<u64> = v
            .chunks_exact(8)
            .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(words[0] as usize, words.len() - 1, "record {id}");
        out.insert(id, words[1..].to_vec());
    }
    out
}

/// Every page of the newest snapshot is used once: a meta page, a page of
/// the main or the free DB, or in exactly one free record (C's
/// `check-freelist` in spirit, for an environment with the main DB only).
/// A count check: the free records' pages are distinct and in range, and
/// together with the trees' page counts cover the file exactly. The
/// crate's own page audit, which walks the trees, runs in the unit test
/// `split_free_pages_audit`.
fn check_pages(env: &Environment) {
    let txn = env.begin_ro_txn().unwrap();
    let last = env.info().unwrap().last_pgno as u64;
    let in_trees: usize = [Dbi::MAIN, Dbi::FREE]
        .iter()
        .map(|&db| {
            let st = txn.stat(db).unwrap();
            st.branch_pages + st.leaf_pages + st.overflow_pages
        })
        .sum();
    drop(txn);
    let mut seen = HashSet::new();
    for (id, pages) in free_records(env) {
        assert!(pages.windows(2).all(|w| w[0] > w[1]), "record {id}");
        for p in pages {
            assert!((2..=last).contains(&p), "page {p} of record {id}");
            assert!(seen.insert(p), "page {p} twice in the free DB");
        }
    }
    assert_eq!(2 + in_trees + seen.len(), last as usize + 1, "pages lost");
}

/// The first `n` pages of the data file of an environment in `dir`.
fn file_pages(dir: &Path, ps: usize, n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut buf = vec![0; n * ps];
    std::fs::File::open(dir.join("data.mdb"))
        .unwrap()
        .read_exact(&mut buf)
        .unwrap();
    buf
}

/// The two meta pages in the data file of `env` in `dir` (page 0, page 1).
fn metas(env: &Environment, dir: &Path) -> [Meta; 2] {
    let ps = env.page_size();
    let file = file_pages(dir, ps, 2);
    let hdr = meta_header(env);
    [0, 1].map(|i| Meta::read(&file[i * ps + hdr..]).unwrap())
}

/// The txnid stamped on page `pgno` of the data file of a 1.0 environment
/// in `dir` (`mp_txnid`, at offset 8 of the page header).
fn page_stamp(dir: &Path, ps: usize, pgno: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(dir.join("data.mdb")).unwrap();
    f.seek(SeekFrom::Start(pgno * ps as u64 + 8)).unwrap();
    let mut buf = [0; 8];
    f.read_exact(&mut buf).unwrap();
    u64::from_ne_bytes(buf)
}

/// The page header before the meta record, for the format of `env`.
fn meta_header(env: &Environment) -> usize {
    match env.format() {
        Format::V09 => 16,
        _ => 24,
    }
}

/// After a split commit that began as `t` and published `f`: meta page
/// `f & 1` holds `f`, the other `f - 1`, both the same snapshot; `f - 1`
/// has `t`'s parity.
fn check_pair(env: &Environment, dir: &Path, t: u64, f: u64) {
    let m = metas(env, dir);
    let (second, first) = (m[(f & 1) as usize], m[((f - 1) & 1) as usize]);
    assert_eq!((second.txnid, first.txnid), (f, f - 1), "{t}: {m:?}");
    assert_eq!(first.txnid & 1, t & 1, "the parity of the first meta page");
    assert_eq!(first.dbs, second.dbs, "the same snapshot twice");
    assert_eq!(first.last_pgno, second.last_pgno);
    assert_eq!(first.map_size, second.map_size);
    assert_eq!(env.info().unwrap().last_txnid as u64, f);
    if env.format() == Format::V10 {
        // The pages carry the first txnid: a writer under PREV_SNAPSHOT
        // begins as `f` and must not take them for its own.
        let free_root = first.dbs[0].root;
        assert_eq!(page_stamp(dir, env.page_size(), free_root), f - 1);
    }
}

/// `rollback(txnid)` is refused.
fn cant_rollback(env: &Environment, txnid: u64) {
    assert!(
        matches!(
            env.rollback(txnid),
            Err(rlmdb::Error::Mdb(MdbError::CantRollback))
        ),
        "rollback({txnid})"
    );
}

/// Deletes values `from .. from + count` and prepares; returns the ID the
/// transaction had while open and the prepared transaction's ID, after
/// committing.
fn delete_prepared(env: &Environment, from: u32, count: u32) -> (u64, u64) {
    let mut txn = env.begin_rw_txn().unwrap();
    for i in from..from + count {
        assert!(txn.del(Dbi::MAIN, &key(i), None).unwrap());
    }
    let open_id = txn.id();
    let prepared = txn.prepare().unwrap();
    let id = prepared.id();
    prepared.commit().unwrap();
    (open_id, id)
}

/// Checks the records a split commit of a transaction that began as `t`
/// and published `f` left: keys `t .. t + n` with `n > 1` and
/// `f == t + (n | 1)` (the first meta page's txnid `f - 1` is the last key
/// or one above it), full records but the last, the lowest pages under
/// the lowest key. Returns `n`.
fn check_split(env: &Environment, t: u64, f: u64) -> u64 {
    let max = maxfree(env);
    let recs = free_records(env);
    let split: Vec<(&u64, &Vec<u64>)> = recs.range(t..).collect();
    let n = split.len() as u64;
    assert!(n > 1, "{n} records from {t}");
    let keys: Vec<u64> = split.iter().map(|(k, _)| **k).collect();
    assert_eq!(keys, (t..t + n).collect::<Vec<_>>());
    assert_eq!(f, t + (n | 1), "published ID of {n} records from {t}");
    assert_ne!(f & 1, t & 1, "the second meta page has the other parity");
    assert!(t + n - 1 < f, "every key below the published ID");
    for (i, (id, pages)) in split.iter().enumerate() {
        if i + 1 < split.len() {
            assert_eq!(pages.len(), max, "record {id} is not full");
        } else {
            assert!(!pages.is_empty() && pages.len() <= max, "record {id}");
        }
    }
    for w in split.windows(2) {
        // Descending lists: the first entry is the highest page.
        assert!(w[0].1[0] < *w[1].1.last().unwrap(), "pages out of order");
    }
    n
}

#[test]
fn off_by_default_one_record() {
    for (format, psize) in layouts() {
        let dir = common::tempdir();
        let env = builder(format, psize, false).open(dir.path()).unwrap();
        assert!(!env.has_feature(Feature::SplitFreelist));
        let count = values_for(&env, 2.5);
        fill(&env, 0, count, 1);
        let (open_id, id) = delete_prepared(&env, 0, count);
        assert_eq!(open_id, id, "{format:?} {psize:?}");
        assert_eq!(env.info().unwrap().last_txnid as u64, id);
        let recs = free_records(&env);
        assert!(recs[&id].len() > 2 * maxfree(&env), "one big record");
        assert!(recs.range(id + 1..).next().is_none());
        check_pages(&env);
    }
}

#[test]
fn split_records_and_the_commit_id() {
    for (format, psize) in layouts() {
        // 0.9 at the OS page size needs megabytes per record: one size.
        let sizes: &[f64] = match format {
            Format::V09 => &[2.5],
            _ => &[1.5, 2.5, 3.5, 4.5, 7.2],
        };
        let mut parities = HashSet::new();
        for &records in sizes {
            let dir = common::tempdir();
            let env = builder(format, psize, true).open(dir.path()).unwrap();
            assert!(env.has_feature(Feature::SplitFreelist));
            let count = values_for(&env, records);
            fill(&env, 0, count, 1);
            let (t, f) = delete_prepared(&env, 0, count);
            let n = check_split(&env, t, f);
            parities.insert(n & 1);
            // Both meta pages hold the snapshot: the first under `T`'s
            // parity, the second under the next txnid, which is published.
            check_pair(&env, dir.path(), t, f);
            let info = env.info().unwrap();
            assert_eq!(info.last_txnid as u64, f, "{format:?} {psize:?} {records}");
            check_pages(&env);
            // The next transaction follows the ID published.
            let txn = env.begin_rw_txn().unwrap();
            assert_eq!(txn.id(), f + 1);
            drop(txn);
            assert_eq!(env.begin_ro_txn().unwrap().id(), f);
            // And so after a reopen.
            drop(env);
            let env = builder(format, psize, false).open(dir.path()).unwrap();
            assert_eq!(env.info().unwrap().last_txnid as u64, f);
            assert_eq!(env.begin_rw_txn().unwrap().id(), f + 1);
            check_pages(&env);
        }
        if format == Format::V10 {
            assert_eq!(parities.len(), 2, "both parities of n");
        }
    }
}

#[test]
fn one_record_where_it_fits() {
    for (format, psize) in layouts() {
        let dir = common::tempdir();
        let env = builder(format, psize, true).open(dir.path()).unwrap();
        // Well within one page of record.
        let count = values_for(&env, 0.5);
        fill(&env, 0, count, 1);
        let (open_id, id) = delete_prepared(&env, 0, count);
        assert_eq!(open_id, id);
        assert!(free_records(&env).range(id + 1..).next().is_none());
        check_pages(&env);
        // A transaction without changes prepares as an empty commit.
        let txn = env.begin_rw_txn().unwrap();
        let open_id = txn.id();
        let prepared = txn.prepare().unwrap();
        assert_eq!(prepared.id(), open_id);
        prepared.commit().unwrap();
        assert_eq!(env.info().unwrap().last_txnid as u64, id);
    }
}

#[test]
fn a_plain_commit_splits_too() {
    let dir = common::tempdir();
    let env = builder(Format::V10, Some(512), true)
        .open(dir.path())
        .unwrap();
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 0..count {
        txn.del(Dbi::MAIN, &key(i), None).unwrap();
    }
    let t = txn.id();
    txn.commit().unwrap();
    let f = env.info().unwrap().last_txnid as u64;
    assert!(f > t);
    check_split(&env, t, f);
    check_pages(&env);
}

#[test]
fn nested_transactions() {
    let dir = common::tempdir();
    let env = builder(Format::V10, Some(512), true)
        .open(dir.path())
        .unwrap();
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let mut txn = env.begin_rw_txn().unwrap();
    let t = txn.id();
    {
        let mut child = txn.begin_nested_txn().unwrap();
        assert_eq!(child.id(), t);
        for i in 0..count / 2 {
            child.del(Dbi::MAIN, &key(i), None).unwrap();
        }
        child.commit().unwrap();
    }
    {
        // An aborted child frees nothing.
        let mut child = txn.begin_nested_txn().unwrap();
        for i in count / 2..count {
            child.del(Dbi::MAIN, &key(i), None).unwrap();
        }
    }
    for i in count / 2..count {
        txn.del(Dbi::MAIN, &key(i), None).unwrap();
    }
    assert_eq!(txn.id(), t);
    let prepared = txn.prepare().unwrap();
    let f = prepared.id();
    prepared.commit().unwrap();
    check_split(&env, t, f);
    check_pages(&env);
}

/// A reader of the snapshot before a split commit keeps all its pages,
/// however much later writers write; once it is gone, the pages are
/// reused.
#[test]
fn a_reader_keeps_its_pages() {
    for (format, psize) in layouts().into_iter().skip(1) {
        let dir = common::tempdir();
        let env = builder(format, psize, true).open(dir.path()).unwrap();
        let len = big(&env);
        let count = values_for(&env, 4.5);
        fill(&env, 0, count, 1);
        let reader = env.begin_ro_txn().unwrap();
        let (t, f) = delete_prepared(&env, 0, count);
        check_split(&env, t, f);
        for round in 2..6u8 {
            fill(&env, count, count, round);
            let mut txn = env.begin_rw_txn().unwrap();
            for i in count..2 * count {
                txn.del(Dbi::MAIN, &key(i), None).unwrap();
            }
            txn.commit().unwrap();
            check_pages(&env);
        }
        for i in 0..count {
            let v = reader.get(Dbi::MAIN, &key(i)).unwrap();
            assert_eq!(v, Some(&value(i, 1, len)[..]), "value {i} changed");
        }
        drop(reader);
        // Without the reader, the pages of the old values are reused.
        let last = env.info().unwrap().last_pgno;
        for round in 6..9u8 {
            fill(&env, 0, count, round);
            let mut txn = env.begin_rw_txn().unwrap();
            for i in 0..count {
                txn.del(Dbi::MAIN, &key(i), None).unwrap();
            }
            txn.commit().unwrap();
            check_pages(&env);
        }
        assert_eq!(env.info().unwrap().last_pgno, last, "{format:?} {psize:?}");
    }
}

/// The writer after a split commit may reuse the pages it freed (those
/// below its ID), where after a single record it must wait a commit more:
/// writing the same data again grows the file less with the feature.
#[test]
fn freed_pages_are_reused_sooner() {
    let growth = |split: bool| {
        let dir = common::tempdir();
        let env = builder(Format::V10, Some(1024), split)
            .open(dir.path())
            .unwrap();
        let count = values_for(&env, 6.0);
        fill(&env, 0, count, 1);
        let before = env.info().unwrap().last_pgno;
        delete_prepared(&env, 0, count);
        fill(&env, 0, count, 2);
        check_pages(&env);
        env.info().unwrap().last_pgno - before
    };
    let (with, without) = (growth(true), growth(false));
    assert!(
        with * 4 < without,
        "grew {with} pages with, {without} without"
    );
}

/// A split commit cannot be rolled back: both meta pages hold its
/// snapshot, so a rollback could only step back to the same data, which
/// is refused (C 1.0's rollback would step back and report success). A
/// program that needs an undo sees at the prepare that the ID rose, and
/// aborts the prepared transaction instead: that always works.
#[test]
fn a_split_commit_cannot_be_rolled_back() {
    for (format, psize) in layouts().into_iter().skip(1) {
        let dir = common::tempdir();
        let env = builder(format, psize, true).open(dir.path()).unwrap();
        let len = big(&env);
        let count = values_for(&env, 4.5);
        fill(&env, 0, count, 1);
        let before = env.info().unwrap();

        // The two-phase pattern: the ID rose, so abort instead.
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..count {
            txn.del(Dbi::MAIN, &key(i), None).unwrap();
        }
        let t = txn.id();
        let prepared = txn.prepare().unwrap();
        assert!(prepared.id() > t);
        prepared.abort();
        assert_eq!(env.info().unwrap().last_txnid, before.last_txnid);
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(
            txn.get(Dbi::MAIN, &key(0)).unwrap(),
            Some(&value(0, 1, len)[..])
        );
        drop(txn);
        check_pages(&env);

        // Committed: neither ID of the pair, nor the one it began with,
        // can be rolled back, and the data stays.
        let (t, f) = delete_prepared(&env, 0, count);
        check_split(&env, t, f);
        for id in [f, f - 1, t, t + 1] {
            cant_rollback(&env, id);
        }
        check_pair(&env, dir.path(), t, f);
        assert_eq!(
            env.begin_ro_txn().unwrap().get(Dbi::MAIN, &key(0)).unwrap(),
            None
        );
        check_pages(&env);
        // The next writer follows the published ID.
        assert_eq!(env.begin_rw_txn().unwrap().id(), f + 1);
    }
}

/// The case a single meta page would get wrong: after the split commit,
/// the next writer reuses the pages of the snapshot before it,
/// writes them (a prepare) and does not commit. The previous meta page
/// holds the split commit's own snapshot, so `PREV_SNAPSHOT` and a
/// rollback find intact data, not the overwritten snapshot.
#[test]
fn an_aborting_successor_leaves_the_previous_meta_page_intact() {
    for (format, psize) in layouts() {
        let dir = common::tempdir();
        let env = builder(format, psize, true).open(dir.path()).unwrap();
        let len = big(&env);
        let count = values_for(&env, 3.5);
        fill(&env, 0, count, 1);
        let (t, f) = delete_prepared(&env, 0, count);
        check_split(&env, t, f);

        // The successor reuses the freed pages (same keys, same sizes),
        // writes them, then gives up.
        let mut txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), f + 1);
        for i in 0..count {
            txn.put(Dbi::MAIN, &key(i), &value(i, 2, len), WriteFlags::empty())
                .unwrap();
        }
        txn.prepare().unwrap().abort();
        assert_eq!(env.info().unwrap().last_txnid as u64, f);
        check_pages(&env);
        cant_rollback(&env, f);
        drop(env);

        // The previous snapshot: the split commit's, with the values gone.
        let mut b = builder(format, psize, false);
        b.flags(EnvironmentFlags::PREV_SNAPSHOT | EnvironmentFlags::READ_ONLY);
        let prev = b.open(dir.path()).unwrap();
        assert_eq!(prev.info().unwrap().last_txnid as u64, f - 1);
        let txn = prev.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), f - 1);
        let mut c = txn.cursor(Dbi::MAIN).unwrap();
        let mut n = 0;
        while let Some((k, v)) = c.next().unwrap() {
            n += 1;
            assert!(u32::from_be_bytes(k.try_into().unwrap()) >= count || v.is_empty());
        }
        assert_eq!(n, 0, "{format:?} {psize:?}: the deleted values are back");
        drop(txn);
        drop(prev);

        // And the newest one, as before.
        let env = builder(format, psize, false).open(dir.path()).unwrap();
        check_pair(&env, dir.path(), t, f);
        check_pages(&env);
    }
}

/// A process died between the two meta writes (here: the second page is
/// put back by hand): the file holds the split commit's snapshot and the
/// one before it, two or more txnids apart. Nothing repairs it: an
/// environment opened with `PREV_SNAPSHOT` reads the old snapshot, a
/// writer begins after the newest one without writing anything, and its
/// commit replaces the old page.
#[test]
fn a_half_written_pair_is_left_as_it_is() {
    for (format, psize) in layouts() {
        let dir = common::tempdir();
        let env = builder(format, psize, true).open(dir.path()).unwrap();
        let len = big(&env);
        let count = values_for(&env, 2.5);
        fill(&env, 0, count, 1);
        let old_id = env.info().unwrap().last_txnid as u64;
        let ps = env.page_size();
        let old_page = (old_id & 1) as usize;
        let file = dir.path().join("data.mdb");
        let before = file_pages(dir.path(), ps, 2)[old_page * ps..(old_page + 1) * ps].to_vec();
        let (t, f) = delete_prepared(&env, 0, count);
        check_split(&env, t, f);
        assert_eq!((f & 1) as usize, old_page);
        drop(env);

        // Put the old snapshot back: the file as the dying process left it.
        write_page(&file, ps, old_page, &before);

        // The old snapshot can still be read on purpose, intact.
        let mut b = builder(format, psize, false);
        b.flags(EnvironmentFlags::PREV_SNAPSHOT | EnvironmentFlags::READ_ONLY);
        let prev = b.open(dir.path()).unwrap();
        assert_eq!(prev.info().unwrap().last_txnid as u64, old_id);
        let txn = prev.begin_ro_txn().unwrap();
        assert_eq!(
            txn.get(Dbi::MAIN, &key(0)).unwrap(),
            Some(&value(0, 1, len)[..])
        );
        drop(txn);
        drop(prev);

        // A writer begins after the newest snapshot and writes nothing
        // before its commit, which replaces the old page.
        let env = builder(format, psize, false).open(dir.path()).unwrap();
        let pair = |env: &Environment| {
            let m = metas(env, dir.path());
            [m[old_page].txnid, m[old_page ^ 1].txnid]
        };
        assert_eq!(pair(&env), [old_id, f - 1]);
        assert_eq!(env.info().unwrap().last_txnid as u64, f - 1);
        let mut txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), f);
        assert_eq!(pair(&env), [old_id, f - 1]);
        txn.put(Dbi::MAIN, &key(0), &value(0, 2, len), WriteFlags::empty())
            .unwrap();
        txn.commit().unwrap();
        assert_eq!(pair(&env), [f, f - 1]);
        check_pages(&env);
    }
}

/// Writes `page` (of `ps` bytes) as page `pgno` of the data file `file`.
fn write_page(file: &Path, ps: usize, pgno: usize, page: &[u8]) {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(file).unwrap();
    f.seek(SeekFrom::Start((pgno * ps) as u64)).unwrap();
    f.write_all(page).unwrap();
    f.sync_all().unwrap();
}

/// The first meta write of a split commit fails (the sync after it,
/// injected): as for any commit, the error is returned, nothing is
/// published, the data is as before, and the environment is unusable in
/// this process. The next writer begins at the ID the failed one began
/// with.
#[test]
fn a_failed_first_meta_write_fails_the_commit() {
    let dir = common::tempdir();
    let env = builder(Format::V10, Some(1024), true)
        .open(dir.path())
        .unwrap();
    let len = big(&env);
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let old_id = env.info().unwrap().last_txnid as u64;
    let mut txn = env.begin_rw_txn().unwrap();
    for i in 0..count {
        txn.del(Dbi::MAIN, &key(i), None).unwrap();
    }
    let t = txn.id();
    let prepared = txn.prepare().unwrap();
    assert!(prepared.id() > t);
    env.fail_next_meta_sync(libc::EIO);
    assert!(prepared.commit().is_err());
    assert!(matches!(
        env.begin_ro_txn().map(drop),
        Err(rlmdb::Error::Mdb(MdbError::Panic))
    ));
    drop(env);
    let env = builder(Format::V10, Some(1024), true)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.info().unwrap().last_txnid as u64, old_id);
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(
        txn.get(Dbi::MAIN, &key(0)).unwrap(),
        Some(&value(0, 1, len)[..])
    );
    drop(txn);
    assert_eq!(env.begin_rw_txn().unwrap().id(), t);
    check_pages(&env);
}

/// The second meta write fails and the page did not reach the file (the
/// injected failure puts the old page back): the commit stands under the
/// first txnid, which stays published, and returns success. The
/// environment is unusable in this process. A reopened environment finds
/// the commit, and the next commit takes the txnid the prepare reported.
#[test]
fn a_failed_second_meta_write_that_did_not_land() {
    let dir = common::tempdir();
    let env = std::sync::Arc::new(
        builder(Format::V10, Some(1024), true)
            .open(dir.path())
            .unwrap(),
    );
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let old_id = env.info().unwrap().last_txnid as u64;
    let ps = env.page_size();
    let old_page = (old_id & 1) as usize;
    let file = dir.path().join("data.mdb");
    let before = file_pages(dir.path(), ps, 2)[old_page * ps..(old_page + 1) * ps].to_vec();
    // After the first write: the second fails, and the disk "loses" it.
    let hooked = std::sync::Arc::clone(&env);
    let restore = file.clone();
    env.after_next_meta_write(move || {
        hooked.fail_next_meta_sync(libc::EIO);
        hooked.after_next_meta_write(move || write_page(&restore, ps, old_page, &before));
    });
    let (t, f) = delete_prepared(&env, 0, count);
    assert_eq!(env.info().unwrap().last_txnid as u64, f - 1);
    let pair = |env: &Environment| {
        let m = metas(env, dir.path());
        [m[old_page].txnid, m[old_page ^ 1].txnid]
    };
    assert_eq!(pair(&env), [old_id, f - 1]);
    assert!(matches!(
        env.begin_rw_txn().map(drop),
        Err(rlmdb::Error::Mdb(MdbError::Panic))
    ));
    drop(env);
    let again = builder(Format::V10, Some(1024), true)
        .open(dir.path())
        .unwrap();
    assert_eq!(again.info().unwrap().last_txnid as u64, f - 1);
    check_split(&again, t, f);
    let mut txn = again.begin_rw_txn().unwrap();
    assert_eq!(txn.id(), f);
    txn.put(
        Dbi::MAIN,
        &key(0),
        &value(0, 2, big(&again)),
        WriteFlags::empty(),
    )
    .unwrap();
    txn.commit().unwrap();
    assert_eq!(pair(&again), [f, f - 1]);
    check_pages(&again);
}

/// The second meta write fails (the sync after it, injected): the commit
/// stands, returns success, and the environment is unusable in this
/// process. Here the page did reach the file, so the second txnid is
/// published after all; a reopened environment finds the pair complete.
#[test]
fn a_failed_second_meta_write_keeps_the_commit() {
    let dir = common::tempdir();
    let env = std::sync::Arc::new(
        builder(Format::V10, Some(1024), true)
            .open(dir.path())
            .unwrap(),
    );
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    // The first meta write succeeds; the hook then makes the next one
    // (the duplicate's) fail.
    let hooked = std::sync::Arc::clone(&env);
    env.after_next_meta_write(move || hooked.fail_next_meta_sync(libc::EIO));
    let (t, f) = delete_prepared(&env, 0, count);
    // The commit returned success; the environment is unusable in this
    // process, for writing and reading, until it is opened again.
    for r in [env.begin_rw_txn().map(drop), env.begin_ro_txn().map(drop)] {
        assert!(matches!(r, Err(rlmdb::Error::Mdb(MdbError::Panic))));
    }
    drop(env);
    let again = builder(Format::V10, Some(1024), false)
        .open(dir.path())
        .unwrap();
    check_split(&again, t, f);
    check_pair(&again, dir.path(), t, f);
    assert_eq!(again.begin_rw_txn().unwrap().id(), f + 1);
    check_pages(&again);
}

/// `NO_LOCK`: another process did the split commit, which this process
/// does not learn of (no lock file), so a reader here still starts from
/// the txnid it last saw. Its meta page now holds the split commit's
/// snapshot, not the one its txnid names, and a writer of this process,
/// which reads the file's newest meta page, reuses the old snapshot's
/// pages meanwhile. The reader sees intact data (the committed state)
/// throughout. With a single meta write it would read the old snapshot
/// while the writer overwrote it.
#[test]
fn nolock_stale_reader_keeps_intact_data() {
    let dir = common::tempdir();
    let b = nolock_builder();
    let env = b.open(dir.path()).unwrap();
    let len = big(&env);
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let old_id = env.info().unwrap().last_txnid as u64;
    let f = helper_split(dir.path());

    let mut w = env.begin_rw_txn().unwrap();
    assert_eq!(
        w.id(),
        f + 1,
        "the writer begins after the file's newest commit"
    );
    for i in 0..count {
        w.put(Dbi::MAIN, &key(i), &value(i, 2, len), WriteFlags::empty())
            .unwrap();
    }
    let r = env.begin_ro_txn().unwrap();
    assert!(r.id() == old_id || r.id() == f, "{}", r.id());
    let seen: Vec<Option<Vec<u8>>> = (0..count)
        .map(|i| r.get(Dbi::MAIN, &key(i)).unwrap().map(<[u8]>::to_vec))
        .collect();
    // The committed state (the values deleted), never the old snapshot.
    assert!(
        seen.iter().all(Option::is_none),
        "the reader found the deleted values"
    );
    let p = w.prepare().unwrap();
    for i in 0..count {
        assert_eq!(
            r.get(Dbi::MAIN, &key(i)).unwrap(),
            None,
            "value {i} appeared"
        );
    }
    p.abort();
    drop(r);
    check_pages(&env);
}

fn nolock_builder() -> EnvironmentBuilder {
    let mut b = builder(Format::V10, Some(1024), true);
    b.flags(EnvironmentFlags::NO_LOCK);
    b
}

const HELPER: &str = "RLMDB_SPLIT_FREELIST_HELPER";

/// Runs the helper process on `dir`: it deletes every value in one split
/// commit, without a lock file. Returns the ID it published.
fn helper_split(dir: &Path) -> u64 {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "helper_process",
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(HELPER, dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    // On the harness's own line with `--nocapture`.
    let tag = "helper: split ";
    let line = stdout
        .lines()
        .find_map(|l| l.find(tag).map(|i| &l[i + tag.len()..]))
        .unwrap_or_else(|| panic!("{stdout}\n{stderr}"));
    line.trim().parse().unwrap()
}

#[test]
#[ignore = "helper process of nolock_stale_reader_keeps_intact_data"]
fn helper_process() {
    let Ok(dir) = std::env::var(HELPER) else {
        return;
    };
    let env = nolock_builder().open(PathBuf::from(dir)).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let keys: Vec<Vec<u8>> = {
        let mut c = txn.cursor(Dbi::MAIN).unwrap();
        let mut keys = Vec::new();
        while let Some((k, _)) = c.next().unwrap() {
            keys.push(k.to_vec());
        }
        keys
    };
    for k in &keys {
        txn.del(Dbi::MAIN, k, None).unwrap();
    }
    let t = txn.id();
    let p = txn.prepare().unwrap();
    let f = p.id();
    assert!(f > t);
    p.commit().unwrap();
    println!("helper: split {f}");
}

#[test]
fn without_a_lock_file() {
    let dir = common::tempdir();
    let mut b = builder(Format::V10, Some(512), true);
    b.flags(EnvironmentFlags::NO_SYNC | EnvironmentFlags::NO_LOCK);
    let env = b.open(dir.path()).unwrap();
    let other = b.open(dir.path()).unwrap();
    let count = values_for(&env, 3.5);
    fill(&env, 0, count, 1);
    let (t, f) = delete_prepared(&env, 0, count);
    check_split(&env, t, f);
    for e in [&env, &other] {
        let txn = e.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), f);
        assert_eq!(txn.get(Dbi::MAIN, &key(0)).unwrap(), None);
        drop(txn);
        let txn = e.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), f + 1);
    }
    check_pages(&env);
}

#[test]
fn with_no_meta_sync() {
    let dir = common::tempdir();
    let mut b = builder(Format::V09, None, true);
    b.flags(EnvironmentFlags::NO_META_SYNC);
    let env = b.open(dir.path()).unwrap();
    let count = values_for(&env, 2.5);
    fill(&env, 0, count, 1);
    let (t, f) = delete_prepared(&env, 0, count);
    check_split(&env, t, f);
    drop(env);
    let env = b.open(dir.path()).unwrap();
    assert_eq!(env.info().unwrap().last_txnid as u64, f);
    check_pages(&env);
}

/// Many transactions that free and reclaim pages at once, with readers
/// that pin snapshots across some of them; every page accounted for and
/// every reader's data intact.
#[test]
fn churn() {
    for psize in [256, 1024] {
        let dir = common::tempdir();
        let env = builder(Format::V10, Some(psize), true)
            .open(dir.path())
            .unwrap();
        let mut state = 0x5eed_u64;
        let mut model: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut readers = Vec::new();
        let mut splits = 0;
        for round in 0..60u32 {
            let mut txn = env.begin_rw_txn().unwrap();
            let t = txn.id();
            let n = 1 + common::dataset::splitmix64(&mut state) % 400;
            for _ in 0..n {
                let r = common::dataset::splitmix64(&mut state);
                let k = (r % 600) as u32;
                if r & (1 << 40) != 0 && model.contains_key(&k) {
                    txn.del(Dbi::MAIN, &key(k), None).unwrap();
                    model.remove(&k);
                } else {
                    let len = 8 + (r >> 20) as usize % (3 * psize);
                    let v = value(k, round as u8, len);
                    txn.put(Dbi::MAIN, &key(k), &v, WriteFlags::empty())
                        .unwrap();
                    model.insert(k, v);
                }
            }
            if round % 7 == 3 {
                // A big delete: half of what there is.
                let keys: Vec<u32> = model.keys().copied().step_by(2).collect();
                for k in keys {
                    txn.del(Dbi::MAIN, &key(k), None).unwrap();
                    model.remove(&k);
                }
            }
            let prepared = txn.prepare().unwrap();
            let f = prepared.id();
            prepared.commit().unwrap();
            if f != t {
                splits += 1;
                check_split(&env, t, f);
            }
            check_pages(&env);
            if round % 5 == 0 {
                readers.push((env.begin_ro_txn().unwrap(), model.clone()));
            }
            if readers.len() > 3 {
                let (txn, expect) = readers.remove(0);
                check_reader(&txn, &expect);
            }
        }
        for (txn, expect) in &readers {
            check_reader(txn, expect);
        }
        assert!(splits >= 5, "only {splits} split commits");
    }
}

/// The reader `txn` sees exactly `expect`.
fn check_reader(txn: &rlmdb::RoTxn<'_>, expect: &BTreeMap<u32, Vec<u8>>) {
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut got = BTreeMap::new();
    while let Some((k, v)) = c.next().unwrap() {
        got.insert(u32::from_be_bytes(k.try_into().unwrap()), v.to_vec());
    }
    assert!(got == *expect, "a reader's snapshot changed");
}
