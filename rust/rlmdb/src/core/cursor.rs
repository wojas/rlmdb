//! Cursor internals: B-tree search and movement, ported from `mdb.c`.
//!
//! A `Cursor` is `MDB_cursor` without its sub-cursor: a stack of pages
//! from the root to a leaf (`mc_pg[]`), the index taken at each level
//! (`mc_ki[]`), and state flags. A `MDB_DUPSORT` database pairs the main
//! cursor with a second `Cursor` over the duplicates of the current key
//! (`MDB_xcursor`); duplicates live either in a *sub-page* stored inline in
//! the leaf node (`F_DUPDATA`) or in a *sub-DB*, a B-tree of its own whose
//! `MDB_db` record is the node's data (`F_DUPDATA|F_SUBDATA`). Keys of a
//! sub-DB or sub-page are the duplicate data items; with `MDB_DUPFIXED` they
//! are packed in `P_LEAF2` pages.
//!
//! The functions here are faithful ports, including C's quirks, because
//! cursor positioning after edge cases (end of data, failed `SET`, …) is
//! observable behaviour:
//!
//! | here | C |
//! |---|---|
//! | `Cursor::page_search` | `mdb_page_search`, `mdb_page_search_root` (line 5630) |
//! | `Cursor::node_search` | `mdb_node_search` (line 5422) |
//! | `Cursor::sibling` | `mdb_cursor_sibling` (line 5956) |
//! | `cursor_first`, `cursor_last` | `mdb_cursor_first`, `mdb_cursor_last` |
//! | `cursor_next`, `cursor_prev` | `mdb_cursor_next`, `mdb_cursor_prev` |
//! | `cursor_set` | `mdb_cursor_set` (line 6173) |
//! | `xcursor_init1` | `mdb_xcursor_init1` (line 7735) |
//! | `node_read` | `mdb_node_read` (overflow pages) |
//!
//! C passes `key`/`data` out-pointers that may be `NULL`; here each function
//! returns a `Kv`. For a sub-cursor the "key" is the duplicate data item and
//! the data part is empty, exactly like C's recursive calls with
//! `data == NULL`. Page lookups go through `TxnCore::page`, the single
//! point where a write transaction will later add its dirty pages.

use std::cmp::Ordering;

use crate::cmp::{cmp_memn, Comparator};
use crate::consts::*;
use crate::core::dbi::TxnDb;
use crate::core::txn::TxnCore;
use crate::error::{Error, MdbError, Result};
use crate::format::{Db, Format, Layout, NodeRef, PageRef};

pub use super::ro_cursor::{CursorOp, RoCursor};

/// `C_INITIALIZED`: the cursor is positioned.
pub(crate) const C_INITIALIZED: u8 = 0x01;
/// `C_EOF`: the cursor is past the last item (after `MDB_LAST`, or a move
/// off the end).
pub(crate) const C_EOF: u8 = 0x02;
/// `C_SUB`: this is the sub-cursor of a `MDB_DUPSORT` cursor.
pub(crate) const C_SUB: u8 = 0x04;
/// `C_DEL`: the last operation was a delete (write path).
pub(crate) const C_DEL: u8 = 0x08;

/// `MDB_PS_ROOTONLY`: stop at the root page.
pub(crate) const PS_ROOTONLY: u8 = 2;
/// `MDB_PS_FIRST`: descend to the first leaf.
pub(crate) const PS_FIRST: u8 = 4;
/// `MDB_PS_LAST`: descend to the last leaf.
pub(crate) const PS_LAST: u8 = 8;

/// Evaluates `$body` once per data-file format, with `$l` bound to that
/// format's `&'static Layout` (the static itself, not a pointer loaded at
/// run time), chosen by `$format`. Pages the body builds with `$l` then
/// have every layout number folded into a constant, as in a build that
/// knows only one layout; otherwise each access selects between the
/// formats' numbers, which made scans about 1 ns per item slower. For the
/// few functions that read pages once per item of a scan. (Not for
/// `node_search`: two copies of the binary search made point lookups
/// slower than the selects do.)
macro_rules! per_format {
    ($format:expr, |$l:ident| $body:expr) => {
        match $format {
            Format::V09 => {
                let $l: &'static Layout = &crate::format::layout::V09;
                $body
            }
            Format::V10 => {
                let $l: &'static Layout = &crate::format::layout::V10;
                $body
            }
        }
    };
}

/// A header-only page (of either layout: 24 bytes) used to fill unused
/// cursor stack slots. A constant, so that filling a cursor's stack with it
/// compiles to plain stores. An empty header has no keys.
const EMPTY_PAGE: &[u8] = &[0; 24];

/// See [`EMPTY_PAGE`].
pub(crate) fn empty_page() -> &'static [u8] {
    EMPTY_PAGE
}

pub(crate) fn not_found() -> Error {
    MdbError::NotFound.into()
}

pub(crate) fn corrupted() -> Error {
    MdbError::Corrupted.into()
}

/// A key/data pair at a cursor position. For a sub-cursor `key` is the
/// duplicate data item and `data` is empty.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Kv<'t> {
    pub(crate) key: &'t [u8],
    pub(crate) data: &'t [u8],
}

/// `MDB_cursor` without the sub-cursor.
///
/// `N` is the capacity of the page stack. Cursors that outlive a call
/// have C's [`CURSOR_STACK`] levels. Safe Rust initialises the whole
/// stack when a cursor is created, where C leaves `mc_pg` uninitialised;
/// for the temporary cursor of every `mdb_get` that was a measurable part
/// of a lookup in a small tree, so `TxnCore::get` uses a shorter stack
/// and only falls back to a full one for a deeper tree. A push beyond `N`
/// levels fails with `MDB_CURSOR_FULL`, as C's does beyond `CURSOR_STACK`.
///
/// The stack holds the bytes of each page, and the cursor the format they
/// all share (that of its environment), rather than a [`PageRef`] per
/// level: a `PageRef` is three words, and the larger stack made filling
/// and copying cursors measurably slower.
#[derive(Clone)]
pub(crate) struct Cursor<'t, const N: usize = CURSOR_STACK> {
    /// `mc_pg`: the page stack, root first (the bytes of each page).
    pg: [&'t [u8]; N],
    /// `mc_ki`: the index taken at each level.
    ki: [u16; N],
    /// `mc_snum`: number of pushed pages.
    snum: usize,
    /// `mc_top`: index of the top page, normally `snum - 1`.
    top: usize,
    /// `mc_flags`: `C_*` bits.
    pub(crate) flags: u8,
    /// `*mc_db`: the tree's record (for a sub-cursor, `mx_db`).
    pub(crate) db: Db,
    /// `mc_dbx->md_cmp`: key order (for a sub-cursor, the dup order).
    pub(crate) cmp: Comparator,
    /// For a sub-cursor over a `P_SUBP` sub-page: that page, which is the
    /// root of its one-page tree (C finds it through `mc_pg[0]`).
    subpage: Option<&'t [u8]>,
    /// The format of every page on the stack. Kept as the [`Format`] value
    /// rather than the `&'static Layout`, so that building a [`PageRef`]
    /// from the stack needs no load through a pointer: the layout is
    /// re-derived from it and its numbers fold into constants (see
    /// `PageRef::l`).
    format: Format,
}

