//! Corrupt-input robustness: mutation testing of the core against damaged
//! data files.
//!
//! Valid environments written by the C peer (`interop/c/cpeer`, or by
//! the Rust writer when the C tools cannot be built), and LMDB 1.0
//! environments written by the Rust writer with pages of 1 KiB, 4 KiB and
//! 64 KiB (the `*10` bases), are copied, damaged
//! with random byte-level and structure-aware mutations (bit flips, zeroed
//! and random ranges, swapped and duplicated pages, truncation, bogus
//! `mp_lower`/`mp_upper`/`mp_ptrs`, child and overflow page numbers that
//! are huge, point at meta pages or form cycles, node sizes beyond the page,
//! `P_LEAF2` key sizes of 0 or more than a page, overflow page counts past
//! the file, impossible depths and roots in DB records, lying meta pages),
//! and then opened, read (every cursor operation, `get`, `stat`, full walks
//! both ways), written (puts, deletes, cursor writes, nested transactions,
//! `clear_db`/`drop_db`, commit) and copied (plain and compacting).
//!
//! The only requirement is robustness: no panic (debug builds also check
//! integer overflow) and no endless loop (a watchdog bounds every case).
//! Errors of any kind are fine; C itself crashes on many of these inputs.
//!
//! Knobs (environment variables):
//!
//! * `ROBUST_CASES=N`: cases per base environment (default 60);
//! * `ROBUST_SEED=<base>:<seed>`: run just that case (as printed on
//!   failure), e.g. `ROBUST_SEED=named:1234567 cargo test -p rlmdb
//!   --test robust_mutation -- --nocapture`;
//! * `ROBUST_SEED_OFFSET=K`: start at the K-th seed (to explore further);
//! * `ROBUST_TIMEOUT=S`: watchdog per case in seconds (default 60);
//! * `ROBUST_BACKTRACE=1`: report panics with a backtrace.
//!
//! A failing case also leaves its mutated data file in
//! `target/tmp/robust/<base>-<seed>.mdb`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::Rng;
use rlmdb::consts::*;
use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, RoCursor, Transaction, WriteFlags,
};

// ---------------------------------------------------------------------------
// Base environments
// ---------------------------------------------------------------------------

/// A valid environment to start mutating from: its data file and the named
/// databases in it.
struct Base {
    name: &'static str,
    data: Vec<u8>,
    names: Vec<String>,
}

fn peer(c: &common::CTools, args: &[&str]) {
    c.peer_ok(args);
}

/// The data set of `name` written by the C peer (several DB variants per
/// environment, with deletions so that free lists are not empty).
fn build_with_c(c: &common::CTools, name: &str, dir: &Path) -> Vec<String> {
    let p = dir.to_str().unwrap();
    match name {
        "main" => {
            // Plain keys, overflow values, deletions (free pages).
            peer(c, &["create", p, "67108864", "0"]);
            let ds = ["-", "2500", "11", "--valsize", "40", "--big", "6000"];
            peer(c, &[&["fill", p][..], &ds].concat());
            peer(c, &[&["delete", p][..], &ds, &["--every", "5"]].concat());
            Vec::new()
        }
        "big" => {
            // Overflow values of 2+ pages also with 16 KiB pages, a few
            // deleted, a few overwritten with small values (free runs).
            peer(c, &["create", p, "67108864", "0"]);
            let ds = ["-", "240", "17", "--valsize", "100", "--big", "17000"];
            peer(c, &[&["fill", p][..], &ds].concat());
            peer(c, &[&["delete", p][..], &ds, &["--every", "7"]].concat());
            Vec::new()
        }
        "named" => {
            peer(c, &["create", p, "67108864", "8"]);
            let sets: [&[&str]; 5] = [
                &["dups", "400", "12", "--dupsort", "--dups", "300"],
                &["fixed", "200", "13", "--dupfixed", "--dups", "600"],
                &["ik", "800", "14", "--intkey", "--big", "3000"],
                &["rk", "300", "15", "--revkey"],
                &["idup", "150", "16", "--intdup", "--dups", "200"],
            ];
            for ds in sets {
                peer(c, &[&["fill", p][..], ds].concat());
            }
            peer(
                c,
                &[&["delete", p][..], sets[0], &["--every", "3"]].concat(),
            );
            peer(
                c,
                &[&["delete", p][..], sets[2], &["--every", "4"]].concat(),
            );
            ["dups", "fixed", "ik", "rk", "idup"]
                .map(str::to_owned)
                .to_vec()
        }
        _ => unreachable!(),
    }
}

