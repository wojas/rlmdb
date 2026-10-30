//! Pure parsing and serialisation of LMDB's on-disk structures.
//!
//! Everything in here works on byte slices: `&[u8]` for reading and
//! `&mut [u8]` for writing. There is no `unsafe`, no pointer casts and no
//! `#[repr(C)]` overlay of mapped memory; integers are decoded with
//! `from_ne_bytes` because LMDB stores everything in host byte order.
//!
//! Only the 64-bit layout is implemented (`sizeof(size_t) == 8`), which is the
//! layout of every 64-bit C build of LMDB (`MDB_VL32` is out of scope).
//!
//! The page header, the node data offset and the overflow reference differ
//! between the LMDB 0.9 and 1.0 data formats. A [`Layout`] describes one
//! format ([`layout::V09`], [`layout::V10`]); the page and node views carry
//! the layout of the environment they belong to and read every
//! format-dependent offset from it. Their plain constructors
//! ([`PageRef::new`], [`NodeRef::new`], [`PageMut::new`]) and the free size
//! functions below use the 0.9 layout, for tools and tests that only handle
//! 0.9 files; the engine uses the `with_layout` constructors and the
//! [`Layout`] methods.
//!
//! | C structure | Rust | size (0.9 / 1.0) |
//! |---|---|---|
//! | `MDB_page` header | [`page::PageRef`] / [`page::PageMut`] | 16 / 24 |
//! | `MDB_node` header | [`node::NodeRef`] | 8 |
//! | `MDB_db` | [`db::Db`] | 48 |
//! | `MDB_meta` | [`meta::Meta`] | 136 |
//! | `MDB_ovpage` | [`layout::OverflowRef`] | 8 / 24 |
//! | `MDB_txninfo` / `MDB_reader` | [`lockfile`] | platform dependent |

#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "lmdb: only 64-bit targets are supported (the file format depends on sizeof(size_t))"
);

pub mod db;
pub mod layout;
pub mod lockfile;
pub mod meta;
pub mod node;
pub mod page;

pub use db::Db;
pub use layout::{Format, Layout, OverflowRef};
pub use meta::Meta;
pub use node::NodeRef;
pub use page::{PageMut, PageRef};

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

/// `OVPAGES(size, psize)` of the 0.9 layout: number of overflow pages
/// needed for `size` bytes of data (the first page also holds the 16-byte
/// header). The engine uses [`Layout::overflow_pages_for`].
pub fn overflow_pages_for(size: usize, page_size: usize) -> usize {
    layout::V09.overflow_pages_for(size, page_size)
}

/// `me_nodemax` of the 0.9 layout: the largest node a page may hold before
/// its data goes to an overflow page (`mdb_env_open2`). The engine uses
/// [`Layout::node_max`].
pub fn node_max(page_size: usize) -> usize {
    layout::V09.node_max(page_size)
}

/// `me_maxfree_1pg` of the 0.9 layout: how many page numbers fit in one
/// free-list overflow page. The engine uses [`Layout::max_free_1pg`].
pub fn max_free_1pg(page_size: usize) -> usize {
    layout::V09.max_free_1pg(page_size)
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
