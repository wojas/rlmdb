//! `MDB_node`: a key (and, on leaf pages, data) inside a branch or leaf page.
//!
//! Layout (mdb.c line 918, `NODESIZE == 8`):
//!
//! | offset | field | meaning |
//! |---|---|---|
//! | 0 / 2 | `mn_lo` | low 16 bits of data size (leaf) or child pgno (branch) |
//! | 2 / 0 | `mn_hi` | next 16 bits |
//! | 4 | `mn_flags` | leaf: `F_BIGDATA`/`F_SUBDATA`/`F_DUPDATA`; branch: pgno bits 32..48 |
//! | 6 | `mn_ksize` | key size |
//! | 8 | `mn_data` | key bytes, then data bytes (`NODEDATA = mn_data + mn_ksize`) |
//!
//! `mn_lo`/`mn_hi` swap places on big-endian hosts so that the pair can be
//! read as one native 32-bit word; the offsets live in [`super::layout`].
//!
//! A leaf node's data is one of:
//!
//! * plain bytes (`NODEDSZ` of them), or
//! * `F_BIGDATA`: an 8-byte page number of the first overflow page; the data
//!   (still `NODEDSZ` bytes) starts 16 bytes into that page,
//! * `F_SUBDATA`: a 48-byte `MDB_db` (a named database, or with `F_DUPDATA`
//!   a sub-DB of duplicates),
//! * `F_DUPDATA` without `F_SUBDATA`: a `P_SUBP` sub-page of duplicates.

use super::layout::*;
use super::{get_u16, get_u64};

/// `F_BIGDATA`: data is on overflow pages.
pub const F_BIGDATA: u16 = 0x01;
/// `F_SUBDATA`: data is an `MDB_db` (named DB or sub-DB of duplicates).
pub const F_SUBDATA: u16 = 0x02;
/// `F_DUPDATA`: data holds duplicates (sub-page, or sub-DB with `F_SUBDATA`).
pub const F_DUPDATA: u16 = 0x04;

/// A read-only view of one node. The slice starts at the node header and
/// extends to the end of the containing page, so every accessor can
/// bounds-check against the page.
#[derive(Debug, Clone, Copy)]
pub struct NodeRef<'a> {
    buf: &'a [u8],
}

impl<'a> NodeRef<'a> {
    /// Wraps the bytes from a node header to the end of its page.
    /// Returns `None` if there is no room for the header.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Option<NodeRef<'a>> {
        if buf.len() < NODE_SIZE {
            return None;
        }
        Some(NodeRef { buf })
    }

    #[inline]
    fn field(&self, off: usize) -> u16 {
        // In bounds: `new` checked for NODE_SIZE bytes.
        get_u16(self.buf, off).unwrap_or(0)
    }

    /// `mn_lo`.
    #[inline]
    pub fn lo(&self) -> u16 {
        self.field(NODE_LO)
    }

    /// `mn_hi`.
    #[inline]
    pub fn hi(&self) -> u16 {
        self.field(NODE_HI)
    }

    /// `mn_flags` (on branch nodes these are pgno bits, not flags).
    #[inline]
    pub fn flags(&self) -> u16 {
        self.field(NODE_FLAGS)
    }

    /// `mn_ksize`: the key length.
    #[inline]
    pub fn key_size(&self) -> usize {
        usize::from(self.field(NODE_KSIZE))
    }

    /// `NODEDSZ`: the data size of a leaf node (`mn_lo | mn_hi << 16`).
    #[inline]
    pub fn data_size(&self) -> usize {
        (u32::from(self.lo()) | (u32::from(self.hi()) << 16)) as usize
    }

    /// `NODEPGNO`: the child page of a branch node
    /// (`mn_lo | mn_hi << 16 | mn_flags << 32`).
    #[inline]
    pub fn child_pgno(&self) -> u64 {
        u64::from(self.lo()) | (u64::from(self.hi()) << 16) | (u64::from(self.flags()) << 32)
    }

    /// `true` if the data lives on overflow pages.
    #[inline]
    pub fn is_bigdata(&self) -> bool {
        self.flags() & F_BIGDATA != 0
    }

    /// `true` if the data is an `MDB_db` record.
    #[inline]
    pub fn is_subdata(&self) -> bool {
        self.flags() & F_SUBDATA != 0
    }

    /// `true` if the node holds duplicates (sub-page or sub-DB).
    #[inline]
    pub fn is_dupdata(&self) -> bool {
        self.flags() & F_DUPDATA != 0
    }

    /// The key bytes (`NODEKEY`).
    #[inline]
    pub fn key(&self) -> Option<&'a [u8]> {
        self.buf.get(NODE_DATA..NODE_DATA + self.key_size())
    }

    /// The bytes from the key to the end of the page: the key followed by
    /// whatever comes after it (see `Comparator::compare_in_page`).
    #[inline]
    pub fn key_tail(&self) -> &'a [u8] {
        self.buf.get(NODE_DATA..).unwrap_or(&[])
    }

    /// The bytes from the data part to the end of the page.
    #[inline]
    pub fn data_tail(&self) -> &'a [u8] {
        self.buf.get(self.data_offset()..).unwrap_or(&[])
    }

    /// Offset of the data part from the node start.
    #[inline]
    fn data_offset(&self) -> usize {
        NODE_DATA + self.key_size()
    }

    /// The inline data bytes (`NODEDATA`, `NODEDSZ` long) of a leaf node
    /// without `F_BIGDATA`: plain data, a sub-page or an `MDB_db`.
    #[inline]
    pub fn inline_data(&self) -> Option<&'a [u8]> {
        let off = self.data_offset();
        self.buf.get(off..off.checked_add(self.data_size())?)
    }

    /// The overflow page number stored as the data of an `F_BIGDATA` node.
    #[inline]
    pub fn overflow_pgno(&self) -> Option<u64> {
        get_u64(self.buf, self.data_offset())
    }

    /// The whole node (header, key, data) as stored in the page, for a leaf
    /// node; branch nodes have no data part. Mostly useful for tests and
    /// copying nodes between pages.
    pub fn raw(&self, is_leaf: bool) -> Option<&'a [u8]> {
        let mut len = self.data_offset();
        if is_leaf {
            len += if self.is_bigdata() {
                8
            } else {
                self.data_size()
            };
        }
        self.buf.get(..len)
    }
}