impl<'t> Cursor<'t> {
    /// `mdb_cursor_init` / `mdb_xcursor_init0`: an unpositioned cursor
    /// with C's full stack, over pages of layout `layout`.
    pub(crate) fn new(db: Db, cmp: Comparator, flags: u8, layout: &'static Layout) -> Cursor<'t> {
        Cursor::unpositioned(db, cmp, flags, layout)
    }

    /// Loads the state of a write cursor (`wcursor`) into a cursor fresh
    /// from [`Cursor::new`]: its flags, the indices, `snum` and `top`, for
    /// a sub-cursor over a `P_SUBP` sub-page that sub-page, and `page(i)`
    /// as the page of each of the `snum` levels. Done in place: cursors are
    /// some 700 bytes, and the write path rebuilds them for every read
    /// operation, so moving them around is measurably slow.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn load(
        &mut self,
        flags: u8,
        ki: &[u16; CURSOR_STACK],
        snum: usize,
        top: usize,
        subpage: Option<PageRef<'t>>,
        mut page: impl FnMut(usize) -> Result<PageRef<'t>>,
    ) -> Result<()> {
        self.flags = flags;
        self.ki = *ki;
        self.snum = snum;
        self.top = top;
        self.subpage = subpage.map(|p| self.stack_bytes(p));
        for i in 0..snum.min(CURSOR_STACK) {
            let p = page(i)?;
            self.pg[i] = self.stack_bytes(p);
        }
        Ok(())
    }

    /// `mc_ki`.
    #[inline]
    pub(crate) fn ki_array(&self) -> &[u16; CURSOR_STACK] {
        &self.ki
    }
}

