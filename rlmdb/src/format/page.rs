//! `MDB_page`: page header accessors, the node index and `P_LEAF2` keys.
//!
//! A page is `psize` bytes (a power of two up to 32 KiB, recorded in the meta).
//! It starts with a 16-byte header (mdb.c line 858):
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 8 | `mp_pgno` |
//! | 8 | 2 | `mp_pad` (key size on `P_LEAF2` pages) |
//! | 10 | 2 | `mp_flags` |
//! | 12 | 2 | `mp_lower` (overflow pages: low half of `mp_pages`) |
//! | 14 | 2 | `mp_upper` (overflow pages: high half of `mp_pages`) |
//!
//! Branch and leaf pages continue with `mp_ptrs[]`, one `u16` offset per node,
//! sorted by key; the nodes themselves are packed at the end of the page.
//! `NUMKEYS = (mp_lower - 16) / 2`. `P_LEAF2` pages (`MDB_DUPFIXED`
//! duplicates) have no pointers or node headers: `NUMKEYS` keys of `mp_pad`
//! bytes each are packed from offset 16, while `mp_lower` still counts as if
//! there were pointers. Overflow pages hold one big data item; only the first
//! page of a run has a header and `mp_pages` gives the run length.
//!
//! `P_SUBP` sub-pages (sorted duplicates stored inline in a leaf node) use the
//! same layout inside the node's data, with offsets relative to the start of
//! the sub-page, so [`PageRef`] works on them unchanged.
//!
//! That is the LMDB 0.9 layout. LMDB 1.0 inserts `mp_txnid` (u64) after
//! `mp_pgno`, so its header is 24 bytes and `mp_pad` .. `mp_upper` move up
//! by 8, and it stores `mp_lower`, `mp_upper` and `mp_ptrs[]` relative to the
//! end of the header (`PAGEBASE`, ITS#7713) so that a 64 KiB page fits
//! `u16` offsets: `NUMKEYS = mp_lower >> 1`, node `i` is at `mp_ptrs[i] + 24`.
//! A page view carries the [`Layout`] of its environment and reads every
//! such offset from it. [`PageRef::lower`] and [`PageRef::upper`] return
//! the fields as stored; [`PageRef::node_offset`] resolves a pointer to an
//! offset from the page start.
//!
//! All accessors on [`PageRef`] are bounds-checked and return `None` on
//! malformed input instead of panicking; callers turn that into
//! `MDB_CORRUPTED`.

use super::layout::{
    even, Layout, OverflowRef, NODE_FLAGS, NODE_HI, NODE_KSIZE, NODE_LO, NODE_SIZE, V09,
};
use super::node::NodeRef;
use super::{get_u16, get_u64, put_u16, put_u32, put_u64};

/// `P_BRANCH`: branch page.
pub const P_BRANCH: u16 = 0x01;
/// `P_LEAF`: leaf page.
pub const P_LEAF: u16 = 0x02;
/// `P_OVERFLOW`: first page of an overflow run.
pub const P_OVERFLOW: u16 = 0x04;
/// `P_META`: meta page.
pub const P_META: u16 = 0x08;
/// `P_DIRTY`: dirty page (in memory only; also set on `P_SUBP` pages).
pub const P_DIRTY: u16 = 0x10;
/// `P_LEAF2`: leaf page of a `MDB_DUPFIXED` sub-DB.
pub const P_LEAF2: u16 = 0x20;
/// `P_SUBP`: sub-page of duplicates inside a leaf node.
pub const P_SUBP: u16 = 0x40;
/// `P_LOOSE`: page was dirtied then freed, can be reused (in memory only).
pub const P_LOOSE: u16 = 0x4000;
/// `P_KEEP`: leave this page alone during spill (in memory only).
pub const P_KEEP: u16 = 0x8000;

/// Offsets of `mp_pad`, `mp_flags`, `mp_lower` and `mp_upper` within the
/// last 8 bytes of the page header, where both formats keep them.
const FIELD_PAD: usize = 0;
/// See [`FIELD_PAD`].
const FIELD_FLAGS: usize = 2;
/// See [`FIELD_PAD`].
const FIELD_LOWER: usize = 4;
/// See [`FIELD_PAD`].
const FIELD_UPPER: usize = 6;

