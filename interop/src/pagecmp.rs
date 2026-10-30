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
//!   `mp_upper`, or `mp_pages` of an overflow page; in LMDB 1.0 also
//!   `mp_txnid`);
//! * branch and leaf pages: the node pointers and every node's header, key
//!   and data (a sub-page in canonical form, recursively, its header
//!   included), nothing else; the data of an `F_BIGDATA` node is its
//!   overflow reference: the page number in 0.9, the whole `MDB_ovpage`
//!   (`op_pgno`, `op_txnid`, `op_pages`) in 1.0;
//! * `P_LEAF2` pages: the packed keys;
//! * overflow pages: the value's bytes;
//! * both meta pages: the `MDB_meta` fields.
//!
//! Both data-file formats are understood: LMDB 0.9 (`MDB_DATA_VERSION` 1)
//! and LMDB 1.0 (`MDB_DATA_VERSION` 3, see `DUAL_FORMAT.md`), told apart by
//! where the meta page holds the magic and which version it says
//! ([`data_format`]). Files of different formats are never equal: comparing
//! them is an error. Offsets are those of `DESIGN.md` ("On-disk format")
//! and `DUAL_FORMAT.md` §1; the parsing is independent of `rlmdb` on
//! purpose.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use crate::{Error, Result};

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
/// `mm_magic`.
const MDB_MAGIC: u32 = 0xBEEF_C0DE;
/// `mm_version` within `MDB_meta` (`mm_magic` is at 0).
const META_VERSION: usize = 4;
/// `mm_dbs[0]` (free DB) and `mm_dbs[1]` (main DB) within `MDB_meta`.
const META_DBS: [usize; 2] = [24, 72];
/// `mm_txnid` within `MDB_meta`.
const META_TXNID: usize = 128;

/// What differs between the two data-file formats, as far as this walk is
/// concerned (`DUAL_FORMAT.md` §1). The node header, `MDB_db` and
/// `MDB_meta` are the same in both, and so are the last 8 bytes of the page
/// header (`mp_pad`, `mp_flags`, `mp_lower`/`mp_upper` or `mp_pages`).
#[derive(Debug, PartialEq, Eq)]
struct Format {
    /// The release that writes it, for messages.
    name: &'static str,
    /// `mm_version`: `MDB_DATA_VERSION`.
    version: u32,
    /// `PAGEHDRSZ`; the `MDB_meta` of pages 0 and 1 follows it.
    hdr: usize,
    /// `PAGEBASE`: what `mp_lower`, `mp_upper` and `mp_ptrs[]` count from.
    base: usize,
    /// `NODEDATA` pads the key to an even length.
    even_keys: bool,
    /// The data of an `F_BIGDATA` node: `pgno_t`, or `MDB_ovpage`.
    ovpage: usize,
    /// The smallest page size this module accepts.
    min_psize: usize,
}

/// LMDB 0.9: 16-byte page header, offsets from the page start.
const V09: Format = Format {
    name: "LMDB 0.9",
    version: 1,
    hdr: 16,
    base: 0,
    even_keys: false,
    ovpage: 8,
    min_psize: 512,
};

/// LMDB 1.0: `mp_txnid` after `mp_pgno`, offsets from the end of the
/// header (ITS#7713), `MDB_ovpage {pgno, txnid, pages}`, pages from 256
/// bytes.
const V10: Format = Format {
    name: "LMDB 1.0",
    version: 3,
    hdr: 24,
    base: 24,
    even_keys: true,
    ovpage: 24,
    min_psize: 256,
};