impl<'t, const N: usize> Cursor<'t, N> {
    /// `mdb_cursor_init` / `mdb_xcursor_init0`: an unpositioned cursor
    /// with a stack of `N` levels.
    #[inline]
    pub(crate) fn unpositioned(
        db: Db,
        cmp: Comparator,
        flags: u8,
        layout: &'static Layout,
    ) -> Cursor<'t, N> {
        Cursor {
            pg: [empty_page(); N],
            ki: [0; N],
            snum: 0,
            top: 0,
            flags,
            db,
            cmp,
            subpage: None,
            format: layout.format,
        }
    }

    /// A page on the stack as a [`PageRef`]. Every slice on the stack is
    /// either [`EMPTY_PAGE`] or came from a `PageRef` of this cursor's
    /// format (see [`Cursor::stack_bytes`]), so it holds a header.
    #[inline]
    fn page_from(&self, bytes: &'t [u8]) -> PageRef<'t> {
        PageRef::from_parts(bytes, self.format.layout())
    }

    /// The bytes of `mp` for the stack. All pages of a cursor come from
    /// one environment, so they share its format.
    #[inline]
    fn stack_bytes(&self, mp: PageRef<'t>) -> &'t [u8] {
        debug_assert_eq!(mp.layout().format, self.format);
        mp.bytes()
    }

    /// `mc_top`.
    #[inline]
    pub(crate) fn top_level(&self) -> usize {
        self.top
    }

    /// `mc_pg[i]`.
    #[inline]
    pub(crate) fn page_at(&self, i: usize) -> PageRef<'t> {
        self.page_from(self.pg[i])
    }

    /// The sub-page this sub-cursor walks, if any.
    #[inline]
    pub(crate) fn subpage(&self) -> Option<PageRef<'t>> {
        self.subpage.map(|b| self.page_from(b))
    }

    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        self.flags & C_INITIALIZED != 0
    }

    #[inline]
    pub(crate) fn top_page(&self) -> PageRef<'t> {
        self.page_from(self.pg[self.top])
    }

    /// [`Cursor::top_page`] read with `l`, the cursor's layout given as a
    /// constant (see `per_format!`).
    #[inline(always)]
    fn top_page_as(&self, l: &'static Layout) -> PageRef<'t> {
        debug_assert_eq!(l.format, self.format);
        PageRef::from_parts(self.pg[self.top], l)
    }

    #[inline]
    pub(crate) fn ki_top(&self) -> usize {
        usize::from(self.ki[self.top])
    }

    /// Sets `mc_ki[mc_top]` (an `indx_t`, i.e. truncated to 16 bits as in C).
    #[inline]
    pub(crate) fn set_ki_top(&mut self, i: usize) {
        self.ki[self.top] = i as u16;
    }

    /// `mc_snum`.
    #[inline]
    pub(crate) fn depth(&self) -> usize {
        self.snum
    }

    /// The `MDB_db.md_pad` LEAF2 key size of this tree.
    #[inline]
    fn leaf2_ksize(&self) -> usize {
        self.db.pad as usize
    }

    /// The node at the cursor position (`NODEPTR(mc_pg[top], mc_ki[top])`).
    pub(crate) fn current_node(&self) -> Result<NodeRef<'t>> {
        self.top_page().node(self.ki_top()).ok_or_else(corrupted)
    }

    /// The key at index `i` of `mp`, for `P_LEAF2` and node pages alike,
    /// plus the bytes from the key to the end of the page (C's integer
    /// comparators may read past a short stored key).
    fn key_at(&self, mp: PageRef<'t>, i: usize) -> Result<(&'t [u8], &'t [u8])> {
        let k = if mp.is_leaf2() {
            let ks = self.leaf2_ksize();
            mp.leaf2_key(i, ks).zip(mp.leaf2_key_tail(i, ks))
        } else {
            mp.node(i).and_then(|n| Some((n.key()?, n.key_tail())))
        };
        k.ok_or_else(corrupted)
    }

    /// Compares `key` with the stored key at index `i` of `mp`.
    fn cmp_at(&self, cmp: Comparator, key: &[u8], mp: PageRef<'t>, i: usize) -> Result<Ordering> {
        let (k, tail) = self.key_at(mp, i)?;
        Ok(cmp.compare_in_page(key, k, tail))
    }

    /// `mdb_cursor_push`.
    #[inline]
    pub(crate) fn push(&mut self, mp: PageRef<'t>) -> Result<()> {
        // Indexing with the level just checked against `N` lets the
        // compiler drop the bounds checks of the two stores.
        let level = self.snum;
        if level >= N {
            return Err(MdbError::CursorFull.into());
        }
        self.pg[level] = self.stack_bytes(mp);
        self.ki[level] = 0;
        self.top = level;
        self.snum = level + 1;
        Ok(())
    }

    /// `mdb_cursor_pop`.
    fn pop(&mut self) {
        if self.snum > 0 {
            self.snum -= 1;
            if self.snum > 0 {
                self.top -= 1;
            } else {
                self.flags &= !C_INITIALIZED;
            }
        }
    }

    /// `mdb_node_search`: binary search of `key` in the top page. Sets
    /// `mc_ki[top]` to the smallest entry `>= key` and returns
    /// `(found, exact)`: `found` is false if every entry is smaller.
    /// Branch pages start at index 1 (the first branch key is implicit).
    pub(crate) fn node_search(&mut self, key: &[u8]) -> Result<(bool, bool)> {
        let mp = self.top_page();
        let nkeys = mp.num_keys();
        let low: isize = if mp.is_leaf() { 0 } else { 1 };
        let high: isize = nkeys as isize - 1;
        // Branch pages have no data, so with integer keys alignment is
        // guaranteed and C uses the faster mdb_cmp_long/mdb_cmp_int, chosen
        // by the size of the second key on the page.
        let cmp = if self.cmp.is_cint() && mp.is_branch() {
            match mp.node(1).map(|n| n.key_size()) {
                Some(8) => Comparator::Long,
                _ => Comparator::Int,
            }
        } else {
            self.cmp
        };
        // The page layout and the comparator are resolved once per search,
        // not once per probe; the default order (memcmp) gets its own loop.
        let (mut i, rc) = match (mp.is_leaf2(), cmp) {
            (false, Comparator::Memn) => bsearch(low, high, |i| {
                Ok(cmp_memn(key, mp.node_key(i).ok_or_else(corrupted)?))
            })?,
            _ => bsearch(low, high, |i| self.cmp_at(cmp, key, mp, i))?,
        };
        if rc == Ordering::Greater {
            // Found entry is less than the key: skip to the next one.
            i += 1;
        }
        let exact = rc == Ordering::Equal && nkeys > 0;
        self.set_ki_top(i as usize);
        Ok(((i as usize) < nkeys, exact))
    }

    /// `mdb_page_search`: put the root on the stack and descend to the leaf
    /// for `key` (or the first/last leaf with `PS_FIRST`/`PS_LAST`).
    /// `MDB_NOTFOUND` for an empty tree.
    pub(crate) fn page_search(
        &mut self,
        txn: &'t TxnCore<'_>,
        key: Option<&[u8]>,
        flags: u8,
    ) -> Result<()> {
        let root = match self.subpage {
            Some(sp) => sp,
            None => {
                if self.db.root == P_INVALID {
                    return Err(not_found());
                }
                let root = txn.page(self.db.root)?;
                self.stack_bytes(root)
            }
        };
        self.pg[0] = root;
        self.snum = 1;
        self.top = 0;
        if flags & PS_ROOTONLY != 0 {
            return Ok(());
        }
        self.page_search_root(txn, key, flags)
    }

    /// `mdb_page_search_root`: descend from the page on top of the stack.
    pub(crate) fn page_search_root(
        &mut self,
        txn: &'t TxnCore<'_>,
        key: Option<&[u8]>,
        flags: u8,
    ) -> Result<()> {
        let mut mp = self.top_page();
        while mp.is_branch() {
            let nkeys = mp.num_keys();
            if nkeys == 0 {
                return Err(corrupted());
            }
            let mut i = 0;
            if flags & (PS_FIRST | PS_LAST) != 0 {
                if flags & PS_LAST != 0 {
                    i = nkeys - 1;
                    // If already initialised, see if we are already in the
                    // right place: the child we descended into last time is
                    // still on the stack (snapshots never change).
                    if self.is_initialized() && self.ki_top() == i {
                        if self.snum >= N {
                            return Err(MdbError::CursorFull.into());
                        }
                        self.top = self.snum;
                        self.snum += 1;
                        mp = self.top_page();
                        continue;
                    }
                }
            } else {
                let (found, exact) = self.node_search(key.unwrap_or(&[]))?;
                if !found {
                    i = nkeys - 1;
                } else {
                    i = self.ki_top();
                    if !exact {
                        if i == 0 {
                            return Err(corrupted());
                        }
                        i -= 1;
                    }
                }
            }
            let node = mp.node(i).ok_or_else(corrupted)?;
            let child = txn.page(node.child_pgno())?;
            self.set_ki_top(i);
            self.push(child)?;
            mp = child;
        }
        if !mp.is_leaf() {
            return Err(corrupted());
        }
        self.flags |= C_INITIALIZED;
        self.flags &= !C_EOF;
        Ok(())
    }

    /// `mdb_cursor_sibling`: replace the top page with its right (or left)
    /// neighbour, climbing as far as needed. `MDB_NOTFOUND` at the edge of
    /// the tree, leaving the stack as it was.
    pub(crate) fn sibling(&mut self, txn: &'t TxnCore<'_>, move_right: bool) -> Result<()> {
        if self.snum < 2 {
            return Err(not_found()); // root has no siblings
        }
        self.pop();
        let parent = self.top_page();
        let ki = self.ki_top();
        let at_edge = if move_right {
            ki + 1 >= parent.num_keys()
        } else {
            ki == 0
        };
        if at_edge {
            if let Err(e) = self.sibling(txn, move_right) {
                // Undo the pop before returning.
                self.top += 1;
                self.snum += 1;
                return Err(e);
            }
        } else if move_right {
            self.set_ki_top(ki + 1);
        } else {
            self.set_ki_top(ki - 1);
        }
        let child = self
            .current_node()
            .and_then(|node| txn.page(node.child_pgno()));
        let mp = match child {
            Ok(mp) => mp,
            Err(e) => {
                self.flags &= !(C_INITIALIZED | C_EOF);
                return Err(e);
            }
        };
        self.push(mp)?;
        if !move_right {
            self.set_ki_top(mp.num_keys().saturating_sub(1));
        }
        Ok(())
    }

    /// `MDB_GET_MULTIPLE`'s `fetchm`: all items of the sub-cursor's current
    /// `P_LEAF2` page, leaving the sub-cursor on the last of them.
    pub(crate) fn fetch_multiple(&mut self) -> Result<&'t [u8]> {
        let mp = self.top_page();
        if !mp.is_leaf2() {
            return Err(corrupted());
        }
        let data = mp.leaf2_keys(self.leaf2_ksize()).ok_or_else(corrupted)?;
        self.set_ki_top(mp.num_keys().saturating_sub(1));
        Ok(data)
    }
}

