//! Model-based randomized test of the write path.
//!
//! Random write transactions drive the library and an in-memory model of
//! every database (`BTreeMap` of keys to sets of data items, ordered with
//! the comparators C uses, `rlmdb::Comparator`), with deterministic
//! seeds:
//!
//! * puts with every flag (`NO_OVERWRITE`, `NO_DUP_DATA`, `APPEND`,
//!   `APPEND_DUP`), `MDB_RESERVE`, `MDB_MULTIPLE`, values from empty to
//!   several overflow pages, deletes with and without data;
//! * several live cursors per database, positioned and moved with every
//!   cursor operation and written (`MDB_CURRENT` included) and deleted
//!   through; after every change, made through any of them or the
//!   transaction, each is checked against the model: a cursor stays on its
//!   item, and a cursor whose item was deleted sits before the item's
//!   successor (`MDB_NEXT` returns the successor, `MDB_PREV` the
//!   predecessor);
//! * nested transactions up to three deep, committed or aborted, aborted
//!   top-level transactions, `clear_db`/`drop_db` and re-creation;
//! * small maps that fill up (`MDB_MAP_FULL`: the transaction must then
//!   refuse to work and to commit) and grow with `set_map_size`, small
//!   dirty-page limits so that transactions spill, environment reopens.
//!
//! The databases cover `DUP_SORT`, `DUP_FIXED`, `INTEGER_KEY`,
//! `INTEGER_DUP` (4- and 8-byte, with and without `DUP_FIXED`),
//! `REVERSE_KEY` and `REVERSE_DUP`. Long keys make trees three or four
//! levels deep, so branch pages split and merge too.
//!
//! After every commit (and abort) the content and entry count of the
//! databases are compared with the model by cursor walks in both
//! directions; now and then the C page audit (`cpeer check-freelist`) runs
//! and C's `mdb_dump` must agree with the model.
//!
//! Runs seed after seed for about `MODEL_SECS` seconds (default 30).
//! `MODEL_SEED=<n>` runs only seed `n`, `MODEL_TXNS=<n>` sets the number of
//! transactions per seed (default 150), `MODEL_LOG=1` prints every
//! operation.

mod common;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound::{Excluded, Included, Unbounded};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{CTools, Rng};
use rlmdb::{
    Comparator, CursorId, CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error,
    MdbError, RwTxn, Transaction, WriteFlags,
};

type Pair = (Vec<u8>, Vec<u8>);

// ---------------------------------------------------------------------------
// The databases
// ---------------------------------------------------------------------------

/// How a database's keys look.
#[derive(Clone, Copy)]
enum Keys {
    /// Text keys `prefix/0000123` padded to up to `max` bytes; `n` of them.
    Text { n: u64, max: usize },
    /// Native 4-byte integers.
    U32 { n: u64 },
    /// Native 8-byte integers.
    U64 { n: u64 },
}

/// How a database's data items look.
#[derive(Clone, Copy)]
enum Vals {
    /// Anything from empty to three overflow pages (not `DUP_SORT`).
    Any,
    /// `n` different items of 1 to `max` bytes.
    Dups { n: u64, max: usize },
    /// `n` different items of `size` bytes (`DUP_FIXED`).
    Fixed { n: u64, size: usize },
    /// Native 4-byte integers.
    U32 { n: u64 },
    /// Native 8-byte integers.
    U64 { n: u64 },
}

struct Cfg {
    name: &'static str,
    flags: DatabaseFlags,
    keys: Keys,
    vals: Vals,
    /// Emptied and deleted often.
    scratch: bool,
}

fn configs() -> Vec<Cfg> {
    use DatabaseFlags as F;
    let cfg = |name, flags, keys, vals| Cfg {
        name,
        flags,
        keys,
        vals,
        scratch: false,
    };
    vec![
        cfg(
            "plain",
            F::empty(),
            Keys::Text { n: 6000, max: 511 },
            Vals::Any,
        ),
        cfg("intkey", F::INTEGER_KEY, Keys::U64 { n: 8000 }, Vals::Any),
        cfg(
            "revkey",
            F::REVERSE_KEY,
            Keys::Text { n: 3000, max: 300 },
            Vals::Any,
        ),
        cfg(
            "dups",
            F::DUP_SORT,
            Keys::Text { n: 400, max: 200 },
            Vals::Dups { n: 3000, max: 511 },
        ),
        cfg(
            "revdups",
            F::DUP_SORT | F::REVERSE_KEY | F::REVERSE_DUP,
            Keys::Text { n: 300, max: 120 },
            Vals::Dups { n: 1000, max: 200 },
        ),
        cfg(
            "fixed",
            F::DUP_SORT | F::DUP_FIXED,
            Keys::Text { n: 200, max: 100 },
            Vals::Fixed { n: 20000, size: 12 },
        ),
        cfg(
            "intdup4",
            F::DUP_SORT | F::DUP_FIXED | F::INTEGER_DUP | F::INTEGER_KEY,
            Keys::U32 { n: 300 },
            Vals::U32 { n: 50000 },
        ),
        cfg(
            "intdup8",
            F::DUP_SORT | F::DUP_FIXED | F::INTEGER_DUP,
            Keys::Text { n: 100, max: 60 },
            Vals::U64 { n: 50000 },
        ),
        cfg(
            "cintdup",
            F::DUP_SORT | F::INTEGER_DUP,
            Keys::Text { n: 200, max: 80 },
            Vals::U64 { n: 5000 },
        ),
        Cfg {
            scratch: true,
            ..cfg(
                "scratch",
                F::DUP_SORT,
                Keys::Text { n: 500, max: 100 },
                Vals::Dups { n: 500, max: 300 },
            )
        },
        Cfg {
            scratch: true,
            ..cfg(
                "scratch2",
                F::empty(),
                Keys::Text { n: 500, max: 511 },
                Vals::Any,
            )
        },
    ]
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^ (x >> 33)
}

/// `len` pseudo-random bytes from `seed`.
fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (x >> 56) as u8
        })
        .collect()
}

const PREFIXES: [&[u8]; 6] = [
    b"k",
    b"user",
    b"zz",
    b"m",
    b"pppppppppppppppppppppppppppppppppppppppppppp",
    b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
];

/// Text key number `i`: a prefix without `/`, `/`, seven digits, padding.
/// Different numbers give different keys.
fn text_key(i: u64, max: usize) -> Vec<u8> {
    let h = mix(i ^ 0x1234);
    let np = if max >= 100 { 6 } else { 4 };
    let mut k = PREFIXES[(h % np) as usize].to_vec();
    k.push(b'/');
    k.extend_from_slice(format!("{i:07}").as_bytes());
    let min = k.len();
    let target = match (h >> 8) % 10 {
        0..=3 => min,
        4..=6 => min + ((h >> 16) % 40) as usize,
        _ => max - ((h >> 16) % (max as u64 / 2)) as usize,
    }
    .clamp(min, max.max(min));
    let mut j = 0;
    while k.len() < target {
        k.push(b'a' + (mix(h ^ j) % 26) as u8);
        j += 1;
    }
    k
}

/// A value for a database without duplicates.
fn any_val(rng: &mut Rng, ps: usize) -> Vec<u8> {
    let len = match rng.below(100) {
        0..=3 => 0,
        4..=49 => 1 + rng.below(24),
        50..=79 => 25 + rng.below(300),
        80..=89 => 300 + rng.below(1700),
        _ => ps / 2 + rng.below(2 * ps), // on overflow pages
    };
    bytes(rng.next(), len)
}

impl Cfg {
    fn dupsort(&self) -> bool {
        self.flags.contains(DatabaseFlags::DUP_SORT)
    }

    fn dupfixed(&self) -> bool {
        self.flags.contains(DatabaseFlags::DUP_FIXED)
    }

    fn key_count(&self) -> u64 {
        match self.keys {
            Keys::Text { n, .. } | Keys::U32 { n } | Keys::U64 { n } => n,
        }
    }

    fn key(&self, i: u64) -> Vec<u8> {
        match self.keys {
            Keys::Text { max, .. } => text_key(i, max),
            Keys::U32 { .. } => (i as u32)
                .wrapping_mul(2_654_435_761)
                .to_ne_bytes()
                .to_vec(),
            Keys::U64 { .. } => i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_ne_bytes().to_vec(),
        }
    }