/// A read-only view of a page (or sub-page) of a given [`Layout`].
#[derive(Debug, Clone, Copy)]
pub struct PageRef<'a> {
    buf: &'a [u8],
    layout: &'static Layout,
}

impl<'a> PageRef<'a> {
    /// Wraps a page in the LMDB 0.9 layout. Returns `None` if `buf` cannot
    /// even hold a header.
    #[inline(always)]
    pub const fn new(buf: &'a [u8]) -> Option<PageRef<'a>> {
        PageRef::with_layout(buf, &V09)
    }

    /// Wraps a page of the format `layout` describes. Returns `None` if
    /// `buf` cannot even hold a header.
    #[inline(always)]
    pub const fn with_layout(buf: &'a [u8], layout: &'static Layout) -> Option<PageRef<'a>> {
        if buf.len() < layout.page_hdr {
            return None;
        }
        Some(PageRef { buf, layout })
    }

    /// A page view without the length check of [`PageRef::with_layout`],
    /// for bytes that were one already (the cursor stacks keep only the
    /// bytes). The accessors stay bounds-checked: on a short buffer they
    /// read zeros or `None`, never out of bounds.
    #[inline(always)]
    pub(crate) const fn from_parts(buf: &'a [u8], layout: &'static Layout) -> PageRef<'a> {
        PageRef { buf, layout }
    }

    /// The page bytes.
    #[inline(always)]
    pub fn bytes(&self) -> &'a [u8] {
        self.buf
    }

    /// The layout this page is read with.
    #[inline(always)]
    pub fn layout(&self) -> &'static Layout {
        self.layout
    }

    /// The layout as the accessors read it: re-derived from its format, so
    /// that it is known to be one of the two statics. After inlining, the
    /// compiler then folds each number an accessor needs (header size,
    /// `PAGEBASE`, …) into a constant per format instead of loading it
    /// through the pointer on every call.
    #[inline(always)]
    fn l(&self) -> &'static Layout {
        self.layout.format.layout()
    }

    /// The last 8 header bytes: `mp_pad`, `mp_flags`, `mp_lower`,
    /// `mp_upper`. `with_layout` checked that they exist (and `from_parts`
    /// is only given such bytes), so the fallback is never used; it keeps
    /// the accessors free of panics. Reading the fields from a fixed-size
    /// array leaves one length check per read, which compiles to a
    /// conditional select instead of a branch per field offset.
    #[inline(always)]
    fn fields(&self) -> &'a [u8; 8] {
        self.buf
            .get(..self.l().page_hdr)
            .and_then(|h| h.last_chunk())
            .unwrap_or(&[0; 8])
    }

    /// The `u16` at `off` of [`PageRef::fields`] (one of the `FIELD_*`
    /// offsets), so the indexing compiles to a plain load.
    #[inline(always)]
    fn hdr16(&self, off: usize) -> u16 {
        let h = self.fields();
        u16::from_ne_bytes([h[off], h[off + 1]])
    }

    /// `mp_pgno` (the first 8 bytes in both formats).
    #[inline(always)]
    pub fn pgno(&self) -> u64 {
        self.buf
            .first_chunk()
            .map_or(0, |b: &[u8; 8]| u64::from_ne_bytes(*b))
    }

    /// `mp_txnid`, in formats whose header has it (LMDB 1.0).
    #[inline(always)]
    pub fn txnid(&self) -> Option<u64> {
        get_u64(self.buf, self.l().txnid_offset()?)
    }

    /// `mp_pad`: the key size on `P_LEAF2` pages.
    #[inline(always)]
    pub fn pad(&self) -> u16 {
        self.hdr16(FIELD_PAD)
    }

    /// `mp_flags`.
    #[inline(always)]
    pub fn flags(&self) -> u16 {
        self.hdr16(FIELD_FLAGS)
    }

    /// `mp_lower` as stored: end of the pointer array, relative to
    /// `PAGEBASE` ([`Layout::ptr_base`]).
    #[inline(always)]
    pub fn lower(&self) -> u16 {
        self.hdr16(FIELD_LOWER)
    }

    /// `mp_upper` as stored: start of the node area, relative to
    /// `PAGEBASE` ([`Layout::ptr_base`]).
    #[inline(always)]
    pub fn upper(&self) -> u16 {
        self.hdr16(FIELD_UPPER)
    }

    /// `mp_pages`: length of an overflow run (overlays lower/upper).
    #[inline(always)]
    pub fn overflow_pages(&self) -> u32 {
        let h = self.fields();
        u32::from_ne_bytes([
            h[FIELD_LOWER],
            h[FIELD_LOWER + 1],
            h[FIELD_LOWER + 2],
            h[FIELD_LOWER + 3],
        ])
    }

    /// `NUMKEYS`: number of nodes (or `P_LEAF2` keys). A corrupt `mp_lower`
    /// below the header size yields 0.
    #[inline(always)]
    pub fn num_keys(&self) -> usize {
        self.l().num_keys(self.lower())
    }

    /// `SIZELEFT`: free bytes between pointers and nodes.
    ///
    /// C computes `(indx_t)(mp_upper - mp_lower)`, modulo 2^16. That
    /// matters for `P_LEAF2` pages, which only count with the two fields
    /// (`mp_upper` grows past the page end and wraps on 64 KiB pages of
    /// 1-byte items, while the difference stays exact; see
    /// `core::page_ops::leaf2_count`): their free space is the wrapping
    /// difference, as in C. On other pages `mp_upper` is an offset, and a
    /// corrupt one below `mp_lower` yields 0.
    #[inline(always)]
    pub fn size_left(&self) -> usize {
        if self.is_leaf2() {
            usize::from(self.upper().wrapping_sub(self.lower()))
        } else {
            usize::from(self.upper().saturating_sub(self.lower()))
        }
    }

    /// `IS_BRANCH`.
    #[inline(always)]
    pub fn is_branch(&self) -> bool {
        self.flags() & P_BRANCH != 0
    }

    /// `IS_LEAF`.
    #[inline(always)]
    pub fn is_leaf(&self) -> bool {
        self.flags() & P_LEAF != 0
    }

    /// `IS_LEAF2`.
    #[inline(always)]
    pub fn is_leaf2(&self) -> bool {
        self.flags() & P_LEAF2 != 0
    }

    /// `IS_OVERFLOW`.
    #[inline(always)]
    pub fn is_overflow(&self) -> bool {
        self.flags() & P_OVERFLOW != 0
    }

    /// `IS_SUBP`.
    #[inline(always)]
    pub fn is_subpage(&self) -> bool {
        self.flags() & P_SUBP != 0
    }

    /// `true` for meta pages.
    #[inline(always)]
    pub fn is_meta(&self) -> bool {
        self.flags() & P_META != 0
    }

    /// `mp_ptrs[i] + PAGEBASE`: the offset of node `i` from the page start.
    #[inline(always)]
    pub fn node_offset(&self, i: usize) -> Option<usize> {
        if i >= self.num_keys() {
            return None;
        }
        let l = self.l();
        get_u16(self.buf, l.ptrs_offset() + 2 * i).map(|p| usize::from(p) + l.ptr_base)
    }

    /// `NODEPTR(p, i)`: node `i`, or `None` if `i` is out of range or the
    /// pointer leads outside the page.
    #[inline(always)]
    pub fn node(&self, i: usize) -> Option<NodeRef<'a>> {
        let off = self.node_offset(i)?;
        NodeRef::with_layout(self.buf.get(off..)?, self.layout)
    }

    /// `NODEKEY(NODEPTR(p, i))`: the key of node `i`; the same as
    /// `self.node(i)?.key()` (`None` in the same cases), without building
    /// the [`NodeRef`]. Used by binary searches.
    #[inline(always)]
    pub fn node_key(&self, i: usize) -> Option<&'a [u8]> {
        let off = self.node_offset(i)?;
        // The key size field ends where the node header does, so reading
        // it checks that the header lies within the page, as `node` does.
        let ksize = usize::from(get_u16(self.buf, off.checked_add(NODE_KSIZE)?)?);
        // NODEKEY: the key follows the node header in both formats.
        let start = off + NODE_SIZE;
        self.buf.get(start..start.checked_add(ksize)?)
    }

    /// `LEAF2KEY(p, i, ks)`: key `i` of a `P_LEAF2` page with `ksize`-byte keys.
    #[inline(always)]
    pub fn leaf2_key(&self, i: usize, ksize: usize) -> Option<&'a [u8]> {
        if i >= self.num_keys() {
            return None;
        }
        let start = self.l().page_hdr.checked_add(i.checked_mul(ksize)?)?;
        self.buf.get(start..start.checked_add(ksize)?)
    }

    /// The bytes from `LEAF2KEY(p, i, ks)` to the end of the page.
    #[inline(always)]
    pub fn leaf2_key_tail(&self, i: usize, ksize: usize) -> Option<&'a [u8]> {
        if i >= self.num_keys() {
            return None;
        }
        self.buf
            .get(self.l().page_hdr.checked_add(i.checked_mul(ksize)?)?..)
    }

    /// All keys of a `P_LEAF2` page as one contiguous slice
    /// (`NUMKEYS * ksize` bytes from `METADATA(p)`), as returned by
    /// `MDB_GET_MULTIPLE`.
    #[inline(always)]
    pub fn leaf2_keys(&self, ksize: usize) -> Option<&'a [u8]> {
        let len = self.num_keys().checked_mul(ksize)?;
        let start = self.l().page_hdr;
        self.buf.get(start..start.checked_add(len)?)
    }

    /// The data of an overflow run: `len` bytes right after the header of
    /// the first page (`METADATA(omp)`).
    #[inline(always)]
    pub fn overflow_data(&self, len: usize) -> Option<&'a [u8]> {
        let start = self.l().page_hdr;
        self.buf.get(start..start.checked_add(len)?)
    }
}