/// The binary search loop of `mdb_node_search` over the indices
/// `low..=high`: `probe(i)` compares the search key with entry `i`. Returns
/// the last index probed and its result (`Equal` at once on a match;
/// `(0, Equal)` if the range is empty).
#[inline(always)]
fn bsearch(
    mut low: isize,
    mut high: isize,
    mut probe: impl FnMut(usize) -> Result<Ordering>,
) -> Result<(isize, Ordering)> {
    let mut i: isize = 0;
    let mut rc = Ordering::Equal;
    while low <= high {
        i = (low + high) >> 1;
        rc = probe(i as usize)?;
        match rc {
            Ordering::Equal => break,
            Ordering::Greater => low = i + 1,
            Ordering::Less => high = i - 1,
        }
    }
    Ok((i, rc))
}

/// A sub-cursor for `entry` if it is a `MDB_DUPSORT` database
/// (`mdb_xcursor_init0`), else `None`.
pub(crate) fn new_xcursor<'t, const N: usize>(
    entry: &TxnDb,
    layout: &'static Layout,
) -> Option<Cursor<'t, N>> {
    if u32::from(entry.db.flags) & MDB_DUPSORT == 0 {
        return None;
    }
    // Until mdb_xcursor_init1 runs the sub-tree is empty. (C leaves mx_db
    // uninitialised and asserts if it is searched before that.)
    Some(Cursor::unpositioned(
        Db::empty(0),
        entry.dup_cmp.unwrap_or(Comparator::Memn),
        C_SUB,
        layout,
    ))
}

/// `mdb_node_read`: the data of a leaf node, following `F_BIGDATA` to its
/// overflow pages. Always inlined: called out of line, the three-word
/// [`NodeRef`] goes through memory, which costs a scan step about 1 ns.
#[inline(always)]
pub(crate) fn node_read<'t>(txn: &'t TxnCore<'_>, leaf: NodeRef<'t>) -> Result<&'t [u8]> {
    if !leaf.is_bigdata() {
        return leaf.inline_data().ok_or_else(corrupted);
    }
    overflow_read(txn, leaf.overflow_pgno(), leaf.data_size())
}

/// [`node_read`] of an `F_BIGDATA` node (its overflow page and data size).
/// Takes the two numbers rather than the [`NodeRef`]: a node is three
/// words and so passed through memory, which made the caller keep it on
/// its stack also on the common path.
#[inline(never)]
fn overflow_read<'t>(txn: &'t TxnCore<'_>, pgno: Option<u64>, len: usize) -> Result<&'t [u8]> {
    txn.overflow_data(pgno.ok_or_else(corrupted)?, len)
}

/// `mdb_xcursor_init1`: point the sub-cursor `mx` at the duplicates in
/// `node` (a sub-DB record or a sub-page).
pub(crate) fn xcursor_init1<'t, const N: usize>(
    mc: &Cursor<'t, N>,
    mx: &mut Cursor<'t, N>,
    node: NodeRef<'t>,
) -> Result<()> {
    let data = node.inline_data().ok_or_else(corrupted)?;
    if node.is_subdata() {
        mx.db = Db::read(data).ok_or_else(corrupted)?;
        mx.subpage = None;
        mx.pg[0] = empty_page();
        mx.snum = 0;
        mx.top = 0;
        mx.flags = C_SUB;
    } else {
        // A sub-page has the layout of the page it is stored in.
        let fp = PageRef::with_layout(data, node.layout()).ok_or_else(corrupted)?;
        mx.db = Db {
            pad: 0,
            flags: 0,
            depth: 1,
            branch_pages: 0,
            leaf_pages: 1,
            overflow_pages: 0,
            entries: fp.num_keys() as u64,
            root: fp.pgno(),
        };
        let fp_bytes = mx.stack_bytes(fp);
        mx.subpage = Some(fp_bytes);
        mx.snum = 1;
        mx.top = 0;
        mx.flags = C_INITIALIZED | C_SUB;
        mx.pg[0] = fp_bytes;
        mx.ki[0] = 0;
        let parent_flags = u32::from(mc.db.flags);
        if parent_flags & MDB_DUPFIXED != 0 {
            mx.db.flags = MDB_DUPFIXED as u16;
            mx.db.pad = u32::from(fp.pad());
            if parent_flags & MDB_INTEGERDUP != 0 {
                mx.db.flags |= MDB_INTEGERKEY as u16;
            }
        }
    }
    // `#if UINT_MAX < SIZE_MAX`: 8-byte integer duplicates are size_t's.
    if mx.cmp.is_int() && mx.db.pad == 8 {
        mx.cmp = Comparator::Clong;
    }
    Ok(())
}

/// Which duplicate to land on when entering a node with duplicates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DupPos {
    First,
    Last,
}

/// The common tail of first/last/next/prev: read the entry at the cursor
/// position; for a node with duplicates, set up the sub-cursor on its first
/// or last duplicate and return that as the data.
fn read_position<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    pos: DupPos,
) -> Result<Kv<'t>> {
    per_format!(mc.format, |l| read_position_in(txn, mc, mx, pos, l))
}

/// The body of [`read_position`], compiled once per format: `l` is the
/// cursor's layout as a constant.
#[inline(always)]
fn read_position_in<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    pos: DupPos,
    l: &'static Layout,
) -> Result<Kv<'t>> {
    let mp = mc.top_page_as(l);
    if !mp.is_leaf() {
        return Err(corrupted());
    }
    let ki = mc.ki_top();
    if mp.is_leaf2() {
        let key = mp.leaf2_key(ki, mc.leaf2_ksize()).ok_or_else(corrupted)?;
        return Ok(Kv { key, data: &[] });
    }
    let leaf = mp.node(ki).ok_or_else(corrupted)?;
    let data = if leaf.is_dupdata() {
        enter_dups(txn, mc, mx, leaf.bytes(), pos)?
    } else if mc.flags & C_SUB == 0 {
        node_read(txn, leaf)?
    } else {
        &[]
    };
    Ok(Kv {
        key: leaf.key().ok_or_else(corrupted)?,
        data,
    })
}