impl Format {
    /// The format whose `MDB_meta` (magic, version) a meta page starting
    /// with `page` holds. The two cannot be confused: where 1.0 keeps the
    /// magic, 0.9 keeps `mm_address`; where 0.9 keeps it, 1.0 keeps
    /// `mp_pad` and `mp_flags`.
    fn of_meta_page(page: &[u8]) -> Option<&'static Format> {
        [&V09, &V10].into_iter().find(|f| {
            u32_at(page, f.hdr).ok() == Some(MDB_MAGIC)
                && u32_at(page, f.hdr + META_VERSION).ok() == Some(f.version)
        })
    }

    /// The format with `mm_version` `version`.
    fn of_version(version: u32) -> Option<&'static Format> {
        [&V09, &V10].into_iter().find(|f| f.version == version)
    }

    /// `mp_pad`: the key size of a `P_LEAF2` page.
    fn pad(&self, page: &[u8]) -> Result<u16> {
        u16_at(page, self.hdr - 8)
    }

    /// `mp_flags`.
    fn flags(&self, page: &[u8]) -> Result<u16> {
        u16_at(page, self.hdr - 6)
    }

    /// `mp_lower` as stored (`PAGEBASE`-relative).
    fn lower(&self, page: &[u8]) -> Result<usize> {
        Ok(usize::from(u16_at(page, self.hdr - 4)?))
    }

    /// The stored `mp_lower` of a page without nodes.
    fn empty_lower(&self) -> usize {
        self.hdr - self.base
    }

    /// The bytes a key of `ksize` bytes takes in a node.
    fn key_len(&self, ksize: usize) -> usize {
        if self.even_keys {
            (ksize + 1) & !1
        } else {
            ksize
        }
    }
}

fn u16_at(b: &[u8], off: usize) -> Result<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_ne_bytes(s.try_into().expect("2 bytes")))
        .ok_or_else(|| Error::new(format!("read past the end at {off}")))
}

fn u32_at(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_ne_bytes(s.try_into().expect("4 bytes")))
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

/// The data-file format of the environment data file `path`, as the
/// `mm_version` (`MDB_DATA_VERSION`) of its first meta page: 1 for LMDB 0.9,
/// 3 for LMDB 1.0. An error if the file starts with neither format's meta
/// page.
pub fn data_format(path: &Path) -> Result<u32> {
    let mut head = Vec::with_capacity(V10.hdr + META_SIZE);
    std::fs::File::open(path)?
        .take((V10.hdr + META_SIZE) as u64)
        .read_to_end(&mut head)?;
    Format::of_meta_page(&head)
        .map(|f| f.version)
        .ok_or_else(|| {
            Error::new(format!(
                "{}: not an LMDB 0.9 or 1.0 data file (no meta page with magic {MDB_MAGIC:#x} \
                 and data version 1 or 3)",
                path.display()
            ))
        })
}

/// A data file and the canonical form of its reachable pages.
struct Image {
    data: Vec<u8>,
    fmt: &'static Format,
    psize: usize,
    /// Canonical bytes of every reachable page (first page of an overflow
    /// run: the whole run's value), by page number.
    pages: BTreeMap<u64, Vec<u8>>,
}

impl Image {
    fn load(path: &Path) -> Result<Image> {
        let data = std::fs::read(path)?;
        let fmt = Format::of_meta_page(&data)
            .ok_or_else(|| Error::new("not an LMDB 0.9 or 1.0 data file"))?;
        // mm_dbs[0].md_pad is the page size.
        let psize = u32_at(&data, fmt.hdr + META_DBS[0])? as usize;
        if psize < fmt.min_psize {
            return Err(Error::new(format!("{}: page size {psize}", path.display())));
        }
        let mut img = Image {
            data,
            fmt,
            psize,
            pages: BTreeMap::new(),
        };
        let meta = img.newest_meta()?;
        for db in META_DBS {
            let root = u64_at(&img.data, meta * psize + fmt.hdr + db + DB_ROOT)?;
            img.walk(root)?;
        }
        Ok(img)
    }

    /// The meta page (0 or 1) with the larger txnid.
    fn newest_meta(&self) -> Result<usize> {
        let txnid = |m: usize| u64_at(&self.data, m * self.psize + self.fmt.hdr + META_TXNID);
        Ok(usize::from(txnid(1)? > txnid(0)?))
    }

