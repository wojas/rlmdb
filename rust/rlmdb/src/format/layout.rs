//! Where things are in a page: the byte offsets of the 0.9 structures, and
//! the per-environment [`Layout`] descriptor of the two data-file formats.
//!
//! LMDB 0.9 (`MDB_DATA_VERSION 1`) and LMDB 1.0 (`MDB_DATA_VERSION 3`) share
//! every on-disk structure except a few page-level details (see
//! `DUAL_FORMAT.md` §1 in the `rust/` directory):
//!
//! | item | 0.9 | 1.0 |
//! |---|---|---|
//! | `PAGEHDRSZ` | 16 | 24 (`mp_txnid` after `mp_pgno`) |
//! | `PAGEBASE` | 0 | 24: `mp_lower`, `mp_upper`, `mp_ptrs[]` are header-relative |
//! | `NODEDATA` | `mn_data + ksize` | `mn_data + EVEN(ksize)` |
//! | `F_BIGDATA` data | 8-byte `pgno` | 24-byte `MDB_ovpage {pgno, txnid, pages}` |
//! | `P_SUBP` flags byte | `P_LEAF\|P_DIRTY\|P_SUBP` | `P_LEAF\|P_SUBP` |
//! | `MAX_PAGESIZE` | 32 KiB | 64 KiB |
//!
//! The engine reads all of these from a [`Layout`], one `static` per
//! [`Format`], that the environment picks at open and hands to every page
//! and node view. Only [`V09`] is used at run time so far.
//!
//! The `const`s below are the offsets of the 0.9 layout. The node header,
//! `MDB_db` and `MDB_meta` offsets hold for both formats; the page header
//! constants (`PAGE_HDR_SIZE`, `PAGE_PAD`, … `PAGE_PTRS`) and
//! [`META_OFFSET_IN_PAGE`] are 0.9 only: engine code gets those from the
//! [`Layout`] (`page_hdr`, [`Layout::flags_offset`], [`Layout::meta_offset`],
//! …). They stay for the 0.9-only tools and tests that decode pages
//! directly.

use super::page::P_DIRTY;
use super::{get_u64, put_u64};
use crate::consts::MDB_MINKEYS;

// --- MDB_page ---------------------------------------------------------

/// `PAGEHDRSZ` of LMDB 0.9: size of the page header
/// (`offsetof(MDB_page, mp_ptrs)`). Engine code uses [`Layout::page_hdr`].
pub const PAGE_HDR_SIZE: usize = 16;
/// Offset of `mp_pgno` (u64), in both formats.
pub const PAGE_PGNO: usize = 0;
/// Offset of `mp_pad` (u16): key size on `P_LEAF2` pages (0.9).
pub const PAGE_PAD: usize = 8;
/// Offset of `mp_flags` (u16) (0.9).
pub const PAGE_FLAGS: usize = 10;
/// Offset of `mp_lower` (u16), lower bound of free space (0.9).
pub const PAGE_LOWER: usize = 12;
/// Offset of `mp_upper` (u16), upper bound of free space (0.9).
pub const PAGE_UPPER: usize = 14;
/// Offset of `mp_pages` (u32): overflow page count, overlays lower/upper
/// (0.9).
pub const PAGE_OVERFLOW_PAGES: usize = 12;
/// Offset of `mp_ptrs[0]` (u16 node offsets) (0.9).
pub const PAGE_PTRS: usize = 16;

// --- MDB_node ---------------------------------------------------------