/// The duplicates branch of [`read_position`]: point the sub-cursor at
/// the duplicates in the leaf node at `leaf` (its bytes, on a page of `mc`)
/// and return the first or last of them. Out of line, so that the common
/// case, an item without duplicates, gets by with a smaller stack frame;
/// for the same reason it takes the node's bytes, not the three-word
/// [`NodeRef`] (see [`overflow_read`]).
#[inline(never)]
fn enter_dups<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    leaf: &'t [u8],
    pos: DupPos,
) -> Result<&'t [u8]> {
    let mx = mx.ok_or_else(corrupted)?;
    let leaf = NodeRef::with_layout(leaf, mc.format.layout()).ok_or_else(corrupted)?;
    xcursor_init1(mc, mx, leaf)?;
    let dup = match pos {
        DupPos::First => cursor_first(txn, mx, None)?,
        DupPos::Last => cursor_last(txn, mx, None)?,
    };
    Ok(dup.key)
}

fn reset_sub<const N: usize>(mx: &mut Option<&mut Cursor<'_, N>>) {
    if let Some(x) = mx.as_deref_mut() {
        x.flags &= !(C_INITIALIZED | C_EOF);
    }
}

/// `mdb_cursor_first`.
pub(crate) fn cursor_first<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
) -> Result<Kv<'t>> {
    reset_sub(&mut mx);
    if !mc.is_initialized() || mc.top > 0 {
        mc.page_search(txn, None, PS_FIRST)?;
    }
    mc.flags |= C_INITIALIZED;
    mc.flags &= !C_EOF;
    mc.set_ki_top(0);
    read_position(txn, mc, mx, DupPos::First)
}

/// `mdb_cursor_last`. Leaves `C_EOF` set, as C does.
pub(crate) fn cursor_last<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
) -> Result<Kv<'t>> {
    reset_sub(&mut mx);
    if !mc.is_initialized() || mc.top > 0 {
        mc.page_search(txn, None, PS_LAST)?;
    }
    let nkeys = mc.top_page().num_keys();
    if nkeys == 0 {
        return Err(corrupted());
    }
    mc.set_ki_top(nkeys - 1);
    mc.flags |= C_INITIALIZED | C_EOF;
    read_position(txn, mc, mx, DupPos::Last)
}

/// `mdb_cursor_last` as `mdb_cursor_put` uses it for `MDB_APPEND`, where the
/// tree may be empty: a new key whose first duplicate does not fit a node
/// goes straight to a new sub-DB whose root leaf has no items yet, and
/// `MDB_APPENDDUP` then appends to that sub-DB. C sets `mc_ki` to
/// `NUMKEYS - 1` (0xffff) there and compares the new item with a node read
/// out of bounds (`KNOWN_C_BUGS.md` #32; only reachable with LMDB 1.0's
/// small pages). Here an empty leaf yields `None` with the cursor before
/// its first slot (`mc_ki` wraps to 0 when the caller moves it on), which
/// is where C inserts whenever its garbage compares lower.
pub(crate) fn cursor_last_for_append<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
) -> Result<Option<Kv<'t>>> {
    reset_sub(&mut mx);
    if !mc.is_initialized() || mc.top > 0 {
        mc.page_search(txn, None, PS_LAST)?;
    }
    // The rest of `cursor_last` (not called: it would search again).
    let top = mc.top_page();
    let nkeys = top.num_keys();
    if nkeys == 0 {
        if !top.is_leaf() {
            return Err(corrupted());
        }
        mc.set_ki_top(usize::from(u16::MAX));
        mc.flags |= C_INITIALIZED | C_EOF;
        return Ok(None);
    }
    mc.set_ki_top(nkeys - 1);
    mc.flags |= C_INITIALIZED | C_EOF;
    read_position(txn, mc, mx, DupPos::Last).map(Some)
}

/// The common case of [`cursor_next`] and [`cursor_prev`] in one pass over
/// the page: the cursor is on a leaf of a tree without duplicates (neither
/// past its end nor after a delete), its neighbour on the same page
/// (`forward`: the next item, else the previous one) has its key and data
/// in the page, and the cursor moves there and returns it. Returns `None`,
/// with the cursor unchanged, in every other case, and the caller then
/// takes the general path from the same state, so the result is the same
/// either way.
///
/// When this succeeds the general path would have done the same: with
/// `C_EOF` and `C_DEL` clear and no `MDB_DUPSORT`, `mdb_cursor_next` and
/// `mdb_cursor_prev` move the index by one within the page if they can,
/// whatever the operation, and then read the item there
/// ([`read_position`]: a `P_LEAF2` key, a key without data on a
/// sub-cursor, or a node's key and inline data); the flags stay as they
/// were. The general path dispatches on the format, checks the stack
/// index and reads `NUMKEYS` once to move and again in `read_position`,
/// and checks the key and the data against the page separately; here each
/// happens once. (`l` is the cursor's layout as a constant, see
/// `per_format!`.)
#[inline(always)]
fn step_in_leaf<'t, const N: usize>(
    mc: &mut Cursor<'t, N>,
    forward: bool,
    l: &'static Layout,
) -> Option<Kv<'t>> {
    if mc.flags & (C_INITIALIZED | C_EOF | C_DEL) != C_INITIALIZED
        || u32::from(mc.db.flags) & MDB_DUPSORT != 0
    {
        return None;
    }
    let ki = if forward {
        mc.ki_top() + 1
    } else {
        mc.ki_top().checked_sub(1)?
    };
    let kv = item_in_leaf(mc, mc.top_page_as(l), ki)?;
    // `ki < NUMKEYS`, so it fits an `indx_t`.
    mc.set_ki_top(ki);
    Some(kv)
}

/// What [`read_position`] returns for item `ki` of `mp` (the top page of
/// `mc`), if that needs neither the sub-cursor nor overflow pages; `None`
/// if it does, and also if the general path would fail there (a branch
/// page, `ki` out of range, a node outside the page).
#[inline(always)]
fn item_in_leaf<'t, const N: usize>(
    mc: &Cursor<'t, N>,
    mp: PageRef<'t>,
    ki: usize,
) -> Option<Kv<'t>> {
    if !mp.is_leaf() {
        return None;
    }
    if mp.is_leaf2() {
        let key = mp.leaf2_key(ki, mc.leaf2_ksize())?;
        return Some(Kv { key, data: &[] });
    }
    let leaf = mp.node(ki)?;
    if leaf.is_dupdata() || leaf.is_bigdata() {
        return None;
    }
    if mc.flags & C_SUB != 0 {
        return Some(Kv {
            key: leaf.key()?,
            data: &[],
        });
    }
    let (key, data) = leaf.key_and_data()?;
    Some(Kv { key, data })
}

/// [`step_in_leaf`] forward: the next item if it is on the same page and
/// needs neither the sub-cursor nor overflow pages, else `None` with the
/// cursor unchanged (then [`cursor_next_any`] does the step).
#[inline(always)]
pub(crate) fn next_in_page<'t, const N: usize>(mc: &mut Cursor<'t, N>) -> Option<Kv<'t>> {
    per_format!(mc.format, |l| step_in_leaf(mc, true, l))
}