/// The same shapes written by the Rust writer: in the LMDB 0.9 format when
/// the C tools are unavailable, and in the LMDB 1.0 format with pages of
/// `v10_page_size` bytes for the `*10` bases (the 1.0 C tools are only
/// built by `interop`; `interop/tests/format10.rs` checks that C 1.0 reads
/// what this writes).
fn build_with_rust(name: &str, dir: &Path, v10_page_size: Option<usize>) -> Vec<String> {
    let mut builder = Environment::builder();
    builder
        .map_size(64 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = v10_page_size {
        builder.format(rlmdb::Format::V10).page_size(ps);
    }
    let env = builder.open(dir).unwrap();
    // Values that need overflow pages at every page size (at 64 KiB pages
    // a node holds up to about 32 KiB).
    let big = 17000.max(env.page_size() * 2 / 3);
    let mut txn = env.begin_rw_txn().unwrap();
    let names: Vec<String> = match name {
        "big" => {
            for i in 0..240u32 {
                let v = if i % 3 == 0 {
                    vec![7u8; big]
                } else {
                    vec![i as u8; 100]
                };
                txn.put(
                    Dbi::MAIN,
                    format!("k{i:06}").as_bytes(),
                    &v,
                    WriteFlags::empty(),
                )
                .unwrap();
            }
            for i in (0..240u32).step_by(7) {
                txn.del(Dbi::MAIN, format!("k{i:06}").as_bytes(), None)
                    .unwrap();
            }
            Vec::new()
        }
        "main" => {
            for i in 0..2500u32 {
                let v = if i % 3 == 0 {
                    vec![7u8; 6000]
                } else {
                    vec![i as u8; 40]
                };
                txn.put(
                    Dbi::MAIN,
                    format!("k{i:06}").as_bytes(),
                    &v,
                    WriteFlags::empty(),
                )
                .unwrap();
            }
            for i in (0..2500u32).step_by(5) {
                txn.del(Dbi::MAIN, format!("k{i:06}").as_bytes(), None)
                    .unwrap();
            }
            Vec::new()
        }
        "named" => {
            let specs = [
                ("dups", DatabaseFlags::DUP_SORT),
                ("fixed", DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED),
                ("ik", DatabaseFlags::INTEGER_KEY),
                ("rk", DatabaseFlags::REVERSE_KEY),
                (
                    "idup",
                    DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED | DatabaseFlags::INTEGER_DUP,
                ),
            ];
            for (n, f) in specs {
                let db = txn.create_db(Some(n), f | DatabaseFlags::CREATE).unwrap();
                for i in 0..300u64 {
                    let key = if n == "ik" {
                        i.to_ne_bytes().to_vec()
                    } else {
                        format!("{n}{i:05}").into_bytes()
                    };
                    let dups = if f.contains(DatabaseFlags::DUP_SORT) {
                        1 + i % 120
                    } else {
                        1
                    };
                    for d in 0..dups {
                        let v = if n == "idup" {
                            d.to_ne_bytes().to_vec()
                        } else if f.contains(DatabaseFlags::DUP_FIXED) {
                            format!("{d:016}").into_bytes()
                        } else if n == "ik" && i % 4 == 0 {
                            vec![3u8; 3000]
                        } else {
                            format!("v{d:05}-{i}").into_bytes()
                        };
                        txn.put(db, &key, &v, WriteFlags::empty()).unwrap();
                    }
                }
            }
            specs.iter().map(|(n, _)| (*n).to_owned()).collect()
        }
        _ => unreachable!(),
    };
    txn.commit().unwrap();
    names
}

fn base(name: &'static str) -> &'static Base {
    static BASES: OnceLock<Mutex<Vec<&'static Base>>> = OnceLock::new();
    let mut all = BASES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(b) = all.iter().find(|b| b.name == name) {
        return b;
    }
    let dir = tempfile::tempdir().unwrap();
    let names = match (name, common::c_tools()) {
        // LMDB 1.0 bases: small pages for deep trees and many overflow
        // pages.
        ("main10", _) => build_with_rust("main", dir.path(), Some(1024)),
        ("named10", _) => build_with_rust("named", dir.path(), Some(1024)),
        ("big10", _) => build_with_rust("big", dir.path(), Some(4096)),
        // And the largest pages 1.0 allows: offsets near the 16-bit limit
        // (an empty page's `mp_upper` is 65512), large sub-pages.
        ("named10_64k", _) => build_with_rust("named", dir.path(), Some(65536)),
        ("big10_64k", _) => build_with_rust("big", dir.path(), Some(65536)),
        (_, Some(c)) => build_with_c(c, name, dir.path()),
        (_, None) => build_with_rust(name, dir.path(), None),
    };
    let data = std::fs::read(dir.path().join("data.mdb")).unwrap();
    let b: &'static Base = Box::leak(Box::new(Base { name, data, names }));
    all.push(b);
    b
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// A data file image with page-level helpers for the structure-aware
/// mutations, in the layout of its format (taken from the first meta page:
/// LMDB 0.9 has a 16-byte page header with offsets from the page start,
/// 1.0 a 24-byte one with `mp_lower`, `mp_upper` and `mp_ptrs[]` relative
/// to its end and node data after the key padded to an even length).
struct Img {
    buf: Vec<u8>,
    ps: usize,
    /// `PAGEHDRSZ`: 16 or 24.
    hdr: usize,
    /// `PAGEBASE`: 0 or 24.
    pbase: usize,
    /// Whether node data starts after the key padded to an even length.
    even: bool,
}

const P_BRANCH: u16 = 0x01;
const P_LEAF: u16 = 0x02;
const P_OVERFLOW: u16 = 0x04;
const P_LEAF2: u16 = 0x20;
const F_BIGDATA: u16 = 0x01;
const F_SUBDATA: u16 = 0x02;
const F_DUPDATA: u16 = 0x04;

impl Img {
    /// The image of data file `buf` (of a valid base environment).
    fn new(buf: Vec<u8>) -> Img {
        let magic_at =
            |off: usize| buf.get(off..off + 4) == Some(&0xBEEF_C0DEu32.to_ne_bytes()[..]);
        let (hdr, pbase, even) = if magic_at(16) {
            (16, 0, false)
        } else {
            (24, 24, true)
        };
        assert!(magic_at(hdr), "base is not an LMDB data file");
        // mm_psize: mm_dbs[FREE_DBI].md_pad, at 24 in MDB_meta.
        let ps = u32::from_ne_bytes(buf[hdr + 24..hdr + 28].try_into().unwrap()) as usize;
        Img {
            buf,
            ps,
            hdr,
            pbase,
            even,
        }
    }

    /// Offset of `mp_pad` in a page.
    fn f_pad(&self) -> usize {
        self.hdr - 8
    }

    /// Offset of `mp_flags`.
    fn f_flags(&self) -> usize {
        self.hdr - 6
    }

    /// Offset of `mp_lower` (and of `mp_pages` on overflow pages).
    fn f_lower(&self) -> usize {
        self.hdr - 4
    }

    /// Offset of `mp_upper`.
    fn f_upper(&self) -> usize {
        self.hdr - 2
    }

    /// Offset of the data of the node at absolute offset `n`.
    fn node_data(&self, n: usize) -> usize {
        let ks = self.get16(n + 6).unwrap_or(0) as usize;
        n + 8 + if self.even { (ks + 1) & !1 } else { ks }
    }

    fn npages(&self) -> usize {
        self.buf.len() / self.ps
    }

    fn get16(&self, off: usize) -> Option<u16> {
        Some(u16::from_ne_bytes(
            self.buf.get(off..off + 2)?.try_into().ok()?,
        ))
    }

    fn put16(&mut self, off: usize, v: u16) {
        if let Some(b) = self.buf.get_mut(off..off + 2) {
            b.copy_from_slice(&v.to_ne_bytes());
        }
    }

    fn put32(&mut self, off: usize, v: u32) {
        if let Some(b) = self.buf.get_mut(off..off + 4) {
            b.copy_from_slice(&v.to_ne_bytes());
        }
    }

    fn put64(&mut self, off: usize, v: u64) {
        if let Some(b) = self.buf.get_mut(off..off + 8) {
            b.copy_from_slice(&v.to_ne_bytes());
        }
    }

    fn flags(&self, pg: usize) -> u16 {
        self.get16(pg * self.ps + self.f_flags()).unwrap_or(0)
    }

    /// Pages 2.. classified by their header (overflow runs skipped).
    fn pages_with(&self, want: impl Fn(u16) -> bool) -> Vec<usize> {
        let mut out = Vec::new();
        let mut pg = 2;
        while pg < self.npages() {
            let f = self.flags(pg);
            if want(f) {
                out.push(pg);
            }
            if f & P_OVERFLOW != 0 {
                let n = u32::from_ne_bytes(
                    self.buf[pg * self.ps + self.f_lower()..pg * self.ps + self.f_lower() + 4]
                        .try_into()
                        .unwrap(),
                ) as usize;
                pg += n.clamp(1, self.npages());
            } else {
                pg += 1;
            }
        }
        out
    }

    /// Absolute offsets of the nodes of branch/leaf page `pg` that lie in
    /// the page.
    fn nodes(&self, pg: usize) -> Vec<usize> {
        let base = pg * self.ps;
        let lower = self.get16(base + self.f_lower()).unwrap_or(0) as usize;
        let n = lower.saturating_sub(self.hdr - self.pbase) / 2;
        (0..n.min(self.ps / 2))
            .filter_map(|i| {
                let ptr = self.get16(base + self.hdr + 2 * i)? as usize + self.pbase;
                (ptr >= self.hdr && ptr + 8 <= self.ps).then_some(base + ptr)
            })
            .collect()
    }

    /// Leaf nodes (absolute offsets) whose flags satisfy `want`.
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
}

fn pick<T: Copy>(rng: &mut Rng, v: &[T]) -> Option<T> {
    (!v.is_empty()).then(|| v[rng.below(v.len())])
}

/// Interesting page numbers relative to the image: meta pages, huge,
/// `P_INVALID`, the last page and just past it, a random page.
fn bogus_pgno(rng: &mut Rng, img: &Img, this: usize) -> u64 {
    let n = img.npages() as u64;
    match rng.below(9) {
        0 => 0,
        1 => 1,
        2 => u64::MAX,
        3 => 1 << 47,
        4 => n.saturating_sub(1),
        5 => n,
        6 => n + 1 + rng.below(1000) as u64,
        7 => this as u64,
        _ => 2 + rng.below(img.npages().max(3) - 2) as u64,
    }
}

fn bogus_u16(rng: &mut Rng, ps: usize) -> u16 {
    let ps = ps as u32;
    let v: [u32; 9] = [0, 1, 2, 15, 16, 17, ps - 1, ps, 0xffff];
    if rng.below(4) == 0 {
        rng.next() as u16
    } else {
        v[rng.below(v.len())].min(0xffff) as u16
    }
}

/// Applies one random mutation to `img`; returns its description.
fn mutate(rng: &mut Rng, img: &mut Img) -> String {
    let ps = img.ps;
    let np = img.npages().max(3);
    let any_page = |rng: &mut Rng| 2 + rng.below(np - 2);
    match rng.below(21) {
        0 => {
            let off = rng.below(img.buf.len().max(1));
            let bit = rng.below(8);
            if let Some(b) = img.buf.get_mut(off) {
                *b ^= 1 << bit;
            }
            format!("bitflip off={off} bit={bit}")
        }
        1 => {
            let pg = rng.below(np);
            let off = pg * ps + rng.below(64);
            let bit = rng.below(8);
            if let Some(b) = img.buf.get_mut(off) {
                *b ^= 1 << bit;
            }
            format!("header bitflip pg={pg} off={off} bit={bit}")
        }
        2 | 3 => {
            let pg = any_page(rng);
            let start = rng.below(ps);
            let len = 1 + rng.below(ps - start);
            let zero = rng.below(2) == 0;
            let base = pg * ps + start;
            for i in 0..len {
                let v = if zero { 0 } else { rng.next() as u8 };
                if let Some(b) = img.buf.get_mut(base + i) {
                    *b = v;
                }
            }
            format!(
                "{} pg={pg} start={start} len={len}",
                if zero { "zero" } else { "random" }
            )
        }
        4 => {
            let (a, b) = (any_page(rng), any_page(rng));
            if a != b && (a.max(b) + 1) * ps <= img.buf.len() {
                let (lo, hi) = (a.min(b), a.max(b));
                let (x, y) = img.buf.split_at_mut(hi * ps);
                x[lo * ps..(lo + 1) * ps].swap_with_slice(&mut y[..ps]);
            }
            format!("swap pages {a} {b}")
        }
        5 => {
            let (a, b) = (rng.below(np), any_page(rng));
            if (a.max(b) + 1) * ps <= img.buf.len() {
                let src = img.buf[a * ps..(a + 1) * ps].to_vec();
                img.buf[b * ps..(b + 1) * ps].copy_from_slice(&src);
            }
            format!("copy page {a} over {b}")
        }
        6 => {
            let len = match rng.below(4) {
                0 => rng.below(2 * ps + 1),
                1 => (2 + rng.below(np)) * ps,
                _ => rng.below(img.buf.len() + 1),
            };
            img.buf.truncate(len);
            format!("truncate to {len}")
        }
        7 => {
            let extra = (1 + rng.below(4)) * ps;
            let tail: Vec<u8> = (0..extra).map(|_| rng.next() as u8).collect();
            img.buf.extend_from_slice(&tail);
            format!("extend by {extra} random bytes")
        }
        8 => {
            let pages = img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0);
            let Some(pg) = pick(rng, &pages) else {
                return "lower/upper: no page".into();
            };
            let lower = rng.below(2) == 0;
            let field = if lower { img.f_lower() } else { img.f_upper() };
            let v = bogus_u16(rng, ps);
            img.put16(pg * ps + field, v);
            format!("pg={pg} {} = {v}", if lower { "lower" } else { "upper" })
        }
        9 => {
            let pages = img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0 && f & P_LEAF2 == 0);
            let Some(pg) = pick(rng, &pages) else {
                return "ptr: no page".into();
            };
            let n = img.nodes(pg).len().max(1);
            let i = rng.below(n);
            let v = match rng.below(5) {
                0 => 0,
                1 => 8,
                2 => (ps - 1) as u16,
                3 => (ps - 4) as u16,
                _ => bogus_u16(rng, ps),
            };
            img.put16(pg * ps + img.hdr + 2 * i, v);
            format!("pg={pg} ptr[{i}] = {v}")
        }
        10 => {
            let pages = img.pages_with(|f| f & P_BRANCH != 0);
            let Some(pg) = pick(rng, &pages) else {
                return "child: no branch page".into();
            };
            let nodes = img.nodes(pg);
            let Some(n) = pick(rng, &nodes) else {
                return "child: no node".into();
            };
            // A cycle: point at an ancestor (or this page) as well.
            let target = if rng.below(3) == 0 {
                pick(rng, &pages).unwrap_or(pg) as u64
            } else {
                bogus_pgno(rng, img, pg)
            };
            img.put16(n, target as u16);
            img.put16(n + 2, (target >> 16) as u16);
            img.put16(n + 4, (target >> 32) as u16);
            format!("pg={pg} child at {n} -> {target}")
        }
        11 => {
            let nodes = img.leaf_nodes_with(|f| f & F_BIGDATA != 0);
            let Some(n) = pick(rng, &nodes) else {
                return "ovpgno: no node".into();
            };
            let target = bogus_pgno(rng, img, n / ps);
            img.put64(img.node_data(n), target);
            format!("bigdata node at {n} -> pgno {target}")
        }
        12 => {
            let pages = img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0 && f & P_LEAF2 == 0);
            let Some(pg) = pick(rng, &pages) else {
                return "node size: no page".into();
            };
            let nodes = img.nodes(pg);
            let Some(n) = pick(rng, &nodes) else {
                return "node size: no node".into();
            };
            let v = bogus_u16(rng, ps);
            let field = [0, 2, 6][rng.below(3)];
            img.put16(n + field, v);
            format!("pg={pg} node at {n} field+{field} = {v}")
        }
        13 => {
            let pages = img.pages_with(|f| f & P_LEAF2 != 0);
            let Some(pg) = pick(rng, &pages) else {
                return "leaf2: no page".into();
            };
            let v = [0u16, 1, 3, ps as u16, (ps / 2) as u16, 0xffff][rng.below(6)];
            img.put16(pg * ps + img.f_pad(), v);
            format!("leaf2 pg={pg} ksize = {v}")
        }
        14 => {
            let pages = img.pages_with(|f| f & P_OVERFLOW != 0);
            let Some(pg) = pick(rng, &pages) else {
                return "ovpages: no page".into();
            };
            let v = match rng.below(5) {
                0 => 0,
                1 => 1,
                2 => u32::MAX,
                3 => (np - pg + 5) as u32,
                _ => rng.next() as u32,
            };
            img.put32(pg * ps + img.f_lower(), v);
            format!("overflow pg={pg} pages = {v}")
        }
        15 => {
            // A DB record: in a meta page, or the data of an F_SUBDATA node.
            let recs: Vec<usize> = {
                let h = img.hdr;
                let mut v = vec![h + 24, h + 72, ps + h + 24, ps + h + 72];
                for n in img.leaf_nodes_with(|f| f & F_SUBDATA != 0) {
                    v.push(img.node_data(n));
                }
                v
            };
            let r = pick(rng, &recs).unwrap();
            let (what, field, value): (&str, usize, u64) = match rng.below(5) {
                0 => ("depth", 6, [0u64, 1, 33, 100, 0xffff][rng.below(5)]),
                1 => ("root", 40, bogus_pgno(rng, img, 0)),
                2 => ("flags", 4, rng.next() & 0x7e),
                3 => ("entries", 32, [0u64, 1, u64::MAX][rng.below(3)]),
                _ => ("pad", 0, rng.next() & 0xffff_ffff),
            };
            match field {
                4 | 6 => img.put16(r + field, value as u16),
                0 => img.put32(r, value as u32),
                _ => img.put64(r + field, value),
            }
            format!("db record at {r} {what} = {value:#x}")
        }
        16 => {
            let m = rng.below(2) * ps + img.hdr;
            let (what, off, v): (&str, usize, u64) = match rng.below(7) {
                0 => ("mapsize", 16, [0u64, 4096, 1 << 62, 3][rng.below(4)]),
                1 => (
                    "last_pg",
                    120,
                    [0u64, 1, 2, (np / 2) as u64, np as u64 + 50, u64::MAX >> 13][rng.below(6)],
                ),
                2 => ("txnid", 128, [0u64, 1, u64::MAX, 1 << 40][rng.below(4)]),
                3 => ("magic", 0, rng.next() & 0xffff_ffff),
                // Including the other format's version word.
                4 => ("version", 4, [0u64, 1, 2, 3, 999][rng.below(5)]),
                5 => (
                    "psize",
                    24,
                    [0u64, 512, 1024, 65536, ps as u64 / 2, ps as u64 * 2, 4097][rng.below(7)],
                ),
                _ => ("address", 8, rng.next()),
            };
            match off {
                0 | 4 | 24 => img.put32(m + off, v as u32),
                _ => img.put64(m + off, v),
            }
            format!("meta {} {what} = {v:#x}", m / ps)
        }
        17 => {
            let pg = any_page(rng);
            let v = [
                0u16,
                1,
                2,
                3,
                4,
                5,
                0x22,
                0x40,
                0x42,
                0x08,
                0x12,
                0x4002,
                rng.next() as u16,
            ][rng.below(13)];
            img.put16(pg * ps + img.f_flags(), v);
            format!("pg={pg} flags = {v:#x}")
        }
        18 => {
            // A sub-page: data of an F_DUPDATA node without F_SUBDATA.
            let nodes = img.leaf_nodes_with(|f| f & F_DUPDATA != 0 && f & F_SUBDATA == 0);
            let Some(n) = pick(rng, &nodes) else {
                return "subpage: no node".into();
            };
            let sp = img.node_data(n);
            // mp_pad, mp_flags, mp_lower, mp_upper, mp_ptrs[0..2].
            let field = img.hdr - 8 + 2 * rng.below(6);
            let v = bogus_u16(rng, ps);
            img.put16(sp + field, v);
            format!("subpage at {sp} field+{field} = {v}")
        }
        19 => {
            // Free DB data (IDLs): plausible values whose count is a lie,
            // or whose page numbers are bogus.
            let nodes = img.leaf_nodes_with(|f| f == 0);
            let idls: Vec<usize> = nodes
                .into_iter()
                .filter(|&n| {
                    let ks = img.get16(n + 6).unwrap_or(0) as usize;
                    let ds = img.get16(n).unwrap_or(0) as usize;
                    ks == 8 && ds >= 16 && ds % 8 == 0
                })
                .collect();
            let Some(n) = pick(rng, &idls) else {
                return "idl: no node".into();
            };
            let ds = img.get16(n).unwrap_or(0) as usize;
            let d = n + 16;
            if rng.below(2) == 0 {
                let v = [0u64, 1, u64::MAX, 1 << 40, (ds / 8) as u64][rng.below(5)];
                img.put64(d, v);
                format!("idl at {d} count = {v}")
            } else {
                let i = 1 + rng.below((ds / 8).max(2) - 1);
                let v = bogus_pgno(rng, img, 0);
                img.put64(d + 8 * i, v);
                format!("idl at {d} pgno[{i}] = {v}")
            }
        }
        _ => {
            // Several random bytes in one leaf or branch page.
            let pages = img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0);
            let Some(pg) = pick(rng, &pages) else {
                return "spray: no page".into();
            };
            let k = 1 + rng.below(8);
            for _ in 0..k {
                let off = pg * ps + rng.below(ps);
                img.buf[off] = rng.next() as u8;
            }
            format!("spray {k} bytes on pg={pg}")
        }
    }
}

