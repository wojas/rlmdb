//! Crafted-file cases mirroring the LMDB hardening patches that py-lmdb
//! (https://github.com/jnwatson/py-lmdb, `lib/py-lmdb/*.patch`) carries on
//! top of upstream 0.9.36: the CVE-2019-16224..16228 fixes and the
//! `validate-*` series. Each case applies the exact on-disk edit the patch
//! guards against to a valid file written by the Rust engine, then opens
//! the file and drives reads, writes, nested transactions, drops and copies
//! through it.
//!
//! The requirement is the one the Rust port makes for corrupt input: no
//! panic (debug builds also trap integer overflow) and no endless loop; any
//! error is fine. A few cases additionally assert the error the patch
//! produces in C (`MDB_INVALID` for a bad page size) or a property the C
//! bug violated (a forged `P_DIRTY` must not let a write land on a snapshot
//! page).
//!
//! Not committed to the repository: produced by the py-lmdb patch audit.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error, MdbError, RoCursor,
    Transaction, WriteFlags,
};

// ---------------------------------------------------------------------------
// On-disk constants (64-bit LMDB 0.9 layout)
// ---------------------------------------------------------------------------

const P_BRANCH: u16 = 0x01;
const P_LEAF: u16 = 0x02;
const P_OVERFLOW: u16 = 0x04;
const P_DIRTY: u16 = 0x10;
const P_LEAF2: u16 = 0x20;
const F_BIGDATA: u16 = 0x01;
const F_SUBDATA: u16 = 0x02;
const F_DUPDATA: u16 = 0x04;
const MDB_DUPFIXED: u16 = 0x10;
/// Offsets of `MDB_node`'s `mn_lo` and `mn_hi`, which C declares in the
/// other order on big-endian hosts.
const NODE_LO: usize = if cfg!(target_endian = "little") { 0 } else { 2 };
const NODE_HI: usize = 2 - NODE_LO;

const MAIN_KEYS: usize = 300;
const BIG_KEYS: usize = 6;
const DUP_KEYS: usize = 60;
const FIXED_KEYS: usize = 60;

fn main_key(i: usize) -> Vec<u8> {
    format!("k{i:05}").into_bytes()
}
fn big_key(i: usize) -> Vec<u8> {
    format!("big{i:02}").into_bytes()
}
fn dup_key(i: usize) -> Vec<u8> {
    format!("d{i:03}").into_bytes()
}
fn fixed_key(i: usize) -> Vec<u8> {
    format!("f{i:03}").into_bytes()
}
fn big_value(i: usize) -> Vec<u8> {
    // Larger than any page size we support (32 KiB), so always on overflow
    // pages, more than one of them.
    vec![b'A' + (i as u8); 70_000]
}

// ---------------------------------------------------------------------------
// The base environment (written by the Rust engine, no C needed)
// ---------------------------------------------------------------------------

struct Base {
    data: Vec<u8>,
    ps: usize,
}

