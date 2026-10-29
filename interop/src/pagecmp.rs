//! Structural comparison of two data files: do two environments hold the
//! same B-trees on the same page numbers, byte for byte where it matters?
//!
//! Two implementations that make the same decisions (page allocation,
//! split points, free-list handling, sub-page growth, overflow placement)
//! produce the same pages for the same sequence of writes. Their files are
//! still not byte-identical: C does not initialise every byte it writes
//! (node padding, the gap between `mp_lower` and `mp_upper`, the free
//! space of sub-pages, the tail of an overflow run's last page are whatever
//! the reused page buffer held), and pages on the free list keep stale
//! contents. So this module compares a *canonical* form of every page
//! reachable from the newest meta page (the trees of the free DB, the main
//! DB, every named DB and every DUPSORT sub-DB, with their overflow pages):
//!
//! * the page header (`mp_pgno`, `mp_pad`, `mp_flags`, `mp_lower`,
//!   `mp_upper`, or `mp_pages` of an overflow page);
//! * branch and leaf pages: the node pointers and every node's header, key
//!   and data (a sub-page in canonical form, recursively), nothing else;
//! * `P_LEAF2` pages: the packed keys;
//! * overflow pages: the value's bytes;
//! * both meta pages: the `MDB_meta` fields.
//!
//! Offsets are those of `DESIGN.md` ("On-disk format"); the parsing is
//! independent of `lmdb-core` on purpose.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Error, Result};

const PAGE_HDR: usize = 16;
const NODE_HDR: usize = 8;
const META_SIZE: usize = 136;
const P_BRANCH: u16 = 0x01;
const P_LEAF: u16 = 0x02;
const P_OVERFLOW: u16 = 0x04;
const P_LEAF2: u16 = 0x20;
const F_BIGDATA: u16 = 0x01;
const F_SUBDATA: u16 = 0x02;
const F_DUPDATA: u16 = 0x04;
const P_INVALID: u64 = u64::MAX;
/// `MDB_db.md_root` within the 48-byte record.
const DB_ROOT: usize = 40;
/// `mm_dbs[0]` (free DB) and `mm_dbs[1]` (main DB) within `MDB_meta`.
const META_DBS: [usize; 2] = [24, 72];
/// `mm_txnid` within `MDB_meta`.
const META_TXNID: usize = 128;

fn u16_at(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_ne_bytes(s.try_into().expect("2 bytes")))
        .ok_or_else(|| Error::new(format!("read past the end at {off}")))
}

fn u64_at(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_ne_bytes(s.try_into().expect("8 bytes")))
        .ok_or_else(|| Error::new(format!("read past the end at {off}")))
}

fn slice(b: &[u8], start: usize, len: usize) -> Result<&[u8]> {
    b.get(start..start + len)
        .ok_or_else(|| Error::new(format!("{len} bytes at {start} are out of range")))
}

/// A data file and the canonical form of its reachable pages.
struct Image {
    data: Vec<u8>,
    psize: usize,
    /// Canonical bytes of every reachable page (first page of an overflow
    /// run: the whole run's value), by page number.
    pages: BTreeMap<u64, Vec<u8>>,
}

impl Image {
    fn load(path: &Path) -> Result<Image> {
        let data = std::fs::read(path)?;
        // mm_dbs[0].md_pad is the page size.
        let psize = u16_at(&data, PAGE_HDR + META_DBS[0])? as usize
            | (u16_at(&data, PAGE_HDR + META_DBS[0] + 2)? as usize) << 16;
        if psize < 512 {
            return Err(Error::new(format!("{}: page size {psize}", path.display())));
        }
        let mut img = Image {
            data,
            psize,
            pages: BTreeMap::new(),
        };
        let meta = img.newest_meta()?;
        for db in META_DBS {
            let root = u64_at(&img.data, meta * psize + PAGE_HDR + db + DB_ROOT)?;
            img.walk(root)?;
        }
        Ok(img)
    }

    /// The meta page (0 or 1) with the larger txnid.
    fn newest_meta(&self) -> Result<usize> {
        let txnid = |m: usize| u64_at(&self.data, m * self.psize + PAGE_HDR + META_TXNID);
        Ok(usize::from(txnid(1)? > txnid(0)?))
    }

    /// The `MDB_meta` of both meta pages.
    fn metas(&self) -> Result<Vec<u8>> {
        let mut v = slice(&self.data, PAGE_HDR, META_SIZE)?.to_vec();
        v.extend_from_slice(slice(&self.data, self.psize + PAGE_HDR, META_SIZE)?);
        Ok(v)
    }

    fn page(&self, pgno: u64) -> Result<&[u8]> {
        let off = usize::try_from(pgno)
            .ok()
            .and_then(|p| p.checked_mul(self.psize))
            .ok_or_else(|| Error::new(format!("page {pgno} out of range")))?;
        slice(&self.data, off, self.psize)
    }