    fn val(&self, rng: &mut Rng, ps: usize) -> Vec<u8> {
        match self.vals {
            Vals::Any => any_val(rng, ps),
            Vals::Dups { n, max } => {
                let m = rng.next() % n;
                let h = mix(m ^ 0xd0d0);
                let len = match h % 10 {
                    0..=6 => 1 + (h >> 8) % 12,
                    7..=8 => 13 + (h >> 8) % 70,
                    _ => 83 + (h >> 8) % (max as u64 - 82),
                } as usize;
                bytes(h, len.min(max))
            }
            Vals::Fixed { n, size } => bytes(mix(rng.next() % n), size),
            Vals::U32 { n } => ((rng.next() % n) as u32)
                .wrapping_mul(0x9e37_79b1)
                .to_ne_bytes()
                .to_vec(),
            Vals::U64 { n } => (rng.next() % n)
                .wrapping_mul(0xd6e8_feb8_6659_fd93)
                .to_ne_bytes()
                .to_vec(),
        }
    }

    /// The key order (`mdb_default_cmp`).
    fn key_cmp(&self) -> Comparator {
        Comparator::default_key(self.flags.bits())
    }

    /// The order of duplicates in the sub-DBs: C's `mdb_cmp_int` becomes
    /// `mdb_cmp_clong` for 8-byte items (`mdb_xcursor_init1`).
    fn dup_cmp(&self) -> Comparator {
        match Comparator::default_dup(self.flags.bits()) {
            Some(c) if c.is_int() && matches!(self.vals, Vals::U64 { .. }) => Comparator::Clong,
            Some(c) => c,
            None => Comparator::Memn,
        }
    }
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// Bytes ordered by a comparator.
#[derive(Clone)]
struct Ob {
    b: Vec<u8>,
    c: Comparator,
}

impl PartialEq for Ob {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}

impl Eq for Ob {}

impl PartialOrd for Ob {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl Ord for Ob {
    fn cmp(&self, o: &Self) -> Ordering {
        self.c.compare(&self.b, &o.b)
    }
}

/// The expected contents of one database.
#[derive(Clone)]
struct DbModel {
    exists: bool,
    dupsort: bool,
    kc: Comparator,
    dc: Comparator,
    map: BTreeMap<Ob, BTreeSet<Ob>>,
}

fn first_of(k: &Ob, s: &BTreeSet<Ob>) -> Pair {
    (
        k.b.clone(),
        s.iter().next().expect("empty dup set").b.clone(),
    )
}

fn last_of(k: &Ob, s: &BTreeSet<Ob>) -> Pair {
    (
        k.b.clone(),
        s.iter().next_back().expect("empty dup set").b.clone(),
    )
}

impl DbModel {
    fn new(cfg: &Cfg) -> DbModel {
        DbModel {
            exists: false,
            dupsort: cfg.dupsort(),
            kc: cfg.key_cmp(),
            dc: cfg.dup_cmp(),
            map: BTreeMap::new(),
        }
    }

    fn ok(&self, k: &[u8]) -> Ob {
        Ob {
            b: k.to_vec(),
            c: self.kc,
        }
    }

    fn od(&self, d: &[u8]) -> Ob {
        Ob {
            b: d.to_vec(),
            c: self.dc,
        }
    }

    fn count(&self) -> usize {
        self.map.values().map(BTreeSet::len).sum()
    }

    fn flat(&self) -> Vec<Pair> {
        self.map
            .iter()
            .flat_map(|(k, s)| s.iter().map(move |d| (k.b.clone(), d.b.clone())))
            .collect()
    }

    fn has_key(&self, k: &[u8]) -> bool {
        self.map.contains_key(&self.ok(k))
    }

    fn dups(&self, k: &[u8]) -> Option<&BTreeSet<Ob>> {
        self.map.get(&self.ok(k))
    }

    fn contains(&self, k: &[u8], d: &[u8]) -> bool {
        match self.dups(k) {
            Some(s) if self.dupsort => s.contains(&self.od(d)),
            Some(s) => s.iter().next().is_some_and(|v| v.b == d),
            None => false,
        }
    }

    /// The item a cursor positioned at `(k, d)` is on, if it still exists
    /// (without duplicates: the key's current value).
    fn at(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        let s = self.dups(k)?;
        if self.dupsort {
            s.contains(&self.od(d)).then(|| (k.to_vec(), d.to_vec()))
        } else {
            Some(first_of(&self.ok(k), s))
        }
    }

    fn first(&self) -> Option<Pair> {
        self.map.iter().next().map(|(k, s)| first_of(k, s))
    }

    fn last(&self) -> Option<Pair> {
        self.map.iter().next_back().map(|(k, s)| last_of(k, s))
    }

    fn last_key(&self) -> Option<&Ob> {
        self.map.keys().next_back()
    }

    fn next_key_first(&self, k: &[u8]) -> Option<Pair> {
        self.map
            .range((Excluded(self.ok(k)), Unbounded))
            .next()
            .map(|(k, s)| first_of(k, s))
    }

    fn prev_key_last(&self, k: &[u8]) -> Option<Pair> {
        self.map
            .range(..self.ok(k))
            .next_back()
            .map(|(k, s)| last_of(k, s))
    }

    fn next_dup(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        if !self.dupsort {
            return None;
        }
        let s = self.dups(k)?;
        s.range((Excluded(self.od(d)), Unbounded))
            .next()
            .map(|v| (k.to_vec(), v.b.clone()))
    }

    fn prev_dup(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        if !self.dupsort {
            return None;
        }
        let s = self.dups(k)?;
        s.range(..self.od(d))
            .next_back()
            .map(|v| (k.to_vec(), v.b.clone()))
    }

    /// The first item after `(k, d)` (which need not exist).
    fn succ(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        self.next_dup(k, d).or_else(|| self.next_key_first(k))
    }

    /// The last item before `(k, d)`.
    fn pred(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        self.prev_dup(k, d).or_else(|| self.prev_key_last(k))
    }

    fn key_first(&self, k: &[u8]) -> Option<Pair> {
        self.dups(k).map(|s| first_of(&self.ok(k), s))
    }

    fn key_last(&self, k: &[u8]) -> Option<Pair> {
        self.dups(k).map(|s| last_of(&self.ok(k), s))
    }

    fn set_range(&self, k: &[u8]) -> Option<Pair> {
        self.map
            .range((Included(self.ok(k)), Unbounded))
            .next()
            .map(|(k, s)| first_of(k, s))
    }

    fn both_range(&self, k: &[u8], d: &[u8]) -> Option<Pair> {
        let s = self.dups(k)?;
        s.range((Included(self.od(d)), Unbounded))
            .next()
            .map(|v| (k.to_vec(), v.b.clone()))
    }