    /// The `MDB_meta` of both meta pages.
    fn metas(&self) -> Result<Vec<u8>> {
        let hdr = self.fmt.hdr;
        let mut v = slice(&self.data, hdr, META_SIZE)?.to_vec();
        v.extend_from_slice(slice(&self.data, self.psize + hdr, META_SIZE)?);
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
        let fmt = self.fmt;
        let page = self.page(pgno)?.to_vec();
        let flags = fmt.flags(&page)?;
        let mut children = Vec::new();
        let mut big = Vec::new();
        let canon = canonical(fmt, &page, &mut children, &mut big)?;
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
            let mut v = slice(&self.data, start, fmt.hdr)?.to_vec();
            if fmt.flags(&v)? & P_OVERFLOW == 0 {
                return Err(Error::new(format!("page {ovpgno} is not an overflow page")));
            }
            v.extend_from_slice(slice(&self.data, start + fmt.hdr, len)?);
            self.pages.insert(ovpgno, v);
        }
        for child in children {
            self.walk(child)?;
        }
        Ok(())
    }
}

/// The canonical form of a branch, leaf or `P_LEAF2` page (or sub-page) of
/// format `fmt`; collects the pages it refers to: child pages and sub-DB
/// roots in `children`, overflow values as `(pgno, length)` in `big`.
fn canonical(
    fmt: &Format,
    page: &[u8],
    children: &mut Vec<u64>,
    big: &mut Vec<(u64, usize)>,
) -> Result<Vec<u8>> {
    let hdr = fmt.hdr;
    let flags = fmt.flags(page)?;
    let lower = fmt.lower(page)?;
    let mut out = slice(page, 0, hdr)?.to_vec();
    if lower < fmt.empty_lower() {
        return Err(Error::new(format!("mp_lower {lower} below the header")));
    }
    let nkeys = (lower - fmt.empty_lower()) / 2;
    if flags & P_LEAF2 != 0 {
        let ksize = usize::from(fmt.pad(page)?);
        out.extend_from_slice(slice(page, hdr, nkeys * ksize)?);
        return Ok(out);
    }
    out.extend_from_slice(slice(page, hdr, lower - fmt.empty_lower())?);
    for i in 0..nkeys {
        let ptr = usize::from(u16_at(page, hdr + 2 * i)?) + fmt.base;
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
        // 1.0 pads the key to an even length; the pad byte is not compared.
        let dstart = ptr + NODE_HDR + fmt.key_len(ksize);
        if nflags & F_BIGDATA != 0 {
            // pgno_t (0.9) or MDB_ovpage {pgno, txnid, pages} (1.0).
            let ovref = slice(page, dstart, fmt.ovpage)?;
            out.extend_from_slice(ovref);
            big.push((u64_at(ovref, 0)?, dsize));
            continue;
        }
        let data = slice(page, dstart, dsize)?;
        if nflags & F_SUBDATA != 0 {
            // A named DB's or a sub-DB's record: walk its tree too.
            children.push(u64_at(data, DB_ROOT)?);
            out.extend_from_slice(data);
        } else if nflags & F_DUPDATA != 0 {
            out.extend_from_slice(&canonical(fmt, data, children, big)?);
        } else {
            out.extend_from_slice(data);
        }
    }
    Ok(out)
}