/// `NODESIZE`: size of the node header (`offsetof(MDB_node, mn_data)`), in
/// both formats. The key starts here.
pub const NODE_SIZE: usize = 8;
/// Offset of `mn_lo` (u16). `MDB_node` declares `mn_lo, mn_hi` on
/// little-endian hosts and `mn_hi, mn_lo` on big-endian hosts.
#[cfg(target_endian = "little")]
pub const NODE_LO: usize = 0;
/// Offset of `mn_hi` (u16).
#[cfg(target_endian = "little")]
pub const NODE_HI: usize = 2;
/// Offset of `mn_lo` (u16) on big-endian hosts.
#[cfg(target_endian = "big")]
pub const NODE_LO: usize = 2;
/// Offset of `mn_hi` (u16) on big-endian hosts.
#[cfg(target_endian = "big")]
pub const NODE_HI: usize = 0;
/// Offset of `mn_flags` (u16); on branch nodes the top 16 bits of the pgno.
pub const NODE_FLAGS: usize = 4;
/// Offset of `mn_ksize` (u16).
pub const NODE_KSIZE: usize = 6;
/// Offset of `mn_data` (key bytes, then data bytes), in both formats. Where
/// the data starts depends on the format: [`Layout::node_data_offset`].
pub const NODE_DATA: usize = 8;

// --- MDB_db -----------------------------------------------------------

/// `sizeof(MDB_db)`.
pub const DB_SIZE: usize = 48;
/// Offset of `md_pad` (u32): page size in the free DB record, LEAF2 key size otherwise.
pub const DB_PAD: usize = 0;
/// Offset of `md_flags` (u16).
pub const DB_FLAGS: usize = 4;
/// Offset of `md_depth` (u16).
pub const DB_DEPTH: usize = 6;
/// Offset of `md_branch_pages` (u64).
pub const DB_BRANCH_PAGES: usize = 8;
/// Offset of `md_leaf_pages` (u64).
pub const DB_LEAF_PAGES: usize = 16;
/// Offset of `md_overflow_pages` (u64).
pub const DB_OVERFLOW_PAGES: usize = 24;
/// Offset of `md_entries` (u64).
pub const DB_ENTRIES: usize = 32;
/// Offset of `md_root` (u64).
pub const DB_ROOT: usize = 40;

// --- MDB_meta ---------------------------------------------------------

/// `sizeof(MDB_meta)`.
pub const META_SIZE: usize = 136;
/// Offset of the `MDB_meta` inside a 0.9 meta page (`METADATA(p)`).
/// Engine code uses [`Layout::meta_offset`].
pub const META_OFFSET_IN_PAGE: usize = PAGE_HDR_SIZE;
/// Offset of `mm_magic` (u32).
pub const META_MAGIC: usize = 0;
/// Offset of `mm_version` (u32).
pub const META_VERSION: usize = 4;
/// Offset of `mm_address` (pointer, 8 bytes).
pub const META_ADDRESS: usize = 8;
/// Offset of `mm_mapsize` (u64). `mdb_env_write_meta` rewrites the meta
/// from this offset to the end.
pub const META_MAPSIZE: usize = 16;
/// Offset of `mm_dbs[FREE_DBI]`.
pub const META_FREE_DB: usize = 24;
/// Offset of `mm_dbs[MAIN_DBI]`.
pub const META_MAIN_DB: usize = 24 + DB_SIZE;
/// Offset of `mm_last_pg` (u64).
pub const META_LAST_PGNO: usize = 24 + 2 * DB_SIZE;
/// Offset of `mm_txnid` (u64).
pub const META_TXNID: usize = 24 + 2 * DB_SIZE + 8;

// --- MDB_txninfo / MDB_reader (lock file) ------------------------------

pub use super::lockfile::{
    LOCK_FORMAT, MNAME_LEN, READERS_OFFSET, READER_PID, READER_SIZE, READER_TID, READER_TXNID,
    TXNINFO_FORMAT, TXNINFO_HEADER_SIZE, TXNINFO_MAGIC, TXNINFO_NUMREADERS, TXNINFO_RMUTEX,
    TXNINFO_TXNID, TXNINFO_WMUTEX,
};

// --- the format descriptor ----------------------------------------------

/// A data-file format, named after the C release that writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// LMDB 0.9 (`MDB_DATA_VERSION 1`), what every 0.9.x release reads and
    /// writes.
    V09,
    /// LMDB 1.0 (`MDB_DATA_VERSION 3`).
    V10,
}

impl Format {
    /// The page layout of this format.
    #[inline(always)]
    pub const fn layout(self) -> &'static Layout {
        match self {
            Format::V09 => &V09,
            Format::V10 => &V10,
        }
    }
}