/// `mdb_cursor_next` for `op` in `Next`, `NextDup`, `NextNoDup`. Inlined
/// for the step within a page ([`next_in_page`]); everything else is
/// [`cursor_next_any`].
#[inline]
pub(crate) fn cursor_next<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    match next_in_page(mc) {
        Some(kv) => Ok(kv),
        None => cursor_next_any(txn, mc, mx, op),
    }
}

/// [`cursor_next`] in every case, including those [`next_in_page`]
/// leaves to it.
#[inline(never)]
pub(crate) fn cursor_next_any<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    per_format!(mc.format, |l| cursor_next_in(txn, mc, mx, op, l))
}

/// The body of [`cursor_next`], compiled once per format: `l` is the
/// cursor's layout as a constant.
#[inline(always)]
fn cursor_next_in<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
    l: &'static Layout,
) -> Result<Kv<'t>> {
    if mc.flags & C_DEL != 0 && op == CursorOp::NextDup {
        return Err(not_found());
    }
    if !mc.is_initialized() {
        return cursor_first(txn, mc, mx);
    }
    let mp = mc.top_page_as(l);
    if mc.flags & C_EOF != 0 {
        // C returns MDB_NOTFOUND at once for a cursor on the last item of
        // its page. In a write transaction the key may have got more
        // duplicates since, or the tree a page to the right (MDB_APPEND),
        // so look (KNOWN_C_BUGS.md #28; MDB_PREV_MULTIPLE also leaves a
        // sub-cursor so marked on an earlier page). Only a cursor past its
        // page's end stops here.
        if mc.ki_top() >= mp.num_keys() {
            return Err(not_found());
        }
        mc.flags ^= C_EOF;
    }

    if u32::from(mc.db.flags) & MDB_DUPSORT != 0 {
        // C reads NODEPTR(mp, ki) here without checking ki < NUMKEYS (the
        // cursor may sit past the end after a failed MDB_SET); we treat a
        // position past the end as an item without duplicates
        // (KNOWN_C_BUGS.md).
        match mp.node(mc.ki_top()) {
            Some(leaf) if leaf.is_dupdata() => {
                if op == CursorOp::Next || op == CursorOp::NextDup {
                    let x = mx.as_deref_mut().ok_or_else(corrupted)?;
                    let rc = cursor_next(txn, x, None, CursorOp::Next);
                    if op != CursorOp::Next || !matches!(&rc, Err(e) if e.not_found()) {
                        let dup = rc?;
                        return Ok(Kv {
                            key: leaf.key().ok_or_else(corrupted)?,
                            data: dup.key,
                        });
                    }
                }
            }
            _ => {
                reset_sub(&mut mx);
                if op == CursorOp::NextDup {
                    return Err(not_found());
                }
            }
        }
    }

    if mc.flags & C_DEL != 0 {
        mc.flags ^= C_DEL;
    } else if mc.ki_top() + 1 >= mp.num_keys() {
        if let Err(e) = mc.sibling(txn, true) {
            mc.flags |= C_EOF;
            return Err(e);
        }
    } else {
        mc.ki[mc.top] += 1;
    }
    read_position(txn, mc, mx, DupPos::First)
}

/// [`step_in_leaf`] backward: the previous item if it is on the same page
/// and needs neither the sub-cursor nor overflow pages, else `None` with
/// the cursor unchanged (then [`cursor_prev_any`] does the step).
#[inline(always)]
pub(crate) fn prev_in_page<'t, const N: usize>(mc: &mut Cursor<'t, N>) -> Option<Kv<'t>> {
    per_format!(mc.format, |l| step_in_leaf(mc, false, l))
}

/// `mdb_cursor_prev` for `op` in `Prev`, `PrevDup`, `PrevNoDup`. Inlined
/// for the step within a page ([`prev_in_page`]); everything else is
/// [`cursor_prev_any`].
#[inline]
pub(crate) fn cursor_prev<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    match prev_in_page(mc) {
        Some(kv) => Ok(kv),
        None => cursor_prev_any(txn, mc, mx, op),
    }
}

/// [`cursor_prev`] in every case, including those [`prev_in_page`]
/// leaves to it.
#[inline(never)]
pub(crate) fn cursor_prev_any<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    per_format!(mc.format, |l| cursor_prev_in(txn, mc, mx, op, l))
}

/// The body of [`cursor_prev`], compiled once per format: `l` is the
/// cursor's layout as a constant.
#[inline(always)]
fn cursor_prev_in<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
    l: &'static Layout,
) -> Result<Kv<'t>> {
    if !mc.is_initialized() {
        cursor_last(txn, mc, mx.as_deref_mut())?;
        mc.ki[mc.top] = mc.ki[mc.top].wrapping_add(1);
    }
    let mp = mc.top_page_as(l);
    if u32::from(mc.db.flags) & MDB_DUPSORT != 0 && mc.ki_top() < mp.num_keys() {
        let leaf = mp.node(mc.ki_top()).ok_or_else(corrupted)?;
        if leaf.is_dupdata() {
            if op == CursorOp::Prev || op == CursorOp::PrevDup {
                let x = mx.as_deref_mut().ok_or_else(corrupted)?;
                let rc = cursor_prev(txn, x, None, CursorOp::Prev);
                if op != CursorOp::Prev || !matches!(&rc, Err(e) if e.not_found()) {
                    let dup = rc?;
                    mc.flags &= !C_EOF;
                    return Ok(Kv {
                        key: leaf.key().ok_or_else(corrupted)?,
                        data: dup.key,
                    });
                }
            }
        } else {
            reset_sub(&mut mx);
            if op == CursorOp::PrevDup {
                return Err(not_found());
            }
        }
    }

    mc.flags &= !(C_EOF | C_DEL);
    if mc.ki_top() == 0 {
        mc.sibling(txn, false)?;
        let n = mc.top_page().num_keys();
        mc.set_ki_top(n.saturating_sub(1));
    } else {
        mc.ki[mc.top] -= 1;
    }
    read_position(txn, mc, mx, DupPos::Last)
}