    /// Stores `(k, d)`; returns `true` if the key is new.
    fn insert(&mut self, k: &[u8], d: &[u8]) -> bool {
        let (key, val) = (self.ok(k), self.od(d));
        if self.dupsort {
            let new = !self.map.contains_key(&key);
            self.map.entry(key).or_default().insert(val);
            new
        } else {
            self.map.insert(key, BTreeSet::from([val])).is_none()
        }
    }
}

/// Where the model thinks a cursor is.
#[derive(Clone, Debug)]
enum Pos {
    /// Never positioned, or its database was emptied.
    Unset,
    /// Not tracked (after a failed search, an ambiguous change, ...).
    Unknown,
    /// On this item (without duplicates the data is the key's value).
    On(Vec<u8>, Vec<u8>),
    /// Its item was deleted by the change numbered `epoch`; it now sits
    /// before the item's successor.
    Parked(Vec<u8>, Vec<u8>, u64),
}

#[derive(Clone)]
struct Cur {
    id: CursorId,
    db: usize,
    /// Nesting depth of the transaction that opened it.
    owner: usize,
    pos: Pos,
    /// Its database was deleted.
    dead: bool,
}

/// The model state of a transaction.
#[derive(Clone)]
struct St {
    dbs: Vec<DbModel>,
    curs: Vec<Cur>,
    /// Number of changes so far (for `Pos::Parked`).
    epoch: u64,
}

#[derive(Default, Debug)]
struct Stats {
    seeds: u64,
    txns: u64,
    ops: u64,
    commits: u64,
    aborts: u64,
    nested: u64,
    nested_aborts: u64,
    map_full: u64,
    txn_full: u64,
    cursor_checks: u64,
    parked_checks: u64,
    reopens: u64,
    c_checks: u64,
}

/// The transaction hit `MDB_MAP_FULL` or `MDB_TXN_FULL` (expected with
/// small maps and dirty limits): it can only be aborted now.
struct Full;

struct Cx<'a> {
    cfgs: &'a [Cfg],
    rng: Rng,
    ps: usize,
    seed: u64,
    txn_no: usize,
    log: VecDeque<String>,
    verbose: bool,
    /// Operation mix of the current transaction: 0 mixed, 1 growing, 2
    /// shrinking.
    mode: usize,
    /// The databases the current transaction works on.
    focus: Vec<usize>,
    /// Start of the key-number window the current transaction prefers.
    window: u64,
    stats: Stats,
}

impl Cx<'_> {
    fn note(&mut self, s: String) {
        if self.verbose {
            eprintln!("{s}");
        }
        self.log.push_back(s);
        if self.log.len() > 300 {
            self.log.pop_front();
        }
    }

    #[track_caller]
    fn fail(&self, msg: impl std::fmt::Display) -> ! {
        let log: Vec<&str> = self.log.iter().map(String::as_str).collect();
        panic!(
            "model mismatch (seed {}, txn {}): {msg}\nlast operations:\n{}",
            self.seed,
            self.txn_no,
            log.join("\n")
        );
    }

    /// A key of database `cfg`: from the transaction's window half the
    /// time (so that changes cluster and pages fill and empty), else any.
    fn key(&mut self, cfg: &Cfg) -> Vec<u8> {
        let n = cfg.key_count();
        let i = if self.rng.below(2) == 0 {
            (self.window + self.rng.below(150) as u64) % n
        } else {
            self.rng.next() % n
        };
        cfg.key(i)
    }

    /// A key, existing with probability `pct` percent.
    fn key_existing(&mut self, cfg: &Cfg, m: &DbModel, pct: usize) -> Vec<u8> {
        if !m.map.is_empty() && self.rng.below(100) < pct {
            let n = m.map.len();
            let j = self.rng.below(n);
            if j < n / 2 {
                m.map.keys().nth(j).unwrap().b.clone()
            } else {
                m.map.keys().nth_back(n - 1 - j).unwrap().b.clone()
            }
        } else {
            self.key(cfg)
        }
    }

    /// A data item for key `k`, an existing one with probability `pct`.
    fn val_existing(&mut self, cfg: &Cfg, m: &DbModel, k: &[u8], pct: usize) -> Vec<u8> {
        if let Some(s) = m.dups(k) {
            if self.rng.below(100) < pct {
                return s.iter().nth(self.rng.below(s.len())).unwrap().b.clone();
            }
        }
        cfg.val(&mut self.rng, self.ps)
    }
}

fn hexs(b: &[u8]) -> String {
    let h: String = b.iter().take(20).map(|x| format!("{x:02x}")).collect();
    if b.len() > 20 {
        format!("{h}..(len {})", b.len())
    } else {
        h
    }
}

fn show(p: &Option<Pair>) -> String {
    match p {
        Some((k, d)) => format!("({}, {})", hexs(k), hexs(d)),
        None => "NOTFOUND".into(),
    }
}

fn show_res(r: &rlmdb::Result<Option<Pair>>) -> String {
    match r {
        Ok(p) => show(p),
        Err(e) => format!("error {e:?}"),
    }
}

fn is_full(e: &Error) -> bool {
    matches!(e, Error::Mdb(MdbError::MapFull | MdbError::TxnFull))
}

fn is_key_exist(e: &Error) -> bool {
    matches!(e, Error::Mdb(MdbError::KeyExist))
}

const EINVAL: i32 = 22;

/// `mdb_cursor_get` on a handle, result copied.
fn cget(
    t: &mut RwTxn<'_>,
    id: CursorId,
    key: Option<&[u8]>,
    data: Option<&[u8]>,
    op: CursorOp,
) -> rlmdb::Result<Option<Pair>> {
    t.cursor_get(id, key, data, op)
        .map(|o| o.map(|(k, d)| (k.to_vec(), d.to_vec())))
}

/// After `MDB_MAP_FULL`/`MDB_TXN_FULL`: the transaction must refuse to go
/// on (C sets `MDB_TXN_ERROR`).
fn full<T>(t: &RwTxn<'_>, cx: &mut Cx, what: &str, e: Error) -> Result<T, Full> {
    match e {
        Error::Mdb(MdbError::MapFull) => cx.stats.map_full += 1,
        _ => cx.stats.txn_full += 1,
    }
    cx.note(format!("{what}: {e:?}, transaction failed"));
    match t.get(Dbi::MAIN, b"x") {
        Err(Error::Mdb(MdbError::BadTxn)) => Err(Full),
        other => cx.fail(format!(
            "{what} failed with {e:?} but the transaction still works: {other:?}"
        )),
    }
}

// ---------------------------------------------------------------------------
// Changes to the model
// ---------------------------------------------------------------------------

/// After storing `(k, d)` in database `i`.
fn model_put(st: &mut St, i: usize, k: &[u8], d: &[u8]) {
    let m = &mut st.dbs[i];
    m.insert(k, d);
    if !m.dupsort {
        // Cursors on the key see the new value.
        for c in st.curs.iter_mut().filter(|c| c.db == i) {
            if let Pos::On(ck, cd) = &mut c.pos {
                if m.kc.compare(ck, k) == Ordering::Equal {
                    *cd = d.to_vec();
                }
            }
        }
    }
    st.epoch += 1;
}