/// Compare two data files structurally (see the module documentation).
/// `Ok(())` if they hold the same pages; else an error describing the
/// first differences. Files of different data formats (LMDB 0.9 and 1.0)
/// are an error of their own.
pub fn compare_data_files(a: &Path, b: &Path) -> Result<()> {
    let fa = data_format(a)?;
    let fb = data_format(b)?;
    if fa != fb {
        let name = |v: u32| Format::of_version(v).map_or("unknown", |f| f.name);
        return Err(Error::new(format!(
            "{} is an {} data file (data version {fa}), {} an {} one (data version {fb}): \
             files of different formats cannot be compared",
            a.display(),
            name(fa),
            b.display(),
            name(fb)
        )));
    }
    let ia = Image::load(a).map_err(|e| e.context(a.display()))?;
    let ib = Image::load(b).map_err(|e| e.context(b.display()))?;
    let fmt = ia.fmt;
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
                    fmt.flags(ca)?,
                    fmt.flags(cb)?
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The first bytes of a meta page of format `f` (magic, version).
    fn meta_page(f: &Format) -> Vec<u8> {
        let mut page = vec![0u8; 512];
        page[f.hdr - 6..f.hdr - 4].copy_from_slice(&0x08u16.to_ne_bytes()); // P_META
        page[f.hdr..f.hdr + 4].copy_from_slice(&MDB_MAGIC.to_ne_bytes());
        page[f.hdr + 4..f.hdr + 8].copy_from_slice(&f.version.to_ne_bytes());
        page
    }

    #[test]
    fn formats_are_told_apart() {
        assert_eq!(Format::of_meta_page(&meta_page(&V09)), Some(&V09));
        assert_eq!(Format::of_meta_page(&meta_page(&V10)), Some(&V10));
        // The magic where 1.0 has it, with 0.9's version: neither.
        let mut page = meta_page(&V10);
        page[V10.hdr + 4..V10.hdr + 8].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(Format::of_meta_page(&page), None);
        assert_eq!(Format::of_meta_page(&[0u8; 20]), None);
        assert_eq!(Format::of_version(3), Some(&V10));
        assert_eq!(Format::of_version(2), None);
    }

    /// A 1.0 leaf page with one node whose key has an odd length: the data
    /// starts after the pad byte, which is not part of the canonical form,
    /// and `mp_txnid` is.
    #[test]
    fn canonical_1_0_leaf() {
        let mut page = vec![0xeeu8; 256];
        page[..8].copy_from_slice(&7u64.to_ne_bytes()); // mp_pgno
        page[8..16].copy_from_slice(&5u64.to_ne_bytes()); // mp_txnid
        page[16..18].copy_from_slice(&0u16.to_ne_bytes()); // mp_pad
        page[18..20].copy_from_slice(&P_LEAF.to_ne_bytes());
        page[20..22].copy_from_slice(&2u16.to_ne_bytes()); // mp_lower: one pointer
        let node = 256 - 16; // 8-byte header, key "abc" + pad, 4 data bytes
        page[22..24].copy_from_slice(&((node - 24) as u16).to_ne_bytes()); // mp_upper
        page[24..26].copy_from_slice(&((node - 24) as u16).to_ne_bytes()); // mp_ptrs[0]
        page[node..node + 2].copy_from_slice(&4u16.to_ne_bytes()); // mn_lo: dsize
        page[node + 2..node + 4].copy_from_slice(&0u16.to_ne_bytes());
        page[node + 4..node + 6].copy_from_slice(&0u16.to_ne_bytes()); // mn_flags
        page[node + 6..node + 8].copy_from_slice(&3u16.to_ne_bytes()); // mn_ksize
        page[node + 8..node + 11].copy_from_slice(b"abc");
        page[node + 12..node + 16].copy_from_slice(b"wxyz");
        let canon = canonical(&V10, &page, &mut Vec::new(), &mut Vec::new()).unwrap();
        let mut want = page[..26].to_vec();
        want.extend_from_slice(&page[node..node + 11]);
        want.extend_from_slice(b"wxyz");
        assert_eq!(canon, want);
        // The pad byte does not count; the txnid does.
        page[node + 11] = 0x11;
        assert_eq!(
            canonical(&V10, &page, &mut Vec::new(), &mut Vec::new()).unwrap(),
            want
        );
        page[8] = 6;
        assert_ne!(
            canonical(&V10, &page, &mut Vec::new(), &mut Vec::new()).unwrap(),
            want
        );
    }
}