/// The overflow reference stored as the data of an `F_BIGDATA` node: just
/// the page number in 0.9, `MDB_ovpage` in 1.0 (mdb.c 1.0, line 1169).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverflowRef {
    /// `op_pgno`: the first page of the overflow run.
    pub pgno: u64,
    /// `op_txnid` (1.0 only, 0 when read from a 0.9 node): the id of the
    /// transaction that wrote the run.
    pub txnid: u64,
    /// `op_pages` (1.0 only, 0 when read from a 0.9 node): the length of the
    /// run in pages.
    pub pages: u64,
}

/// What differs between the data-file formats, as values the page and
/// node accessors read. One `static` per format ([`V09`], [`V10`]); pages
/// and nodes carry a `&'static Layout`, so the format costs one pointer
/// and a load per field instead of a type parameter on every structure.
///
/// The lock file is not described here yet: it is still always version 1.
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    /// The format this describes.
    pub format: Format,
    /// `MDB_DATA_VERSION`: `mm_version` of the meta pages.
    pub data_version: u32,
    /// `PAGEHDRSZ`: size of the page header. The fields `mp_pad`,
    /// `mp_flags`, `mp_lower`/`mp_upper` (or `mp_pages`) are its last 8
    /// bytes in both formats.
    pub page_hdr: usize,
    /// `PAGEBASE`: what `mp_ptrs[]`, `mp_lower` and `mp_upper` are relative
    /// to (0 = the page start; 1.0: the end of the header, ITS#7713).
    pub ptr_base: usize,
    /// Whether nodes store key and data padded to even lengths (1.0's
    /// `NODEDATA` and `LEAFSIZE` use `EVEN()`).
    pub even_nodes: bool,
    /// Size of the overflow reference of an `F_BIGDATA` node:
    /// `sizeof(pgno_t)` or `sizeof(MDB_ovpage)`.
    pub ovpage_size: usize,
    /// The flags byte C writes into a new `P_SUBP` sub-page.
    pub subpage_flags: u16,
    /// `MAX_PAGESIZE`: the largest page size of the format.
    pub max_pagesize: usize,
}

/// The LMDB 0.9 layout (`MDB_DATA_VERSION 1`, mdb.c of this tree).
pub static V09: Layout = Layout {
    format: Format::V09,
    data_version: 1,
    page_hdr: 16,
    ptr_base: 0,
    even_nodes: false,
    ovpage_size: 8,
    // P_LEAF | P_DIRTY | P_SUBP (mdb.c line 6920).
    subpage_flags: 0x52,
    max_pagesize: 0x8000,
};

/// The LMDB 1.0 layout (`MDB_DATA_VERSION 3`, `mdb.RE/1.0`). Described for
/// the format work that follows; nothing opens an environment with it yet.
pub static V10: Layout = Layout {
    format: Format::V10,
    data_version: 3,
    page_hdr: 24,
    ptr_base: 24,
    even_nodes: true,
    ovpage_size: 24,
    // P_LEAF | P_SUBP (mdb.c 1.0, line 8806): no P_DIRTY in 1.0.
    subpage_flags: 0x42,
    max_pagesize: 0x10000,
};

/// `EVEN(n)`: round up to an even number.
#[inline(always)]
pub const fn even(n: usize) -> usize {
    (n + 1) & !1
}

impl Layout {
    // --- page header ------------------------------------------------------

    /// Offset of `mp_txnid` (u64): `Some(8)` in 1.0, `None` in 0.9, whose
    /// header has no such field.
    #[inline(always)]
    pub const fn txnid_offset(&self) -> Option<usize> {
        match self.format {
            Format::V09 => None,
            Format::V10 => Some(8),
        }
    }

    /// Offset of `mp_pad` (u16, `MP_PAD`): key size on `P_LEAF2` pages.
    #[inline(always)]
    pub const fn pad_offset(&self) -> usize {
        self.page_hdr - 8
    }

    /// Offset of `mp_flags` (u16, `MP_FLAGS`).
    #[inline(always)]
    pub const fn flags_offset(&self) -> usize {
        self.page_hdr - 6
    }