/// Applies one structure-aware mutation to every page (or node) of its
/// kind, with values drawn per page: bogus counts in all overflow runs,
/// bogus children in all branch pages, and so on.
fn mutate_all(rng: &mut Rng, img: &mut Img) -> String {
    let ps = img.ps;
    match rng.below(6) {
        0 => {
            let pages = img.pages_with(|f| f & P_OVERFLOW != 0);
            for &pg in &pages {
                let v = [0u32, u32::MAX, 1 << 20, (img.npages() + 3 - pg) as u32][rng.below(4)];
                img.put32(pg * ps + img.f_lower(), v);
            }
            format!("all {} overflow runs: bogus page counts", pages.len())
        }
        1 => {
            let pages = img.pages_with(|f| f & P_BRANCH != 0);
            for &pg in &pages {
                for n in img.nodes(pg) {
                    if rng.below(4) == 0 {
                        let t = bogus_pgno(rng, img, pg);
                        img.put16(n, t as u16);
                        img.put16(n + 2, (t >> 16) as u16);
                        img.put16(n + 4, (t >> 32) as u16);
                    }
                }
            }
            format!("children of all {} branch pages", pages.len())
        }
        2 => {
            let nodes = img.leaf_nodes_with(|f| f & F_BIGDATA != 0);
            for &n in &nodes {
                let t = bogus_pgno(rng, img, n / ps);
                img.put64(img.node_data(n), t);
            }
            format!("all {} overflow pgnos", nodes.len())
        }
        3 => {
            let pages = img.pages_with(|f| f & P_LEAF2 != 0);
            for &pg in &pages {
                let v = [0u16, 1, 3, ps as u16, 0xffff][rng.below(5)];
                img.put16(pg * ps + img.f_pad(), v);
            }
            format!("key size of all {} LEAF2 pages", pages.len())
        }
        4 => {
            let pages = img.pages_with(|f| f & (P_BRANCH | P_LEAF) != 0);
            for &pg in &pages {
                if rng.below(3) == 0 {
                    let field = if rng.below(2) == 0 {
                        img.f_lower()
                    } else {
                        img.f_upper()
                    };
                    let v = bogus_u16(rng, ps);
                    img.put16(pg * ps + field, v);
                }
            }
            format!("lower/upper of a third of {} pages", pages.len())
        }
        _ => {
            let nodes = img.leaf_nodes_with(|f| f & F_DUPDATA != 0);
            for &n in &nodes {
                let f =
                    img.get16(n + 4).unwrap_or(0) ^ [F_SUBDATA, F_DUPDATA, F_BIGDATA][rng.below(3)];
                img.put16(n + 4, f);
            }
            format!("flags of all {} duplicate nodes", nodes.len())
        }
    }
}

