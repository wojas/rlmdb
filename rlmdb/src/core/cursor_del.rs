//! Deleting items and keeping the tree balanced: `mdb_cursor_del` (mdb.c
//! line 7257), `mdb_cursor_del0` (line 8631), `mdb_rebalance` (line 8447),
//! `mdb_page_merge` (line 8281), `mdb_node_move` (line 8052),
//! `mdb_del0` (line 8759) and `mdb_drop0` (line 10126).
//!
//! These are needed by the write path itself: `mdb_freelist_save` deletes
//! the free DB records it consumed. After a delete, a page that is less
//! than a quarter full (`FILL_THRESHOLD`, or with too few keys) borrows a
//! node from a neighbour that can spare one, or is merged into it; a
//! branch root left with a single child is replaced by that child. As
//! everywhere in the write path, all tracked cursors are adjusted exactly
//! as C adjusts them.

use crate::consts::*;
use crate::core::cursor::{
    corrupted, cursor_first, cursor_set, CursorOp, C_DEL, C_EOF, C_INITIALIZED, PS_FIRST,
};
use crate::core::cursor_put::{NodeVal, MDB_NOSPILL};
use crate::core::dbi::{DB_DIRTY, DB_STALE};
use crate::core::page_ops;
use crate::core::split::node_body;
use crate::core::txn_rw::{FailOnPanic, RwTxn, TXN_DIRTY, TXN_ERROR};
use crate::core::wcursor::{CRef, SUBPAGE};
use crate::error::{Error, MdbError, Result};
use crate::format::node::{F_DUPDATA, F_SUBDATA};
use crate::format::page::{P_LEAF2, P_SUBP};
use crate::format::{Db, PageRef};

/// `FILL_THRESHOLD`: pages emptier than 25% (in tenths of a percent) are
/// candidates for merging.
const FILL_THRESHOLD: usize = 250;

/// `PAGEFILL`: how full a page is, in tenths of a percent of the space
/// after the header (`me_pagespace`).
fn page_fill(ps: usize, p: &PageRef<'_>) -> usize {
    let space = p.layout().page_space(ps);
    1000 * space.saturating_sub(p.size_left()) / space
}

/// The key of the first item of a page (`LEAF2KEY(p, 0, ksize)` or
/// `NODEKEY(NODEPTR(p, 0))`), copied.
fn first_key(p: &PageRef<'_>, ksize: usize) -> Result<Vec<u8>> {
    let k = if p.is_leaf2() {
        p.leaf2_key(0, ksize)
    } else {
        p.node(0).and_then(|n| n.key())
    };
    Ok(k.ok_or_else(corrupted)?.to_vec())
}

impl<'env> RwTxn<'env> {
    /// `_mdb_cursor_del`: delete the item at `mc`; with `MDB_NODUPDATA` all
    /// duplicates of its key.
    pub(crate) fn del_at(&mut self, mc: CRef, flags: u32) -> Result<()> {
        let mut txn = FailOnPanic(self);
        let rc = txn.del_at0(mc, flags);
        if let Err(e) = &rc {
            txn.fail_unless_clean(e);
        }
        rc
    }

    fn del_at0(&mut self, mc: CRef, flags: u32) -> Result<()> {
        self.check_usable()?;
        if !self.cur(mc).is_init() {
            return Err(Error::einval());
        }
        let ki = self.cur(mc).ki_top();
        if ki >= self.top_page(mc)?.num_keys() {
            return Err(MdbError::NotFound.into());
        }
        if flags & MDB_NOSPILL == 0 {
            let depth = self.db_of(mc).depth;
            self.spill_check(mc, depth, None)?;
        }
        self.cursor_touch(mc)?;
        let (is_leaf, is_leaf2) = {
            let p = self.top_page(mc)?;
            (p.is_leaf(), p.is_leaf2())
        };
        if !is_leaf {
            return Err(MdbError::Corrupted.into());
        }
        if !is_leaf2 && self.del_node_payload(mc, flags)? {
            return Ok(());
        }
        // del_key:
        self.cursor_del0(mc)
    }