    /// Offset of `mp_lower` (u16, `MP_LOWER`).
    #[inline(always)]
    pub const fn lower_offset(&self) -> usize {
        self.page_hdr - 4
    }

    /// Offset of `mp_upper` (u16, `MP_UPPER`).
    #[inline(always)]
    pub const fn upper_offset(&self) -> usize {
        self.page_hdr - 2
    }

    /// Offset of `mp_pages` (u32): length of an overflow run, overlaying
    /// `mp_lower`/`mp_upper`.
    #[inline(always)]
    pub const fn pages_offset(&self) -> usize {
        self.page_hdr - 4
    }

    /// Offset of `mp_ptrs[0]` (`MP_PTRS`), the first byte after the header.
    #[inline(always)]
    pub const fn ptrs_offset(&self) -> usize {
        self.page_hdr
    }

    /// `METADATA(p)` of a meta page: where the `MDB_meta` starts (C 1.0
    /// spells the page type `MDB_metabuf`).
    #[inline(always)]
    pub const fn meta_offset(&self) -> usize {
        self.page_hdr
    }

    /// The stored `mp_lower` of an empty page, `PAGEHDRSZ - PAGEBASE`
    /// (`mdb_page_new`); also what `NUMKEYS` subtracts.
    #[inline(always)]
    pub const fn empty_lower(&self) -> usize {
        self.page_hdr - self.ptr_base
    }

    /// `NUMKEYS(p)` from a stored `mp_lower`:
    /// `(MP_LOWER(p) - (PAGEHDRSZ - PAGEBASE)) >> 1`. A corrupt `mp_lower`
    /// below the header yields 0.
    #[inline(always)]
    pub const fn num_keys(&self, lower: u16) -> usize {
        (lower as usize).saturating_sub(self.empty_lower()) >> 1
    }

    /// Finalises the header of a dirty page as it is written to the data
    /// file (`mdb_page_flush`): 1.0 stamps `mp_txnid` with the id of the
    /// writing transaction (mdb.c 1.0, line 4198), which C 1.0 writers use
    /// to tell their own pages from shared ones; 0.9 has no such field and
    /// nothing to do. The flags are the caller's business in both formats.
    /// A buffer shorter than the header is left alone.
    #[inline(always)]
    pub fn stamp_clean_page(&self, page: &mut [u8], txnid: u64) {
        if let Some(off) = self.txnid_offset() {
            if let Some(dst) = page.get_mut(off..off + 8) {
                dst.copy_from_slice(&txnid.to_ne_bytes());
            }
        }
    }

    // --- nodes -------------------------------------------------------------

    /// The bytes a key of `ksize` bytes takes in a node: `ksize`, or
    /// `EVEN(ksize)` in 1.0.
    #[inline(always)]
    pub const fn node_key_len(&self, ksize: usize) -> usize {
        if self.even_nodes {
            even(ksize)
        } else {
            ksize
        }
    }

    /// The bytes inline data of `dsize` bytes takes in a node: `dsize`, or
    /// `EVEN(dsize)` in 1.0.
    #[inline(always)]
    pub const fn node_data_len(&self, dsize: usize) -> usize {
        if self.even_nodes {
            even(dsize)
        } else {
            dsize
        }
    }

    /// The bytes the data part of a leaf node takes: the overflow
    /// reference for an `F_BIGDATA` node, else [`Layout::node_data_len`].
    /// The sum with the header and key is what `mdb_node_del` and the split
    /// point scan of `mdb_page_split` add up.
    #[inline(always)]
    pub const fn stored_data_len(&self, dsize: usize, bigdata: bool) -> usize {
        if bigdata {
            self.ovpage_size
        } else {
            self.node_data_len(dsize)
        }
    }

    /// `NODEDATA(node) - node`: offset of the data part from the node
    /// start, `NODESIZE + ksize` (1.0: `NODESIZE + EVEN(ksize)`).
    #[inline(always)]
    pub const fn node_data_offset(&self, ksize: usize) -> usize {
        NODE_SIZE + self.node_key_len(ksize)
    }