/// `mdb_cursor_set` for `op` in `Set`, `SetKey`, `SetRange`, `GetBoth`,
/// `GetBothRange`. `want_exact` corresponds to C's non-NULL `exactp`
/// (everything but `SetRange`). Returns the position and whether the key
/// matched exactly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cursor_set<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
    dcmp: Option<Comparator>,
    key: &[u8],
    data: Option<&[u8]>,
    op: CursorOp,
    want_exact: bool,
) -> Result<(Kv<'t>, bool)> {
    #[derive(PartialEq)]
    enum Step {
        /// Search the tree from the root.
        Search,
        /// The top page is the right leaf: search within it.
        SearchLeaf,
        /// `mc_ki[top]` is the position.
        Positioned,
    }

    if key.is_empty() {
        return Err(MdbError::BadValSize.into());
    }
    reset_sub(&mut mx);

    let mut exact = false;
    let mut step = Step::Search;

    // See if we're already on the right page.
    if mc.is_initialized() {
        let mp = mc.top_page();
        let nkeys = mp.num_keys();
        if nkeys == 0 {
            mc.set_ki_top(0);
            return Err(not_found());
        }
        let rc = mc.cmp_at(mc.cmp, key, mp, 0)?;
        if rc == Ordering::Equal {
            // Probably happens rarely, but first node on the page was the
            // one we wanted.
            mc.set_ki_top(0);
            exact = true;
            step = Step::Positioned;
        } else if rc == Ordering::Greater {
            if nkeys > 1 {
                let rc = mc.cmp_at(mc.cmp, key, mp, nkeys - 1)?;
                if rc == Ordering::Equal {
                    // Last node was the one we wanted.
                    mc.set_ki_top(nkeys - 1);
                    exact = true;
                    step = Step::Positioned;
                } else if rc == Ordering::Less {
                    // This is definitely the right page, skip the tree search.
                    step = Step::SearchLeaf;
                    let ki = mc.ki_top();
                    if ki < nkeys && mc.cmp_at(mc.cmp, key, mp, ki)? == Ordering::Equal {
                        // Current node was the one we wanted.
                        exact = true;
                        step = Step::Positioned;
                    } else {
                        mc.flags &= !C_EOF;
                    }
                }
            }
            if step == Step::Search {
                // If any parents have right siblings, search; otherwise
                // there's nothing further.
                let has_right =
                    (0..mc.top).any(|i| usize::from(mc.ki[i]) + 1 < mc.page_at(i).num_keys());
                if !has_right {
                    mc.set_ki_top(nkeys);
                    return Err(not_found());
                }
            }
        }
        if step == Step::Search && mc.top == 0 {
            // There are no other pages.
            mc.set_ki_top(0);
            if op == CursorOp::SetRange && !want_exact {
                step = Step::Positioned;
            } else {
                return Err(not_found());
            }
        }
    }

    if step == Step::Search {
        mc.page_search(txn, Some(key), 0)?;
        step = Step::SearchLeaf;
    }
    if step == Step::SearchLeaf {
        let (found, ex) = mc.node_search(key)?;
        exact = ex;
        if want_exact && !exact {
            // MDB_SET specified and not an exact match.
            return Err(not_found());
        }
        if !found {
            // Inexact leaf not found: the key is past this page; the
            // position is the first entry of the next page.
            if let Err(e) = mc.sibling(txn, true) {
                mc.flags |= C_EOF;
                return Err(e);
            }
        }
    }

    // set1:
    mc.flags |= C_INITIALIZED;
    mc.flags &= !C_EOF;
    let mp = mc.top_page();
    let ki = mc.ki_top();
    if mp.is_leaf2() {
        let k = mp.leaf2_key(ki, mc.leaf2_ksize()).ok_or_else(corrupted)?;
        return Ok((Kv { key: k, data: &[] }, exact));
    }
    let leaf = mp.node(ki).ok_or_else(corrupted)?;
    let mut data_out: &'t [u8] = &[];
    if leaf.is_dupdata() {
        let x = mx.ok_or_else(corrupted)?;
        xcursor_init1(mc, x, leaf)?;
        if matches!(op, CursorOp::Set | CursorOp::SetKey | CursorOp::SetRange) {
            data_out = cursor_first(txn, x, None)?.key;
        } else {
            let d = data.ok_or_else(Error::einval)?;
            let (dup, _) = cursor_set(
                txn,
                x,
                None,
                None,
                d,
                None,
                CursorOp::SetRange,
                op == CursorOp::GetBoth,
            )?;
            data_out = dup.key;
        }
    } else if mc.flags & C_SUB == 0 {
        if matches!(op, CursorOp::GetBoth | CursorOp::GetBothRange) {
            let olddata = node_read(txn, leaf)?;
            let d = data.ok_or_else(Error::einval)?;
            let mut dcmp = dcmp.ok_or(MdbError::Incompatible)?;
            if dcmp.is_int() && olddata.len() == 8 {
                dcmp = Comparator::Clong;
            }
            let tail = if leaf.is_bigdata() {
                olddata
            } else {
                leaf.data_tail()
            };
            match dcmp.compare_in_page(d, olddata, tail) {
                Ordering::Equal => {}
                rc => {
                    if op == CursorOp::GetBoth || rc == Ordering::Greater {
                        return Err(not_found());
                    }
                }
            }
            data_out = olddata;
        } else {
            reset_sub(&mut mx);
            data_out = node_read(txn, leaf)?;
        }
    }
    let key_out = leaf.key().ok_or_else(corrupted)?;
    Ok((
        Kv {
            key: key_out,
            data: data_out,
        },
        exact,
    ))
}