/// `INDXSIZE(k)` without the pointer: header plus key for a branch node.
pub fn branch_node_size(key_len: usize) -> usize {
    NODE_SIZE + key_len
}

/// `LEAFSIZE(k, d)`: header plus key plus data for a leaf node.
pub fn leaf_node_size(key_len: usize, data_len: usize) -> usize {
    NODE_SIZE + key_len + data_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_bytes(lo: u16, hi: u16, flags: u16, key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; NODE_SIZE];
        v[NODE_LO..NODE_LO + 2].copy_from_slice(&lo.to_ne_bytes());
        v[NODE_HI..NODE_HI + 2].copy_from_slice(&hi.to_ne_bytes());
        v[NODE_FLAGS..NODE_FLAGS + 2].copy_from_slice(&flags.to_ne_bytes());
        v[NODE_KSIZE..NODE_KSIZE + 2].copy_from_slice(&(key.len() as u16).to_ne_bytes());
        v.extend_from_slice(key);
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn leaf_node() {
        let b = node_bytes(5, 0, 0, b"key", b"hello");
        let n = NodeRef::new(&b).unwrap();
        assert_eq!(n.key(), Some(&b"key"[..]));
        assert_eq!(n.data_size(), 5);
        assert_eq!(n.inline_data(), Some(&b"hello"[..]));
        assert!(!n.is_bigdata() && !n.is_subdata() && !n.is_dupdata());
        assert_eq!(n.raw(true).map(|r| r.len()), Some(16));
    }

    #[test]
    fn data_size_uses_hi_word() {
        // 0x0001_0002 bytes: lo = 2, hi = 1.
        let b = node_bytes(2, 1, F_BIGDATA, b"k", &77u64.to_ne_bytes());
        let n = NodeRef::new(&b).unwrap();
        assert_eq!(n.data_size(), 0x1_0002);
        assert!(n.is_bigdata());
        assert_eq!(n.overflow_pgno(), Some(77));
        // The inline data would run past the buffer: no panic, just None.
        assert_eq!(n.inline_data(), None);
    }

    #[test]
    fn branch_pgno_uses_flags_word() {
        let b = node_bytes(0x3456, 0x9012, 0x0078, b"", &[]);
        let n = NodeRef::new(&b).unwrap();
        assert_eq!(n.child_pgno(), 0x0078_9012_3456);
    }

    #[test]
    fn lo_hi_order_matches_host_word() {
        // On any host, mn_lo/mn_hi read as one native u32 equal the size.
        let b = node_bytes(0xbeef, 0x0012, 0, b"", &[]);
        let word = u32::from_ne_bytes(b[0..4].try_into().unwrap());
        assert_eq!(word, 0x0012_beef);
        assert_eq!(NodeRef::new(&b).unwrap().data_size(), 0x0012_beef);
    }

    #[test]
    fn truncated() {
        assert!(NodeRef::new(&[0u8; 7]).is_none());
        let b = node_bytes(0, 0, 0, b"abcdef", &[]);
        let n = NodeRef::new(&b[..10]).unwrap();
        assert_eq!(n.key(), None);
    }
}
