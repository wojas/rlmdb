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
use crate::format::{Db, NodeRef, PageRef};

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

/// A header-only page used to fill unused cursor stack slots. A constant,
/// so that filling a cursor's stack with it compiles to plain stores.
const EMPTY_PAGE: PageRef<'static> = match PageRef::new(&[0; 16]) {
    Some(p) => p,
    // 16 bytes hold exactly a header, so `new` cannot fail.
    None => unreachable!(),
};

pub(crate) fn empty_page() -> PageRef<'static> {
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
#[derive(Clone)]
pub(crate) struct Cursor<'t, const N: usize = CURSOR_STACK> {
    /// `mc_pg`: the page stack, root first.
    pg: [PageRef<'t>; N],
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
    subpage: Option<PageRef<'t>>,
}

impl<'t> Cursor<'t> {
    /// `mdb_cursor_init` / `mdb_xcursor_init0`: an unpositioned cursor
    /// with C's full stack.
    pub(crate) fn new(db: Db, cmp: Comparator, flags: u8) -> Cursor<'t> {
        Cursor::unpositioned(db, cmp, flags)
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
        self.subpage = subpage;
        for (i, slot) in self.pg.iter_mut().enumerate().take(snum) {
            *slot = page(i)?;
        }
        Ok(())
    }

    /// `mc_pg[i]`.
    #[inline]
    pub(crate) fn page_at(&self, i: usize) -> PageRef<'t> {
        self.pg[i]
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
    pub(crate) fn unpositioned(db: Db, cmp: Comparator, flags: u8) -> Cursor<'t, N> {
        Cursor {
            pg: [empty_page(); N],
            ki: [0; N],
            snum: 0,
            top: 0,
            flags,
            db,
            cmp,
            subpage: None,
        }
    }

    /// `mc_top`.
    #[inline]
    pub(crate) fn top_level(&self) -> usize {
        self.top
    }

    /// The sub-page this sub-cursor walks, if any.
    #[inline]
    pub(crate) fn subpage(&self) -> Option<PageRef<'t>> {
        self.subpage
    }

    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        self.flags & C_INITIALIZED != 0
    }

    #[inline]
    pub(crate) fn top_page(&self) -> PageRef<'t> {
        self.pg[self.top]
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
        self.pg[level] = mp;
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
                txn.page(self.db.root)?
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
pub(crate) fn new_xcursor<'t, const N: usize>(entry: &TxnDb) -> Option<Cursor<'t, N>> {
    if u32::from(entry.db.flags) & MDB_DUPSORT == 0 {
        return None;
    }
    // Until mdb_xcursor_init1 runs the sub-tree is empty. (C leaves mx_db
    // uninitialised and asserts if it is searched before that.)
    Some(Cursor::unpositioned(
        Db::empty(0),
        entry.dup_cmp.unwrap_or(Comparator::Memn),
        C_SUB,
    ))
}

/// `mdb_node_read`: the data of a leaf node, following `F_BIGDATA` to its
/// overflow pages.
#[inline]
pub(crate) fn node_read<'t>(txn: &'t TxnCore<'_>, leaf: NodeRef<'t>) -> Result<&'t [u8]> {
    if !leaf.is_bigdata() {
        return leaf.inline_data().ok_or_else(corrupted);
    }
    overflow_read(txn, leaf)
}

/// [`node_read`] of an `F_BIGDATA` node.
#[inline(never)]
fn overflow_read<'t>(txn: &'t TxnCore<'_>, leaf: NodeRef<'t>) -> Result<&'t [u8]> {
    let pgno = leaf.overflow_pgno().ok_or_else(corrupted)?;
    txn.overflow_data(pgno, leaf.data_size())
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
        let fp = PageRef::new(data).ok_or_else(corrupted)?;
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
        mx.subpage = Some(fp);
        mx.snum = 1;
        mx.top = 0;
        mx.flags = C_INITIALIZED | C_SUB;
        mx.pg[0] = fp;
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
    let mp = mc.top_page();
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
        enter_dups(txn, mc, mx, leaf, pos)?
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
/// the duplicates in `leaf` and return the first or last of them. Out of
/// line, so that the common case, an item without duplicates, gets by with
/// a smaller stack frame.
#[inline(never)]
fn enter_dups<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &Cursor<'t, N>,
    mx: Option<&mut Cursor<'t, N>>,
    leaf: NodeRef<'t>,
    pos: DupPos,
) -> Result<&'t [u8]> {
    let mx = mx.ok_or_else(corrupted)?;
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

/// `mdb_cursor_next` for `op` in `Next`, `NextDup`, `NextNoDup`.
pub(crate) fn cursor_next<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    if mc.flags & C_DEL != 0 && op == CursorOp::NextDup {
        return Err(not_found());
    }
    if !mc.is_initialized() {
        return cursor_first(txn, mc, mx);
    }
    let mp = mc.top_page();
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

/// `mdb_cursor_prev` for `op` in `Prev`, `PrevDup`, `PrevNoDup`.
pub(crate) fn cursor_prev<'t, const N: usize>(
    txn: &'t TxnCore<'_>,
    mc: &mut Cursor<'t, N>,
    mut mx: Option<&mut Cursor<'t, N>>,
    op: CursorOp,
) -> Result<Kv<'t>> {
    if !mc.is_initialized() {
        cursor_last(txn, mc, mx.as_deref_mut())?;
        mc.ki[mc.top] = mc.ki[mc.top].wrapping_add(1);
    }
    let mp = mc.top_page();
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
                    (0..mc.top).any(|i| usize::from(mc.ki[i]) + 1 < mc.pg[i].num_keys());
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