fn build_base(dir: &Path) {
    let env = Environment::builder()
        .map_size(256 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let main = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    let dups = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    let fixed = txn
        .create_db(
            Some("fixed"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    for i in 0..MAIN_KEYS {
        txn.put(main, &main_key(i), &[b'v'; 40], WriteFlags::empty())
            .unwrap();
    }
    for i in 0..BIG_KEYS {
        txn.put(main, &big_key(i), &big_value(i), WriteFlags::empty())
            .unwrap();
    }
    // DUPSORT: a few duplicates per key (sub-pages) and one key with many
    // (a sub-DB).
    for i in 0..DUP_KEYS {
        for j in 0..3 {
            txn.put(
                dups,
                &dup_key(i),
                format!("v{j:03}").as_bytes(),
                WriteFlags::empty(),
            )
            .unwrap();
        }
    }
    for j in 0..3000u32 {
        txn.put(
            dups,
            b"dsub",
            format!("value-{j:06}").as_bytes(),
            WriteFlags::empty(),
        )
        .unwrap();
    }
    // DUPFIXED: 8-byte items, sub-pages (P_LEAF2) and one sub-DB of LEAF2
    // pages.
    for i in 0..FIXED_KEYS {
        for j in 0..3u64 {
            txn.put(fixed, &fixed_key(i), &j.to_be_bytes(), WriteFlags::empty())
                .unwrap();
        }
    }
    for j in 0..6000u64 {
        txn.put(fixed, b"fsub", &j.to_be_bytes(), WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();

    // A second and third transaction so that the free DB holds records
    // (deleted keys, rewritten overflow values).
    let mut txn = env.begin_rw_txn().unwrap();
    for i in (0..MAIN_KEYS).step_by(5) {
        txn.del(main, &main_key(i), None).unwrap();
    }
    txn.del(main, &big_key(0), None).unwrap();
    for i in (0..DUP_KEYS).step_by(7) {
        txn.del(dups, &dup_key(i), Some(b"v001")).unwrap();
    }
    txn.commit().unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    for i in (1..MAIN_KEYS).step_by(5) {
        txn.put(main, &main_key(i), &[b'w'; 60], WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
}

fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        build_base(dir.path());
        let data = std::fs::read(dir.path().join("data.mdb")).unwrap();
        let ps = u32::from_ne_bytes(data[40..44].try_into().unwrap()) as usize;
        Base { data, ps }
    })
}

// ---------------------------------------------------------------------------
// A file image with structure-aware accessors
// ---------------------------------------------------------------------------

struct Img {
    buf: Vec<u8>,
    ps: usize,
}

impl Img {
    fn npages(&self) -> usize {
        self.buf.len() / self.ps
    }
    fn get16(&self, off: usize) -> Option<u16> {
        Some(u16::from_ne_bytes(
            self.buf.get(off..off + 2)?.try_into().ok()?,
        ))
    }
    fn get32(&self, off: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(
            self.buf.get(off..off + 4)?.try_into().ok()?,
        ))
    }
    fn get64(&self, off: usize) -> Option<u64> {
        Some(u64::from_ne_bytes(
            self.buf.get(off..off + 8)?.try_into().ok()?,
        ))
    }
    fn put16(&mut self, off: usize, v: u16) {
        self.buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
    }
    fn put32(&mut self, off: usize, v: u32) {
        self.buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }
    fn put64(&mut self, off: usize, v: u64) {
        self.buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
    }
    fn flags(&self, pg: usize) -> u16 {
        self.get16(pg * self.ps + 10).unwrap_or(0)
    }

    /// The newer meta page's offset (0 or ps).
    fn meta(&self) -> usize {
        let t0 = self.get64(16 + 128).unwrap_or(0);
        let t1 = self.get64(self.ps + 16 + 128).unwrap_or(0);
        if t0 < t1 {
            self.ps
        } else {
            0
        }
    }

    /// Pages 2.. (overflow runs skipped) whose flags satisfy `want`.
    fn pages_with(&self, want: impl Fn(u16) -> bool) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pg = 2;
        while pg < self.npages() {
            let f = self.flags(pg);
            if want(f) {
                out.push(pg);
            }
            if f & P_OVERFLOW != 0 {
                let n = self.get32(pg * self.ps + 12).unwrap_or(1) as usize;
                pg += n.clamp(1, self.npages());
            } else {
                pg += 1;
            }
        }
        out
    }

    /// Absolute offsets of the nodes of page `pg`.
    fn nodes(&self, pg: usize) -> Vec<usize> {
        let base = pg * self.ps;
        let lower = self.get16(base + 12).unwrap_or(0) as usize;
        let n = lower.saturating_sub(16) / 2;
        (0..n.min(self.ps / 2))
            .filter_map(|i| {
                let ptr = self.get16(base + 16 + 2 * i)? as usize;
                (ptr >= 16 && ptr + 8 <= self.ps).then_some(base + ptr)
            })
            .collect()
    }

    /// Leaf nodes whose flags satisfy `want`.
    fn leaf_nodes_with(&self, want: impl Fn(u16) -> bool) -> Vec<usize> {
        let mut out = Vec::new();
        for pg in self.pages_with(|f| f & P_LEAF != 0 && f & P_LEAF2 == 0) {
            for n in self.nodes(pg) {
                if want(self.get16(n + 4).unwrap_or(0)) {
                    out.push(n);
                }
            }
        }
        out
    }

    fn node_key(&self, n: usize) -> &[u8] {
        let ks = self.get16(n + 6).unwrap_or(0) as usize;
        self.buf.get(n + 8..n + 8 + ks).unwrap_or(&[])
    }

    /// Offset of the data of node `n`.
    fn node_data(&self, n: usize) -> usize {
        n + 8 + self.get16(n + 6).unwrap_or(0) as usize
    }

    fn node_dsize(&self, n: usize) -> usize {
        (self.get16(n + NODE_LO).unwrap_or(0) as usize)
            | ((self.get16(n + NODE_HI).unwrap_or(0) as usize) << 16)
    }

    fn set_node_dsize(&mut self, n: usize, size: u32) {
        self.put16(n + NODE_LO, size as u16);
        self.put16(n + NODE_HI, (size >> 16) as u16);
    }

    /// Named DB records: data of `F_SUBDATA` nodes without `F_DUPDATA`.
    fn named_db_records(&self) -> Vec<usize> {
        self.leaf_nodes_with(|f| f & (F_SUBDATA | F_DUPDATA) == F_SUBDATA)
            .into_iter()
            .map(|n| self.node_data(n))
            .collect()
    }

    /// Sub-DB records of DUPSORT keys: data of `F_SUBDATA|F_DUPDATA` nodes.
    fn subdb_records(&self) -> Vec<usize> {
        self.leaf_nodes_with(|f| f & (F_SUBDATA | F_DUPDATA) == (F_SUBDATA | F_DUPDATA))
            .into_iter()
            .map(|n| self.node_data(n))
            .collect()
    }

    /// Nodes holding a sub-page (`F_DUPDATA` without `F_SUBDATA`).
    fn subpage_nodes(&self) -> Vec<usize> {
        self.leaf_nodes_with(|f| f & (F_SUBDATA | F_DUPDATA) == F_DUPDATA)
    }

    /// Nodes of the free DB (8-byte integer keys, IDL data).
    fn freedb_nodes(&self) -> Vec<usize> {
        self.leaf_nodes_with(|f| f == 0)
            .into_iter()
            .filter(|&n| {
                let ks = self.get16(n + 6).unwrap_or(0) as usize;
                let ds = self.node_dsize(n);
                ks == 8 && ds >= 16 && ds % 8 == 0 && {
                    let count = self.get64(self.node_data(n)).unwrap_or(u64::MAX);
                    count as usize + 1 == ds / 8
                }
            })
            .collect()
    }

    /// Plain leaf nodes of the main DB (keys `k.....` / `big..`).
    fn main_nodes(&self) -> Vec<usize> {
        self.leaf_nodes_with(|f| f & (F_SUBDATA | F_DUPDATA) == 0)
            .into_iter()
            .filter(|&n| {
                let k = self.node_key(n);
                k.starts_with(b"k") || k.starts_with(b"big")
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Exercising a damaged environment
// ---------------------------------------------------------------------------

const WALK_CAP: usize = 200_000;

fn walk(c: &mut RoCursor<'_>, first: CursorOp, next: CursorOp) -> usize {
    let mut n = 0;
    let mut op = first;
    while n < WALK_CAP {
        match c.get(None, None, op) {
            Ok(Some(_)) => n += 1,
            _ => break,
        }
        op = next;
    }
    n
}

fn open_dbs(txn: &impl Transaction) -> Vec<(Dbi, bool)> {
    let mut v = vec![(Dbi::MAIN, false)];
    if let Ok(d) = txn.open_db(Some("dups")) {
        v.push((d, true));
    }
    if let Ok(d) = txn.open_db(Some("fixed")) {
        v.push((d, true));
    }
    v
}

fn read_env(env: &Environment) {
    let Ok(txn) = env.begin_ro_txn() else {
        return;
    };
    let _ = txn.stat(Dbi::MAIN);
    let _ = env.freelist();
    for (db, dup) in open_dbs(&txn) {
        let _ = txn.stat(db);
        let Ok(mut c) = txn.cursor(db) else {
            continue;
        };
        walk(&mut c, CursorOp::First, CursorOp::Next);
        walk(&mut c, CursorOp::Last, CursorOp::Prev);
        walk(&mut c, CursorOp::First, CursorOp::NextNoDup);
        walk(&mut c, CursorOp::Last, CursorOp::PrevNoDup);
        for k in [
            main_key(7),
            main_key(8),
            big_key(1),
            dup_key(3),
            dup_key(7),
            b"dsub".to_vec(),
            fixed_key(5),
            b"fsub".to_vec(),
            b"zzz".to_vec(),
        ] {
            let _ = txn.get(db, &k);
            let _ = c.get(Some(&k), None, CursorOp::Set);
            let _ = c.get(Some(&k), None, CursorOp::SetKey);
            let _ = c.get(Some(&k), None, CursorOp::SetRange);
            let _ = c.get(None, None, CursorOp::GetCurrent);
            let _ = c.count();
            if dup {
                let _ = c.get(Some(&k), Some(b"v001"), CursorOp::GetBoth);
                let _ = c.get(Some(&k), Some(b"v"), CursorOp::GetBothRange);
                let _ = c.get(Some(&k), Some(&5u64.to_be_bytes()), CursorOp::GetBothRange);
                let _ = c.get(None, None, CursorOp::FirstDup);
                let _ = c.get(None, None, CursorOp::LastDup);
                walk(&mut c, CursorOp::FirstDup, CursorOp::NextDup);
                walk(&mut c, CursorOp::LastDup, CursorOp::PrevDup);
                let _ = c.get(None, None, CursorOp::GetMultiple);
                let mut n = 0;
                while n < 1000 && matches!(c.get(None, None, CursorOp::NextMultiple), Ok(Some(_))) {
                    n += 1;
                }
                let _ = c.get(None, None, CursorOp::PrevMultiple);
            }
            let _ = c.get(None, None, CursorOp::Next);
            let _ = c.get(None, None, CursorOp::Prev);
        }
    }
}

fn write_env(env: &Environment) -> Vec<Error> {
    let mut errs = Vec::new();
    let Ok(mut txn) = env.begin_rw_txn() else {
        return errs;
    };
    let dbs = open_dbs(&txn);
    let mut rec = |r: rlmdb::Result<()>| {
        if let Err(e) = r {
            errs.push(e);
        }
    };
    for &(db, dup) in &dbs {
        // New keys, overwrites, big values, deletes.
        for i in 0..40 {
            rec(txn.put(
                db,
                &format!("n{i:04}").into_bytes(),
                &[1; 30],
                WriteFlags::empty(),
            ));
        }
        rec(txn.put(db, &main_key(11), &[2; 20], WriteFlags::empty()));
        rec(txn.put(db, &big_key(1), &[3; 10], WriteFlags::empty()));
        rec(txn.put(db, &big_key(2), &big_value(9), WriteFlags::empty()));
        rec(txn.put(db, &big_key(3), &vec![4u8; 200_000], WriteFlags::empty()));
        rec(txn.put(db, b"zzzz", &[5; 8], WriteFlags::APPEND));
        rec(txn.del(db, &main_key(12), None).map(|_| ()));
        rec(txn.del(db, &big_key(4), None).map(|_| ()));
        rec(txn.del(db, &main_key(13), Some(&[b'v'; 40])).map(|_| ()));
        if dup {
            for k in [
                dup_key(3),
                dup_key(7),
                b"dsub".to_vec(),
                fixed_key(5),
                b"fsub".to_vec(),
            ] {
                rec(txn.put(db, &k, b"a", WriteFlags::empty()));
                rec(txn.put(db, &k, b"zzzzzzzz", WriteFlags::APPEND_DUP));
                rec(txn.put(db, &k, b"v001", WriteFlags::NO_DUP_DATA));
                rec(txn.put(db, &k, &7u64.to_be_bytes(), WriteFlags::empty()));
                rec(txn.put(db, &k, &9u64.to_be_bytes(), WriteFlags::APPEND_DUP));
                rec(txn.del(db, &k, Some(b"v001")).map(|_| ()));
                rec(txn.del(db, &k, Some(&1u64.to_be_bytes())).map(|_| ()));
            }
            rec(txn.del(db, &dup_key(9), None).map(|_| ()));
            rec(txn.del(db, &fixed_key(9), None).map(|_| ()));
        }
        // Cursor writes, including MDB_CURRENT on duplicates.
        if let Ok(mut c) = txn.cursor_mut(db) {
            for k in [
                main_key(20),
                dup_key(20),
                b"dsub".to_vec(),
                fixed_key(20),
                b"fsub".to_vec(),
            ] {
                if let Ok(Some(_)) = c.set_range(&k) {
                    let _ = c.get_current();
                    let _ = c.put(&k, b"cur", WriteFlags::CURRENT);
                    let _ = c.put(&k, &3u64.to_be_bytes(), WriteFlags::CURRENT);
                    let _ = c.next();
                    let _ = c.del(WriteFlags::empty());
                    let _ = c.next();
                    let _ = c.del(WriteFlags::NO_DUP_DATA);
                    let _ = c.put(&k, b"after", WriteFlags::empty());
                    let _ = c.count();
                }
            }
            let _ = c.first();
            let _ = c.del(WriteFlags::empty());
            let _ = c.last();
            let _ = c.del(WriteFlags::empty());
        }
    }
    // A nested transaction that writes and aborts, and one that commits.
    if let Ok(mut child) = txn.begin_nested_txn() {
        for &(db, _) in &dbs {
            let _ = child.put(db, b"child", b"x", WriteFlags::empty());
            let _ = child.del(db, &main_key(30), None);
        }
        child.abort();
    }
    if let Ok(mut child) = txn.begin_nested_txn() {
        for &(db, _) in &dbs {
            let _ = child.put(db, b"child2", b"y", WriteFlags::empty());
        }
        let _ = child.commit();
    }
    rec(txn.commit());

    // Clear and drop the named DBs, then force allocations from the free
    // list.
    if let Ok(mut txn) = env.begin_rw_txn() {
        if let Ok(d) = txn.open_db(Some("dups")) {
            let _ = txn.clear_db(d);
        }
        if let Ok(d) = txn.open_db(Some("fixed")) {
            let _ = txn.drop_db(d);
        }
        for i in 0..2000u32 {
            let _ = txn.put(Dbi::MAIN, &i.to_be_bytes(), &[6; 100], WriteFlags::empty());
        }
        let _ = txn.commit();
    }
    errs
}

fn copy_env(env: &Environment, dir: &Path) {
    for (i, compact) in [false, true].into_iter().enumerate() {
        let target = dir.join(format!("copy{i}"));
        std::fs::create_dir_all(&target).unwrap();
        if env.copy_to_path(&target, compact).is_err() {
            continue;
        }
        if let Ok(copy) = Environment::builder()
            .max_dbs(8)
            .flags(EnvironmentFlags::NO_SYNC)
            .open(&target)
        {
            read_env(&copy);
        }
    }
}

fn open(dir: &Path) -> rlmdb::Result<Environment> {
    Environment::builder()
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
}

/// Everything a case does; returns the open error, if any.
fn exercise(dir: &Path) -> Option<Error> {
    let env = match open(dir) {
        Ok(env) => env,
        Err(e) => return Some(e),
    };
    read_env(&env);
    let _ = write_env(&env);
    read_env(&env);
    let _ = write_env(&env);
    copy_env(&env, dir);
    drop(env);
    // Reopen after the writes (the file may now be larger or carry a new
    // snapshot).
    if let Ok(env) = open(dir) {
        read_env(&env);
    }
    None
}

// ---------------------------------------------------------------------------
// The driver: watchdog and panic capture
// ---------------------------------------------------------------------------

fn panics() -> &'static Mutex<Vec<(String, String)>> {
    static P: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

fn install_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let name = std::thread::current().name().unwrap_or("?").to_string();
            if name.starts_with("case-") {
                let msg = info.to_string();
                panics()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((name, msg));
            } else {
                prev(info);
            }
        }));
    });
}