    /// `LEAFSIZE(k, d)`: header, key and inline data of a leaf node, without
    /// its pointer: `NODESIZE + ksize + dsize` (1.0: both `EVEN`).
    #[inline(always)]
    pub const fn leaf_size(&self, ksize: usize, dsize: usize) -> usize {
        NODE_SIZE + self.node_key_len(ksize) + self.node_data_len(dsize)
    }

    /// `INDXSIZE(k)`: header and key of a branch node, without its pointer:
    /// `NODESIZE + ksize` in both formats (1.0 rounds the key when the node
    /// is added and in `mdb_branch_size`, not in this macro).
    #[inline(always)]
    pub const fn branch_size(&self, ksize: usize) -> usize {
        NODE_SIZE + ksize
    }

    /// Reads the overflow reference at the start of `data` (the data part
    /// of an `F_BIGDATA` node). `None` if `data` is too short.
    #[inline(always)]
    pub fn read_overflow_ref(&self, data: &[u8]) -> Option<OverflowRef> {
        let pgno = get_u64(data, 0)?;
        match self.format {
            Format::V09 => Some(OverflowRef {
                pgno,
                ..OverflowRef::default()
            }),
            // MDB_ovpage {op_pgno, op_txnid, op_pages}.
            Format::V10 => Some(OverflowRef {
                pgno,
                txnid: get_u64(data, 8)?,
                pages: get_u64(data, 16)?,
            }),
        }
    }

    /// Writes `r` at the start of `data` as the data part of an
    /// `F_BIGDATA` node ([`Layout::ovpage_size`] bytes; 0.9 stores only the
    /// page number). `None` if `data` is too short.
    #[inline(always)]
    pub fn write_overflow_ref(&self, data: &mut [u8], r: OverflowRef) -> Option<()> {
        let dst = data.get_mut(..self.ovpage_size)?;
        put_u64(dst, 0, r.pgno);
        if let Format::V10 = self.format {
            put_u64(dst, 8, r.txnid);
            put_u64(dst, 16, r.pages);
        }
        Some(())
    }

    // --- per-environment sizes ----------------------------------------------

    /// `me_pagespace`: the bytes after the header of a page of `psize`
    /// bytes, `psize - PAGEHDRSZ` (1.0 names it; 0.9 writes it out in
    /// `PAGEFILL`, `me_nodemax` and `me_maxfree_1pg`).
    #[inline(always)]
    pub const fn page_space(&self, psize: usize) -> usize {
        psize - self.page_hdr
    }

    /// `me_nodemax`: the largest node a page may hold before its data goes
    /// to an overflow page (`mdb_env_open2`):
    /// `((me_pagespace / MDB_MINKEYS) & -2) - sizeof(indx_t)`.
    #[inline(always)]
    pub const fn node_max(&self, psize: usize) -> usize {
        ((self.page_space(psize) / MDB_MINKEYS) & !1) - 2
    }

    /// `mdb_env_get_maxkeysize`: the largest key (and `MDB_DUPSORT` data
    /// item). 0.9 has the compile-time `MDB_MAXKEYSIZE` 511; 1.0 computes
    /// `me_maxkey = me_nodemax - (NODESIZE + sizeof(MDB_db))` at open
    /// (mdb.c 1.0, lines 5669-5673).
    #[inline(always)]
    pub const fn max_key_size(&self, psize: usize) -> usize {
        match self.format {
            Format::V09 => crate::consts::MDB_MAXKEYSIZE,
            Format::V10 => self.node_max(psize) - (NODE_SIZE + DB_SIZE),
        }
    }

    /// `me_maxfree_1pg`: how many page numbers fit in one free-list
    /// overflow page, `me_pagespace / sizeof(pgno_t) - 1`.
    #[inline(always)]
    pub const fn max_free_1pg(&self, psize: usize) -> usize {
        self.page_space(psize) / 8 - 1
    }

    /// `OVPAGES(size, psize)`: number of overflow pages needed for `size`
    /// bytes of data, `(PAGEHDRSZ - 1 + size) / psize + 1` (the first page
    /// also holds the header).
    #[inline(always)]
    pub const fn overflow_pages_for(&self, size: usize, psize: usize) -> usize {
        (self.page_hdr - 1 + size) / psize + 1
    }