/// A mutable view of a page of a given [`Layout`], for building pages
/// (meta pages of a new environment, test fixtures).
#[derive(Debug)]
pub struct PageMut<'a> {
    buf: &'a mut [u8],
    layout: &'static Layout,
}

/// Data for [`PageMut::add_node`].
#[derive(Debug, Clone, Copy)]
pub enum NodeData<'d> {
    /// A branch node pointing at a child page.
    Child(u64),
    /// A leaf node with inline data (plain bytes, a sub-page or an `MDB_db`).
    Inline(&'d [u8]),
    /// A leaf node whose `data_len` bytes live on the overflow run starting
    /// at the given page (`F_BIGDATA` is set automatically). In the 1.0
    /// layout the `MDB_ovpage` gets `op_txnid` 0 and the run length the
    /// data needs on pages of the buffer's size.
    Overflow {
        /// First page of the overflow run.
        pgno: u64,
        /// Length of the data stored there.
        data_len: usize,
    },
}

impl<'a> PageMut<'a> {
    /// Wraps a page buffer in the LMDB 0.9 layout.
    ///
    /// # Panics
    /// If `buf` is shorter than the page header.
    pub fn new(buf: &'a mut [u8]) -> PageMut<'a> {
        PageMut::with_layout(buf, &V09)
    }

    /// Wraps a page buffer of the format `layout` describes.
    ///
    /// # Panics
    /// If `buf` is shorter than the page header.
    pub fn with_layout(buf: &'a mut [u8], layout: &'static Layout) -> PageMut<'a> {
        assert!(buf.len() >= layout.page_hdr, "page buffer too small");
        PageMut { buf, layout }
    }