enum Outcome {
    Ok,
    Panic(String),
    Hang,
}

/// Writes the mutated image to a fresh directory and runs `f` on it in a
/// watched thread.
fn run_case<T: Send + 'static>(
    name: &str,
    mutate: impl FnOnce(&mut Img),
    f: impl FnOnce(&Path) -> T + Send + 'static,
) -> (Outcome, Option<T>, PathBuf) {
    install_hook();
    let base = base();
    let mut img = Img {
        buf: base.data.clone(),
        ps: base.ps,
    };
    mutate(&mut img);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    std::fs::write(path.join("data.mdb"), &img.buf).unwrap();
    let tname = format!("case-{name}");
    let (tx, rx) = mpsc::channel();
    let d = path.clone();
    let handle = std::thread::Builder::new()
        .name(tname.clone())
        .spawn(move || {
            let r = f(&d);
            let _ = tx.send(r);
        })
        .unwrap();
    let outcome = match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(r) => {
            let _ = handle.join();
            return (Outcome::Ok, Some(r), path);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            let msg = panics()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|(n, _)| *n == tname)
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| "panic (no message)".into());
            Outcome::Panic(msg)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::Hang,
    };
    // Keep the directory for inspection.
    let keep = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pylmdb-audit");
    let _ = std::fs::create_dir_all(&keep);
    let _ = std::fs::write(keep.join(format!("{name}.mdb")), &img.buf);
    std::mem::forget(dir);
    (outcome, None, path)
}

