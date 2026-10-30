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
//! The header is the same in both formats; where the data starts is not:
//! LMDB 1.0 pads the key to an even length (`NODEDATA = mn_data +
//! EVEN(mn_ksize)`), so a [`NodeRef`] carries the [`Layout`] of its page.
//!
//! A leaf node's data is one of:
//!
//! * plain bytes (`NODEDSZ` of them), or
//! * `F_BIGDATA`: a reference to the first overflow page (0.9: its 8-byte
//!   page number; 1.0: `MDB_ovpage`, see [`OverflowRef`]); the data (still
//!   `NODEDSZ` bytes) starts right after the header of that page,
//! * `F_SUBDATA`: a 48-byte `MDB_db` (a named database, or with `F_DUPDATA`
//!   a sub-DB of duplicates),
//! * `F_DUPDATA` without `F_SUBDATA`: a `P_SUBP` sub-page of duplicates.

use super::get_u16;
use super::layout::{
    Layout, OverflowRef, NODE_FLAGS, NODE_HI, NODE_KSIZE, NODE_LO, NODE_SIZE, V09,
};

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
    layout: &'static Layout,
}

impl<'a> NodeRef<'a> {
    /// Wraps the bytes from a node header to the end of its page, in the
    /// LMDB 0.9 layout. Returns `None` if there is no room for the header.
    #[inline(always)]
    pub fn new(buf: &'a [u8]) -> Option<NodeRef<'a>> {
        NodeRef::with_layout(buf, &V09)
    }

    /// Wraps the bytes from a node header to the end of its page, a page of
    /// the format `layout` describes. Returns `None` if there is no room
    /// for the header.
    #[inline(always)]
    pub fn with_layout(buf: &'a [u8], layout: &'static Layout) -> Option<NodeRef<'a>> {
        if buf.len() < NODE_SIZE {
            return None;
        }
        Some(NodeRef { buf, layout })
    }

    /// The layout of the page this node is on.
    #[inline(always)]
    pub fn layout(&self) -> &'static Layout {
        self.layout
    }

    /// The bytes from the node header to the end of the page (what
    /// [`NodeRef::with_layout`] was given).
    #[inline(always)]
    pub fn bytes(&self) -> &'a [u8] {
        self.buf
    }

    /// The layout as the accessors read it, re-derived from its format so
    /// that its numbers fold into constants (see `PageRef::l`).
    #[inline(always)]
    fn l(&self) -> &'static Layout {
        self.layout.format.layout()
    }

    #[inline(always)]
    fn field(&self, off: usize) -> u16 {
        // In bounds: `new` checked for NODE_SIZE bytes.
        get_u16(self.buf, off).unwrap_or(0)
    }

    /// `mn_lo`.
    #[inline(always)]
    pub fn lo(&self) -> u16 {
        self.field(NODE_LO)
    }

    /// `mn_hi`.
    #[inline(always)]
    pub fn hi(&self) -> u16 {
        self.field(NODE_HI)
    }

    /// `mn_flags` (on branch nodes these are pgno bits, not flags).
    #[inline(always)]
    pub fn flags(&self) -> u16 {
        self.field(NODE_FLAGS)
    }

    /// `mn_ksize`: the key length.
    #[inline(always)]
    pub fn key_size(&self) -> usize {
        usize::from(self.field(NODE_KSIZE))
    }

    /// `NODEDSZ`: the data size of a leaf node (`mn_lo | mn_hi << 16`).
    #[inline(always)]
    pub fn data_size(&self) -> usize {
        (u32::from(self.lo()) | (u32::from(self.hi()) << 16)) as usize
    }

    /// `NODEPGNO`: the child page of a branch node
    /// (`mn_lo | mn_hi << 16 | mn_flags << 32`).
    #[inline(always)]
    pub fn child_pgno(&self) -> u64 {
        u64::from(self.lo()) | (u64::from(self.hi()) << 16) | (u64::from(self.flags()) << 32)
    }

    /// `true` if the data lives on overflow pages.
    #[inline(always)]
    pub fn is_bigdata(&self) -> bool {
        self.flags() & F_BIGDATA != 0
    }

    /// `true` if the data is an `MDB_db` record.
    #[inline(always)]
    pub fn is_subdata(&self) -> bool {
        self.flags() & F_SUBDATA != 0
    }

    /// `true` if the node holds duplicates (sub-page or sub-DB).
    #[inline(always)]
    pub fn is_dupdata(&self) -> bool {
        self.flags() & F_DUPDATA != 0
    }

    /// The key bytes (`NODEKEY`: `mn_data`, right after the header in both
    /// formats).
    #[inline(always)]
    pub fn key(&self) -> Option<&'a [u8]> {
        self.buf.get(NODE_SIZE..NODE_SIZE + self.key_size())
    }

    /// The bytes from the key to the end of the page: the key followed by
    /// whatever comes after it (see `Comparator::compare_in_page`).
    #[inline(always)]
    pub fn key_tail(&self) -> &'a [u8] {
        self.buf.get(NODE_SIZE..).unwrap_or(&[])
    }

    /// The bytes from the data part to the end of the page.
    #[inline(always)]
    pub fn data_tail(&self) -> &'a [u8] {
        self.buf.get(self.data_offset()..).unwrap_or(&[])
    }

    /// Offset of the data part from the node start (`NODEDATA`).
    #[inline(always)]
    fn data_offset(&self) -> usize {
        self.l().node_data_offset(self.key_size())
    }

    /// The inline data bytes (`NODEDATA`, `NODEDSZ` long) of a leaf node
    /// without `F_BIGDATA`: plain data, a sub-page or an `MDB_db`.
    #[inline(always)]
    pub fn inline_data(&self) -> Option<&'a [u8]> {
        let off = self.data_offset();
        self.buf.get(off..off.checked_add(self.data_size())?)
    }

    /// The key and the inline data of a leaf node without `F_BIGDATA`:
    /// `Some((self.key()?, self.inline_data()?))`, with one bounds check
    /// instead of two. The key ends where the data starts (1.0: at most one
    /// padding byte before), so data within the page means a key within it
    /// too, and the result is `None` exactly when `inline_data` is.
    #[inline(always)]
    pub(crate) fn key_and_data(&self) -> Option<(&'a [u8], &'a [u8])> {
        let ksize = self.key_size();
        let off = self.data_offset();
        let node = self.buf.get(..off.checked_add(self.data_size())?)?;
        let (head, data) = node.split_at_checked(off)?;
        Some((head.get(NODE_SIZE..NODE_SIZE + ksize)?, data))
    }

    /// The overflow page number stored as the data of an `F_BIGDATA` node.
    #[inline(always)]
    pub fn overflow_pgno(&self) -> Option<u64> {
        self.overflow_ref().map(|r| r.pgno)
    }

    /// The overflow reference stored as the data of an `F_BIGDATA` node
    /// (in 0.9 only its page number is stored).
    #[inline(always)]
    pub fn overflow_ref(&self) -> Option<OverflowRef> {
        self.l()
            .read_overflow_ref(self.buf.get(self.data_offset()..)?)
    }

    /// The whole node (header, key, data) as stored in the page, for a leaf
    /// node; branch nodes have no data part. The data part of an
    /// `F_BIGDATA` node is its overflow reference. Padding after the data
    /// (1.0, or the `EVEN` rounding of the node) is not included. Mostly
    /// useful for tests and copying nodes between pages.
    pub fn raw(&self, is_leaf: bool) -> Option<&'a [u8]> {
        let mut len = self.data_offset();
        if is_leaf {
            len += if self.is_bigdata() {
                self.l().ovpage_size
            } else {
                self.data_size()
            };
        }
        self.buf.get(..len)
    }
}