/// After deleting `(k, d)` (`d = None`: all of `k`'s items) from database
/// `i`: cursors on deleted items are parked.
fn model_del(st: &mut St, i: usize, k: &[u8], d: Option<&[u8]>) {
    st.epoch += 1;
    let epoch = st.epoch;
    let m = &mut st.dbs[i];
    let key = m.ok(k);
    let gone: Vec<Vec<u8>> = match (d, m.dupsort) {
        (Some(d), true) => {
            let s = m.map.get_mut(&key).expect("deleting a missing item");
            assert!(s.remove(&Ob {
                b: d.to_vec(),
                c: m.dc
            }));
            if s.is_empty() {
                m.map.remove(&key);
            }
            vec![d.to_vec()]
        }
        _ => m
            .map
            .remove(&key)
            .expect("deleting a missing key")
            .into_iter()
            .map(|o| o.b)
            .collect(),
    };
    let (kc, dc, dupsort) = (m.kc, m.dc, m.dupsort);
    for c in st.curs.iter_mut().filter(|c| c.db == i && !c.dead) {
        if let Pos::On(ck, cd) = &c.pos {
            let hit = kc.compare(ck, k) == Ordering::Equal
                && (!dupsort || gone.iter().any(|g| dc.compare(g, cd) == Ordering::Equal));
            if hit {
                c.pos = Pos::Parked(ck.clone(), cd.clone(), epoch);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// After a change to database `i`: every cursor on it is where the model
/// says (checked with `MDB_GET_CURRENT` most of the time; parked cursors
/// with a move now and then).
fn after_write(t: &mut RwTxn<'_>, st: &mut St, i: usize, cx: &mut Cx) {
    for j in 0..st.curs.len() {
        if st.curs[j].db != i || st.curs[j].dead {
            continue;
        }
        match st.curs[j].pos.clone() {
            Pos::On(k, d) => {
                let Some(want) = st.dbs[i].at(&k, &d) else {
                    cx.fail(format!("model: cursor {j} on a deleted item"));
                };
                if cx.rng.below(10) < 7 {
                    cx.stats.cursor_checks += 1;
                    let got = cget(t, st.curs[j].id, None, None, CursorOp::GetCurrent);
                    if !matches!(&got, Ok(Some(p)) if *p == want) {
                        cx.fail(format!(
                            "cursor {j} (db {}): GET_CURRENT gives {}, model has it on {}",
                            cx.cfgs[i].name,
                            show_res(&got),
                            show(&Some(want))
                        ));
                    }
                }
            }
            Pos::Parked(_, _, e) if e == st.epoch && cx.rng.below(2) == 0 => {
                check_parked(t, st, j, cx);
            }
            _ => {}
        }
    }
}

/// Moves parked cursor `j` (its item `(k, d)` was the last thing deleted)
/// and checks where it lands.
fn check_parked(t: &mut RwTxn<'_>, st: &mut St, j: usize, cx: &mut Cx) {
    let Pos::Parked(k, d, _) = st.curs[j].pos.clone() else {
        return;
    };
    cx.stats.parked_checks += 1;
    let i = st.curs[j].db;
    let m = &st.dbs[i];
    let key_left = m.has_key(&k);
    let mut ops = vec![
        CursorOp::GetCurrent,
        CursorOp::Next,
        CursorOp::Prev,
        CursorOp::NextNoDup,
        CursorOp::PrevNoDup,
    ];
    if m.dupsort {
        ops.extend([CursorOp::NextDup, CursorOp::PrevDup]);
    }
    let op = ops[cx.rng.below(ops.len())];
    let mut want: Vec<Option<Pair>> = match op {
        CursorOp::GetCurrent => vec![m.succ(&k, &d), None],
        CursorOp::Next => vec![m.succ(&k, &d)],
        CursorOp::Prev => vec![m.pred(&k, &d)],
        CursorOp::NextNoDup => vec![m.next_key_first(&k)],
        CursorOp::PrevNoDup => vec![m.prev_key_last(&k)],
        CursorOp::NextDup if key_left => vec![m.next_dup(&k, &d)],
        CursorOp::PrevDup if key_left => vec![m.prev_dup(&k, &d)],
        _ => vec![None],
    };
    if op == CursorOp::PrevDup && m.succ(&k, &d).is_none() {
        // Past the last item there is no current key: C's
        // mdb_cursor_prev skips its duplicates branch for `ki == NUMKEYS`
        // and moves to the previous item (ported).
        want.push(m.pred(&k, &d));
    }
    let empty = m.map.is_empty();
    let got = cget(t, st.curs[j].id, None, None, op);
    let ok = match &got {
        Ok(p) => want.contains(p),
        // The database became empty: the cursor is unpositioned.
        Err(e) => empty && op == CursorOp::GetCurrent && e.to_err_code() == EINVAL,
    };
    cx.note(format!("  parked cursor {j} {op:?} -> {}", show_res(&got)));
    if !ok {
        cx.fail(format!(
            "cursor {j} (db {}) after its item ({}, {}) was deleted: {op:?} gives {}, model expects one of {:?}",
            cx.cfgs[i].name,
            hexs(&k),
            hexs(&d),
            show_res(&got),
            want.iter().map(show).collect::<Vec<_>>()
        ));
    }
    st.curs[j].pos = match got {
        Ok(Some((k, d))) => Pos::On(k, d),
        _ => Pos::Unknown,
    };
}

/// Walks database `db` of `txn` in both directions and compares with the
/// model, plus the entry count.
fn check_db<T: Transaction>(txn: &T, db: Dbi, m: &DbModel, what: &str, cx: &Cx) {
    let want = m.flat();
    let walk = |rev: bool| -> rlmdb::Result<Vec<Pair>> {
        let mut c = txn.cursor(db)?;
        let mut out = Vec::new();
        loop {
            let item = if rev { c.prev()? } else { c.next()? };
            match item {
                Some((k, d)) => out.push((k.to_vec(), d.to_vec())),
                None => break,
            }
        }
        if rev {
            out.reverse();
        }
        Ok(out)
    };
    for rev in [false, true] {
        let got = walk(rev).unwrap_or_else(|e| cx.fail(format!("{what}: walk: {e:?}")));
        if got != want {
            let n = got.iter().zip(&want).take_while(|(a, b)| a == b).count();
            cx.fail(format!(
                "{what}: {} walk differs from the model at item {n} of {}/{}: got {}, model {}",
                if rev { "backward" } else { "forward" },
                got.len(),
                want.len(),
                show(&got.get(n).cloned()),
                show(&want.get(n).cloned())
            ));
        }
    }
    let stat = txn
        .stat(db)
        .unwrap_or_else(|e| cx.fail(format!("{what}: stat: {e:?}")));
    if stat.entries != want.len() {
        cx.fail(format!(
            "{what}: entries {} but the model has {}",
            stat.entries,
            want.len()
        ));
    }
}

/// Compares the committed state with the model: databases `which` (all if
/// `None`) and the list of named databases in the main DB.
fn check_committed(env: &Environment, dbs: &[DbModel], which: Option<&[usize]>, cx: &Cx) {
    let txn = env
        .begin_ro_txn()
        .unwrap_or_else(|e| cx.fail(format!("begin_ro_txn: {e:?}")));
    let mut names: Vec<Vec<u8>> = Vec::new();
    for (i, cfg) in cx.cfgs.iter().enumerate() {
        let m = &dbs[i];
        if m.exists {
            names.push(cfg.name.as_bytes().to_vec());
        }
        if which.is_some_and(|w| !w.contains(&i)) {
            continue;
        }
        match txn.open_db(Some(cfg.name)) {
            Ok(db) => {
                if !m.exists {
                    cx.fail(format!("{} exists, the model has no such DB", cfg.name));
                }
                let flags = txn.db_flags(db).unwrap();
                if flags != cfg.flags {
                    cx.fail(format!("{}: flags {flags:?}", cfg.name));
                }
                check_db(&txn, db, m, cfg.name, cx);
            }
            Err(e) if e.not_found() => {
                if m.exists {
                    cx.fail(format!("{} is missing", cfg.name));
                }
            }
            Err(e) => cx.fail(format!("open_db {}: {e:?}", cfg.name)),
        }
    }
    names.sort();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut main = Vec::new();
    while let Some((k, _)) = c.next().unwrap() {
        main.push(k.to_vec());
    }
    if main != names {
        cx.fail(format!(
            "main DB names {:?}, model {:?}",
            main.iter()
                .map(|n| String::from_utf8_lossy(n))
                .collect::<Vec<_>>(),
            names
                .iter()
                .map(|n| String::from_utf8_lossy(n))
                .collect::<Vec<_>>()
        ));
    }
}

/// The C page audit, and C's `mdb_dump` of every database.
fn check_with_c(c: &CTools, path: &Path, dbs: &[DbModel], cx: &mut Cx) {
    cx.stats.c_checks += 1;
    let p = path.to_str().unwrap();
    let out = c.peer(&["check-freelist", p]);
    if out.code != 0 {
        cx.fail(format!(
            "cpeer check-freelist: {}\n{}",
            out.stdout, out.stderr
        ));
    }
    for (i, cfg) in cx.cfgs.iter().enumerate() {
        if dbs[i].exists && c.dump(path, Some(cfg.name)) != dbs[i].flat() {
            cx.fail(format!("mdb_dump of {} differs from the model", cfg.name));
        }
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// The handle of database `i` (created if it does not exist).
fn handle(t: &mut RwTxn<'_>, st: &mut St, i: usize, cx: &mut Cx) -> Result<Dbi, Full> {
    let cfg = &cx.cfgs[i];
    match t.create_db(Some(cfg.name), cfg.flags | DatabaseFlags::CREATE) {
        Ok(db) => {
            if !st.dbs[i].exists {
                st.dbs[i].exists = true;
                cx.note(format!("create {}", cfg.name));
            }
            Ok(db)
        }
        Err(e) if is_full(&e) => full(t, cx, "create_db", e),
        Err(e) => cx.fail(format!("create_db {}: {e:?}", cfg.name)),
    }
}

/// Write flags for a put of `(k, d)` and whether the model expects it to
/// succeed (else `MDB_KEYEXIST`); may replace the key or data by one that
/// suits `APPEND`/`APPEND_DUP`.
fn put_flags(
    cx: &mut Cx,
    cfg: &Cfg,
    m: &DbModel,
    k: &mut Vec<u8>,
    d: &mut Vec<u8>,
) -> (WriteFlags, bool) {
    let r = cx.rng.below(100);
    if r < 55 {
        (WriteFlags::empty(), true)
    } else if r < 70 {
        (WriteFlags::NO_OVERWRITE, !m.has_key(k))
    } else if r < 80 && cfg.dupsort() {
        (WriteFlags::NO_DUP_DATA, !m.contains(k, d))
    } else if r < 90 {
        // The largest of a few keys: often beyond the last one.
        for _ in 0..12 {
            let c = cx.key(cfg);
            if m.kc.compare(&c, k) == Ordering::Greater {
                *k = c;
            }
        }
        let ok = m
            .last_key()
            .is_none_or(|l| m.kc.compare(k, &l.b) == Ordering::Greater);
        (WriteFlags::APPEND, ok)
    } else if cfg.dupsort() {
        for _ in 0..12 {
            let c = cfg.val(&mut cx.rng, cx.ps);
            if m.dc.compare(&c, d) == Ordering::Greater {
                *d = c;
            }
        }
        let ok = match m.key_last(k) {
            None => true,
            Some((_, last)) => m.dc.compare(d, &last) == Ordering::Greater,
        };
        (WriteFlags::APPEND_DUP, ok)
    } else {
        (WriteFlags::empty(), true)
    }
}

fn op_put(t: &mut RwTxn<'_>, st: &mut St, i: usize, db: Dbi, cx: &mut Cx) -> Result<(), Full> {
    let cfg = &cx.cfgs[i];
    let m = &st.dbs[i];
    let mut k = cx.key_existing(cfg, m, 15);
    let mut d = cx.val_existing(cfg, m, &k, 15);
    let (flags, ok) = put_flags(cx, cfg, m, &mut k, &mut d);
    let reserve = !cfg.dupsort() && cx.rng.below(6) == 0;
    cx.note(format!(
        "put{} {} {} {} {flags:?}",
        if reserve { " (reserve)" } else { "" },
        cfg.name,
        hexs(&k),
        hexs(&d)
    ));
    let r = if reserve {
        t.reserve(db, &k, d.len(), flags)
            .map(|buf| buf.copy_from_slice(&d))
    } else {
        t.put(db, &k, &d, flags)
    };
    match r {
        Ok(()) if ok => model_put(st, i, &k, &d),
        Err(e) if !ok && is_key_exist(&e) => {}
        Err(e) if is_full(&e) => return full(t, cx, "put", e),
        other => cx.fail(format!(
            "put: {other:?}, the model expects {}",
            if ok { "success" } else { "KEYEXIST" }
        )),
    }
    after_write(t, st, i, cx);
    Ok(())
}

fn op_del(t: &mut RwTxn<'_>, st: &mut St, i: usize, db: Dbi, cx: &mut Cx) -> Result<(), Full> {
    let cfg = &cx.cfgs[i];
    let m = &st.dbs[i];
    let k = cx.key_existing(cfg, m, 40);
    let data = (cx.rng.below(2) == 0).then(|| cx.val_existing(cfg, m, &k, 70));
    // Data is ignored unless DUP_SORT.
    let found = match (&data, cfg.dupsort()) {
        (Some(d), true) => m.contains(&k, d),
        _ => m.has_key(&k),
    };
    cx.note(format!(
        "del {} {} {}",
        cfg.name,
        hexs(&k),
        data.as_deref().map_or("-".into(), hexs)
    ));
    match t.del(db, &k, data.as_deref()) {
        Ok(f) if f == found => {
            if f {
                model_del(st, i, &k, data.as_deref().filter(|_| cfg.dupsort()));
            }
        }
        Err(e) if is_full(&e) => return full(t, cx, "del", e),
        other => cx.fail(format!("del: {other:?}, the model expects Ok({found})")),
    }
    after_write(t, st, i, cx);
    Ok(())
}

/// A cursor on database `i`: an existing one or a new one.
fn pick_cursor(
    t: &mut RwTxn<'_>,
    st: &mut St,
    i: usize,
    db: Dbi,
    depth: usize,
    cx: &mut Cx,
) -> usize {
    let cands: Vec<usize> = (0..st.curs.len())
        .filter(|&j| st.curs[j].db == i && !st.curs[j].dead)
        .collect();
    if cands.is_empty() || (cands.len() < 4 && cx.rng.below(6) == 0) {
        let id = t
            .open_cursor(db)
            .unwrap_or_else(|e| cx.fail(format!("open_cursor: {e:?}")));
        st.curs.push(Cur {
            id,
            db: i,
            owner: depth,
            pos: Pos::Unset,
            dead: false,
        });
        cx.note(format!(
            "open cursor {} on {} (depth {depth})",
            st.curs.len() - 1,
            cx.cfgs[i].name
        ));
        st.curs.len() - 1
    } else {
        cands[cx.rng.below(cands.len())]
    }
}

/// What the model predicts for a cursor read.
enum Want {
    /// One of these results.
    OneOf(Vec<Option<Pair>>),
    /// This error code.
    Code(i32),
}

/// Moves a cursor with a random operation and checks the result.
fn op_cursor_move(
    t: &mut RwTxn<'_>,
    st: &mut St,
    i: usize,
    db: Dbi,
    depth: usize,
    cx: &mut Cx,
) -> Result<(), Full> {
    let cfg = &cx.cfgs[i];
    let j = pick_cursor(t, st, i, db, depth, cx);
    if matches!(st.curs[j].pos, Pos::Parked(_, _, e) if e == st.epoch) {
        check_parked(t, st, j, cx);
        return Ok(());
    }
    let m = &st.dbs[i];
    let dupsort = cfg.dupsort();
    let cur = st.curs[j].clone();
    let incompatible = MdbError::Incompatible.to_err_code();
    let mut ops = vec![
        CursorOp::First,
        CursorOp::Last,
        CursorOp::Set,
        CursorOp::SetKey,
        CursorOp::SetRange,
        CursorOp::GetBoth,
        CursorOp::GetBothRange,
    ];
    match &cur.pos {
        Pos::On(..) => {
            ops.extend([
                CursorOp::GetCurrent,
                CursorOp::Next,
                CursorOp::Next,
                CursorOp::Prev,
                CursorOp::NextNoDup,
                CursorOp::PrevNoDup,
                CursorOp::FirstDup,
                CursorOp::LastDup,
            ]);
            if dupsort {
                ops.extend([CursorOp::NextDup, CursorOp::NextDup, CursorOp::PrevDup]);
            }
            if cfg.dupfixed() {
                ops.extend([CursorOp::GetMultiple, CursorOp::NextMultiple]);
            }
        }
        Pos::Unset => ops.extend([
            CursorOp::GetCurrent,
            CursorOp::Next,
            CursorOp::Prev,
            CursorOp::NextNoDup,
            CursorOp::PrevNoDup,
        ]),
        _ => {}
    }
    // mdb_cursor_count, now and then.
    if matches!(cur.pos, Pos::On(..)) && cx.rng.below(10) == 0 {
        let Pos::On(k, _) = &cur.pos else {
            unreachable!()
        };
        let want = if dupsort {
            Ok(m.dups(k).map_or(0, BTreeSet::len) as u64)
        } else {
            Err(incompatible)
        };
        let got = t.cursor_count(cur.id).map_err(|e| e.to_err_code());
        if got != want {
            cx.fail(format!("cursor {j}: count {got:?}, model {want:?}"));
        }
        return Ok(());
    }
    let op = ops[cx.rng.below(ops.len())];
    let probe_k = cx.key_existing(cfg, m, 50);
    let probe_d = cx.val_existing(cfg, m, &probe_k, 50);
    let (key, data) = match op {
        CursorOp::Set | CursorOp::SetKey | CursorOp::SetRange => (Some(&probe_k[..]), None),
        CursorOp::GetBoth | CursorOp::GetBothRange => (Some(&probe_k[..]), Some(&probe_d[..])),
        _ => (None, None),
    };
    let one = |p: Option<Pair>| Want::OneOf(vec![p]);
    let want = match (&cur.pos, op) {
        (_, CursorOp::First) => one(m.first()),
        (_, CursorOp::Last) => one(m.last()),
        (_, CursorOp::Set | CursorOp::SetKey) => one(m.key_first(&probe_k)),
        (_, CursorOp::SetRange) => one(m.set_range(&probe_k)),
        (_, CursorOp::GetBoth) if !dupsort => Want::Code(incompatible),
        (_, CursorOp::GetBothRange) if !dupsort => Want::Code(incompatible),
        (_, CursorOp::GetBoth) => one(m
            .contains(&probe_k, &probe_d)
            .then(|| (probe_k.clone(), probe_d.clone()))),
        (_, CursorOp::GetBothRange) => one(m.both_range(&probe_k, &probe_d)),
        (Pos::Unset, CursorOp::GetCurrent) => Want::Code(EINVAL),
        (Pos::Unset, CursorOp::Next | CursorOp::NextNoDup) => one(m.first()),
        (Pos::Unset, CursorOp::Prev | CursorOp::PrevNoDup) => one(m.last()),
        (Pos::On(k, d), op) => {
            let here = m.at(k, d);
            let w = match op {
                CursorOp::GetCurrent => vec![here.clone()],
                CursorOp::Next => vec![m.succ(k, d)],
                CursorOp::Prev => vec![m.pred(k, d)],
                CursorOp::NextNoDup => vec![m.next_key_first(k)],
                CursorOp::PrevNoDup => vec![m.prev_key_last(k)],
                CursorOp::NextDup => vec![m.next_dup(k, d)],
                CursorOp::PrevDup => vec![m.prev_dup(k, d)],
                CursorOp::FirstDup | CursorOp::LastDup if !dupsort => vec![],
                CursorOp::FirstDup => vec![m.key_first(k)],
                CursorOp::LastDup => vec![m.key_last(k)],
                _ => vec![],
            };
            if w.is_empty() {
                Want::Code(incompatible)
            } else {
                Want::OneOf(w)
            }
        }
        _ => unreachable!("{op:?} from {:?}", cur.pos),
    };
    if matches!(op, CursorOp::GetMultiple | CursorOp::NextMultiple) {
        return multiple(t, st, j, op, cx);
    }
    let got = cget(t, cur.id, key, data, op);
    cx.note(format!(
        "cursor {j} {} {op:?} {} -> {}",
        cfg.name,
        key.map_or(String::new(), hexs),
        show_res(&got)
    ));
    let ok = match (&want, &got) {
        (Want::OneOf(w), Ok(p)) => w.contains(p),
        (Want::Code(c), Err(e)) => e.to_err_code() == *c,
        _ => false,
    };
    if !ok {
        let w = match &want {
            Want::OneOf(w) => format!("{:?}", w.iter().map(show).collect::<Vec<_>>()),
            Want::Code(c) => format!("error {c}"),
        };
        cx.fail(format!(
            "cursor {j} (db {}, at {:?}): {op:?} gives {}, model expects {w}",
            cfg.name,
            cur.pos,
            show_res(&got)
        ));
    }
    let c = &mut st.curs[j];
    match got {
        Ok(Some((k, d))) => {
            c.pos = Pos::On(k, d);
        }
        Ok(None) => {
            c.pos = Pos::Unknown;
        }
        Err(_) => {}
    }
    Ok(())
}

/// `MDB_GET_MULTIPLE` / `MDB_NEXT_MULTIPLE` (`DUP_FIXED`): a page's worth
/// of consecutive duplicates around the expected one; the cursor ends on
/// the last of them.
fn multiple(
    t: &mut RwTxn<'_>,
    st: &mut St,
    j: usize,
    op: CursorOp,
    cx: &mut Cx,
) -> Result<(), Full> {
    let cur = st.curs[j].clone();
    let i = cur.db;
    let m = &st.dbs[i];
    let Pos::On(k, d) = &cur.pos else {
        unreachable!()
    };
    let dups: Vec<Vec<u8>> = m.dups(k).unwrap().iter().map(|o| o.b.clone()).collect();
    let size = dups[0].len();
    // The items the run must contain one of.
    let mut targets: Vec<Vec<u8>> = Vec::new();
    if op == CursorOp::GetMultiple {
        targets.push(d.clone());
    } else {
        if let Some((_, n)) = m.next_dup(k, d) {
            targets.push(n);
        }
    }
    let got = cget(t, cur.id, None, None, op);
    cx.note(format!(
        "cursor {j} {} {op:?} -> {}",
        cx.cfgs[i].name,
        show_res(&got)
    ));
    let fail = |cx: &Cx, why: &str| -> ! {
        cx.fail(format!(
            "cursor {j} (db {}, at {:?}): {op:?} gives {}: {why}",
            cx.cfgs[i].name,
            cur.pos,
            show_res(&got)
        ))
    };
    let run = match &got {
        Ok(None) if targets.is_empty() => {
            st.curs[j].pos = Pos::Unknown;
            return Ok(());
        }
        // A key whose only item is stored without a sub-page has nothing
        // to fetch.
        Ok(Some((gk, gd))) if gd.is_empty() && op == CursorOp::GetMultiple && dups.len() == 1 => {
            if gk != k {
                fail(cx, "wrong key");
            }
            return Ok(());
        }
        Ok(Some((gk, gd))) => {
            if gk != k || gd.is_empty() || gd.len() % size != 0 {
                fail(cx, "not a run of items of the key");
            }
            gd.chunks(size).map(<[u8]>::to_vec).collect::<Vec<_>>()
        }
        _ => fail(cx, "unexpected result"),
    };
    let start = dups
        .iter()
        .position(|x| *x == run[0])
        .unwrap_or_else(|| fail(cx, "first item not in the model"));
    if dups.get(start..start + run.len()) != Some(&run[..]) {
        fail(cx, "items are not consecutive duplicates");
    }
    if !targets.iter().any(|x| run.contains(x)) {
        fail(cx, "the run does not contain the expected item");
    }
    let c = &mut st.curs[j];
    c.pos = Pos::On(k.clone(), run.last().unwrap().clone());
    Ok(())
}

/// Writes or deletes through a cursor.
fn op_cursor_write(
    t: &mut RwTxn<'_>,
    st: &mut St,
    i: usize,
    db: Dbi,
    depth: usize,
    cx: &mut Cx,
) -> Result<(), Full> {
    let cfg = &cx.cfgs[i];
    let j = pick_cursor(t, st, i, db, depth, cx);
    let cur = st.curs[j].clone();
    let on = match &cur.pos {
        Pos::On(k, d) => Some((k.clone(), d.clone())),
        _ => None,
    };
    let r = cx.rng.below(100);
    if let (Some((k, d)), true) = (&on, r < 35) {
        // mdb_cursor_del.
        let flags = if cx.rng.below(3) == 0 {
            WriteFlags::NO_DUP_DATA
        } else {
            WriteFlags::empty()
        };
        cx.note(format!(
            "cursor {j} del {} {} {} {flags:?}",
            cfg.name,
            hexs(k),
            hexs(d)
        ));
        match t.cursor_del(cur.id, flags) {
            Ok(()) => {
                let whole = flags.contains(WriteFlags::NO_DUP_DATA) || !cfg.dupsort();
                model_del(st, i, k, (!whole).then_some(&d[..]));
            }
            Err(e) if is_full(&e) => return full(t, cx, "cursor_del", e),
            Err(e) => cx.fail(format!("cursor {j} del: {e:?}")),
        }
    } else if let (Some((k, d)), true) = (&on, r < 55) {
        // MDB_CURRENT.
        let m = &st.dbs[i];
        let mut nd = cfg.val(&mut cx.rng, cx.ps);
        if cfg.dupsort() {
            // The new item must sort where the old one is.
            let fits = |x: &[u8]| {
                !m.contains(k, x)
                    && m.prev_dup(k, d)
                        .is_none_or(|(_, p)| m.dc.compare(&p, x) == Ordering::Less)
                    && m.next_dup(k, d)
                        .is_none_or(|(_, n)| m.dc.compare(x, &n) == Ordering::Less)
            };
            if !fits(&nd) {
                nd = d.clone();
            }
        }
        let reserve = !cfg.dupsort() && cx.rng.below(4) == 0;
        cx.note(format!(
            "cursor {j} put CURRENT{} {} {} {} -> {}",
            if reserve { " (reserve)" } else { "" },
            cfg.name,
            hexs(k),
            hexs(d),
            hexs(&nd)
        ));
        let r = if reserve {
            t.cursor_reserve(cur.id, k, nd.len(), WriteFlags::CURRENT)
                .map(|buf| buf.copy_from_slice(&nd))
        } else {
            t.cursor_put(cur.id, k, &nd, WriteFlags::CURRENT)
        };
        match r {
            Ok(()) => {
                if cfg.dupsort() {
                    if nd != *d {
                        // Deleted and inserted, or rewritten in place: other
                        // cursors on the item are on one or the other.
                        model_del(st, i, k, Some(d));
                        for c in st.curs.iter_mut().filter(|c| c.db == i) {
                            if matches!(c.pos, Pos::Parked(..)) {
                                c.pos = Pos::Unknown;
                            }
                        }
                    }
                    model_put(st, i, k, &nd);
                } else {
                    model_put(st, i, k, &nd);
                }
                let c = &mut st.curs[j];
                c.pos = Pos::On(k.clone(), nd);
            }
            Err(e) if is_full(&e) => return full(t, cx, "cursor_put CURRENT", e),
            Err(e) => cx.fail(format!("cursor {j} put CURRENT: {e:?}")),
        }
    } else if cfg.dupfixed() && r < 70 {
        // MDB_MULTIPLE.
        let m = &st.dbs[i];
        let k = cx.key_existing(cfg, m, 50);
        let n = 1 + cx.rng.below(40);
        let items: Vec<Vec<u8>> = (0..n).map(|_| cx.val_existing(cfg, m, &k, 20)).collect();
        let size = items[0].len();
        let flags = if cx.rng.below(3) == 0 {
            WriteFlags::NO_DUP_DATA
        } else {
            WriteFlags::empty()
        };
        cx.note(format!(
            "cursor {j} put MULTIPLE {} {} {n} items {flags:?}",
            cfg.name,
            hexs(&k)
        ));
        let packed: Vec<u8> = items.concat();
        let got = t.cursor_put_multiple(cur.id, &k, &packed, size, flags);
        // Simulate: with NO_DUP_DATA the first existing item stops it with
        // KEYEXIST.
        let mut sim = st.dbs[i].clone();
        let mut stop = None;
        for (x, item) in items.iter().enumerate() {
            if sim.contains(&k, item) && flags.contains(WriteFlags::NO_DUP_DATA) {
                stop = Some(x);
                break;
            }
            sim.insert(&k, item);
        }
        let stored = match (&got, stop) {
            (Ok(c), None) if *c == n => *c,
            (Err(e), Some(x)) if is_key_exist(e) => x,
            (Err(e), _) if is_full(e) => {
                let e = got.unwrap_err();
                return full(t, cx, "cursor_put MULTIPLE", e);
            }
            _ => cx.fail(format!(
                "cursor {j} put MULTIPLE: {got:?}, model: stop at {stop:?}"
            )),
        };
        for item in &items[..stored] {
            model_put(st, i, &k, item);
        }
        let c = &mut st.curs[j];
        c.pos = match got {
            Ok(c) if c == n => Pos::On(k.clone(), items[n - 1].clone()),
            _ => Pos::Unknown,
        };
    } else {
        // A put through the cursor.
        let m = &st.dbs[i];
        let mut k = cx.key_existing(cfg, m, 15);
        let mut d = cx.val_existing(cfg, m, &k, 15);
        let (flags, ok) = put_flags(cx, cfg, m, &mut k, &mut d);
        let reserve = !cfg.dupsort() && cx.rng.below(6) == 0;
        cx.note(format!(
            "cursor {j} put{} {} {} {} {flags:?}",
            if reserve { " (reserve)" } else { "" },
            cfg.name,
            hexs(&k),
            hexs(&d)
        ));
        let r = if reserve {
            t.cursor_reserve(cur.id, &k, d.len(), flags)
                .map(|buf| buf.copy_from_slice(&d))
        } else {
            t.cursor_put(cur.id, &k, &d, flags)
        };
        match r {
            Ok(()) if ok => {
                model_put(st, i, &k, &d);
                let c = &mut st.curs[j];
                c.pos = Pos::On(k, d);
            }
            Err(e) if !ok && is_key_exist(&e) => {
                let c = &mut st.curs[j];
                c.pos = Pos::Unknown;
            }
            Err(e) if is_full(&e) => return full(t, cx, "cursor_put", e),
            other => cx.fail(format!(
                "cursor {j} put: {other:?}, the model expects {}",
                if ok { "success" } else { "KEYEXIST" }
            )),
        }
    }
    after_write(t, st, i, cx);
    Ok(())
}

/// `get`, compared with the model.
fn op_get(t: &mut RwTxn<'_>, st: &mut St, i: usize, db: Dbi, cx: &mut Cx) {
    let cfg = &cx.cfgs[i];
    let m = &st.dbs[i];
    let k = cx.key_existing(cfg, m, 50);
    let want = m.key_first(&k).map(|(_, d)| d);
    let got = t.get(db, &k).map(|o| o.map(<[u8]>::to_vec));
    if !matches!(&got, Ok(g) if *g == want) {
        cx.fail(format!(
            "get {} {}: {got:?}, model {:?}",
            cfg.name,
            hexs(&k),
            want.as_deref().map(hexs)
        ));
    }
}

/// Empties or deletes database `i`.
fn op_clear_or_drop(
    t: &mut RwTxn<'_>,
    st: &mut St,
    i: usize,
    db: Dbi,
    cx: &mut Cx,
) -> Result<(), Full> {
    let drop = cx.rng.below(2) == 0;
    cx.note(format!(
        "{} {} ({} items)",
        if drop { "drop_db" } else { "clear_db" },
        cx.cfgs[i].name,
        st.dbs[i].count()
    ));
    let r = if drop { t.drop_db(db) } else { t.clear_db(db) };
    match r {
        Ok(()) => {}
        Err(e) if is_full(&e) => return full(t, cx, "drop", e),
        Err(e) => cx.fail(format!("drop/clear: {e:?}")),
    }
    st.epoch += 1;
    st.dbs[i].map.clear();
    if drop {
        st.dbs[i].exists = false;
    }
    for c in st.curs.iter_mut().filter(|c| c.db == i) {
        // A dropped DB's handle is closed: its cursors are of no use.
        if drop {
            c.dead = true;
        }
        c.pos = Pos::Unset;
    }
    Ok(())
}

/// A nested transaction, committed or aborted.
fn nested(t: &mut RwTxn<'_>, st: &mut St, depth: usize, cx: &mut Cx) -> Result<(), Full> {
    cx.stats.nested += 1;
    cx.note(format!("{depth}: begin nested"));
    let mut child = t
        .begin_nested_txn()
        .unwrap_or_else(|e| cx.fail(format!("begin_nested_txn: {e:?}")));
    let mut st2 = st.clone();
    let end = run_txn(&mut child, &mut st2, depth + 1, cx);
    // The child's cursors end with it.
    st2.curs.retain(|c| c.owner <= depth);
    match end {
        Ok(()) => {
            if cx.rng.below(3) == 0 {
                cx.note(format!("{depth}: abort nested"));
                cx.stats.nested_aborts += 1;
                child.abort();
            } else {
                cx.note(format!("{depth}: commit nested"));
                if let Err(e) = child.commit() {
                    cx.fail(format!("nested commit: {e:?}"));
                }
                st2.epoch += 1;
                *st = st2;
            }
            Ok(())
        }
        Err(Full) => {
            if cx.rng.below(2) == 0 {
                cx.note(format!("{depth}: abort failed nested"));
                cx.stats.nested_aborts += 1;
                child.abort();
                Ok(())
            } else {
                // Committing a failed child fails, and fails the parent.
                cx.note(format!("{depth}: commit failed nested"));
                match child.commit() {
                    Err(Error::Mdb(MdbError::BadTxn)) => {}
                    other => cx.fail(format!("commit of a failed nested txn: {other:?}")),
                }
                match t.get(Dbi::MAIN, b"x") {
                    Err(Error::Mdb(MdbError::BadTxn)) => Err(Full),
                    other => cx.fail(format!("parent of a failed child still works: {other:?}")),
                }
            }
        }
    }
}

/// The operations of one (possibly nested) transaction.
fn run_txn(t: &mut RwTxn<'_>, st: &mut St, depth: usize, cx: &mut Cx) -> Result<(), Full> {
    let n = 1 + cx.rng.below(if depth == 0 { 150 } else { 50 });
    for _ in 0..n {
        cx.stats.ops += 1;
        if depth < 3 && cx.rng.below(40) == 0 {
            nested(t, st, depth, cx)?;
            continue;
        }
        let i = cx.focus[cx.rng.below(cx.focus.len())];
        let db = handle(t, st, i, cx)?;
        let (p_put, p_del) = match cx.mode {
            1 => (50, 8),
            2 => (12, 42),
            _ => (34, 20),
        };
        let r = cx.rng.below(1000);
        if r < p_put * 10 {
            op_put(t, st, i, db, cx)?;
        } else if r < (p_put + p_del) * 10 {
            op_del(t, st, i, db, cx)?;
        } else if r < 820 {
            op_cursor_move(t, st, i, db, depth, cx)?;
        } else if r < 950 {
            op_cursor_write(t, st, i, db, depth, cx)?;
        } else if r < 975 {
            op_get(t, st, i, db, cx);
        } else if r < 985 {
            // The transaction's own view.
            let name = format!("in-txn {} at depth {depth}", cx.cfgs[i].name);
            check_db(&*t, db, &st.dbs[i], &name, cx);
        } else if r < 995 {
            // Close one of this transaction's cursors; closing an
            // ancestor's does nothing.
            if let Some(j) = (0..st.curs.len()).find(|&j| st.curs[j].db == i) {
                let id = st.curs[j].id;
                t.close_cursor(id);
                if st.curs[j].owner == depth {
                    cx.note(format!("close cursor {j}"));
                    st.curs.remove(j);
                }
            }
        } else if cx.cfgs[i].scratch || cx.rng.below(8) == 0 {
            op_clear_or_drop(t, st, i, db, cx)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn open_env(path: &Path, map_size: usize, dirty: Option<usize>) -> Environment {
    let env = Environment::builder()
        .map_size(map_size)
        .max_dbs(16)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap();
    if let Some(d) = dirty {
        env.set_dirty_limit(d);
    }
    env
}

fn run_seed(
    seed: u64,
    txns: usize,
    cfgs: &[Cfg],
    c: Option<&CTools>,
    verbose: bool,
    deadline: Option<Instant>,
    total: &mut Stats,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let ps = common::os_page_size();
    let mut cx = Cx {
        cfgs,
        rng: Rng(mix(seed) | 1),
        ps,
        seed,
        txn_no: 0,
        log: VecDeque::new(),
        verbose,
        mode: 0,
        focus: Vec::new(),
        window: 0,
        stats: Stats::default(),
    };
    let dirty = match cx.rng.below(4) {
        0 => None,
        1 => Some(2048),
        2 => Some(400),
        _ => Some(160),
    };
    let mut map_size = (40 + cx.rng.below(400)) * ps;
    cx.note(format!(
        "seed {seed}: page size {ps}, map {map_size}, dirty limit {dirty:?}"
    ));
    let mut env = open_env(path, map_size, dirty);
    let mut committed: Vec<DbModel> = cfgs.iter().map(DbModel::new).collect();
    for txn_no in 0..txns {
        if deadline.is_some_and(|d| Instant::now() >= d) && txn_no > 0 {
            break;
        }
        cx.txn_no = txn_no;
        cx.stats.txns += 1;
        match cx.rng.below(100) {
            0..=1 => {
                cx.note("reopen environment".into());
                cx.stats.reopens += 1;
                drop(env);
                env = open_env(path, map_size, dirty);
            }
            2 => {
                map_size += map_size / 2;
                cx.note(format!("set_map_size {map_size}"));
                env.set_map_size(map_size).unwrap();
            }
            _ => {}
        }
        cx.mode = cx.rng.below(3);
        let nfocus = 1 + cx.rng.below(3);
        cx.focus = (0..nfocus).map(|_| cx.rng.below(cfgs.len())).collect();
        cx.window = cx.rng.next() % 10_000;
        cx.note(format!(
            "-- txn {txn_no}: mode {}, dbs {:?}",
            cx.mode,
            cx.focus.iter().map(|&i| cfgs[i].name).collect::<Vec<_>>()
        ));
        let mut t = env
            .begin_rw_txn()
            .unwrap_or_else(|e| cx.fail(format!("begin_rw_txn: {e:?}")));
        let mut st = St {
            dbs: committed.clone(),
            curs: Vec::new(),
            epoch: 0,
        };
        let mut grow = false;
        match run_txn(&mut t, &mut st, 0, &mut cx) {
            Ok(()) if cx.rng.below(12) == 0 => {
                cx.note("abort".into());
                cx.stats.aborts += 1;
                t.abort();
            }
            Ok(()) => match t.commit() {
                Ok(()) => {
                    cx.note("commit".into());
                    cx.stats.commits += 1;
                    committed = st.dbs;
                }
                Err(e) if is_full(&e) => {
                    cx.note(format!("commit: {e:?}"));
                    grow = true;
                }
                Err(e) => cx.fail(format!("commit: {e:?}")),
            },
            Err(Full) => {
                match t.commit() {
                    Err(Error::Mdb(MdbError::BadTxn)) => {}
                    other => cx.fail(format!("commit of a failed transaction: {other:?}")),
                }
                grow = true;
            }
        }
        if grow {
            map_size += map_size / 2;
            cx.note(format!("set_map_size {map_size}"));
            env.set_map_size(map_size).unwrap();
        }
        let all = txn_no % 8 == 7;
        let focus = cx.focus.clone();
        check_committed(&env, &committed, (!all).then_some(&focus[..]), &cx);
        if let Some(c) = c {
            if cx.rng.below(15) == 0 {
                check_with_c(c, path, &committed, &mut cx);
            }
        }
    }
    check_committed(&env, &committed, None, &cx);
    if let Some(c) = c {
        check_with_c(c, path, &committed, &mut cx);
    }
    // Every read path of every database, once.
    {
        let txn = env.begin_ro_txn().unwrap();
        for (i, cfg) in cfgs.iter().enumerate() {
            if committed[i].exists {
                let db = txn.open_db(Some(cfg.name)).unwrap();
                common::verify_db(&env, db, &committed[i].flat());
            }
        }
    }
    let s = &cx.stats;
    eprintln!(
        "seed {seed}: {} txns ({} commits, {} aborts, {} nested, {} map full, {} txn full), {} ops",
        s.txns, s.commits, s.aborts, s.nested, s.map_full, s.txn_full, s.ops
    );
    total.seeds += 1;
    total.txns += s.txns;
    total.ops += s.ops;
    total.commits += s.commits;
    total.aborts += s.aborts;
    total.nested += s.nested;
    total.nested_aborts += s.nested_aborts;
    total.map_full += s.map_full;
    total.txn_full += s.txn_full;
    total.cursor_checks += s.cursor_checks;
    total.parked_checks += s.parked_checks;
    total.reopens += s.reopens;
    total.c_checks += s.c_checks;
}

#[test]
fn model() {
    let secs = env_u64("MODEL_SECS", 30);
    let txns = env_u64("MODEL_TXNS", 150) as usize;
    let only = std::env::var("MODEL_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    let verbose = std::env::var("MODEL_LOG").is_ok_and(|v| v == "1");
    let c = common::c_tools();
    if c.is_none() {
        eprintln!("NOTE: running without the C page audit and mdb_dump checks");
    }
    let cfgs = configs();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut total = Stats::default();
    let mut seed = only.unwrap_or(1);
    loop {
        run_seed(
            seed,
            txns,
            &cfgs,
            c,
            verbose,
            only.is_none().then_some(deadline),
            &mut total,
        );
        if only.is_some() || Instant::now() >= deadline {
            break;
        }
        seed += 1;
    }
    eprintln!("model test: {total:?} in {:?}", start.elapsed());
}