/// `MDB_GET_CURRENT`.
pub(crate) fn cursor_get_current<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
) -> Result<Kv<'t>> {
    if !mc.is_initialized() {
        return Err(Error::einval());
    }
    let mp = mc.top_page();
    let nkeys = mp.num_keys();
    if nkeys == 0 || mc.ki_top() >= nkeys {
        mc.set_ki_top(nkeys);
        return Err(not_found());
    }
    if mp.is_leaf2() {
        let key = mp
            .leaf2_key(mc.ki_top(), mc.leaf2_ksize())
            .ok_or_else(corrupted)?;
        return Ok(Kv { key, data: &[] });
    }
    let leaf = mp.node(mc.ki_top()).ok_or_else(corrupted)?;
    let data = if leaf.is_dupdata() {
        let x = mx.ok_or_else(corrupted)?;
        let dup = cursor_get_current(txn, x, None)?;
        x.flags &= !C_DEL;
        dup.key
    } else if mc.flags & C_SUB == 0 {
        node_read(txn, leaf)?
    } else {
        &[]
    };
    Ok(Kv {
        key: leaf.key().ok_or_else(corrupted)?,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Environment;
    use crate::flags::EnvironmentFlags;
    use crate::format::layout::{V09, V10};
    use crate::format::node::{F_DUPDATA, F_SUBDATA};
    use crate::format::page::{NodeData, PageMut, P_BRANCH, P_LEAF, P_LEAF2};

    /// Size of the hand-built pages.
    const PS: usize = 512;

    /// A leaf page of `l` with plain items (odd and even key and data
    /// lengths, an empty key, empty data, a named-DB record), an
    /// `F_BIGDATA` item (pointing at a page that is not there) and an
    /// `F_DUPDATA` sub-page.
    fn leaf(l: &'static Layout) -> Vec<u8> {
        let mut sub = vec![0u8; l.page_hdr + 2 * 2 + 2 * 10];
        {
            let mut p = PageMut::with_layout(&mut sub, l);
            p.init(0, l.subpage_flags);
            p.add_node(0, b"d1", NodeData::Inline(b""), 0).unwrap();
            p.add_node(1, b"d2", NodeData::Inline(b""), 0).unwrap();
        }
        let db = Db::empty(0).to_bytes();
        let items: [(&[u8], NodeData<'_>, u16); 8] = [
            (b"a", NodeData::Inline(b"one"), 0),
            (b"bb", NodeData::Inline(b"four"), 0),
            (b"", NodeData::Inline(b"empty key"), 0),
            (b"ccc", NodeData::Inline(b""), 0),
            (
                b"dd",
                NodeData::Overflow {
                    pgno: 1 << 40,
                    data_len: 9,
                },
                0,
            ),
            (b"eee", NodeData::Inline(&sub), F_DUPDATA),
            (b"named", NodeData::Inline(&db), F_SUBDATA),
            (b"zz", NodeData::Inline(b"last"), 0),
        ];
        let mut page = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut page, l);
        p.init(7, P_LEAF);
        for (i, (k, d, f)) in items.into_iter().enumerate() {
            p.add_node(i, k, d, f).unwrap();
        }
        page
    }

    /// A `P_LEAF2` page of `l` with five 3-byte keys.
    fn leaf2(l: &'static Layout) -> Vec<u8> {
        let mut page = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut page, l);
        p.init(8, P_LEAF | P_LEAF2);
        p.set_pad(3);
        p.set_lower((l.empty_lower() + 2 * 5) as u16);
        p.body_mut()[..15].copy_from_slice(b"k00k01k02k03k04");
        page
    }

    /// The pages of [`leaf`] and [`leaf2`] and copies of them with one
    /// `u16` of the header, the pointer array or a node header replaced by
    /// a boundary value, or cut short.
    fn pages(l: &'static Layout) -> Vec<Vec<u8>> {
        let bogus = [
            0u16, 1, 2, 3, 7, 8, 15, 16, 17, 23, 24, 25, 0x22, 0x42, 0x100, 509, 511, 512, 513,
            0x7fff, 0xffff,
        ];
        let mut out = Vec::new();
        for page in [leaf(l), leaf2(l)] {
            let mp = PageRef::with_layout(&page, l).unwrap();
            let mut fields: Vec<usize> = (l.page_hdr - 8..l.page_hdr).step_by(2).collect();
            fields.extend((0..mp.num_keys()).map(|i| l.ptrs_offset() + 2 * i));
            if !mp.is_leaf2() {
                for i in 0..mp.num_keys() {
                    let off = mp.node_offset(i).unwrap();
                    fields.extend((0..8).step_by(2).map(|f| off + f));
                }
            }
            for &off in &fields {
                for v in bogus {
                    let mut p = page.clone();
                    p[off..off + 2].copy_from_slice(&v.to_ne_bytes());
                    out.push(p);
                }
            }
            let ptrs_end = l.ptrs_offset() + 2 * mp.num_keys();
            for cut in [
                l.page_hdr,
                l.page_hdr + 3,
                ptrs_end,
                ptrs_end + 1,
                300,
                PS - 1,
            ] {
                out.push(page[..cut].to_vec());
            }
            out.push(page);
        }
        // Not a leaf at all.
        let mut branch = leaf(l);
        PageMut::with_layout(&mut branch, l).set_flags(P_BRANCH);
        out.push(branch);
        out
    }

    /// The parts of a cursor's state a step can change.
    fn state<const N: usize>(mc: &Cursor<'_, N>) -> (u8, [u16; N], usize, usize) {
        (mc.flags, mc.ki, mc.snum, mc.top)
    }

    /// [`step_in_leaf`] either does what the general path does (same item,
    /// same cursor state afterwards) or leaves the cursor alone, on valid
    /// and damaged pages of both formats, from every cursor state.
    #[test]
    fn step_in_leaf_matches_the_general_path() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::builder()
            .flags(EnvironmentFlags::NO_SYNC)
            .open(dir.path())
            .unwrap();
        let txn = env.begin_ro_txn().unwrap();
        let core = txn.core();
        let cursor_flags = [
            0,
            C_INITIALIZED,
            C_INITIALIZED | C_EOF,
            C_INITIALIZED | C_DEL,
            C_INITIALIZED | C_SUB,
            C_INITIALIZED | C_SUB | C_EOF,
        ];
        let dbs = [
            (0, 3),
            (0, 0),
            (0, 1000),
            (MDB_DUPSORT as u16, 3),
            (MDB_DUPFIXED as u16, 3),
        ];
        let (mut fast_steps, mut checked) = (0, 0);
        for l in [&V09, &V10] {
            for page in pages(l) {
                let mp = PageRef::with_layout(&page, l).unwrap();
                for flags in cursor_flags {
                    for (db_flags, pad) in dbs {
                        for ki in 0..mp.num_keys().min(12) + 3 {
                            for forward in [true, false] {
                                let db = Db {
                                    flags: db_flags,
                                    pad,
                                    ..Db::empty(0)
                                };
                                let mut mc: Cursor<'_> = Cursor::new(db, Comparator::Memn, 0, l);
                                mc.push(mp).unwrap();
                                mc.flags = flags;
                                mc.set_ki_top(ki);
                                let mut fast = mc.clone();
                                let got =
                                    per_format!(l.format, |l| step_in_leaf(&mut fast, forward, l));
                                let Some(kv) = got else {
                                    assert_eq!(state(&fast), state(&mc), "declined, but moved");
                                    continue;
                                };
                                let mut slow = mc.clone();
                                let want = if forward {
                                    cursor_next_any(core, &mut slow, None, CursorOp::Next)
                                } else {
                                    cursor_prev_any(core, &mut slow, None, CursorOp::Prev)
                                };
                                let want =
                                    want.expect("the general path fails where the step does not");
                                assert_eq!((kv.key, kv.data), (want.key, want.data));
                                assert_eq!(state(&fast), state(&slow));
                                fast_steps += 1;
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
        // The step took over a good part of the cases, so the test is not
        // vacuous.
        assert!(fast_steps > 10_000, "{fast_steps} of {checked}");
    }
}