    /// Read-only view of the same bytes.
    pub fn as_ref(&self) -> PageRef<'_> {
        PageRef {
            buf: self.buf,
            layout: self.layout,
        }
    }

    /// Initialises an empty branch/leaf page: header with `pgno` and
    /// `flags`, no nodes, all of the page free (as `mdb_page_new`: 0.9
    /// stores `mp_lower = 16`, `mp_upper = len`; 1.0 0 and `len - 24`).
    pub fn init(&mut self, pgno: u64, flags: u16) {
        let l = self.layout;
        let len = self.buf.len();
        self.buf[..l.page_hdr].fill(0);
        self.set_pgno(pgno);
        self.set_flags(flags);
        self.set_lower(l.empty_lower() as u16);
        self.set_upper((len - l.ptr_base) as u16);
    }

    /// Initialises the first page of an overflow run of `pages` pages.
    pub fn init_overflow(&mut self, pgno: u64, pages: u32) {
        let l = self.layout;
        self.buf[..l.page_hdr].fill(0);
        self.set_pgno(pgno);
        self.set_flags(P_OVERFLOW);
        put_u32(self.buf, l.pages_offset(), pages);
    }

    /// Sets `mp_pgno`.
    pub fn set_pgno(&mut self, pgno: u64) {
        put_u64(self.buf, 0, pgno);
    }

    /// Sets `mp_pad`.
    pub fn set_pad(&mut self, pad: u16) {
        put_u16(self.buf, self.layout.pad_offset(), pad);
    }

    /// Sets `mp_flags`.
    pub fn set_flags(&mut self, flags: u16) {
        put_u16(self.buf, self.layout.flags_offset(), flags);
    }

    /// Sets `mp_lower` (the stored, `PAGEBASE`-relative value).
    pub fn set_lower(&mut self, lower: u16) {
        put_u16(self.buf, self.layout.lower_offset(), lower);
    }

    /// Sets `mp_upper` (the stored, `PAGEBASE`-relative value).
    pub fn set_upper(&mut self, upper: u16) {
        put_u16(self.buf, self.layout.upper_offset(), upper);
    }

    /// Bytes after the header (`METADATA(p)`), e.g. for overflow data.
    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.layout.page_hdr..]
    }

    /// Inserts a node at index `indx`, laid out exactly as `mdb_node_add`
    /// does: pointers above `indx` move up one slot, the node goes at
    /// `mp_upper - EVEN(size)`, `mp_lower` grows by 2. `flags` are the node
    /// flags (`F_SUBDATA`, `F_DUPDATA`); `F_BIGDATA` is added for
    /// [`NodeData::Overflow`].
    ///
    /// Unlike `mdb_node_add` this never allocates overflow pages; the caller
    /// decides. Returns `Err(())` (`MDB_PAGE_FULL`) if the node does not fit.
    #[allow(clippy::result_unit_err)]
    pub fn add_node(
        &mut self,
        indx: usize,
        key: &[u8],
        data: NodeData<'_>,
        flags: u16,
    ) -> Result<(), ()> {
        let (nkeys, lower, upper, size_left) = {
            let page = self.as_ref();
            (
                page.num_keys(),
                page.lower(),
                page.upper(),
                page.size_left(),
            )
        };
        if indx > nkeys {
            return Err(());
        }
        let l = self.layout;
        let (body_len, flags) = match data {
            NodeData::Child(_) => (0, flags),
            NodeData::Inline(d) => (l.node_data_len(d.len()), flags),
            NodeData::Overflow { .. } => (l.ovpage_size, flags | super::node::F_BIGDATA),
        };
        let node_size = even(NODE_SIZE + l.node_key_len(key.len()) + body_len);
        let room = size_left as isize - 2;
        if node_size as isize > room {
            return Err(());
        }
        // Move higher pointers up one slot (stored values: no PAGEBASE).
        let ptrs = l.ptrs_offset();
        for i in (indx..nkeys).rev() {
            let p = get_u16(self.buf, ptrs + 2 * i).unwrap_or(0);
            put_u16(self.buf, ptrs + 2 * (i + 1), p);
        }
        let stored = usize::from(upper) - node_size;
        put_u16(self.buf, ptrs + 2 * indx, stored as u16);
        self.set_upper(stored as u16);
        self.set_lower(lower + 2);

        let ofs = stored + l.ptr_base;
        let psize = self.buf.len();
        let node = &mut self.buf[ofs..ofs + node_size];
        node.fill(0);
        put_u16(node, NODE_KSIZE, key.len() as u16);
        let (lo_hi, flags_word): (u32, u16) = match data {
            NodeData::Child(pgno) => (pgno as u32, (pgno >> 32) as u16),
            NodeData::Inline(d) => (d.len() as u32, flags),
            NodeData::Overflow { data_len, .. } => (data_len as u32, flags),
        };
        put_u16(node, NODE_LO, lo_hi as u16);
        put_u16(node, NODE_HI, (lo_hi >> 16) as u16);
        put_u16(node, NODE_FLAGS, flags_word);
        node[NODE_SIZE..NODE_SIZE + key.len()].copy_from_slice(key);
        let d = l.node_data_offset(key.len());
        match data {
            NodeData::Child(_) => {}
            NodeData::Inline(bytes) => node[d..d + bytes.len()].copy_from_slice(bytes),
            NodeData::Overflow { pgno, data_len } => {
                let r = OverflowRef {
                    pgno,
                    txnid: 0,
                    pages: l.overflow_pages_for(data_len, psize) as u64,
                };
                l.write_overflow_ref(&mut node[d..], r).ok_or(())?;
            }
        }
        Ok(())
    }

    /// Inserts a key into a `P_LEAF2` page at index `indx`, as the `IS_LEAF2`
    /// branch of `mdb_node_add` does (keys shift up, `mp_lower += 2`,
    /// `mp_upper -= ksize - 2`, modulo 2^16 as in C: `mp_upper` grows for
    /// 1-byte keys). The key size is `key.len()`.
    #[allow(clippy::result_unit_err)]
    pub fn add_leaf2_key(&mut self, indx: usize, key: &[u8]) -> Result<(), ()> {
        let (nkeys, lower, upper, size_left) = {
            let page = self.as_ref();
            (
                page.num_keys(),
                page.lower(),
                page.upper(),
                page.size_left(),
            )
        };
        let ksize = key.len();
        let ksize16 = u16::try_from(ksize).map_err(|_| ())?;
        if indx > nkeys || size_left < ksize {
            return Err(());
        }
        let hdr = self.layout.page_hdr;
        let start = hdr + indx * ksize;
        let end = hdr + nkeys * ksize;
        self.buf.copy_within(start..end, start + ksize);
        self.buf[start..start + ksize].copy_from_slice(key);
        self.set_lower(lower + 2);
        self.set_upper(upper.wrapping_sub(ksize16).wrapping_add(2));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::{PAGE_LOWER, PAGE_PTRS, V10};
    use super::super::node::{F_BIGDATA, F_DUPDATA, F_SUBDATA};
    use super::*;
    use crate::format::db::Db;

    const PS: usize = 4096;

    fn leaf(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(7, P_LEAF);
        for (i, (k, v)) in entries.iter().enumerate() {
            p.add_node(i, k, NodeData::Inline(v), 0).unwrap();
        }
        buf
    }

    #[test]
    fn leaf_page_nodes() {
        let buf = leaf(&[(b"a", b"1"), (b"bb", b"22"), (b"ccc", b"333")]);
        let p = PageRef::new(&buf).unwrap();
        assert_eq!(p.pgno(), 7);
        assert!(p.is_leaf() && !p.is_branch() && !p.is_leaf2());
        assert_eq!(p.num_keys(), 3);
        assert_eq!(p.lower(), 16 + 6);
        // Nodes are packed downwards from the end, each EVEN-sized.
        assert_eq!(p.node_offset(0), Some(PS - 10));
        assert_eq!(p.node_offset(1), Some(PS - 10 - 12));
        for (i, (k, v)) in [(&b"a"[..], &b"1"[..]), (b"bb", b"22"), (b"ccc", b"333")]
            .iter()
            .enumerate()
        {
            let n = p.node(i).unwrap();
            assert_eq!(n.key(), Some(*k));
            assert_eq!(n.inline_data(), Some(*v));
        }
        assert!(p.node(3).is_none());
    }

    #[test]
    fn insert_in_the_middle_keeps_pointer_order() {
        let mut buf = leaf(&[(b"a", b"1"), (b"c", b"3")]);
        PageMut::new(&mut buf)
            .add_node(1, b"b", NodeData::Inline(b"2"), 0)
            .unwrap();
        let p = PageRef::new(&buf).unwrap();
        let keys: Vec<_> = (0..3).map(|i| p.node(i).unwrap().key().unwrap()).collect();
        assert_eq!(keys, [b"a", b"b", b"c"]);
    }

    #[test]
    fn branch_page() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(3, P_BRANCH);
        p.add_node(0, b"", NodeData::Child(10), 0).unwrap();
        p.add_node(1, b"m", NodeData::Child(0x1234_5678_9abc), 0)
            .unwrap();
        let p = PageRef::new(&buf).unwrap();
        assert!(p.is_branch());
        assert_eq!(p.node(0).unwrap().child_pgno(), 10);
        assert_eq!(p.node(0).unwrap().key(), Some(&b""[..]));
        assert_eq!(p.node(1).unwrap().child_pgno(), 0x1234_5678_9abc);
        // Branch nodes have no data part: header + key, even-sized.
        assert_eq!(p.node_offset(0), Some(PS - 8));
        assert_eq!(p.node_offset(1), Some(PS - 8 - 10));
    }

    #[test]
    fn leaf2_page() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(9, P_LEAF | P_LEAF2);
        p.set_pad(4);
        p.add_leaf2_key(0, b"bbbb").unwrap();
        p.add_leaf2_key(0, b"aaaa").unwrap();
        p.add_leaf2_key(2, b"cccc").unwrap();
        let p = PageRef::new(&buf).unwrap();
        assert!(p.is_leaf2());
        assert_eq!(p.num_keys(), 3);
        assert_eq!(p.pad(), 4);
        assert_eq!(p.upper() as usize, PS - 3 * (4 - 2));
        assert_eq!(p.leaf2_key(0, 4), Some(&b"aaaa"[..]));
        assert_eq!(p.leaf2_key(2, 4), Some(&b"cccc"[..]));
        assert_eq!(p.leaf2_key(3, 4), None);
        assert_eq!(p.leaf2_keys(4), Some(&b"aaaabbbbcccc"[..]));
    }

    #[test]
    fn overflow_node_and_run() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(2, P_LEAF);
        p.add_node(
            0,
            b"big",
            NodeData::Overflow {
                pgno: 5,
                data_len: 10_000,
            },
            0,
        )
        .unwrap();
        let p = PageRef::new(&buf).unwrap();
        let n = p.node(0).unwrap();
        assert_eq!(n.flags(), F_BIGDATA);
        assert_eq!(n.data_size(), 10_000);
        assert_eq!(n.overflow_pgno(), Some(5));
        assert_eq!(n.raw(true).unwrap().len(), 8 + 3 + 8);

        let mut run = vec![0u8; 3 * PS];
        let mut o = PageMut::new(&mut run);
        o.init_overflow(5, 3);
        o.body_mut()[..10_000].fill(0xab);
        let o = PageRef::new(&run).unwrap();
        assert!(o.is_overflow());
        assert_eq!(o.overflow_pages(), 3);
        assert_eq!(o.overflow_data(10_000).unwrap().len(), 10_000);
        assert_eq!(o.overflow_data(3 * PS), None);
    }

    #[test]
    fn subdb_node() {
        let mut db = Db::empty(0x04);
        db.root = 42;
        db.entries = 3;
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(2, P_LEAF);
        p.add_node(0, b"named", NodeData::Inline(&db.to_bytes()), F_SUBDATA)
            .unwrap();
        let p = PageRef::new(&buf).unwrap();
        let n = p.node(0).unwrap();
        assert!(n.is_subdata() && !n.is_dupdata());
        assert_eq!(Db::read(n.inline_data().unwrap()), Some(db));
    }

    #[test]
    fn subpage_node() {
        // A P_SUBP sub-page of three duplicates, stored as node data.
        let mut sub = vec![0u8; 16 + 3 * (8 + 2 + 2)];
        let len = sub.len();
        let mut s = PageMut::new(&mut sub);
        s.init(2, P_LEAF | P_SUBP | P_DIRTY);
        for (i, d) in [b"x1", b"x2", b"x3"].iter().enumerate() {
            s.add_node(i, *d, NodeData::Inline(b""), 0).unwrap();
        }
        assert_eq!(s.as_ref().upper() as usize, len - 3 * 10);

        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(2, P_LEAF);
        p.add_node(0, b"key", NodeData::Inline(&sub), F_DUPDATA)
            .unwrap();
        let p = PageRef::new(&buf).unwrap();
        let n = p.node(0).unwrap();
        assert!(n.is_dupdata() && !n.is_subdata());
        let sp = PageRef::new(n.inline_data().unwrap()).unwrap();
        assert!(sp.is_subpage() && sp.is_leaf());
        assert_eq!(sp.num_keys(), 3);
        assert_eq!(sp.node(1).unwrap().key(), Some(&b"x2"[..]));
        assert_eq!(sp.node(2).unwrap().inline_data(), Some(&b""[..]));
    }

    #[test]
    fn corrupt_pointers_do_not_panic() {
        let mut buf = leaf(&[(b"a", b"1")]);
        // Point node 0 past the end of the page.
        buf[PAGE_PTRS..PAGE_PTRS + 2].copy_from_slice(&(PS as u16 - 2).to_ne_bytes());
        let p = PageRef::new(&buf).unwrap();
        assert!(p.node(0).is_none());
        // A huge mp_lower claims many keys; pointers beyond the buffer fail.
        let mut buf = vec![0u8; 64];
        buf[PAGE_LOWER..PAGE_LOWER + 2].copy_from_slice(&0xfff0u16.to_ne_bytes());
        let p = PageRef::new(&buf).unwrap();
        assert!(p.num_keys() > 1000);
        assert!(p.node(1000).is_none());
        assert!(p.leaf2_key(1000, 8).is_none());
        assert!(PageRef::new(&buf[..15]).is_none());
    }

    #[test]
    fn v10_leaf_page() {
        // PAGEBASE == PAGEHDRSZ == 24: lower, upper and the pointers are
        // relative to the end of the header; keys and data are padded.
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut buf, &V10);
        p.init(7, P_LEAF);
        assert_eq!(
            (p.as_ref().lower(), p.as_ref().upper()),
            (0, PS as u16 - 24)
        );
        p.add_node(0, b"a", NodeData::Inline(b"1"), 0).unwrap();
        p.add_node(1, b"bbb", NodeData::Inline(b"22"), 0).unwrap();
        let p = PageRef::with_layout(&buf, &V10).unwrap();
        assert_eq!(p.pgno(), 7);
        assert_eq!(p.txnid(), Some(0));
        assert!(p.is_leaf());
        assert_eq!(p.num_keys(), 2);
        assert_eq!(p.lower(), 4);
        // 8 + EVEN(1) + EVEN(1) = 12; 8 + EVEN(3) + EVEN(2) = 14.
        assert_eq!(p.node_offset(0), Some(PS - 12));
        assert_eq!(p.node_offset(1), Some(PS - 12 - 14));
        assert_eq!(
            get_u16(&buf, 24).map(usize::from),
            Some(PS - 12 - 24),
            "stored pointers are PAGEBASE-relative"
        );
        assert_eq!(p.upper() as usize, PS - 24 - 26);
        let n = p.node(1).unwrap();
        assert_eq!(n.key(), Some(&b"bbb"[..]));
        assert_eq!(n.inline_data(), Some(&b"22"[..]));
        assert_eq!(p.node_key(0), Some(&b"a"[..]));
        // The same bytes do not parse as 0.9 (mp_lower 4 < 16 means 0 keys).
        assert_eq!(PageRef::new(&buf).unwrap().num_keys(), 0);
        assert!(PageRef::with_layout(&buf[..23], &V10).is_none());
    }

    #[test]
    fn v10_overflow_node_and_leaf2() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut buf, &V10);
        p.init(2, P_LEAF);
        p.add_node(
            0,
            b"big",
            NodeData::Overflow {
                pgno: 5,
                data_len: 10_000,
            },
            0,
        )
        .unwrap();
        let p = PageRef::with_layout(&buf, &V10).unwrap();
        let n = p.node(0).unwrap();
        assert_eq!(n.overflow_pgno(), Some(5));
        assert_eq!(n.overflow_ref().unwrap().pages, 3);
        assert_eq!(p.node_offset(0), Some(PS - (8 + 4 + 24)));

        let mut run = vec![0u8; 3 * PS];
        let mut o = PageMut::with_layout(&mut run, &V10);
        o.init_overflow(5, 3);
        o.body_mut()[..4].copy_from_slice(b"data");
        let o = PageRef::with_layout(&run, &V10).unwrap();
        assert!(o.is_overflow());
        assert_eq!(o.overflow_pages(), 3);
        assert_eq!(o.overflow_data(4), Some(&b"data"[..]));
        assert_eq!(&run[24..28], b"data");

        let mut buf = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut buf, &V10);
        p.init(9, P_LEAF | P_LEAF2);
        p.set_pad(4);
        p.add_leaf2_key(0, b"bbbb").unwrap();
        p.add_leaf2_key(0, b"aaaa").unwrap();
        let p = PageRef::with_layout(&buf, &V10).unwrap();
        assert_eq!(p.pad(), 4);
        assert_eq!(p.num_keys(), 2);
        assert_eq!(p.leaf2_keys(4), Some(&b"aaaabbbb"[..]));
        assert_eq!(&buf[24..32], b"aaaabbbb");
    }

    #[cfg(target_endian = "big")]
    #[test]
    fn big_endian_node_word_order() {
        // On big-endian hosts mn_hi comes first.
        assert_eq!(NODE_HI, 0);
        assert_eq!(NODE_LO, 2);
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn little_endian_node_word_order() {
        assert_eq!(NODE_LO, 0);
        assert_eq!(NODE_HI, 2);
    }
}