// ---------------------------------------------------------------------------
// Exercising a damaged environment
// ---------------------------------------------------------------------------

/// How far the cases got (all bases together), to see that the mutations
/// do not just make every open fail.
struct Stats {
    cases: AtomicUsize,
    opened: AtomicUsize,
    entries_read: AtomicUsize,
    rw_begun: AtomicUsize,
    committed: AtomicUsize,
    copied: AtomicUsize,
}

static STATS: Stats = Stats {
    cases: AtomicUsize::new(0),
    opened: AtomicUsize::new(0),
    entries_read: AtomicUsize::new(0),
    rw_begun: AtomicUsize::new(0),
    committed: AtomicUsize::new(0),
    copied: AtomicUsize::new(0),
};

fn stat(counter: &AtomicUsize) {
    counter.fetch_add(1, AtomicOrdering::Relaxed);
}

/// Upper bound of cursor steps per walk: far more than any base holds, so a
/// walk that gets here is revisiting pages (a DAG made by the mutation),
/// which is finite but can be long.
const WALK_CAP: usize = 30_000;

/// Keys and values seen during the walks, to aim later operations at.
#[derive(Default)]
struct Samples {
    keys: Vec<Vec<u8>>,
    data: Vec<Vec<u8>>,
}

impl Samples {
    fn add(&mut self, rng: &mut Rng, k: &[u8], d: &[u8]) {
        if self.keys.len() < 48 {
            self.keys.push(k.to_vec());
            self.data
                .push(d.get(..d.len().min(600)).unwrap_or(d).to_vec());
        } else if rng.below(8) == 0 {
            let i = rng.below(self.keys.len());
            self.keys[i] = k.to_vec();
            self.data[i] = d.get(..d.len().min(600)).unwrap_or(d).to_vec();
        }
    }