/// Runs the standard exercise on the mutated image; fails on panic/hang.
fn check(name: &str, mutate: impl FnOnce(&mut Img)) -> Option<Error> {
    let (outcome, r, _) = run_case(name, mutate, exercise);
    match outcome {
        Outcome::Ok => r.flatten(),
        Outcome::Panic(m) => panic!("case {name}: PANIC: {m}"),
        Outcome::Hang => panic!("case {name}: HANG"),
    }
}

fn is_mdb(e: &Option<Error>, want: MdbError) -> bool {
    matches!(e, Some(Error::Mdb(x)) if *x == want)
}

// ---------------------------------------------------------------------------
// The cases, one group per py-lmdb patch
// ---------------------------------------------------------------------------

#[test]
fn base_is_well_formed() {
    let b = base();
    let img = Img {
        buf: b.data.clone(),
        ps: b.ps,
    };
    // The shapes the cases rely on must exist.
    assert!(!img.subpage_nodes().is_empty(), "no sub-pages");
    // (Stale copies of records survive on freed pages, so "at least".)
    assert!(img.subdb_records().len() >= 2, "sub-DB records");
    assert!(img.named_db_records().len() >= 2, "named DB records");
    assert!(!img.freedb_nodes().is_empty(), "no free DB records");
    assert!(
        !img.pages_with(|f| f & P_OVERFLOW != 0).is_empty(),
        "no overflow"
    );
    assert!(
        !img.pages_with(|f| f & P_LEAF2 != 0).is_empty(),
        "no LEAF2 pages"
    );
    assert!(
        !img.pages_with(|f| f & P_BRANCH != 0).is_empty(),
        "no branch pages"
    );
    assert!(img.main_nodes().len() > 100);
    // And a pristine copy must pass the exercise without error.
    let (outcome, r, _) = run_case("pristine", |_| {}, exercise);
    assert!(matches!(outcome, Outcome::Ok));
    assert!(r.unwrap().is_none());
}