    /// Canonicalise the tree under `pgno` (nothing for an empty tree).
    fn walk(&mut self, pgno: u64) -> Result<()> {
        if pgno == P_INVALID || self.pages.contains_key(&pgno) {
            return Ok(());
        }
        let page = self.page(pgno)?.to_vec();
        let flags = u16_at(&page, 10)?;
        let mut children = Vec::new();
        let mut big = Vec::new();
        let canon = canonical(&page, &mut children, &mut big)?;
        if flags & (P_BRANCH | P_LEAF) == 0 {
            return Err(Error::new(format!(
                "page {pgno} has flags {flags:#x}, not a tree page"
            )));
        }
        self.pages.insert(pgno, canon);
        for (ovpgno, len) in big {
            let start = usize::try_from(ovpgno)
                .ok()
                .and_then(|p| p.checked_mul(self.psize))
                .ok_or_else(|| Error::new(format!("overflow page {ovpgno} out of range")))?;
            let mut v = slice(&self.data, start, PAGE_HDR)?.to_vec();
            if u16_at(&v, 10)? & P_OVERFLOW == 0 {
                return Err(Error::new(format!("page {ovpgno} is not an overflow page")));
            }
            v.extend_from_slice(slice(&self.data, start + PAGE_HDR, len)?);
            self.pages.insert(ovpgno, v);
        }
        for child in children {
            self.walk(child)?;
        }
        Ok(())
    }
}

/// The canonical form of a branch, leaf or `P_LEAF2` page (or sub-page);
/// collects the pages it refers to: child pages and sub-DB roots in
/// `children`, overflow values as `(pgno, length)` in `big`.
fn canonical(page: &[u8], children: &mut Vec<u64>, big: &mut Vec<(u64, usize)>) -> Result<Vec<u8>> {
    let flags = u16_at(page, 10)?;
    let lower = usize::from(u16_at(page, 12)?);
    let mut out = slice(page, 0, PAGE_HDR)?.to_vec();
    if lower < PAGE_HDR {
        return Err(Error::new(format!("mp_lower {lower} below the header")));
    }
    let nkeys = (lower - PAGE_HDR) / 2;
    if flags & P_LEAF2 != 0 {
        let ksize = usize::from(u16_at(page, 8)?);
        out.extend_from_slice(slice(page, PAGE_HDR, nkeys * ksize)?);
        return Ok(out);
    }
    out.extend_from_slice(slice(page, PAGE_HDR, lower - PAGE_HDR)?);
    for i in 0..nkeys {
        let ptr = usize::from(u16_at(page, PAGE_HDR + 2 * i)?);
        let lo = u16_at(page, ptr)?;
        let hi = u16_at(page, ptr + 2)?;
        let nflags = u16_at(page, ptr + 4)?;
        let ksize = usize::from(u16_at(page, ptr + 6)?);
        out.extend_from_slice(slice(page, ptr, NODE_HDR + ksize)?);
        if flags & P_LEAF == 0 {
            // Branch node: the child pgno is lo | hi << 16 | flags << 32.
            children.push(u64::from(lo) | u64::from(hi) << 16 | u64::from(nflags) << 32);
            continue;
        }
        let dsize = usize::from(lo) | usize::from(hi) << 16;
        let dstart = ptr + NODE_HDR + ksize;
        if nflags & F_BIGDATA != 0 {
            let ovpgno = u64_at(page, dstart)?;
            out.extend_from_slice(&ovpgno.to_ne_bytes());
            big.push((ovpgno, dsize));
            continue;
        }
        let data = slice(page, dstart, dsize)?;
        if nflags & F_SUBDATA != 0 {
            // A named DB's or a sub-DB's record: walk its tree too.
            children.push(u64_at(data, DB_ROOT)?);
            out.extend_from_slice(data);
        } else if nflags & F_DUPDATA != 0 {
            out.extend_from_slice(&canonical(data, children, big)?);
        } else {
            out.extend_from_slice(data);
        }
    }
    Ok(out)
}

/// Compare two data files structurally (see the module documentation).
/// `Ok(())` if they hold the same pages; else an error describing the
/// first differences.
pub fn compare_data_files(a: &Path, b: &Path) -> Result<()> {
    let ia = Image::load(a).map_err(|e| e.context(a.display()))?;
    let ib = Image::load(b).map_err(|e| e.context(b.display()))?;
    let mut diffs = Vec::new();
    if ia.psize != ib.psize {
        diffs.push(format!("page sizes {} and {}", ia.psize, ib.psize));
    } else if ia.metas()? != ib.metas()? {
        diffs.push(format!(
            "meta pages differ: newest meta {} vs {}",
            ia.newest_meta()?,
            ib.newest_meta()?
        ));
    }
    let only = |x: &Image, y: &Image| -> Vec<u64> {
        x.pages
            .keys()
            .filter(|p| !y.pages.contains_key(p))
            .copied()
            .collect()
    };
    let (only_a, only_b) = (only(&ia, &ib), only(&ib, &ia));
    if !only_a.is_empty() || !only_b.is_empty() {
        diffs.push(format!(
            "reachable pages differ: only in the first {only_a:?}, only in the second {only_b:?}"
        ));
    }
    for (pgno, ca) in &ia.pages {
        if let Some(cb) = ib.pages.get(pgno) {
            if ca != cb {
                let at = ca.iter().zip(cb).position(|(x, y)| x != y);
                diffs.push(format!(
                    "page {pgno} differs (canonical byte {at:?}; flags {:#x} vs {:#x})",
                    u16_at(ca, 10)?,
                    u16_at(cb, 10)?
                ));
            }
        }
    }
    if diffs.is_empty() {
        return Ok(());
    }
    let n = diffs.len();
    diffs.truncate(12);
    Err(Error::new(format!(
        "{} and {} differ in {n} ways:\n  {}",
        a.display(),
        b.display(),
        diffs.join("\n  ")
    )))
}

/// The number of reachable pages of a data file (for messages).
pub fn reachable_pages(path: &Path) -> Result<usize> {
    Ok(Image::load(path)?.pages.len())
}