    fn key(&self, rng: &mut Rng) -> Vec<u8> {
        let mut k = match pick_vec(rng, &self.keys) {
            Some(k) => k,
            None => b"x".to_vec(),
        };
        match rng.below(6) {
            0 => k.push(rng.next() as u8),
            1 if !k.is_empty() => {
                let i = rng.below(k.len());
                k[i] = k[i].wrapping_add(1);
            }
            2 => k = (0..1 + rng.below(12)).map(|_| rng.next() as u8).collect(),
            3 => k = rng.next().to_ne_bytes().to_vec(),
            _ => {}
        }
        k
    }

    fn data(&self, rng: &mut Rng) -> Vec<u8> {
        let mut d = pick_vec(rng, &self.data).unwrap_or_else(|| b"y".to_vec());
        if rng.below(3) == 0 {
            d.push(rng.next() as u8);
        }
        d
    }
}

fn pick_vec(rng: &mut Rng, v: &[Vec<u8>]) -> Option<Vec<u8>> {
    (!v.is_empty()).then(|| v[rng.below(v.len())].clone())
}

fn walk(c: &mut RoCursor<'_>, rng: &mut Rng, s: &mut Samples, first: CursorOp, next: CursorOp) {
    let mut op = first;
    for _ in 0..WALK_CAP {
        match c.get(None, None, op) {
            Ok(Some((k, d))) => s.add(rng, k, d),
            _ => break,
        }
        stat(&STATS.entries_read);
        op = next;
    }
}

const ALL_OPS: u32 = 19;

/// Random cursor operations aimed at sampled keys and data.
fn random_ops(c: &mut RoCursor<'_>, rng: &mut Rng, s: &mut Samples, n: usize) {
    for _ in 0..n {
        let op = CursorOp::from_raw(rng.below(ALL_OPS as usize) as u32).unwrap();
        let k = s.key(rng);
        let d = s.data(rng);
        let key = (rng.below(8) != 0).then_some(&k[..]);
        let data = (rng.below(8) != 0).then_some(&d[..]);
        if let Ok(Some((k, d))) = c.get(key, data, op) {
            let (k, d) = (k.to_vec(), d.to_vec());
            s.add(rng, &k, &d);
        }
        if rng.below(10) == 0 {
            let _ = c.count();
        }
    }
}

/// The databases of an open transaction: the main DB, the base's names,
/// and names found in the main DB.
fn databases(txn: &impl Transaction, base: &Base) -> Vec<Dbi> {
    let mut dbs = vec![Dbi::MAIN];
    for n in &base.names {
        if let Ok(db) = txn.open_db(Some(n)) {
            dbs.push(db);
        }
    }
    if let Ok(mut c) = txn.cursor(Dbi::MAIN) {
        let mut names = Vec::new();
        let mut op = CursorOp::First;
        for _ in 0..64 {
            match c.get(None, None, op) {
                Ok(Some((k, _))) => names.push(k.to_vec()),
                _ => break,
            }
            op = CursorOp::Next;
        }
        for n in names.iter().take(16) {
            if let Ok(db) = txn.open_db_bytes(Some(n)) {
                if !dbs.contains(&db) {
                    dbs.push(db);
                }
            }
        }
    }
    dbs
}