/// CVE-2019-16228 (`cve-2019-16228-validate-psize`): `mm_psize` of 0, not a
/// power of two, or absurd divides by zero / addresses outside the map.
#[test]
fn cve_2019_16228_page_size() {
    for (what, v) in [
        ("zero", 0u32),
        ("odd", 4097),
        ("tiny", 16),
        ("huge", 1 << 20),
        ("max+1", 0xffff_ffff),
    ] {
        for meta in [0usize, 1] {
            let e = check(&format!("psize-{what}-m{meta}"), |img| {
                img.put32(meta * img.ps + 16 + 24, v);
            });
            // Both metas carry the bad size, or the one we did not touch
            // is older: C validates the newer header. Whatever the engine
            // picks, a bad page size must be refused with MDB_INVALID, or
            // (if the untouched, older meta was picked) open may succeed.
            if let Some(e) = &e {
                assert!(
                    matches!(e, Error::Mdb(MdbError::Invalid | MdbError::Corrupted)),
                    "psize {what} meta {meta}: {e:?}"
                );
            }
        }
        // Both metas at once: must fail.
        let e = check(&format!("psize-{what}-both"), |img| {
            let ps = img.ps;
            img.put32(16 + 24, v);
            img.put32(ps + 16 + 24, v);
        });
        assert!(
            is_mdb(&e, MdbError::Invalid),
            "psize {what} both metas: {e:?}"
        );
    }
    // A power of two that is not the real size: the second meta is read
    // from the wrong offset and nothing lines up.
    for v in [256u32, 512, 32768] {
        let e = check(&format!("psize-wrong-{v}"), |img| {
            let ps = img.ps;
            if v as usize != ps {
                img.put32(16 + 24, v);
                img.put32(ps + 16 + 24, v);
            }
        });
        let _ = e;
    }
}

/// CVE-2019-16224 (`cve-2019-16224-validate-db-flags`): `MDB_DUPFIXED`
/// (and the other DUPSORT-only flags) without `MDB_DUPSORT` in an on-disk
/// `md_flags` makes C build `P_LEAF2` pages for a DB without a sub-cursor.
#[test]
fn cve_2019_16224_db_flags() {
    for flags in [MDB_DUPFIXED, 0x20, 0x40, 0x70, 0x7e] {
        // In the meta page's main DB record.
        check(&format!("dbflags-main-{flags:#x}"), |img| {
            let m = img.meta();
            img.put16(m + 16 + 24 + 48 + 4, flags);
        });
        // In both named DB records.
        check(&format!("dbflags-named-{flags:#x}"), |img| {
            for r in img.named_db_records() {
                img.put16(r + 4, flags);
            }
        });
        // In the sub-DB records.
        check(&format!("dbflags-subdb-{flags:#x}"), |img| {
            for r in img.subdb_records() {
                img.put16(r + 4, flags);
            }
        });
    }
    // The main DB record carrying DUPSORT (so named DBs cannot be opened,
    // and sub-cursors appear on a DB with no duplicates).
    check("dbflags-main-dupsort", |img| {
        let m = img.meta();
        img.put16(m + 16 + 24 + 48 + 4, 0x04);
    });
}

/// CVE-2019-16225 (`cve-2019-16225-reject-dirty-pages`): a page in the file
/// with `P_DIRTY` set makes `mdb_page_touch` skip the copy-on-write, so C
/// writes through the read-only map (SIGSEGV), or with `MDB_WRITEMAP`
/// modifies the previous snapshot in place (`abort` does not roll back).
#[test]
fn cve_2019_16225_dirty_flag_on_disk() {
    let (outcome, r, _) = run_case(
        "p-dirty",
        |img| {
            for pg in img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0) {
                let f = img.flags(pg);
                img.put16(pg * img.ps + 10, f | P_DIRTY);
            }
        },
        |d| {
            let before = std::fs::read(d.join("data.mdb")).unwrap();
            let env = open(d).unwrap();
            let mut txn = env.begin_rw_txn().unwrap();
            let r1 = txn.put(Dbi::MAIN, b"attack", b"MODIFIED", WriteFlags::empty());
            let r2 = txn.put(Dbi::MAIN, &main_key(21), b"MODIFIED", WriteFlags::empty());
            let r3 = txn.del(Dbi::MAIN, &main_key(22), None).map(|_| ());
            txn.abort();
            drop(env);
            let after = std::fs::read(d.join("data.mdb")).unwrap();
            (r1, r2, r3, before == after)
        },
    );
    match outcome {
        Outcome::Panic(m) => panic!("p-dirty: PANIC: {m}"),
        Outcome::Hang => panic!("p-dirty: HANG"),
        Outcome::Ok => {}
    }
    let (r1, r2, r3, unchanged) = r.unwrap();
    // Either the engine refuses to write through such a page, or it
    // copies it properly; it must never write the snapshot in place.
    assert!(
        unchanged,
        "a write with forged P_DIRTY changed the file after abort"
    );
    eprintln!("p-dirty: put {r1:?} / {r2:?} / del {r3:?} (file unchanged)");
    // And the full exercise must not panic.
    check("p-dirty-exercise", |img| {
        for pg in img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0) {
            let f = img.flags(pg);
            img.put16(pg * img.ps + 10, f | P_DIRTY);
        }
    });
    // Overflow pages too (ovpage_free / overwrite paths).
    check("p-dirty-overflow", |img| {
        for pg in img.pages_with(|f| f & P_OVERFLOW != 0) {
            let f = img.flags(pg);
            img.put16(pg * img.ps + 10, f | P_DIRTY);
        }
    });
}

/// CVE-2019-16226 (`cve-2019-16226-validate-node-del-size`) and
/// `validate-node-read-size`, `validate-nodedsz-cursor-put`,
/// `validate-nodedsz-page-split`: a node whose `NODEDSZ` (mn_lo/mn_hi)
/// runs past the page is read, memmoved, copied and split by C.
#[test]
fn cve_2019_16226_node_data_size() {
    for (what, size) in [
        ("max", 0xffff_ffffu32),
        ("page", 0x1_0000),
        ("page-ish", 0xfff0),
        ("plus1", 0),
    ] {
        check(&format!("nodedsz-main-{what}"), |img| {
            for n in img.main_nodes() {
                let v = if size == 0 {
                    img.node_dsize(n) as u32 + (img.ps as u32 - 8)
                } else {
                    size
                };
                img.set_node_dsize(n, v);
            }
        });
        check(&format!("nodedsz-all-{what}"), |img| {
            for n in img.leaf_nodes_with(|_| true) {
                let v = if size == 0 {
                    img.node_dsize(n) as u32 + (img.ps as u32 - 8)
                } else {
                    size
                };
                img.set_node_dsize(n, v);
            }
        });
    }
    // Key sizes past the page as well.
    for ks in [0xffffu16, 4096, 600] {
        check(&format!("nodeksize-{ks}"), |img| {
            for pg in img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0 && f & P_LEAF2 == 0) {
                for n in img.nodes(pg) {
                    img.put16(n + 6, ks);
                }
            }
        });
    }
}

