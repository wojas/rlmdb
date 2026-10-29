//! Pure parsing and serialisation of LMDB's on-disk structures.
//!
//! Everything in here works on byte slices: `&[u8]` for reading and
//! `&mut [u8]` for writing. There is no `unsafe`, no pointer casts and no
//! `#[repr(C)]` overlay of mapped memory; integers are decoded with
//! `from_ne_bytes` because LMDB stores everything in host byte order.
//!
//! Only the 64-bit layout is implemented (`sizeof(size_t) == 8`), which is the
//! layout of every 64-bit C build of LMDB 0.9 (`MDB_VL32` is out of scope).
//!
//! | C structure | Rust | size |
//! |---|---|---|
//! | `MDB_page` header | [`page::PageRef`] / [`page::PageMut`] | 16 |
//! | `MDB_node` header | [`node::NodeRef`] | 8 |
//! | `MDB_db` | [`db::Db`] | 48 |
//! | `MDB_meta` | [`meta::Meta`] | 136 |
//! | `MDB_txninfo` / `MDB_reader` | [`lockfile`] | platform dependent |

#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "lmdb: only 64-bit targets are supported (the file format depends on sizeof(size_t))"
);

pub mod db;
pub mod lockfile;
pub mod meta;
pub mod node;
pub mod page;

pub use db::Db;
pub use meta::Meta;
pub use node::NodeRef;
pub use page::{PageMut, PageRef};

/// Byte offsets and sizes of the on-disk structures (64-bit layout).
///
/// Each constant names the C field it locates.
pub mod layout {
    // --- MDB_page ---------------------------------------------------------

    /// `PAGEHDRSZ`: size of the page header (`offsetof(MDB_page, mp_ptrs)`).
    pub const PAGE_HDR_SIZE: usize = 16;
    /// Offset of `mp_pgno` (u64).
    pub const PAGE_PGNO: usize = 0;
    /// Offset of `mp_pad` (u16): key size on `P_LEAF2` pages.
    pub const PAGE_PAD: usize = 8;
    /// Offset of `mp_flags` (u16).
    pub const PAGE_FLAGS: usize = 10;
    /// Offset of `mp_lower` (u16), lower bound of free space.
    pub const PAGE_LOWER: usize = 12;
    /// Offset of `mp_upper` (u16), upper bound of free space.
    pub const PAGE_UPPER: usize = 14;
    /// Offset of `mp_pages` (u32): overflow page count, overlays lower/upper.
    pub const PAGE_OVERFLOW_PAGES: usize = 12;
    /// Offset of `mp_ptrs[0]` (u16 node offsets).
    pub const PAGE_PTRS: usize = 16;

    // --- MDB_node ---------------------------------------------------------

    /// `NODESIZE`: size of the node header (`offsetof(MDB_node, mn_data)`).
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
    /// Offset of `mn_data` (key bytes, then data bytes).
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
    /// Offset of the `MDB_meta` inside a meta page (`METADATA(p)`).
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
}

/// Reads a native-endian `u16` at `off`; `None` if out of bounds.
#[inline]
pub(crate) fn get_u16(buf: &[u8], off: usize) -> Option<u16> {
    let b = buf.get(off..off.checked_add(2)?)?;
    Some(u16::from_ne_bytes([b[0], b[1]]))
}

/// Reads a native-endian `u32` at `off`; `None` if out of bounds.
#[inline]
pub(crate) fn get_u32(buf: &[u8], off: usize) -> Option<u32> {
    let b = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_ne_bytes(b.try_into().ok()?))
}

/// Reads a native-endian `u64` at `off`; `None` if out of bounds.
#[inline]
pub(crate) fn get_u64(buf: &[u8], off: usize) -> Option<u64> {
    let b = buf.get(off..off.checked_add(8)?)?;
    Some(u64::from_ne_bytes(b.try_into().ok()?))
}

/// Writes a native-endian `u16` at `off`. Panics if out of bounds: writers
/// always size their buffers first, so this is a programming error.
#[inline]
pub(crate) fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}

/// Writes a native-endian `u32` at `off` (see [`put_u16`]).
#[inline]
pub(crate) fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

/// Writes a native-endian `u64` at `off` (see [`put_u16`]).
#[inline]
pub(crate) fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

/// `OVPAGES(size, psize)`: number of overflow pages needed for `size` bytes
/// of data (the first page also holds the 16-byte header).
pub fn overflow_pages_for(size: usize, page_size: usize) -> usize {
    (layout::PAGE_HDR_SIZE - 1 + size) / page_size + 1
}

/// `me_nodemax`: the largest node a page may hold before its data goes to an
/// overflow page (`mdb_env_open2`). Uses C's `& -2` (round down to even)
/// followed by `- sizeof(indx_t)`.
pub fn node_max(page_size: usize) -> usize {
    (((page_size - layout::PAGE_HDR_SIZE) / crate::consts::MDB_MINKEYS) & !1) - 2
}

/// `me_maxfree_1pg`: how many page numbers fit in one free-list overflow page.
pub fn max_free_1pg(page_size: usize) -> usize {
    (page_size - layout::PAGE_HDR_SIZE) / 8 - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_sizes() {
        assert_eq!(node_max(4096), 2038);
        assert_eq!(node_max(16384), 8182);
        assert_eq!(max_free_1pg(4096), 509);
        assert_eq!(overflow_pages_for(1, 4096), 1);
        assert_eq!(overflow_pages_for(4080, 4096), 1);
        assert_eq!(overflow_pages_for(4081, 4096), 2);
        assert_eq!(layout::META_TXNID + 8, layout::META_SIZE);
    }
}