    /// The `P_DIRTY` bit, if the format keeps it in `P_SUBP` sub-pages:
    /// 0.9 sets it whenever it builds or touches a sub-page (mdb.c lines
    /// 6961, 7001), 1.0 has no such flag.
    #[inline(always)]
    pub const fn subpage_dirty(&self) -> u16 {
        self.subpage_flags & P_DIRTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::page::{P_LEAF, P_SUBP};

    #[test]
    fn v09_numbers() {
        let l = &V09;
        assert_eq!(l.format, Format::V09);
        assert_eq!(l.data_version, 1);
        assert_eq!(l.page_hdr, 16);
        assert_eq!(l.ptr_base, 0);
        assert!(!l.even_nodes);
        assert_eq!(l.ovpage_size, 8);
        assert_eq!(l.subpage_flags, 0x52);
        assert_eq!(l.subpage_flags, P_LEAF | P_DIRTY | P_SUBP);
        assert_eq!(l.max_pagesize, 0x8000);
        assert_eq!(Format::V09.layout(), l);
        // The 0.9 constants agree with the descriptor.
        assert_eq!(l.page_hdr, PAGE_HDR_SIZE);
        assert_eq!(l.pad_offset(), PAGE_PAD);
        assert_eq!(l.flags_offset(), PAGE_FLAGS);
        assert_eq!(l.lower_offset(), PAGE_LOWER);
        assert_eq!(l.upper_offset(), PAGE_UPPER);
        assert_eq!(l.pages_offset(), PAGE_OVERFLOW_PAGES);
        assert_eq!(l.ptrs_offset(), PAGE_PTRS);
        assert_eq!(l.meta_offset(), META_OFFSET_IN_PAGE);
        assert_eq!(l.txnid_offset(), None);
        assert_eq!(l.max_key_size(4096), crate::consts::MDB_MAXKEYSIZE);
        assert_eq!(l.max_pagesize, crate::consts::MAX_PAGESIZE);
        assert_eq!(l.data_version, crate::consts::MDB_DATA_VERSION);
    }

    #[test]
    fn v10_numbers() {
        let l = &V10;
        assert_eq!(l.format, Format::V10);
        assert_eq!(l.data_version, 3);
        assert_eq!(l.page_hdr, 24);
        assert_eq!(l.ptr_base, 24);
        assert!(l.even_nodes);
        assert_eq!(l.ovpage_size, 24);
        assert_eq!(l.subpage_flags, 0x42);
        assert_eq!(l.subpage_flags, P_LEAF | P_SUBP);
        assert_eq!(l.max_pagesize, 0x10000);
        assert_eq!(Format::V10.layout(), l);
        // MDB_page_header: pgno, txnid, pad, flags, lower/upper.
        assert_eq!(l.txnid_offset(), Some(8));
        assert_eq!(l.pad_offset(), 16);
        assert_eq!(l.flags_offset(), 18);
        assert_eq!(l.lower_offset(), 20);
        assert_eq!(l.upper_offset(), 22);
        assert_eq!(l.pages_offset(), 20);
        assert_eq!(l.ptrs_offset(), 24);
        assert_eq!(l.meta_offset(), 24);
    }

    #[test]
    fn derived_sizes_v09() {
        let l = &V09;
        assert_eq!(l.page_space(4096), 4080);
        assert_eq!(l.node_max(4096), 2038);
        assert_eq!(l.node_max(16384), 8182);
        assert_eq!(l.max_free_1pg(4096), 509);
        assert_eq!(l.max_key_size(16384), 511);
        assert_eq!(l.overflow_pages_for(1, 4096), 1);
        assert_eq!(l.overflow_pages_for(4080, 4096), 1);
        assert_eq!(l.overflow_pages_for(4081, 4096), 2);
        assert_eq!(l.empty_lower(), 16);
        assert_eq!(l.num_keys(16), 0);
        assert_eq!(l.num_keys(22), 3);
        assert_eq!(l.num_keys(3), 0);
        assert_eq!(l.node_data_offset(3), 11);
        assert_eq!(l.leaf_size(3, 5), 16);
        assert_eq!(l.branch_size(5), 13);
        assert_eq!(l.stored_data_len(5, false), 5);
        assert_eq!(l.stored_data_len(5000, true), 8);
        assert_eq!(l.subpage_dirty(), P_DIRTY);
    }

    #[test]
    fn derived_sizes_v10() {
        let l = &V10;
        assert_eq!(l.page_space(4096), 4072);
        // ((4072 / 2) & -2) - 2 and ((16360 / 2) & -2) - 2.
        assert_eq!(l.node_max(4096), 2034);
        assert_eq!(l.node_max(16384), 8178);
        assert_eq!(l.node_max(65536), 32754);
        // me_nodemax - (NODESIZE + sizeof(MDB_db)); C 1.0's
        // mdb_env_get_maxkeysize returns these three.
        assert_eq!(l.max_key_size(4096), 1978);
        assert_eq!(l.max_key_size(16384), 8122);
        assert_eq!(l.max_key_size(65536), 32698);
        assert_eq!(l.max_free_1pg(4096), 508);
        assert_eq!(l.overflow_pages_for(4072, 4096), 1);
        assert_eq!(l.overflow_pages_for(4073, 4096), 2);
        // PAGEBASE == PAGEHDRSZ: an empty page has mp_lower 0.
        assert_eq!(l.empty_lower(), 0);
        assert_eq!(l.num_keys(0), 0);
        assert_eq!(l.num_keys(6), 3);
        assert_eq!(l.node_data_offset(3), 12);
        assert_eq!(l.node_data_offset(4), 12);
        assert_eq!(l.leaf_size(3, 5), 8 + 4 + 6);
        assert_eq!(l.branch_size(5), 13);
        assert_eq!(l.stored_data_len(5, false), 6);
        assert_eq!(l.stored_data_len(5000, true), 24);
        assert_eq!(l.subpage_dirty(), 0);
    }

    #[test]
    fn overflow_refs() {
        let r = OverflowRef {
            pgno: 0x1234_5678_9abc,
            txnid: 77,
            pages: 3,
        };
        let mut buf = [0xffu8; 32];
        V09.write_overflow_ref(&mut buf, r).unwrap();
        assert_eq!(&buf[..8], &r.pgno.to_ne_bytes());
        assert!(buf[8..].iter().all(|&b| b == 0xff), "0.9 writes 8 bytes");
        assert_eq!(
            V09.read_overflow_ref(&buf),
            Some(OverflowRef {
                pgno: r.pgno,
                ..OverflowRef::default()
            })
        );
        assert_eq!(V09.read_overflow_ref(&buf[..7]), None);
        assert_eq!(V09.write_overflow_ref(&mut buf[..7], r), None);

        let mut buf = [0xffu8; 32];
        V10.write_overflow_ref(&mut buf, r).unwrap();
        assert_eq!(&buf[..8], &r.pgno.to_ne_bytes());
        assert_eq!(&buf[8..16], &77u64.to_ne_bytes());
        assert_eq!(&buf[16..24], &3u64.to_ne_bytes());
        assert!(buf[24..].iter().all(|&b| b == 0xff));
        assert_eq!(V10.read_overflow_ref(&buf), Some(r));
        assert_eq!(V10.read_overflow_ref(&buf[..23]), None);
        assert_eq!(V10.write_overflow_ref(&mut buf[..23], r), None);
    }

    #[test]
    fn stamp_clean_page() {
        let mut page = [7u8; 64];
        V09.stamp_clean_page(&mut page, 42);
        assert!(page.iter().all(|&b| b == 7), "0.9 pages have no txnid");
        V10.stamp_clean_page(&mut page, 42);
        assert_eq!(&page[8..16], &42u64.to_ne_bytes());
        assert!(page[..8].iter().chain(&page[16..]).all(|&b| b == 7));
        let mut short = [7u8; 12];
        V10.stamp_clean_page(&mut short, 42);
        assert!(short.iter().all(|&b| b == 7));
    }
}