/// CVE-2019-16227 (`cve-2019-16227-guard-xcursor-null`) and
/// `guard-xcursor-null-d3d4`: `F_DUPDATA` on a node of a database without
/// `MDB_DUPSORT` makes C follow a NULL sub-cursor pointer.
#[test]
fn cve_2019_16227_dupdata_in_plain_db() {
    for flags in [F_DUPDATA, F_DUPDATA | F_SUBDATA, F_SUBDATA, F_BIGDATA, 0x07] {
        check(&format!("plainflags-{flags:#x}"), |img| {
            for n in img.main_nodes() {
                img.put16(n + 4, flags);
            }
        });
        // Only some of them (the cursor then meets a mix).
        check(&format!("plainflags-some-{flags:#x}"), |img| {
            for (i, n) in img.main_nodes().into_iter().enumerate() {
                if i % 3 == 0 {
                    img.put16(n + 4, flags);
                }
            }
        });
    }
}

/// `validate-page-bounds`: `mp_lower` below the header, `mp_upper` past
/// the page, or `mp_lower > mp_upper`: `NUMKEYS` wraps, `SIZELEFT`
/// underflows.
#[test]
fn validate_page_bounds() {
    let ps = base().ps as u16;
    for (what, lower, upper) in [
        ("lower0", Some(0u16), None),
        ("lower8", Some(8), None),
        ("lower-max", Some(0xffff), None),
        ("lower-odd", Some(17), None),
        ("upper0", None, Some(0u16)),
        ("upper-past", None, Some(ps.wrapping_add(100))),
        ("upper-max", None, Some(0xffff)),
        ("crossed", Some(ps - 2), Some(16)),
        ("equal-max", Some(0xffff), Some(0xffff)),
    ] {
        for kind in ["leaf", "branch", "all"] {
            check(&format!("bounds-{what}-{kind}"), |img| {
                let want: fn(u16) -> bool = match kind {
                    "leaf" => |f| f & P_LEAF != 0,
                    "branch" => |f| f & P_BRANCH != 0,
                    _ => |f| f & (P_BRANCH | P_LEAF) != 0,
                };
                for pg in img.pages_with(want) {
                    if let Some(l) = lower {
                        img.put16(pg * img.ps + 12, l);
                    }
                    if let Some(u) = upper {
                        img.put16(pg * img.ps + 14, u);
                    }
                }
            });
        }
    }
}

/// `validate-leaf2-keysize` and `validate-md-pad`: the LEAF2 key size on
/// the page (`mp_pad`) and in the DB record (`md_pad`) are used as pointer
/// strides and as memcpy lengths; nothing bounds them and nothing makes
/// them agree.
#[test]
fn validate_leaf2_key_size() {
    let ps = base().ps as u32;
    for v in [
        0u32,
        1,
        3,
        7,
        9,
        0xffff,
        ps,
        ps / 8,
        ps - 16,
        8 * ps,
        0xffff_ffff,
    ] {
        // On every LEAF2 page.
        if v <= 0xffff {
            check(&format!("mp_pad-{v}"), |img| {
                for pg in img.pages_with(|f| f & P_LEAF2 != 0) {
                    img.put16(pg * img.ps + 8, v as u16);
                }
            });
            // On every LEAF2 sub-page.
            check(&format!("subpage-pad-{v}"), |img| {
                for n in img.subpage_nodes() {
                    let sp = img.node_data(n);
                    if img.get16(sp + 10).unwrap_or(0) & P_LEAF2 != 0 {
                        img.put16(sp + 8, v as u16);
                    }
                }
            });
        }
        // In the sub-DB records (the one every LEAF2 access uses).
        check(&format!("md_pad-subdb-{v}"), |img| {
            for r in img.subdb_records() {
                img.put32(r, v);
            }
        });
        // In the named DB record of the DUPFIXED DB.
        check(&format!("md_pad-named-{v}"), |img| {
            for r in img.named_db_records() {
                img.put32(r, v);
            }
        });
        // In the meta page's main DB record.
        check(&format!("md_pad-main-{v}"), |img| {
            let m = img.meta();
            img.put32(m + 16 + 24 + 48, v);
        });
    }
}

/// `validate-md-depth`: `md_depth > CURSOR_STACK` overruns the cursor's
/// page stack in C.
#[test]
fn validate_md_depth() {
    for v in [0u16, 1, 32, 33, 100, 0xffff] {
        check(&format!("md_depth-{v}"), |img| {
            let m = img.meta();
            img.put16(m + 16 + 24 + 6, v);
            img.put16(m + 16 + 24 + 48 + 6, v);
            for r in img.named_db_records() {
                img.put16(r + 6, v);
            }
            for r in img.subdb_records() {
                img.put16(r + 6, v);
            }
        });
    }
}