    /// The part of `mdb_cursor_del` before `del_key:` for a node page:
    /// delete duplicates and overflow pages. Returns `true` if the node
    /// itself stays (a duplicate was removed and others remain). Errors
    /// mark the transaction failed where C jumps to `fail:`; a failed
    /// delete in the sub-tree is returned as is, as C does.
    fn del_node_payload(&mut self, mc: CRef, flags: u32) -> Result<bool> {
        let fail = |txn: &mut Self, e: Error| -> Result<bool> {
            txn.w.flags |= TXN_ERROR;
            Err(e)
        };
        let ki = self.cur(mc).ki_top();
        let (nflags, ovpgno, leaf_db) = {
            let p = self.top_page(mc)?;
            let n = p.node(ki).ok_or_else(corrupted)?;
            let db = if n.is_subdata() {
                n.inline_data().and_then(Db::read)
            } else {
                None
            };
            (n.flags(), n.overflow_pgno(), db)
        };
        if nflags & F_DUPDATA != 0 {
            let mx = CRef::sub(mc.slot);
            if mc.sub || !self.w.cursors.slot(mc.slot).is_some_and(|s| s.x.is_some()) {
                // F_DUPDATA in a database without MDB_DUPSORT, or among
                // duplicates (C would follow a NULL sub-cursor).
                return fail(self, MdbError::Corrupted.into());
            }
            if flags & MDB_NODUPDATA != 0 {
                // C uses the sub-cursor's record as it is, which is stale if
                // the sub-cursor was never set up for this key (after a
                // failed MDB_SET/MDB_GET_BOTH): it would subtract another
                // key's count, or drop another key's sub-DB. Read the
                // record of this node instead (KNOWN_C_BUGS.md #14).
                self.xcursor_init1(mc)?;
                // cursor_del0 will subtract the final entry.
                let n = self.db_of(mx).entries;
                let db = self.db_mut(mc);
                db.entries = db.entries.wrapping_sub(n.wrapping_sub(1));
                self.cur_mut(mx).flags &= !C_INITIALIZED;
            } else {
                if nflags & F_SUBDATA == 0 {
                    let x = self.cur_mut(mx);
                    x.pg[0] = SUBPAGE;
                    x.subp = true;
                }
                // The sub-cursor keeps its position; make sure its record
                // is this key's (KNOWN_C_BUGS.md #14).
                self.refresh_xdb(mc)?;
                self.del_at(mx, MDB_NOSPILL)?;
                // If sub-DB still has entries, we're done.
                if self.db_of(mx).entries != 0 {
                    if nflags & F_SUBDATA != 0 {
                        // Update sub-DB info.
                        self.store_subdb_record(mc)?;
                    } else {
                        // Shrink fake page. (Other sub-cursors on this page
                        // find their sub-pages through their main cursor,
                        // so C's XCURSOR_REFRESH loop has nothing to do.)
                        let pgno = self.cur(mc).pg_top();
                        let l = self.layout();
                        let page = self.top_page_mut(mc)?;
                        page_ops::shrink_node(l, page, ki, pgno).ok_or_else(corrupted)?;
                    }
                    let db = self.db_mut(mc);
                    db.entries = db.entries.wrapping_sub(1);
                    self.share_xdb(mc);
                    return Ok(true);
                }
                self.cur_mut(mx).flags &= !C_INITIALIZED;
                // Otherwise fall thru and delete the sub-DB.
            }
            if nflags & F_SUBDATA != 0 {
                // Add all the child DB's pages to the free list.
                if let Err(e) = self.drop0(mx, false) {
                    return fail(self, e);
                }
                let tmp = leaf_db.ok_or_else(corrupted)?;
                let db = self.db_mut(mc);
                db.branch_pages = db.branch_pages.wrapping_sub(tmp.branch_pages);
                db.leaf_pages = db.leaf_pages.wrapping_sub(tmp.leaf_pages);
            }
        } else if (u32::from(nflags) ^ flags) & u32::from(F_SUBDATA) != 0 {
            // LMDB passes F_SUBDATA in 'flags' to delete a DB record.
            return fail(self, MdbError::Incompatible.into());
        }
        // Add overflow pages to free list.
        if nflags & crate::format::node::F_BIGDATA != 0 {
            let pg = ovpgno.ok_or_else(corrupted)?;
            if let Err(e) = self.ovpage_free(mc, pg) {
                return fail(self, e);
            }
        }
        Ok(false)
    }

    /// `mdb_cursor_del0`: remove the node at `mc`, adjust the other
    /// cursors, rebalance, and leave every cursor that was on the deleted
    /// item on its successor with `C_DEL` set.
    pub(crate) fn cursor_del0(&mut self, mc: CRef) -> Result<()> {
        let rc = self.cursor_del0_inner(mc);
        if rc.is_err() {
            self.w.flags |= TXN_ERROR;
        }
        rc
    }

    fn cursor_del0_inner(&mut self, mc: CRef) -> Result<()> {
        let (top, ki, snum, dbi, sub) = {
            let w = self.cur(mc);
            (w.top, w.ki[w.top], w.snum, w.dbi, w.is_sub())
        };
        let mp = self.page_id(mc, top);
        let pad = self.db_of(mc).pad as usize;
        self.node_del(mc, pad)?;
        let dupsort = u32::from(self.db_of(mc).flags) & MDB_DUPSORT != 0;
        {
            let db = self.db_mut(mc);
            db.entries = db.entries.wrapping_sub(1);
        }
        // Adjust other cursors pointing to mp.
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if !self.both_init(s, m3) || m3 == mc || self.cur(m3).snum < snum {
                continue;
            }
            if self.page_id(m3, top) != mp {
                continue;
            }
            let w3 = self.cur_mut(m3);
            if w3.ki[top] == ki {
                w3.flags |= C_DEL;
                if dupsort {
                    // Sub-cursor referred into dataset which is gone.
                    if let Some(x) = self.w.cursors.slot_mut(s).and_then(|sl| sl.x.as_mut()) {
                        x.c.flags &= !(C_INITIALIZED | C_EOF);
                    }
                }
                continue;
            } else if w3.ki[top] > ki {
                w3.ki[top] = w3.ki[top].wrapping_sub(1);
            }
        }
        self.rebalance(mc)?;

        // DB is totally empty now, just bail out. Other cursors adjustments
        // were already done by rebalance and aren't needed here.
        if self.cur(mc).snum == 0 {
            self.cur_mut(mc).flags |= C_EOF;
            return Ok(());
        }