/// Every read operation on every database of `txn`.
fn read_txn(txn: &impl Transaction, base: &Base, rng: &mut Rng) {
    let _ = txn.id();
    for db in databases(txn, base) {
        let _ = txn.stat(db);
        let _ = txn.db_flags(db);
        let _ = txn.comparators(db);
        let mut s = Samples::default();
        let Ok(mut c) = txn.cursor(db) else {
            continue;
        };
        walk(&mut c, rng, &mut s, CursorOp::First, CursorOp::Next);
        walk(&mut c, rng, &mut s, CursorOp::Last, CursorOp::Prev);
        walk(&mut c, rng, &mut s, CursorOp::First, CursorOp::NextNoDup);
        walk(&mut c, rng, &mut s, CursorOp::Last, CursorOp::PrevNoDup);
        walk(&mut c, rng, &mut s, CursorOp::First, CursorOp::NextDup);
        walk(&mut c, rng, &mut s, CursorOp::LastDup, CursorOp::PrevDup);
        walk(&mut c, rng, &mut s, CursorOp::First, CursorOp::NextMultiple);
        walk(&mut c, rng, &mut s, CursorOp::Last, CursorOp::PrevMultiple);
        random_ops(&mut c, rng, &mut s, 150);
        // Gets on every database (the free DB included) must not panic
        // either.
        for _ in 0..20 {
            let k = s.key(rng);
            let _ = txn.get(db, &k);
        }
    }
    if let Ok(mut c) = txn.cursor(Dbi::FREE) {
        let mut s = Samples::default();
        walk(&mut c, rng, &mut s, CursorOp::First, CursorOp::Next);
        walk(&mut c, rng, &mut s, CursorOp::Last, CursorOp::Prev);
    }
}

fn read_env(env: &Environment, base: &Base, rng: &mut Rng) {
    let _ = env.stat();
    let _ = env.info();
    let _ = env.freelist();
    let _ = env.reader_list();
    let _ = env.reader_check();
    if let Ok(txn) = env.begin_ro_txn() {
        read_txn(&txn, base, rng);
        // Reset and renew on the same slot.
        if let Ok(txn) = txn.reset().renew() {
            let _ = txn.stat(Dbi::MAIN);
        }
    }
}

fn wflags(rng: &mut Rng) -> WriteFlags {
    [
        WriteFlags::empty(),
        WriteFlags::empty(),
        WriteFlags::NO_OVERWRITE,
        WriteFlags::NO_DUP_DATA,
        WriteFlags::APPEND,
        WriteFlags::APPEND_DUP,
        WriteFlags::CURRENT,
    ][rng.below(7)]
}

fn value(rng: &mut Rng, s: &Samples, ps: usize) -> Vec<u8> {
    match rng.below(6) {
        0 => vec![rng.next() as u8; ps * (1 + rng.below(3)) + rng.below(100)],
        1 => rng.next().to_ne_bytes().to_vec(),
        2 => s.data(rng),
        _ => (0..1 + rng.below(60)).map(|_| rng.next() as u8).collect(),
    }
}

/// A batch of random writes through `txn` (a top-level or nested
/// transaction).
fn write_ops(txn: &mut rlmdb::RwTxn<'_>, dbs: &[Dbi], s: &Samples, rng: &mut Rng, n: usize) {
    let ps = txn.env().page_size();
    for _ in 0..n {
        let Some(db) = pick(rng, dbs) else {
            return;
        };
        match rng.below(10) {
            0..=3 => {
                let (k, v) = (s.key(rng), value(rng, s, ps));
                let _ = txn.put(db, &k, &v, wflags(rng));
            }
            4 => {
                let k = s.key(rng);
                let len = rng.below(2 * ps);
                if let Ok(buf) = txn.reserve(db, &k, len, WriteFlags::empty()) {
                    buf.fill(0x5a);
                }
            }
            5 | 6 => {
                let k = s.key(rng);
                let d = s.data(rng);
                let data = (rng.below(2) == 0).then_some(&d[..]);
                let _ = txn.del(db, &k, data);
            }
            7 | 8 => {
                let Ok(mut c) = txn.cursor_mut(db) else {
                    continue;
                };
                for _ in 0..1 + rng.below(12) {
                    let op = CursorOp::from_raw(rng.below(ALL_OPS as usize) as u32).unwrap();
                    let k = s.key(rng);
                    let d = s.data(rng);
                    let _ = c.get(Some(&k), Some(&d), op);
                    match rng.below(5) {
                        0 => {
                            let f = [WriteFlags::empty(), WriteFlags::NO_DUP_DATA][rng.below(2)];
                            let _ = c.del(f);
                        }
                        1 => {
                            let v = value(rng, s, ps);
                            let _ = c.put(&k, &v, wflags(rng));
                        }
                        2 => {
                            let items = vec![rng.next() as u8; 16 * (1 + rng.below(40))];
                            let _ = c.put_multiple(&k, &items, 16, WriteFlags::empty());
                        }
                        3 => {
                            let _ = c.count();
                        }
                        _ => {}
                    }
                }
            }
            _ => match rng.below(3) {
                0 => {
                    let _ = txn.clear_db(db);
                }
                1 => {
                    let _ = txn.drop_db(db);
                }
                _ => {
                    let name = format!("new{}", rng.below(4));
                    let flags = [
                        DatabaseFlags::empty(),
                        DatabaseFlags::DUP_SORT,
                        DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
                    ][rng.below(3)];
                    let _ = txn.create_db(Some(&name), flags | DatabaseFlags::CREATE);
                }
            },
        }
    }
}