/// `validate-md-root`: a root page number naming a meta page (0 or 1); plus
/// the other bogus roots and child pointers the Rust port must reject.
#[test]
fn validate_md_root() {
    let np = base().data.len() as u64 / base().ps as u64;
    for (what, v) in [
        ("meta0", 0u64),
        ("meta1", 1),
        ("past-end", np + 7),
        ("last", np - 1),
        ("huge", 1 << 47),
        ("max", u64::MAX),
        ("invalid", u64::MAX), // P_INVALID: an empty tree
    ] {
        check(&format!("md_root-{what}"), |img| {
            let m = img.meta();
            img.put64(m + 16 + 24 + 40, v);
            img.put64(m + 16 + 24 + 48 + 40, v);
            for r in img.named_db_records() {
                img.put64(r + 40, v);
            }
            for r in img.subdb_records() {
                img.put64(r + 40, v);
            }
        });
    }
    // Roots pointing at a leaf, an overflow page, and each other.
    check("md_root-leaf", |img| {
        let leaf = img.pages_with(|f| f & P_LEAF != 0)[0] as u64;
        let m = img.meta();
        img.put64(m + 16 + 24 + 48 + 40, leaf);
        for r in img.named_db_records() {
            img.put64(r + 40, leaf);
        }
    });
    check("md_root-overflow", |img| {
        let ov = img.pages_with(|f| f & P_OVERFLOW != 0)[0] as u64;
        let m = img.meta();
        img.put64(m + 16 + 24 + 48 + 40, ov);
        img.put64(m + 16 + 24 + 40, ov);
    });
    // Branch children forming cycles (self and root).
    check("child-cycle", |img| {
        let branches = img.pages_with(|f| f & P_BRANCH != 0);
        for &pg in &branches {
            for (i, n) in img.nodes(pg).into_iter().enumerate() {
                let target = if i % 2 == 0 { pg } else { branches[0] } as u64;
                img.put16(n + NODE_LO, target as u16);
                img.put16(n + NODE_HI, (target >> 16) as u16);
                img.put16(n + 4, (target >> 32) as u16);
            }
        }
    });
    check("child-meta", |img| {
        for pg in img.pages_with(|f| f & P_BRANCH != 0) {
            for n in img.nodes(pg) {
                img.put16(n + NODE_LO, 1);
                img.put16(n + NODE_HI, 0);
                img.put16(n + 4, 0);
            }
        }
    });
}

/// `validate-node-shrink-delta` and `validate-subpage-bounds`: a sub-page
/// whose `mp_lower`/`mp_upper` lie about its size makes `mdb_node_shrink`
/// underflow and the sub-page growth `memcpy` wrap.
#[test]
fn validate_subpage_bounds() {
    for (what, field, v) in [
        ("lower0", 12usize, Some(0u16)),
        ("lower-max", 12, Some(0xffff)),
        ("lower-big", 12, None),
        ("upper0", 14, Some(0)),
        ("upper-max", 14, Some(0xffff)),
        ("upper-past", 14, None),
        ("upper-past2", 14, None),
        ("flags-leaf2", 10, Some(P_LEAF | 0x40 | P_LEAF2)),
        ("flags-branch", 10, Some(P_BRANCH | 0x40)),
        ("flags-overflow", 10, Some(P_OVERFLOW)),
        ("flags-zero", 10, Some(0)),
        ("ptr0", 16, Some(0)),
        ("ptr-max", 16, Some(0xffff)),
    ] {
        check(&format!("subpage-{what}"), |img| {
            for n in img.subpage_nodes() {
                let sp = img.node_data(n);
                let ds = img.node_dsize(n) as u16;
                let v = match (v, what) {
                    (Some(v), _) => v,
                    (None, "lower-big") => ds + 40,
                    (None, "upper-past") => ds + 2,
                    _ => ds + 200,
                };
                img.put16(sp + field, v);
            }
        });
    }
}

/// `validate-xcursor-nodedsz`: an `F_SUBDATA` node whose data is shorter
/// than `sizeof(MDB_db)` is memcpy'd as a DB record. Also sub-page nodes
/// whose data is shorter than a page header (the Rust port must not index
/// a header into a too-short slice).
#[test]
fn validate_xcursor_node_data_size() {
    for size in [0u32, 1, 8, 12, 15, 16, 20, 47] {
        check(&format!("subdb-dsize-{size}"), |img| {
            for n in img.leaf_nodes_with(|f| f & F_SUBDATA != 0) {
                img.set_node_dsize(n, size);
            }
        });
        check(&format!("subpage-dsize-{size}"), |img| {
            for n in img.subpage_nodes() {
                img.set_node_dsize(n, size);
            }
        });
        // Only the sub-page nodes of the DUPFIXED DB (the LEAF2 branch of
        // the put path reads mp_pad from the short data).
        check(&format!("subpage-fixed-dsize-{size}"), |img| {
            for n in img.subpage_nodes() {
                if img.node_key(n).starts_with(b"f") {
                    img.set_node_dsize(n, size);
                }
            }
        });
    }
}

/// `validate-overflow-pages`, `validate-ovpage-free` and
/// `fix-overflow-page-size-mul`: an overflow run's `mp_pages` (and the node
/// data size it is compared with) are used as allocation sizes, free-list
/// extents and `memcpy` lengths without a bound; `num * psize` wraps in
/// 32-bit arithmetic.
#[test]
fn validate_overflow_pages() {
    let np = base().data.len() as u32 / base().ps as u32;
    for (what, v) in [
        ("zero", 0u32),
        ("one", 1),
        ("two", 2),
        ("past-end", np + 5),
        ("100000", 100_000),
        ("mul-wrap", 0x4000_0000),
        ("max", u32::MAX),
    ] {
        check(&format!("ovpages-{what}"), |img| {
            for pg in img.pages_with(|f| f & P_OVERFLOW != 0) {
                img.put32(pg * img.ps + 12, v);
            }
        });
    }
    // Not an overflow page at all: the run's first page claims to be a
    // leaf, a branch, a meta page, or nothing.
    for flags in [P_LEAF, P_BRANCH, 0x08, 0, P_LEAF2 | P_LEAF] {
        check(&format!("ovflags-{flags:#x}"), |img| {
            for pg in img.pages_with(|f| f & P_OVERFLOW != 0) {
                img.put16(pg * img.ps + 10, flags);
            }
        });
    }
    // The F_BIGDATA node pointing at a B-tree page instead.
    check("ovpgno-leaf", |img| {
        let leaf = img.pages_with(|f| f & P_LEAF != 0)[0] as u64;
        for n in img.leaf_nodes_with(|f| f & F_BIGDATA != 0) {
            let d = img.node_data(n);
            img.put64(d, leaf);
        }
    });
    check("ovpgno-meta", |img| {
        for n in img.leaf_nodes_with(|f| f & F_BIGDATA != 0) {
            let d = img.node_data(n);
            img.put64(d, 1);
        }
    });
    check("ovpgno-past", |img| {
        let np = img.npages() as u64;
        for n in img.leaf_nodes_with(|f| f & F_BIGDATA != 0) {
            let d = img.node_data(n);
            img.put64(d, np + 3);
        }
    });
    // Big data sizes that do not match the run.
    for size in [0u32, 16, 0xffff_ffff, 0x1000_0000] {
        check(&format!("bigdata-dsize-{size}"), |img| {
            for n in img.leaf_nodes_with(|f| f & F_BIGDATA != 0) {
                img.set_node_dsize(n, size);
            }
        });
    }
}