        let top = self.cur(mc).top;
        let mp = self.page_id(mc, top);
        let nkeys = self.top_page(mc)?.num_keys();
        let mc_ki = self.cur(mc).ki[top];
        let snum = self.cur(mc).snum;
        // Adjust other cursors pointing to mp.
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if !self.both_init(s, m3) || self.cur(m3).snum < snum {
                continue;
            }
            if self.page_id(m3, top) != mp || self.cur(m3).ki[top] < mc_ki {
                continue;
            }
            // Did the delete put m3 on this node (it was on the deleted
            // one, or past the end of the page)?
            let mut moved = m3 == mc || self.cur(m3).flags & C_DEL != 0;
            // If m3 points past last node in page, find next sibling.
            if usize::from(self.cur(m3).ki[top]) >= nkeys {
                match self.read_op(m3, |txn, c, _| c.sibling(txn, true)) {
                    Ok(()) => moved = true,
                    Err(e) if e.not_found() => {
                        self.cur_mut(m3).flags |= C_EOF;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            // (C also skips cursors with C_EOF here; m3 is on an item now,
            // and C_EOF may be left from before, e.g. by an MDB_SET that
            // did not clear it: KNOWN_C_BUGS.md #28.)
            if !m3.sub && self.w.cursors.slot(s).is_some_and(|sl| sl.x.is_some()) {
                let nflags = {
                    let p = self.top_page(m3)?;
                    p.node(self.cur(m3).ki_top()).ok_or_else(corrupted)?.flags()
                };
                // If this node has dupdata, it may need to be reinited
                // because its data has moved. If the xcursor was not
                // initd it must be reinited. Else if node points to a
                // subDB, nothing is needed. Else (xcursor was initd, not a
                // subDB) its sub-page is found through m3 anyway.
                if nflags & F_DUPDATA != 0 {
                    let mx = CRef::sub(s);
                    // (A cursor the delete moved here starts at the first
                    // duplicate even if its sub-cursor still looks set up:
                    // the deleting cursor's can be left from an earlier
                    // key, KNOWN_C_BUGS.md #18.)
                    if moved || !self.cur(mx).is_init() {
                        self.xcursor_init1(m3)?;
                        self.read_op(mx, |txn, c, _| cursor_first(txn, c, None).map(|_| ()))?;
                    }
                }
                // Its next MDB_NEXT must not skip the node's first
                // duplicate. C marks the sub-cursor of every cursor at or
                // after the deleted node, so one that stayed on its item
                // returns that item again (KNOWN_C_BUGS.md #29).
                if moved {
                    self.cur_mut(CRef::sub(s)).flags |= C_DEL;
                }
            }
        }
        self.cur_mut(mc).flags |= C_DEL;
        Ok(())
    }

    /// `m2->mc_flags & m3->mc_flags & C_INITIALIZED`.
    fn both_init(&self, slot: usize, m3: CRef) -> bool {
        self.w.cursors.slot(slot).is_some_and(|s| s.main.is_init()) && self.cur(m3).is_init()
    }

    /// `mdb_rebalance`: after a delete at `mc`, keep its page at least a
    /// quarter full (branch pages: at least two keys).
    pub(crate) fn rebalance(&mut self, mc: CRef) -> Result<()> {
        let ps = self.psize();
        let (is_branch, fill, nkeys) = {
            let p = self.top_page(mc)?;
            (p.is_branch(), page_fill(ps, &p), p.num_keys())
        };
        let (minkeys, thresh) = if is_branch {
            (2, 1)
        } else {
            (1, FILL_THRESHOLD)
        };
        if fill >= thresh && nkeys >= minkeys {
            // No need to rebalance, above fill threshold.
            return Ok(());
        }
        if self.cur(mc).snum < 2 {
            return self.rebalance_root(mc);
        }

        // The parent (branch page) must have at least 2 pointers, otherwise
        // the tree is invalid.
        let top = self.cur(mc).top;
        let ptop = top - 1;
        if self.page_of(mc, ptop)?.num_keys() < 2 {
            return Err(MdbError::Corrupted.into());
        }

        // Leaf page fill factor is below the threshold. Try to move keys
        // from left or right neighbor, or merge with a neighbor page.
        let mn = self.copy_cursor(mc, false);
        let rc = self.rebalance_with(mc, mn, ptop, thresh, minkeys);
        self.close_slot(mn.slot);
        rc
    }

    fn rebalance_with(
        &mut self,
        mc: CRef,
        mn: CRef,
        ptop: usize,
        thresh: usize,
        minkeys: usize,
    ) -> Result<()> {
        let ps = self.psize();
        let top = self.cur(mc).top;
        let mut oldki = self.cur(mc).ki[top];
        let mc_ki_ptop = usize::from(self.cur(mc).ki[ptop]);
        let fromleft = mc_ki_ptop != 0;
        if !fromleft {
            // We're the leftmost leaf in our parent: read right neighbor.
            let nk = mc_ki_ptop + 1;
            let child = self
                .page_of(mc, ptop)?
                .node(nk)
                .ok_or_else(corrupted)?
                .child_pgno();
            self.core.page(child)?;
            let w = self.cur_mut(mn);
            w.ki[ptop] = nk as u16;
            w.pg[top] = child;
            w.ki[top] = 0;
            let n = self.top_page(mc)?.num_keys();
            self.cur_mut(mc).ki[top] = n as u16;
        } else {
            // There is at least one neighbor to the left.
            let nk = mc_ki_ptop - 1;
            let child = self
                .page_of(mc, ptop)?
                .node(nk)
                .ok_or_else(corrupted)?
                .child_pgno();
            let n = self.core.page(child)?.num_keys();
            let w = self.cur_mut(mn);
            w.ki[ptop] = nk as u16;
            w.pg[top] = child;
            w.ki[top] = n.wrapping_sub(1) as u16;
            self.cur_mut(mc).ki[top] = 0;
        }

        // If the neighbor page is above threshold and has enough keys, move
        // one key from it. Otherwise we should try to merge them. (A branch
        // page must never have less than 2 keys.)
        let (nfill, nkeys) = {
            let p = self.top_page(mn)?;
            (page_fill(ps, &p), p.num_keys())
        };
        let rc;
        if nfill >= thresh && nkeys > minkeys {
            rc = self.node_move(mn, mc, fromleft);
            if fromleft {
                // If we inserted on left, bump position up.
                oldki = oldki.wrapping_add(1);
            }
        } else {
            if !fromleft {
                rc = self.page_merge(mn, mc);
            } else {
                // indx_t arithmetic, as in C (a corrupt page can hold
                // more keys than fit).
                oldki = oldki.wrapping_add(nkeys as u16);
                let t = self.cur(mn).top;
                let add = self.cur(mc).ki[t].wrapping_add(1);
                let ki = &mut self.cur_mut(mn).ki[t];
                *ki = ki.wrapping_add(add);
                // We want rebalance to find mn when doing fixups.
                self.w
                    .cursors
                    .slot_mut(mn.slot)
                    .ok_or_else(corrupted)?
                    .tracked = true;
                rc = self.page_merge(mc, mn);
                if let Some(s) = self.w.cursors.slot_mut(mn.slot) {
                    s.tracked = false;
                }
                self.copy_state(mn, mc);
            }
            self.cur_mut(mc).flags &= !C_EOF;
        }
        let w = self.cur_mut(mc);
        let t = w.top;
        w.ki[t] = oldki;
        rc
    }

    /// Gives up the root page `mp` that `mdb_rebalance` removed from the
    /// tree of `mc`: 0.9 puts it on the transaction's free list, 1.0 calls
    /// `mdb_page_loose`, so a page this transaction dirtied is reused by
    /// its next allocation (`Layout::loosens_rebalanced_root`).
    fn free_rebalanced_root(&mut self, mc: CRef, mp: u64) -> Result<()> {
        if self.layout().loosens_rebalanced_root() {
            self.page_loose(mc, mp)
        } else {
            self.w.free_pgs.append(mp);
            Ok(())
        }
    }

    /// The root branch of `mdb_rebalance`: an empty root empties the tree;
    /// a branch root with one child is replaced by the child.
    fn rebalance_root(&mut self, mc: CRef) -> Result<()> {
        let (mp, subp) = {
            let w = self.cur(mc);
            (w.pg[0], w.subp)
        };
        let (is_subp, nkeys, is_branch, only_child) = {
            let p = self.page_of(mc, 0)?;
            (
                subp || p.flags() & P_SUBP != 0,
                p.num_keys(),
                p.is_branch(),
                p.node(0).map(|n| n.child_pgno()),
            )
        };
        if is_subp {
            // Can't rebalance a subpage, ignoring.
            return Ok(());
        }
        let (dbi, sub) = {
            let w = self.cur(mc);
            (w.dbi, w.is_sub())
        };
        if nkeys == 0 {
            // Tree is completely empty.
            let db = self.db_mut(mc);
            db.root = P_INVALID;
            db.depth = 0;
            db.leaf_pages = 0;
            self.free_rebalanced_root(mc, mp)?;
            // Adjust cursors pointing to mp.
            {
                let w = self.cur_mut(mc);
                w.snum = 0;
                w.top = 0;
                w.flags &= !C_INITIALIZED;
            }
            for s in 0..self.w.cursors.len() {
                let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                    continue;
                };
                let w3 = self.cur_mut(m3);
                if !w3.is_init() {
                    continue;
                }
                if w3.pg[0] == mp && !w3.subp {
                    w3.snum = 0;
                    w3.top = 0;
                    w3.flags &= !C_INITIALIZED;
                }
            }
        } else if is_branch && nkeys == 1 {
            // Collapsing root page.
            self.free_rebalanced_root(mc, mp)?;
            let root = only_child.ok_or_else(corrupted)?;
            self.core.page(root)?;
            let depth = {
                let db = self.db_mut(mc);
                db.root = root;
                db.depth = db.depth.wrapping_sub(1);
                db.branch_pages = db.branch_pages.wrapping_sub(1);
                usize::from(db.depth)
            };
            {
                let w = self.cur_mut(mc);
                w.pg[0] = root;
                w.ki[0] = w.ki[1];
                for i in 1..depth.min(CURSOR_STACK - 1) {
                    w.pg[i] = w.pg[i + 1];
                    w.ki[i] = w.ki[i + 1];
                }
            }
            // Adjust other cursors pointing to mp.
            for s in 0..self.w.cursors.len() {
                let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                    continue;
                };
                if m3 == mc {
                    continue;
                }
                let w3 = self.cur_mut(m3);
                if !w3.is_init() || w3.subp {
                    continue;
                }
                if w3.pg[0] == mp {
                    for i in 0..depth.min(CURSOR_STACK - 1) {
                        w3.pg[i] = w3.pg[i + 1];
                        w3.ki[i] = w3.ki[i + 1];
                    }
                    w3.snum -= 1;
                    w3.top -= 1;
                }
            }
        }
        // else: root page doesn't need rebalancing.
        Ok(())
    }

    /// `mdb_page_search_lowest`: from the branch page on top of `mc`,
    /// descend to the leftmost leaf below it.
    fn page_search_lowest(&mut self, mc: CRef) -> Result<()> {
        self.read_op(mc, |txn, c, _| {
            let node = c.top_page().node(0).ok_or_else(corrupted)?;
            let child = txn.page(node.child_pgno())?;
            c.set_ki_top(0);
            c.push(child)?;
            c.page_search_root(txn, None, PS_FIRST)
        })
    }

    /// The lowest key below `mc`'s current branch page, found with a
    /// temporary copy of the cursor (`mdb_page_search_lowest` on `mn`).
    fn lowest_key_below(&mut self, mc: CRef) -> Result<(Vec<u8>, CRef)> {
        let mn = self.copy_cursor(mc, false);
        if let Err(e) = self.page_search_lowest(mn) {
            self.close_slot(mn.slot);
            return Err(e);
        }
        let ksize = self.db_of(mn).pad as usize;
        let key = first_key(&self.top_page(mn)?, ksize);
        match key {
            Ok(k) => Ok((k, mn)),
            Err(e) => {
                self.close_slot(mn.slot);
                Err(e)
            }
        }
    }

    /// `mdb_page_merge`: move all nodes of `csrc`'s page to `cdst`'s page,
    /// unlink and free the source page, then rebalance the parent.
    pub(crate) fn page_merge(&mut self, csrc: CRef, cdst: CRef) -> Result<()> {
        let ps = self.psize();
        if self.cur(csrc).snum < 2 || self.cur(cdst).snum < 2 {
            return Err(MdbError::Corrupted.into()); // can't merge root page
        }
        // Mark dst as dirty.
        self.page_touch(cdst)?;

        let psrc = self.cur(csrc).pg_top();
        let src: Vec<u8> = self.top_page(csrc)?.bytes().to_vec();
        let srcp = PageRef::with_layout(&src, self.layout()).ok_or_else(corrupted)?;
        let pdst = self.cur(cdst).pg_top();
        let nkeys = self.top_page(cdst)?.num_keys();
        let mut j = nkeys;

        // Move all nodes from src to dst.
        if srcp.flags() & P_LEAF2 != 0 {
            let ksize = self.db_of(csrc).pad as usize;
            for i in 0..srcp.num_keys() {
                let key = srcp.leaf2_key(i, ksize).ok_or_else(corrupted)?;
                self.node_add(cdst, j, key, None, 0, 0)?;
                j += 1;
            }
        } else {
            for i in 0..srcp.num_keys() {
                let srcnode = srcp.node(i).ok_or_else(corrupted)?;
                let lowest;
                let key = if i == 0 && srcp.is_branch() {
                    // Must find the lowest key below src.
                    let (k, mn) = self.lowest_key_below(csrc)?;
                    self.close_slot(mn.slot);
                    lowest = k;
                    &lowest[..]
                } else {
                    srcnode.key().ok_or_else(corrupted)?
                };
                let data = if srcp.is_leaf() {
                    Some(NodeVal {
                        size: srcnode.data_size(),
                        bytes: node_body(&srcnode)?,
                    })
                } else {
                    None
                };
                self.node_add(
                    cdst,
                    j,
                    key,
                    data,
                    srcnode.child_pgno(),
                    u32::from(srcnode.flags()),
                )?;
                j += 1;
            }
        }

        // Unlink the src page from parent and add to free list.
        self.cur_mut(csrc).top -= 1;
        let rc = self.node_del(csrc, 0).and_then(|()| {
            if self.cur(csrc).ki_top() == 0 {
                self.update_key(csrc, &[])
            } else {
                Ok(())
            }
        });
        self.cur_mut(csrc).top += 1;
        rc?;

        // If not operating on FreeDB, allow this page to be reused in this
        // txn. Otherwise just add to free list.
        let src_is_leaf = srcp.is_leaf();
        self.page_loose(csrc, psrc)?;
        {
            let db = self.db_mut(csrc);
            if src_is_leaf {
                db.leaf_pages = db.leaf_pages.wrapping_sub(1);
            } else {
                db.branch_pages = db.branch_pages.wrapping_sub(1);
            }
        }
        // Adjust other cursors pointing to mp.
        let (dbi, sub, top) = {
            let w = self.cur(csrc);
            (w.dbi, w.is_sub(), w.top)
        };
        let (csrc_snum, csrc_pg_up, csrc_ki_up) = {
            let w = self.cur(csrc);
            (w.snum, w.pg[top - 1], w.ki[top - 1])
        };
        let cdst_ki_up = self.cur(cdst).ki[top - 1];
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if m3 == csrc {
                continue;
            }
            let w3 = self.cur_mut(m3);
            if w3.snum < csrc_snum || (w3.subp && top == 0) {
                continue;
            }
            if w3.pg[top] == psrc {
                w3.pg[top] = pdst;
                w3.ki[top] = w3.ki[top].wrapping_add(nkeys as u16);
                w3.ki[top - 1] = cdst_ki_up;
            } else if w3.pg[top - 1] == csrc_pg_up && w3.ki[top - 1] > csrc_ki_up {
                w3.ki[top - 1] = w3.ki[top - 1].wrapping_sub(1);
            }
        }
        let snum = self.cur(cdst).snum;
        let depth = self.db_of(cdst).depth;
        self.cur_mut(cdst).pop();
        let rc = self.rebalance(cdst);
        // Did the tree height change?
        let new_depth = self.db_of(cdst).depth;
        let snum = (snum as isize + isize::from(new_depth as i16 - depth as i16)) as usize;
        let w = self.cur_mut(cdst);
        w.snum = snum;
        w.top = snum.saturating_sub(1);
        let _ = ps;
        rc
    }

    /// `mdb_node_move`: move the node at `csrc` to `cdst`'s position in the
    /// neighbouring page, updating the separator keys in the parents.
    pub(crate) fn node_move(&mut self, csrc: CRef, cdst: CRef, fromleft: bool) -> Result<()> {
        // Mark src and dst as dirty.
        self.page_touch(csrc)?;
        self.page_touch(cdst)?;

        let ksize_pad = self.db_of(csrc).pad as usize;
        let key: Vec<u8>;
        let data: Option<(usize, Vec<u8>)>;
        let srcpg;
        let flags;
        let src_leaf2 = self.top_page(csrc)?.is_leaf2();
        if src_leaf2 {
            let p = self.top_page(csrc)?;
            key = p
                .leaf2_key(self.cur(csrc).ki_top(), ksize_pad)
                .ok_or_else(corrupted)?
                .to_vec();
            data = None;
            srcpg = 0;
            flags = 0;
        } else {
            let (src_is_branch, ki) = (self.top_page(csrc)?.is_branch(), self.cur(csrc).ki_top());
            let (nkey, ndata, pg, fl, is_leaf) = {
                let p = self.top_page(csrc)?;
                let n = p.node(ki).ok_or_else(corrupted)?;
                let d = if p.is_leaf() {
                    Some((n.data_size(), node_body(&n)?.to_vec()))
                } else {
                    None
                };
                (
                    n.key().ok_or_else(corrupted)?.to_vec(),
                    d,
                    n.child_pgno(),
                    n.flags(),
                    p.is_leaf(),
                )
            };
            srcpg = pg;
            flags = u32::from(fl);
            data = if is_leaf {
                ndata
            } else {
                Some((0, Vec::new()))
            };
            if ki == 0 && src_is_branch {
                // Must find the lowest key below src.
                let (snum, top) = (self.cur(csrc).snum, self.cur(csrc).top);
                self.page_search_lowest(csrc)?;
                let k = first_key(&self.top_page(csrc)?, ksize_pad)?;
                let w = self.cur_mut(csrc);
                w.snum = snum;
                w.top = top;
                key = k;
            } else {
                key = nkey;
            }
        }

        if self.top_page(cdst)?.is_branch() && self.cur(cdst).ki_top() == 0 {
            // Must find the lowest key below dst.
            let snum = self.cur(cdst).snum;
            let (bkey, mn) = self.lowest_key_below(cdst)?;
            {
                let w = self.cur_mut(mn);
                w.snum = snum;
                w.top = snum - 1;
                w.ki[snum - 1] = 0;
            }
            let rc = self.update_key(mn, &bkey);
            self.close_slot(mn.slot);
            rc?;
        }

        // Add the node to the destination page.
        let dst_ki = self.cur(cdst).ki_top();
        let dval = data
            .as_ref()
            .map(|(size, bytes)| NodeVal { size: *size, bytes });
        self.node_add(cdst, dst_ki, &key, dval, srcpg, flags)?;
        // Delete the node from the source page.
        self.node_del(csrc, key.len())?;

        self.node_move_fixups(csrc, cdst, fromleft)?;

        // Update the parent separators.
        if self.cur(csrc).ki_top() == 0 {
            let top = self.cur(csrc).top;
            if self.cur(csrc).ki[top - 1] != 0 {
                let k = first_key(&self.top_page(csrc)?, ksize_pad)?;
                self.update_separator(csrc, &k)?;
            }
            if self.top_page(csrc)?.is_branch() {
                let ix = self.cur(csrc).ki[top];
                self.cur_mut(csrc).ki[top] = 0;
                let rc = self.update_key(csrc, &[]);
                self.cur_mut(csrc).ki[top] = ix;
                rc?;
            }
        }
        if self.cur(cdst).ki_top() == 0 {
            let top = self.cur(cdst).top;
            if self.cur(cdst).ki[top - 1] != 0 {
                let ks = if src_leaf2 { key.len() } else { 0 };
                let k = first_key(&self.top_page(cdst)?, ks)?;
                self.update_separator(cdst, &k)?;
            }
            if self.top_page(cdst)?.is_branch() {
                let ix = self.cur(cdst).ki[top];
                self.cur_mut(cdst).ki[top] = 0;
                let rc = self.update_key(cdst, &[]);
                self.cur_mut(cdst).ki[top] = ix;
                rc?;
            }
        }
        Ok(())
    }

    /// Updates the separator key for `c`'s page in its parent, through a
    /// tracked copy one level up (`WITH_CURSOR_TRACKING(mn,
    /// mdb_update_key(&mn, &key))`).
    fn update_separator(&mut self, c: CRef, key: &[u8]) -> Result<()> {
        let mn = self.copy_cursor(c, true);
        {
            let w = self.cur_mut(mn);
            w.snum -= 1;
            w.top -= 1;
        }
        let rc = self.update_key(mn, key);
        self.close_slot(mn.slot);
        rc
    }

    /// The cursor fixups of `mdb_node_move`.
    fn node_move_fixups(&mut self, csrc: CRef, cdst: CRef, fromleft: bool) -> Result<()> {
        let (dbi, sub, top) = {
            let w = self.cur(csrc);
            (w.dbi, w.is_sub(), w.top)
        };
        let mps = self.cur(csrc).pg[top];
        let csrc_ki = self.cur(csrc).ki[top];
        let (cdst_pg, cdst_ki) = {
            let w = self.cur(cdst);
            (w.pg[w.top], w.ki[w.top])
        };
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if fromleft {
                // If we're adding on the left, bump others up.
                let mpd = self.cur(cdst).pg[top];
                let w3 = self.cur_mut(m3);
                if !w3.is_init() || w3.top < top || w3.subp {
                    continue;
                }
                if m3 != cdst && w3.pg[top] == mpd && w3.ki[top] >= cdst_ki {
                    w3.ki[top] = w3.ki[top].wrapping_add(1);
                }
                if m3 != csrc && w3.pg[top] == mps && w3.ki[top] == csrc_ki {
                    w3.pg[top] = cdst_pg;
                    w3.ki[top] = cdst_ki;
                    w3.ki[top - 1] = w3.ki[top - 1].wrapping_add(1);
                }
            } else {
                // Adding on the right, bump others down.
                if m3 == csrc {
                    continue;
                }
                let w3 = self.cur_mut(m3);
                if !w3.is_init() || w3.top < top || w3.subp {
                    continue;
                }
                if w3.pg[top] == mps {
                    if w3.ki[top] == 0 {
                        w3.pg[top] = cdst_pg;
                        w3.ki[top] = cdst_ki;
                        w3.ki[top - 1] = w3.ki[top - 1].wrapping_sub(1);
                    } else {
                        w3.ki[top] = w3.ki[top].wrapping_sub(1);
                    }
                }
            }
        }
        Ok(())
    }

    /// `mdb_del0`: delete `key` (and `data` for `DUP_SORT`, else all its
    /// items) through a temporary tracked cursor.
    pub(crate) fn del0(
        &mut self,
        dbi: usize,
        key: &[u8],
        data: Option<&[u8]>,
        flags: u32,
    ) -> Result<()> {
        let mut txn = FailOnPanic(self);
        let slot = txn.open_slot(dbi, false);
        let rc = txn.del0_with(CRef::main(slot), key, data, flags);
        txn.close_slot(slot);
        rc
    }

    fn del0_with(&mut self, mc: CRef, key: &[u8], data: Option<&[u8]>, flags: u32) -> Result<()> {
        let mut flags = flags;
        let op = if data.is_some() {
            CursorOp::GetBoth
        } else {
            flags |= MDB_NODUPDATA;
            CursorOp::Set
        };
        let dcmp = self.dcmp_of(mc);
        self.read_op(mc, |txn, c, mx| {
            cursor_set(txn, c, mx, dcmp, key, data, op, true).map(|_| ())
        })?;
        // Let page_split know about this cursor if needed: delete will
        // trigger a rebalance; if it needs to move a node from one page to
        // another, it will have to update the parent's separator key(s).
        // If the new sepkey is larger than the current one, the parent page
        // may run out of space, triggering a split. We need this cursor to
        // be consistent until the end of the rebalance.
        self.w
            .cursors
            .slot_mut(mc.slot)
            .ok_or_else(corrupted)?
            .tracked = true;
        let rc = self.del_at(mc, flags);
        if let Some(s) = self.w.cursors.slot_mut(mc.slot) {
            s.tracked = false;
        }
        rc
    }

    /// `mdb_drop` (mdb.c line 10219): free every page of `dbi` (including
    /// overflow pages and, for `MDB_DUPSORT`, the sub-DBs of its keys) and
    /// invalidate the cursors on it. With `del`, also delete the named DB's
    /// record from the main DB and close the handle (the main DB is only
    /// emptied); otherwise reset its record to an empty tree.
    ///
    /// The freed pages go on this transaction's free list, even those it
    /// dirtied itself (which C writes at commit anyway). The handle is
    /// closed for every transaction at once, as in C; if this is a nested
    /// transaction that aborts, it is reopened (KNOWN_C_BUGS.md #20).
    pub(crate) fn mdb_drop(&mut self, dbi: usize, del: bool) -> Result<()> {
        if dbi == MAIN_DBI as usize && self.layout().drops_named_dbs_with_main() {
            let main_flags = u32::from(self.core.dbs.borrow()[dbi].db.flags);
            // Named databases cannot coexist with these main-DB flags.
            if main_flags & (MDB_DUPSORT | MDB_INTEGERKEY) == 0 {
                return self.drop_main_with_named_dbs(del);
            }
        }
        FailOnPanic(self).mdb_drop0(dbi, del)
    }

    /// LMDB 1.0's `mdb_drop` of the main DB when it may record named
    /// databases (see [`Layout::drops_named_dbs_with_main`]): refuse while
    /// a named database is open (`MDB_DBIS_BUSY`); if the first record is a
    /// named database's, delete that database (`mdb_drop(txn, i, 1)`) and
    /// look at the first record again, until the main DB is empty or holds
    /// something else; then empty the main DB. As in C, only the first
    /// record is checked: when named databases are in use, the main DB
    /// holds nothing else.
    ///
    /// [`Layout::drops_named_dbs_with_main`]: crate::format::Layout::drops_named_dbs_with_main
    fn drop_main_with_named_dbs(&mut self, del: bool) -> Result<()> {
        if self.named_dbi_open() {
            return Err(MdbError::DbisBusy.into());
        }
        let main = crate::core::Dbi(MAIN_DBI);
        let id = self.open_cursor(main)?;
        let rc = self.drop_named_dbs(id);
        let rc = match rc {
            // Already empty: nothing to do, the record stays as it is.
            Ok(false) => Ok(()),
            Ok(true) => FailOnPanic(self).mdb_drop0(MAIN_DBI as usize, del),
            Err(e) => Err(e),
        };
        self.close_cursor(id);
        rc
    }

    /// The loop of [`Self::drop_main_with_named_dbs`] with the main-DB
    /// cursor `id`: `Ok(false)` if the main DB was empty to begin with,
    /// `Ok(true)` when it is time to empty it.
    fn drop_named_dbs(&mut self, id: crate::core::CursorId) -> Result<bool> {
        use crate::core::{CursorOp, DatabaseFlags};
        let mut key = match self.cursor_get(id, None, None, CursorOp::First)? {
            Some((k, _)) => k.to_vec(),
            None => return Ok(false),
        };
        if !self.cursor_is_db(id)? {
            // Not a named database: just a normal drop.
            return Ok(true);
        }
        loop {
            // mdb_dbi_open(txn, key.mv_data, 0, &i): the key as a C string
            // (1.0 stores the name with its NUL).
            let name = key.split(|&b| b == 0).next().unwrap_or_default();
            let db = match self.create_db_bytes(Some(name), DatabaseFlags::empty()) {
                Ok(db) => db,
                Err(e) if e.not_found() => return Ok(true),
                Err(e) => return Err(e),
            };
            self.mdb_drop(db.0 as usize, true)?;
            // C: any failure to find a first record ends the loop.
            match self.cursor_get(id, None, None, CursorOp::First) {
                Ok(Some((k, _))) => key = k.to_vec(),
                Ok(None) | Err(_) => return Ok(true),
            }
        }
    }

    /// Whether a named database is open in this transaction (C: a DBI
    /// `>= CORE_DBS` of the transaction with `DB_VALID`): opened by it, or
    /// open in the environment when it began and not yet looked at.
    fn named_dbi_open(&self) -> bool {
        let dbs = self.core.dbs.borrow();
        let slots = self.core.env.dbis.read();
        (CORE_DBS as usize..dbs.len()).any(|i| {
            let state = dbs[i].state;
            if state != 0 {
                state & crate::core::dbi::DB_VALID != 0
            } else {
                slots.get(i).is_some_and(|s| s.valid)
            }
        })
    }

    fn mdb_drop0(&mut self, dbi: usize, del: bool) -> Result<()> {
        let slot = self.open_slot(dbi, true);
        let mc = CRef::main(slot);
        let subs = u32::from(self.db_of(mc).flags) & MDB_DUPSORT != 0;
        let rc = self.drop0(mc, subs);
        // Invalidate the dropped DB's cursors.
        for i in 0..self.w.cursors.len() {
            if let Some(m2) = self.w.cursors.peer(i, dbi, false) {
                self.cur_mut(m2).flags &= !(C_INITIALIZED | C_EOF);
            }
        }
        let rc = rc.and_then(|()| {
            // Can't delete the main DB.
            if del && dbi >= CORE_DBS as usize {
                let name = self.core.env.dbis.name_of(dbi).ok_or(MdbError::BadDbi)?;
                if let Err(e) = self.del0(MAIN_DBI as usize, &name, None, u32::from(F_SUBDATA)) {
                    self.w.flags |= TXN_ERROR;
                    return Err(e);
                }
                self.core.dbs.get_mut()[dbi].state = DB_STALE;
                let seq = self.core.env.dbis.seq_of(dbi);
                if let Some(slot) = self.core.env.dbis.read().get(dbi).cloned() {
                    self.w.dropped.push((dbi, slot, seq));
                }
                self.core.env.dbis.close(crate::core::Dbi(dbi as u32));
            } else {
                // Reset the DB record, mark it dirty.
                let e = &mut self.core.dbs.get_mut()[dbi];
                e.state |= DB_DIRTY;
                e.db.depth = 0;
                e.db.branch_pages = 0;
                e.db.leaf_pages = 0;
                e.db.overflow_pages = 0;
                e.db.entries = 0;
                e.db.root = P_INVALID;
                self.w.flags |= TXN_DIRTY;
            }
            Ok(())
        });
        self.close_slot(slot);
        rc
    }

    /// `mdb_drop0`: add all pages of the (sub-)DB under `mc` to the free
    /// list (with `subs`, also the sub-DBs of its `F_SUBDATA` nodes).
    pub(crate) fn drop0(&mut self, mc: CRef, subs: bool) -> Result<()> {
        let rc = self.read_op(mc, |txn, c, _| c.page_search(txn, None, PS_FIRST));
        match rc {
            Err(e) if e.not_found() => {
                self.cur_mut(mc).flags &= !C_INITIALIZED;
                return Ok(());
            }
            Err(e) => {
                self.cur_mut(mc).flags &= !C_INITIALIZED;
                return Err(e);
            }
            Ok(()) => {}
        }
        let rc = self.drop0_walk(mc, subs);
        if rc.is_err() {
            self.w.flags |= TXN_ERROR;
        }
        self.cur_mut(mc).flags &= !C_INITIALIZED;
        rc
    }

    fn drop0_walk(&mut self, mc: CRef, subs: bool) -> Result<()> {
        // DUPSORT sub-DBs have no ovpages/DBs. Omit scanning leaves. This
        // also avoids any P_LEAF2 pages, which have no nodes. Also if the DB
        // doesn't have sub-DBs and has no overflow pages, omit scanning
        // leaves.
        if self.cur(mc).is_sub() || (!subs && self.db_of(mc).overflow_pages == 0) {
            self.cur_mut(mc).pop();
        }
        let mx = self.cur(mc).clone();
        while self.cur(mc).snum > 0 {
            let top = self.cur(mc).top;
            let (is_leaf, n) = {
                let p = self.top_page(mc)?;
                (p.is_leaf(), p.num_keys())
            };
            let mut i = 0;
            let mut pop = false;
            if is_leaf {
                while i < n {
                    let (nflags, ov) = {
                        let p = self.top_page(mc)?;
                        let node = p.node(i).ok_or_else(corrupted)?;
                        (node.flags(), node.overflow_ref())
                    };
                    if nflags & crate::format::node::F_BIGDATA != 0 {
                        let ov = ov.ok_or_else(corrupted)?;
                        let pg = ov.pgno;
                        let pages = self.dropped_run_pages(ov)?;
                        self.w.free_pgs.append_range(pg, pages);
                        let db = self.db_mut(mc);
                        db.overflow_pages = db.overflow_pages.wrapping_sub(pages as u64);
                        if db.overflow_pages == 0 && !subs {
                            break;
                        }
                    } else if subs && nflags & F_SUBDATA != 0 {
                        let ki = self.cur(mc).ki[top];
                        self.cur_mut(mc).ki[top] = i as u16;
                        self.xcursor_init1(mc)?;
                        self.cur_mut(mc).ki[top] = ki;
                        self.drop0(CRef::sub(mc.slot), false)?;
                    }
                    i += 1;
                }
                if !subs && self.db_of(mc).overflow_pages == 0 {
                    pop = true;
                }
            } else {
                for i2 in 0..n {
                    let pg = self
                        .top_page(mc)?
                        .node(i2)
                        .ok_or_else(corrupted)?
                        .child_pgno();
                    self.w.free_pgs.append(pg);
                }
                i = n;
            }
            if !pop {
                if top == 0 {
                    break;
                }
                self.cur_mut(mc).ki[top] = i as u16;
                match self.read_op(mc, |txn, c, _| c.sibling(txn, true)) {
                    Ok(()) => continue,
                    Err(e) if e.not_found() => pop = true,
                    Err(e) => return Err(e),
                }
            }
            if pop {
                // No more siblings, go back to beginning of previous level.
                let w = self.cur_mut(mc);
                w.pop();
                w.ki[0] = 0;
                for i in 1..w.snum {
                    w.ki[i] = 0;
                    w.pg[i] = mx.pg[i];
                }
            }
        }
        // Free it.
        let root = self.db_of(mc).root;
        self.w.free_pgs.append(root);
        Ok(())
    }
}