/// `INDXSIZE(k)` of the 0.9 layout without the pointer: header plus key for
/// a branch node. The engine uses [`Layout::branch_size`].
pub fn branch_node_size(key_len: usize) -> usize {
    V09.branch_size(key_len)
}

/// `LEAFSIZE(k, d)` of the 0.9 layout: header plus key plus data for a leaf
/// node. The engine uses [`Layout::leaf_size`].
pub fn leaf_node_size(key_len: usize, data_len: usize) -> usize {
    V09.leaf_size(key_len, data_len)
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
    fn even_key_padding_in_v10() {
        use crate::format::layout::V10;
        // 1.0 pads the 3-byte key to 4 before the data starts.
        let mut b = node_bytes(5, 0, 0, b"key", b"");
        b.push(0);
        b.extend_from_slice(b"hello");
        let n = NodeRef::with_layout(&b, &V10).unwrap();
        assert_eq!(n.key(), Some(&b"key"[..]));
        assert_eq!(n.inline_data(), Some(&b"hello"[..]));
        assert_eq!(n.raw(true).map(|r| r.len()), Some(8 + 4 + 5));
        // The same bytes read as 0.9 start the data at the padding byte.
        let n = NodeRef::new(&b).unwrap();
        assert_eq!(n.inline_data(), Some(&b"\0hell"[..]));

        // F_BIGDATA: a 24-byte MDB_ovpage after the padded key.
        let mut ov = [0u8; 24];
        V10.write_overflow_ref(
            &mut ov,
            OverflowRef {
                pgno: 9,
                txnid: 4,
                pages: 2,
            },
        )
        .unwrap();
        let mut b = node_bytes(0, 1, F_BIGDATA, b"k", b"");
        b.push(0);
        b.extend_from_slice(&ov);
        let n = NodeRef::with_layout(&b, &V10).unwrap();
        assert_eq!(n.overflow_pgno(), Some(9));
        assert_eq!(n.overflow_ref().map(|r| (r.txnid, r.pages)), Some((4, 2)));
        assert_eq!(n.raw(true).map(|r| r.len()), Some(8 + 2 + 24));
    }

    #[test]
    fn truncated() {
        assert!(NodeRef::new(&[0u8; 7]).is_none());
        let b = node_bytes(0, 0, 0, b"abcdef", &[]);
        let n = NodeRef::new(&b[..10]).unwrap();
        assert_eq!(n.key(), None);
    }

    #[test]
    fn key_and_data_is_key_then_inline_data() {
        use crate::format::layout::V10;
        // Every key and data size up to 5 bytes, stored and claimed (the
        // size fields may lie), in buffers cut at every length: the fused
        // accessor agrees with the two separate ones in both layouts.
        for layout in [&V09, &V10] {
            for klen in 0..6 {
                for dlen in 0..6u16 {
                    let key = &b"abcde"[..klen];
                    let mut b = node_bytes(dlen, 0, 0, key, b"");
                    b.extend_from_slice(b"\0vwxyz");
                    for cut in NODE_SIZE..=b.len() {
                        let n = NodeRef::with_layout(&b[..cut], layout).unwrap();
                        let both = n.key().zip(n.inline_data());
                        assert_eq!(n.key_and_data(), both, "{klen} {dlen} {cut}");
                        assert_eq!(n.key_and_data().is_some(), n.inline_data().is_some());
                    }
                }
            }
        }
        // A data size beyond the buffer.
        let b = node_bytes(0xffff, 0xffff, 0, b"k", b"data");
        assert_eq!(NodeRef::new(&b).unwrap().key_and_data(), None);
    }
}