/// `validate-freedb-record`: a free DB record whose count lies about the
/// node size, lists meta pages, pages past the end or the same page twice;
/// C merges garbage into `me_pghead`, hands out arbitrary page numbers and
/// aborts on a duplicate.
#[test]
fn validate_freedb_record() {
    let np = base().data.len() as u64 / base().ps as u64;
    for (what, count) in [
        ("count-plus1", None),
        ("count-max", Some(u64::MAX)),
        ("count-0", Some(0u64)),
        ("count-1e6", Some(1_000_000)),
    ] {
        check(&format!("freedb-{what}"), |img| {
            for n in img.freedb_nodes() {
                let d = img.node_data(n);
                let v = count.unwrap_or(img.node_dsize(n) as u64 / 8);
                img.put64(d, v);
            }
        });
    }
    for (what, v) in [
        ("pgno-meta0", 0u64),
        ("pgno-meta1", 1),
        ("pgno-past", np + 10),
        ("pgno-last", np - 1),
        ("pgno-max", u64::MAX),
        ("pgno-huge", 1 << 47),
    ] {
        check(&format!("freedb-{what}"), |img| {
            for n in img.freedb_nodes() {
                let d = img.node_data(n);
                let cnt = img.get64(d).unwrap_or(0) as usize;
                for i in 1..=cnt {
                    if i % 2 == 1 {
                        img.put64(d + 8 * i, v);
                    }
                }
            }
        });
    }
    // Duplicates within a record and across records; a live page (the
    // first leaf) listed as free.
    check("freedb-dup-within", |img| {
        for n in img.freedb_nodes() {
            let d = img.node_data(n);
            let cnt = img.get64(d).unwrap_or(0) as usize;
            if cnt >= 2 {
                let first = img.get64(d + 8).unwrap_or(2);
                for i in 1..=cnt {
                    img.put64(d + 8 * i, first);
                }
            }
        }
    });
    check("freedb-live-page", |img| {
        let leaf = img.pages_with(|f| f & P_LEAF != 0)[0] as u64;
        for n in img.freedb_nodes() {
            let d = img.node_data(n);
            let cnt = img.get64(d).unwrap_or(0) as usize;
            if cnt >= 1 {
                img.put64(d + 8, leaf);
            }
        }
    });
    // The live main DB root listed as free: the first write touches the
    // root, and the copy-on-write may be handed the root's own page
    // number by the allocator (C: the page is then both live and on the
    // free list, so a later transaction overwrites it).
    check("freedb-live-root", |img| {
        let m = img.meta();
        let root = img.get64(m + 16 + 24 + 48 + 40).unwrap();
        // Every record becomes a one-entry record naming the root (the
        // allocator prefers the lowest free page number, so the root must
        // be the only candidate).
        for n in img.freedb_nodes() {
            let d = img.node_data(n);
            img.set_node_dsize(n, 16);
            img.put64(d, 1);
            img.put64(d + 8, root);
        }
    });
    // Unsorted / ascending order (C expects descending).
    check("freedb-ascending", |img| {
        for n in img.freedb_nodes() {
            let d = img.node_data(n);
            let cnt = img.get64(d).unwrap_or(0) as usize;
            let mut ids: Vec<u64> = (1..=cnt).map(|i| img.get64(d + 8 * i).unwrap()).collect();
            ids.sort();
            for (i, v) in ids.into_iter().enumerate() {
                img.put64(d + 8 * (i + 1), v);
            }
        }
    });
    // The record's node data size too short for the count (and odd).
    check("freedb-short-node", |img| {
        for n in img.freedb_nodes() {
            img.set_node_dsize(n, 12);
        }
    });
}

/// `cursor-next-prev-uninitialized`: not a hardening patch (an API
/// behaviour change in py-lmdb). Recorded here only so the mapping is
/// complete: MDB_NEXT on a cursor that was left uninitialised by a delete
/// must not loop forever or panic.
#[test]
fn cursor_next_after_delete_leaves_tree_empty() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path()).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("one"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap();
    txn.put(db, b"k", b"a", WriteFlags::empty()).unwrap();
    txn.put(db, b"k", b"b", WriteFlags::empty()).unwrap();
    {
        let mut c = txn.cursor_mut(db).unwrap();
        c.first().unwrap();
        c.del(WriteFlags::NO_DUP_DATA).unwrap();
        // Tree is now empty; next/prev must terminate.
        for _ in 0..3 {
            let _ = c.next();
            let _ = c.prev();
            let _ = c.get_current();
        }
        c.put(b"k", b"c", WriteFlags::empty()).unwrap();
        c.first().unwrap();
        c.del(WriteFlags::empty()).unwrap();
        for _ in 0..3 {
            let _ = c.next();
            let _ = c.prev();
        }
    }
    txn.commit().unwrap();
}

/// Meta page fields beyond the page size: `mm_last_pg`, `mm_mapsize`,
/// `mm_txnid` lies (the Rust port reads them at every transaction begin).
#[test]
fn meta_page_lies() {
    let np = base().data.len() as u64 / base().ps as u64;
    for (what, off, v) in [
        ("last_pg-0", 16 + 120usize, 0u64),
        ("last_pg-1", 16 + 120, 1),
        ("last_pg-half", 16 + 120, np / 2),
        ("last_pg-past", 16 + 120, np + 50),
        ("last_pg-huge", 16 + 120, u64::MAX >> 13),
        ("last_pg-max", 16 + 120, u64::MAX),
        ("mapsize-0", 16 + 16, 0),
        ("mapsize-3", 16 + 16, 3),
        ("mapsize-huge", 16 + 16, 1 << 62),
        ("txnid-0", 16 + 128, 0),
        ("txnid-max", 16 + 128, u64::MAX),
        ("address", 16 + 8, 0xdead_beef_0000),
    ] {
        check(&format!("meta-{what}"), |img| {
            let m = img.meta();
            img.put64(m + off, v);
        });
    }
}