fn write_env(env: &Environment, base: &Base, rng: &mut Rng) {
    // Aim at keys spread over every database: sample a full walk of each,
    // plus some random lookups.
    let mut s = Samples::default();
    let mut all_keys: Vec<(Dbi, Vec<u8>)> = Vec::new();
    if let Ok(txn) = env.begin_ro_txn() {
        for db in databases(&txn, base) {
            if let Ok(mut c) = txn.cursor(db) {
                let mut op = CursorOp::First;
                for _ in 0..WALK_CAP {
                    match c.get(None, None, op) {
                        Ok(Some((k, d))) => {
                            s.add(rng, k, d);
                            if all_keys.len() < 20_000 {
                                all_keys.push((db, k.to_vec()));
                            }
                        }
                        _ => break,
                    }
                    op = CursorOp::NextNoDup;
                }
                random_ops(&mut c, rng, &mut s, 30);
            }
        }
    }
    let Ok(mut txn) = env.begin_rw_txn() else {
        return;
    };
    stat(&STATS.rw_begun);
    let dbs = databases(&txn, base);
    if rng.below(3) == 0 {
        // A sweep: delete every n-th key and overwrite others with values
        // of another size (rebalancing, merges, overflow runs freed and
        // reallocated).
        let n = 2 + rng.below(4);
        let ps = env.page_size();
        for (i, (db, k)) in all_keys.iter().enumerate() {
            let _ = match i % n {
                0 => txn.del(*db, k, None).map(|_| ()),
                1 => txn.put(*db, k, &value(rng, &s, ps), WriteFlags::empty()),
                _ => Ok(()),
            };
        }
    }
    write_ops(&mut txn, &dbs, &s, rng, 25);
    if rng.below(2) == 0 {
        if let Ok(mut child) = txn.begin_nested_txn() {
            write_ops(&mut child, &dbs, &s, rng, 15);
            if rng.below(3) == 0 {
                if let Ok(mut grandchild) = child.begin_nested_txn() {
                    write_ops(&mut grandchild, &dbs, &s, rng, 8);
                    let _ = grandchild.commit();
                }
            }
            if rng.below(2) == 0 {
                let _ = child.commit();
            } else {
                child.abort();
            }
        }
        write_ops(&mut txn, &dbs, &s, rng, 10);
    }
    read_txn(&txn, base, rng);
    if rng.below(5) == 0 {
        txn.abort();
    } else if txn.commit().is_ok() {
        stat(&STATS.committed);
    }
}

fn copy_env(env: &Environment, dir: &Path, base: &Base, rng: &mut Rng) {
    for (i, compact) in [false, true].into_iter().enumerate() {
        let target = dir.join(format!("copy{i}"));
        std::fs::create_dir_all(&target).unwrap();
        if env.copy_to_path(&target, compact).is_err() {
            continue;
        }
        stat(&STATS.copied);
        let copy = Environment::builder()
            .max_dbs(16)
            .flags(EnvironmentFlags::NO_SYNC)
            .open(&target);
        if let Ok(copy) = copy {
            read_env(&copy, base, rng);
        }
    }
}

/// Everything a case does with the damaged environment in `dir`.
fn exercise(dir: &Path, base: &Base, seed: u64) {
    let mut rng = Rng(seed | 1);
    let env = match Environment::builder()
        .max_dbs(16)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
    {
        Ok(env) => env,
        Err(_) => return,
    };
    stat(&STATS.opened);
    read_env(&env, base, &mut rng);
    write_env(&env, base, &mut rng);
    read_env(&env, base, &mut rng);
    write_env(&env, base, &mut rng);
    copy_env(&env, dir, base, &mut rng);
}

// ---------------------------------------------------------------------------
// The driver: watchdog, panic capture, reporting
// ---------------------------------------------------------------------------

/// Panic messages by thread name, recorded by the panic hook.
fn panics() -> &'static Mutex<Vec<(String, String)>> {
    static P: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}

fn install_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let name = std::thread::current().name().unwrap_or("").to_owned();
            if name.starts_with("robust-") {
                let msg = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let loc = info.location().map(|l| l.to_string()).unwrap_or_default();
                let mut text = format!("{msg} at {loc}");
                if std::env::var_os("ROBUST_BACKTRACE").is_some() {
                    let bt = std::backtrace::Backtrace::force_capture();
                    text.push_str(&format!("\n{bt}"));
                }
                panics()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((name, text));
            } else {
                default(info);
            }
        }));
    });
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

enum Outcome {
    Ok,
    Panic(String),
    Hang,
}

/// Runs one case: mutate a copy of the base, exercise it in a watched
/// thread.
fn run_case(base: &'static Base, seed: u64, scratch: &Path) -> (Outcome, Vec<String>) {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut img = Img::new(base.data.clone());
    let mut what = Vec::new();
    if rng.below(8) == 0 {
        // One structural mutation applied to every page it can hit.
        what.push(mutate_all(&mut rng, &mut img));
    } else {
        for _ in 0..1 + rng.below(3) {
            what.push(mutate(&mut rng, &mut img));
        }
    }
    stat(&STATS.cases);
    let dir = scratch.join(format!("{}-{seed}", base.name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("data.mdb"), &img.buf).unwrap();

    let name = format!("robust-{}-{seed}", base.name);
    let (tx, rx) = mpsc::channel();
    let d = dir.clone();
    let handle = std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            exercise(&d, base, seed);
            let _ = tx.send(());
        })
        .unwrap();
    let timeout = Duration::from_secs(env_u64("ROBUST_TIMEOUT", 60));
    let outcome = match rx.recv_timeout(timeout) {
        Ok(()) => {
            let _ = handle.join();
            Outcome::Ok
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // The thread ended without sending: it panicked.
            let _ = handle.join();
            let msg = panics()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, m)| m.clone())
                .unwrap_or_else(|| "panic (no message)".into());
            Outcome::Panic(msg)
        }
        // The thread keeps running; it is leaked.
        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::Hang,
    };
    if !matches!(outcome, Outcome::Ok) {
        let keep = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("robust");
        let _ = std::fs::create_dir_all(&keep);
        let _ = std::fs::write(keep.join(format!("{}-{seed}.mdb", base.name)), &img.buf);
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    (outcome, what)
}

fn run_base(name: &'static str, first_seed: u64) {
    install_hook();
    let base = base(name);
    let scratch = tempfile::tempdir().unwrap();
    let only = std::env::var("ROBUST_SEED").ok();
    let seeds: Vec<u64> = match only.as_deref().and_then(|s| s.split_once(':')) {
        Some((b, s)) if b == name => vec![s.parse().expect("ROBUST_SEED=<base>:<seed>")],
        Some(_) => return,
        None => {
            let n = env_u64("ROBUST_CASES", 60);
            let offset = env_u64("ROBUST_SEED_OFFSET", 0);
            (0..n).map(|i| first_seed + offset + i).collect()
        }
    };
    let started = Instant::now();
    let mut failures = Vec::new();
    for &seed in &seeds {
        let (outcome, what) = run_case(base, seed, scratch.path());
        match outcome {
            Outcome::Ok => {}
            Outcome::Panic(msg) => failures.push(format!(
                "PANIC ROBUST_SEED={name}:{seed}: {msg}\n    mutations: {what:?}"
            )),
            Outcome::Hang => {
                failures.push(format!(
                    "HANG ROBUST_SEED={name}:{seed}\n    mutations: {what:?}"
                ));
                // The hung thread holds its environment; stop here.
                break;
            }
        }
    }
    eprintln!(
        "robust_mutation[{name}]: {} cases in {:.1?}, {} failures",
        seeds.len(),
        started.elapsed(),
        failures.len()
    );
    let get = |c: &AtomicUsize| c.load(AtomicOrdering::Relaxed);
    eprintln!(
        "robust_mutation (all bases so far): cases={} opened={} entries_read={} \
         write_txns={} committed={} copies={}",
        get(&STATS.cases),
        get(&STATS.opened),
        get(&STATS.entries_read),
        get(&STATS.rw_begun),
        get(&STATS.committed),
        get(&STATS.copied)
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn robust_mutation_main_db() {
    run_base("main", 1_000_000);
}

#[test]
fn robust_mutation_named_dbs() {
    run_base("named", 2_000_000);
}

#[test]
fn robust_mutation_overflow_values() {
    run_base("big", 3_000_000);
}

/// The same mutations on LMDB 1.0 files (24-byte page headers, header-relative
/// pointers, even-padded nodes, 24-byte overflow references), 1 KiB pages.
#[test]
fn robust_mutation_main_db_v10() {
    run_base("main10", 4_000_000);
}

#[test]
fn robust_mutation_named_dbs_v10() {
    run_base("named10", 5_000_000);
}

#[test]
fn robust_mutation_overflow_values_v10() {
    run_base("big10", 6_000_000);
}

/// LMDB 1.0 files with 64 KiB pages.
#[test]
fn robust_mutation_named_dbs_v10_64k() {
    run_base("named10_64k", 7_000_000);
}

#[test]
fn robust_mutation_overflow_values_v10_64k() {
    run_base("big10_64k", 8_000_000);
}

/// The data file is valid but the meta pages lie about the map size: the
/// map is raised to cover the pages in use, and nothing reads past the
/// file.
#[test]
fn robust_short_file_after_reopen() {
    let base = base("main");
    let dir = tempfile::tempdir().unwrap();
    let ps = u32::from_ne_bytes(base.data[40..44].try_into().unwrap()) as usize;
    // Keep the meta pages and a third of the rest: every snapshot page
    // past the end must be PAGE_NOTFOUND, never SIGBUS.
    let keep = (2 + (base.data.len() / ps - 2) / 3) * ps;
    std::fs::write(dir.path().join("data.mdb"), &base.data[..keep]).unwrap();
    let env = Environment::builder()
        .max_dbs(16)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut n = 0;
    let err = loop {
        match c.get(
            None,
            None,
            if n == 0 {
                CursorOp::First
            } else {
                CursorOp::Next
            },
        ) {
            Ok(Some(_)) => n += 1,
            Ok(None) => panic!("walked {n} entries of a truncated file without an error"),
            Err(e) => break e,
        }
    };
    assert!(
        matches!(
            err,
            rlmdb::Error::Mdb(rlmdb::MdbError::PageNotFound | rlmdb::MdbError::Corrupted)
        ),
        "{err:?}"
    );
    let _ = MDB_MAXKEYSIZE;
}

/// Applies `f` to every page of the base image that `want` selects and
/// returns the environment directory of the result.
fn damaged_copy(
    base: &Base,
    want: impl Fn(u16) -> bool,
    f: impl Fn(&mut Img, usize),
) -> tempfile::TempDir {
    let mut img = Img::new(base.data.clone());
    for pg in img.pages_with(want) {
        f(&mut img, pg);
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.mdb"), &img.buf).unwrap();
    dir
}

fn open_damaged(dir: &Path) -> Environment {
    Environment::builder()
        .max_dbs(16)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
        .unwrap()
}

/// Overflow runs whose `mp_pages` claim 4 billion pages (or run past the
/// end of the file) must not make a delete, an overwrite or `mdb_drop`
/// allocate a free list of that size: the run is checked against the
/// snapshot first (C would try and fail with `ENOMEM`).
#[test]
fn robust_huge_overflow_counts() {
    for (name, count) in [
        ("big", u32::MAX),
        ("big", 1 << 20),
        ("big10", u32::MAX),
        ("big10", 1 << 20),
    ] {
        let base = base(name);
        let dir = damaged_copy(
            base,
            |f| f & P_OVERFLOW != 0,
            |img, pg| {
                let off = pg * img.ps + img.f_lower();
                img.put32(off, count);
            },
        );
        let env = open_damaged(dir.path());
        // Delete and overwrite every big value, then empty the DB.
        let keys: Vec<Vec<u8>> = {
            let txn = env.begin_ro_txn().unwrap();
            let mut c = txn.cursor(Dbi::MAIN).unwrap();
            let mut keys = Vec::new();
            let mut op = CursorOp::First;
            while let Ok(Some((k, d))) = c.get(None, None, op) {
                if d.len() > env.page_size() {
                    keys.push(k.to_vec());
                }
                op = CursorOp::Next;
            }
            keys
        };
        assert!(!keys.is_empty());
        let mut txn = env.begin_rw_txn().unwrap();
        for (i, k) in keys.iter().enumerate() {
            let _ = if i % 2 == 0 {
                txn.del(Dbi::MAIN, k, None).map(|_| ())
            } else {
                txn.put(Dbi::MAIN, k, b"small", WriteFlags::empty())
            };
        }
        let _ = txn.commit();
        let mut txn = env.begin_rw_txn().unwrap();
        let _ = txn.clear_db(Dbi::MAIN);
        let _ = txn.commit();
        let mut txn = env.begin_rw_txn().unwrap();
        let _ = txn.drop_db(Dbi::MAIN);
        txn.abort();
    }
}

/// LMDB 1.0's overflow references carry the run length (`op_pages`), which
/// `mdb_drop` trusts instead of the page: absurd lengths in every reference
/// must fail cleanly (checked against the snapshot), not allocate a free
/// list of that size or free pages that are not the run's.
#[test]
fn robust_huge_overflow_refs_v10() {
    let base = base("big10");
    for (field, value) in [(16, u64::MAX), (16, 1 << 40), (16, 0), (8, u64::MAX)] {
        let mut img = Img::new(base.data.clone());
        for n in img.leaf_nodes_with(|f| f & F_BIGDATA != 0) {
            // MDB_ovpage { op_pgno, op_txnid at 8, op_pages at 16 }.
            let d = img.node_data(n);
            img.put64(d + field, value);
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.mdb"), &img.buf).unwrap();
        let env = open_damaged(dir.path());
        assert_eq!(env.format(), rlmdb::Format::V10);
        let mut txn = env.begin_rw_txn().unwrap();
        let _ = txn.clear_db(Dbi::MAIN);
        let _ = txn.commit();
        let mut txn = env.begin_rw_txn().unwrap();
        let keys: Vec<Vec<u8>> = (0..240u32)
            .map(|i| format!("k{i:06}").into_bytes())
            .collect();
        for k in &keys {
            let _ = txn.put(Dbi::MAIN, k, &[9u8; 9000], WriteFlags::empty());
        }
        let _ = txn.commit();
    }
}
