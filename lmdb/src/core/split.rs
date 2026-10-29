//! `mdb_page_split` (mdb.c line 8813): split a full page and insert a new
//! node.
//!
//! The cursor `mc` points at the full page and the index where the new node
//! belongs. A new right sibling `rp` is allocated (and a new root above the
//! page if it was the root). Then:
//!
//! * with `MDB_APPEND`, the new node alone goes to `rp`;
//! * on `P_LEAF2` pages the upper half of the keys moves to `rp`;
//! * otherwise the split point is chosen as C does it (the middle, adjusted
//!   by node sizes when nodes are large or few, and biased so sequential
//!   inserts leave the old page full), and the nodes are redistributed
//!   through a temporary copy of the page, the new node in its place.
//!
//! The first key of `rp` (the separator) is inserted into the parent,
//! splitting the parent recursively if it is full; during that recursion a
//! copy of the cursor is tracked so that the nested split's fixups adjust
//! it (`WITH_CURSOR_TRACKING`). Finally every other cursor on the split
//! page is moved to wherever its node went.

use crate::consts::*;
use crate::core::cursor::{corrupted, C_INITIALIZED};
use crate::core::cursor_put::{branch_size, leaf_size, NodeVal, SPLIT_REPLACE};
use crate::core::page_ops;
use crate::core::txn_rw::{RwTxn, TXN_ERROR};
use crate::core::wcursor::CRef;
use crate::error::Result;
use crate::format::layout::{NODE_SIZE, PAGE_HDR_SIZE, PAGE_PAD};
use crate::format::node::F_BIGDATA;
use crate::format::page::{P_BRANCH, P_LEAF, P_LEAF2};
use crate::format::{put_u16, NodeRef, PageRef};

impl<'env> RwTxn<'env> {
    /// `mdb_page_split`: insert `newkey` (with `newdata`, or child `newpgno`
    /// for a branch page) at `mc`'s position in its full top page.
    pub(crate) fn page_split(
        &mut self,
        mc: CRef,
        newkey: &[u8],
        newdata: Option<NodeVal<'_>>,
        newpgno: u64,
        nflags: u32,
    ) -> Result<()> {
        let mut mn = None;
        let rc = self.page_split0(mc, newkey, newdata, newpgno, nflags, &mut mn);
        if let Some(mn) = mn {
            self.close_slot(mn);
        }
        if rc.is_err() {
            self.w.flags |= TXN_ERROR;
        }
        rc
    }

    #[allow(clippy::too_many_arguments)]
    fn page_split0(
        &mut self,
        mc: CRef,
        newkey: &[u8],
        newdata: Option<NodeVal<'_>>,
        newpgno: u64,
        nflags: u32,
        mn_slot: &mut Option<usize>,
    ) -> Result<()> {
        let ps = self.psize();
        let nodemax = self.core.env.node_max();
        let mp = self.cur(mc).pg_top();
        let newindx = self.cur(mc).ki_top();
        let (mp_flags, mp_pad, mut nkeys) = {
            let p = self.top_page(mc)?;
            (p.flags(), p.pad(), p.num_keys())
        };
        let is_leaf = mp_flags & P_LEAF != 0;
        let is_leaf2 = mp_flags & P_LEAF2 != 0;

        // Create a right sibling.
        let rp = self.page_new(mc, mp_flags, 1)?;
        put_u16(self.dirty_mut(rp)?, PAGE_PAD, mp_pad);

        // Usually when splitting the root page, the cursor height is 1. But
        // when called from update_key, the cursor height may be greater
        // because it walks up the stack while finding the branch slot to
        // update.
        let mut new_root = None;
        let mut ptop;
        if self.cur(mc).top < 1 {
            let pp = self.page_new(mc, P_BRANCH, 1)?;
            // Shift current top to make room for new parent.
            {
                let w = self.cur_mut(mc);
                for i in (1..=w.snum.min(CURSOR_STACK - 1)).rev() {
                    w.pg[i] = w.pg[i - 1];
                    w.ki[i] = w.ki[i - 1];
                }
                w.pg[0] = pp;
                w.ki[0] = 0;
            }
            let db = self.db_mut(mc);
            db.root = pp;
            new_root = Some(usize::from(db.depth));
            db.depth += 1;
            // Add left (implicit) pointer.
            if let Err(e) = self.node_add(mc, 0, &[], None, mp, 0) {
                // Undo the pre-push.
                let w = self.cur_mut(mc);
                w.pg[0] = w.pg[1];
                w.ki[0] = w.ki[1];
                let db = self.db_mut(mc);
                db.root = mp;
                db.depth -= 1;
                return Err(e);
            }
            let w = self.cur_mut(mc);
            w.snum += 1;
            w.top += 1;
            ptop = 0;
        } else {
            ptop = self.cur(mc).top - 1;
        }

        let mn = self.copy_cursor(mc, false);
        *mn_slot = Some(mn.slot);
        {
            let mc_ki_ptop = self.cur(mc).ki[ptop];
            let w = self.cur_mut(mn);
            let t = w.top;
            w.pg[t] = rp;
            w.ki[ptop] = mc_ki_ptop + 1;
        }

        // The page as it was, the temporary page holding the left half, and
        // the offsets of mp's nodes with a slot for the new one (C keeps
        // those in copy->mp_ptrs).
        let mut copy: Vec<u8> = Vec::new();
        let mut copy_ptrs: Vec<usize> = Vec::new();
        let src: Vec<u8> = self.top_page(mc)?.bytes().to_vec();
        let split_indx;
        let sepkey: Vec<u8>;
        if nflags & MDB_APPEND != 0 {
            let t = self.cur(mn).top;
            self.cur_mut(mn).ki[t] = 0;
            sepkey = newkey.to_vec();
            split_indx = newindx;
            nkeys = 0;
        } else {
            let mut si = nkeys.div_ceil(2);
            if is_leaf2 {
                sepkey = self.split_leaf2(mc, mp, rp, &src, newkey, newindx, si, nkeys)?;
            } else {
                // Maximum free space in an empty page.
                let pmax = ps - PAGE_HDR_SIZE;
                // Threshold number of keys considered "small".
                let keythresh = ps >> 7;
                let nsize = if is_leaf {
                    leaf_size(nodemax, newkey.len(), newdata.map_or(0, |d| d.size))
                } else {
                    branch_size(newkey.len())
                };
                let nsize = page_ops::even(nsize);

                // Grab a page to hold a temporary copy.
                copy = vec![0u8; ps];
                copy[..PAGE_HDR_SIZE].copy_from_slice(&src[..PAGE_HDR_SIZE]);
                page_ops::set_lower(&mut copy, PAGE_HDR_SIZE);
                page_ops::set_upper(&mut copy, ps);
                put_u16(&mut copy, PAGE_PAD, 0);

                // Prepare to insert.
                copy_ptrs = Vec::with_capacity(nkeys + 1);
                for i in 0..nkeys {
                    if i == newindx {
                        copy_ptrs.push(0);
                    }
                    copy_ptrs.push(page_ops::ptr(&src, i).ok_or_else(corrupted)?);
                }
                if newindx >= nkeys {
                    copy_ptrs.push(0);
                }

                // When items are relatively large the split point needs to
                // be checked, because being off-by-one will make the
                // difference between success or failure in node_add. It's
                // also relevant if a page happens to be laid out such that
                // one half of its nodes are all "small" and the other half
                // of its nodes are "large". If the new item is also "large"
                // and falls on the half with "large" nodes, it also may not
                // fit. As a final tweak, if the new item goes on the last
                // spot on the page (and thus, onto the new page), bias the
                // split so the new page is emptier than the old page. This
                // yields better packing during sequential inserts.
                if nkeys < keythresh || nsize > pmax / 16 || newindx >= nkeys {
                    // Find split point.
                    let mut psize = 0usize;
                    let (mut i, j, k): (isize, isize, isize) = if newindx <= si || newindx >= nkeys
                    {
                        let k = if newindx >= nkeys {
                            nkeys
                        } else {
                            si + 1 + usize::from(is_leaf)
                        };
                        (0, 1, k as isize)
                    } else {
                        (nkeys as isize, -1, si as isize - 1)
                    };
                    while i != k {
                        if i as usize == newindx {
                            psize += nsize;
                        } else {
                            let node = node_at(&src, copy_ptrs[i as usize])?;
                            psize += NODE_SIZE + node.key_size() + 2;
                            if is_leaf {
                                psize += if node.flags() & F_BIGDATA != 0 {
                                    8
                                } else {
                                    node.data_size()
                                };
                            }
                            psize = page_ops::even(psize);
                        }
                        if psize > pmax || i == k - j {
                            si = (i + isize::from(j < 0)) as usize;
                            break;
                        }
                        i += j;
                    }
                }
                sepkey = if si == newindx {
                    newkey.to_vec()
                } else {
                    let node = node_at(&src, copy_ptrs[si])?;
                    node.key().ok_or_else(corrupted)?.to_vec()
                };
            }
            split_indx = si;
        }

        // Copy separator key to the parent.
        let mut did_split = false;
        let parent_room = self.page_of(mn, ptop)?.size_left();
        if parent_room < branch_size(sepkey.len()) {
            let snum = self.cur(mc).snum;
            {
                let w = self.cur_mut(mn);
                w.snum -= 1;
                w.top -= 1;
            }
            did_split = true;
            // We want other splits to find mn when doing fixups.
            self.w
                .cursors
                .slot_mut(mn.slot)
                .ok_or_else(corrupted)?
                .tracked = true;
            let rc = self.page_split(mn, &sepkey, None, rp, 0);
            if let Some(s) = self.w.cursors.slot_mut(mn.slot) {
                s.tracked = false;
            }
            rc?;

            // Root split?
            if self.cur(mc).snum > snum {
                ptop += 1;
            }
            // Right page might now have changed parent. Check if left page
            // also changed parent.
            let mn_pg_ptop = self.cur(mn).pg[ptop];
            let mc_ki_ptop = usize::from(self.cur(mc).ki[ptop]);
            if mn_pg_ptop != self.cur(mc).pg[ptop]
                && mc_ki_ptop >= self.page_of(mc, ptop)?.num_keys()
            {
                let m = self.cur(mn).clone();
                let w = self.cur_mut(mc);
                for i in 0..ptop {
                    w.pg[i] = m.pg[i];
                    w.ki[i] = m.ki[i];
                }
                w.pg[ptop] = m.pg[ptop];
                if m.ki[ptop] != 0 {
                    w.ki[ptop] = m.ki[ptop] - 1;
                } else {
                    // Find right page's left sibling.
                    w.ki[ptop] = m.ki[ptop];
                    let _ = self.read_op(mc, |txn, c, _| c.sibling(txn, false));
                }
            }
        } else {
            self.cur_mut(mn).top -= 1;
            let ki = usize::from(self.cur(mn).ki[ptop]);
            let rc = self.node_add(mn, ki, &sepkey, None, rp, 0);
            self.cur_mut(mn).top += 1;
            rc?;
        }

        let top = self.cur(mc).top;
        if nflags & MDB_APPEND != 0 {
            {
                let w = self.cur_mut(mc);
                w.pg[top] = rp;
                w.ki[top] = 0;
            }
            self.node_add(mc, 0, newkey, newdata, newpgno, nflags)?;
            let m = self.cur(mn).clone();
            let w = self.cur_mut(mc);
            w.ki[..top].copy_from_slice(&m.ki[..top]);
        } else if !is_leaf2 {
            // Move nodes.
            self.cur_mut(mc).pg[top] = rp;
            let mut i = split_indx;
            let mut j = 0;
            let mut to_copy = false;
            loop {
                let (rkey, rdata, pgno, flags);
                let node_key;
                let node_data;
                if i == newindx {
                    rkey = newkey;
                    rdata = newdata;
                    pgno = newpgno;
                    flags = nflags;
                    // Update index for the new key.
                    self.cur_mut(mc).ki[top] = j as u16;
                } else {
                    let node = node_at(&src, copy_ptrs[i])?;
                    node_key = node.key().ok_or_else(corrupted)?;
                    rkey = node_key;
                    if is_leaf {
                        node_data = node_body(&node)?;
                        rdata = Some(NodeVal {
                            size: node.data_size(),
                            bytes: node_data,
                        });
                        pgno = 0;
                    } else {
                        rdata = None;
                        pgno = node.child_pgno();
                    }
                    flags = u32::from(node.flags());
                }
                // First branch index doesn't need key data.
                let rkey = if !is_leaf && j == 0 { &[][..] } else { rkey };
                if to_copy {
                    self.node_add_to(mc, Some(&mut copy), j, rkey, rdata, pgno, flags)?;
                } else {
                    self.node_add(mc, j, rkey, rdata, pgno, flags)?;
                }
                if i == nkeys {
                    i = 0;
                    j = 0;
                    to_copy = true;
                } else {
                    i += 1;
                    j += 1;
                }
                if i == split_indx {
                    break;
                }
            }
            // Write the left half back into mp.
            let n = page_ops::num_keys(&copy);
            let cu = page_ops::upper(&copy);
            let page = self.dirty_mut(mp)?;
            page[crate::format::layout::PAGE_LOWER..PAGE_HDR_SIZE]
                .copy_from_slice(&copy[crate::format::layout::PAGE_LOWER..PAGE_HDR_SIZE]);
            page[PAGE_HDR_SIZE..PAGE_HDR_SIZE + 2 * n]
                .copy_from_slice(&copy[PAGE_HDR_SIZE..PAGE_HDR_SIZE + 2 * n]);
            page[cu..ps].copy_from_slice(&copy[cu..ps]);

            // Reset back to original page.
            if newindx < split_indx {
                self.cur_mut(mc).pg[top] = mp;
            } else {
                self.move_to_right_page(mc, mn, rp, top, ptop)?;
            }
        } else if newindx >= split_indx {
            self.move_to_right_page(mc, mn, rp, top, ptop)?;
        }

        self.split_fixups(mc, mn, mp, rp, newindx, nflags, new_root, did_split, ptop)
    }

    /// After a split, the cursor's node is on the right page: move there,
    /// and make sure `mc_ki` is still valid (the parent may have split).
    fn move_to_right_page(
        &mut self,
        mc: CRef,
        mn: CRef,
        rp: u64,
        top: usize,
        ptop: usize,
    ) -> Result<()> {
        {
            let w = self.cur_mut(mc);
            w.pg[top] = rp;
            w.ki[ptop] += 1;
        }
        let w = self.cur(mc);
        if self.cur(mn).pg[ptop] != w.pg[ptop]
            && usize::from(w.ki[ptop]) >= self.page_of(mc, ptop)?.num_keys()
        {
            let m = self.cur(mn).clone();
            let w = self.cur_mut(mc);
            w.pg[..=ptop].copy_from_slice(&m.pg[..=ptop]);
            w.ki[..=ptop].copy_from_slice(&m.ki[..=ptop]);
        }
        Ok(())
    }

    /// The `P_LEAF2` split: move keys `split_indx..` of `mp` to `rp`,
    /// inserting the new key on the side it belongs to. Returns the
    /// separator key.
    #[allow(clippy::too_many_arguments)]
    fn split_leaf2(
        &mut self,
        mc: CRef,
        mp: u64,
        rp: u64,
        src: &[u8],
        newkey: &[u8],
        newindx: usize,
        split_indx: usize,
        nkeys: usize,
    ) -> Result<Vec<u8>> {
        let ksize = self.db_of(mc).pad as usize;
        if newkey.len() != ksize {
            return Err(crate::error::MdbError::BadValSize.into());
        }
        let x = newindx as isize - split_indx as isize;
        let split = PAGE_HDR_SIZE + split_indx * ksize;
        let rsize = (nkeys - split_indx) * ksize;
        let lsize = (nkeys - split_indx) * 2;
        let keys = src
            .get(split..split + rsize)
            .ok_or_else(corrupted)?
            .to_vec();
        let sepkey = if newindx == split_indx {
            newkey.to_vec()
        } else {
            keys.get(..ksize).ok_or_else(corrupted)?.to_vec()
        };
        {
            let page = self.dirty_mut(mp)?;
            page_ops::set_lower(page, page_ops::lower(page) - lsize);
            page_ops::set_upper(page, page_ops::upper(page) + rsize - lsize);
        }
        {
            let page = self.dirty_mut(rp)?;
            page_ops::set_lower(page, page_ops::lower(page) + lsize);
            page_ops::set_upper(page, page_ops::upper(page) - (rsize - lsize));
        }
        if x < 0 {
            // The new key goes left: all keys from split_indx move right.
            {
                let page = self.dirty_mut(rp)?;
                page[PAGE_HDR_SIZE..PAGE_HDR_SIZE + rsize].copy_from_slice(&keys);
            }
            let page = self.dirty_mut(mp)?;
            let ins = PAGE_HDR_SIZE + newindx * ksize;
            page.copy_within(ins..split, ins + ksize);
            page[ins..ins + ksize].copy_from_slice(newkey);
            page_ops::set_lower(page, page_ops::lower(page) + 2);
            page_ops::set_upper(page, page_ops::upper(page) - (ksize - 2));
        } else {
            let x = x as usize;
            let page = self.dirty_mut(rp)?;
            page[PAGE_HDR_SIZE..PAGE_HDR_SIZE + x * ksize].copy_from_slice(&keys[..x * ksize]);
            let ins = PAGE_HDR_SIZE + x * ksize;
            page[ins..ins + ksize].copy_from_slice(newkey);
            page[ins + ksize..ins + ksize + rsize - x * ksize].copy_from_slice(&keys[x * ksize..]);
            page_ops::set_lower(page, page_ops::lower(page) + 2);
            page_ops::set_upper(page, page_ops::upper(page) - (ksize - 2));
            let w = self.cur_mut(mc);
            let t = w.top;
            w.ki[t] = x as u16;
        }
        Ok(sepkey)
    }

    /// The cursor fixups at the end of `mdb_page_split`: every other
    /// tracked cursor on the split page `mp` follows its node (to `rp` if
    /// it moved there), and after a root split every cursor on the old root
    /// gets the new root pushed under it.
    #[allow(clippy::too_many_arguments)]
    fn split_fixups(
        &mut self,
        mc: CRef,
        mn: CRef,
        mp: u64,
        rp: u64,
        newindx: usize,
        nflags: u32,
        new_root: Option<usize>,
        did_split: bool,
        ptop: usize,
    ) -> Result<()> {
        let nkeys = self.page_of_pgno(mp)?.num_keys();
        let (dbi, sub, top) = {
            let w = self.cur(mc);
            (w.dbi, w.is_sub(), w.top)
        };
        let root_pg = self.cur(mc).pg[0];
        let mc_pg_ptop = self.cur(mc).pg[ptop];
        let mc_ki_ptop = self.cur(mc).ki[ptop];
        let m = self.cur(mn).clone();
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if m3 == mc {
                continue;
            }
            let m2_init = self.w.cursors.slot(s).is_some_and(|sl| sl.main.is_init());
            let w3 = self.cur_mut(m3);
            if !(m2_init && w3.flags & C_INITIALIZED != 0) {
                continue;
            }
            if w3.subp && w3.snum <= 1 {
                // A sub-page cursor: sub-pages are never split.
                continue;
            }
            if let Some(nr) = new_root {
                // Sub cursors may be on a different DB.
                if w3.pg[0] != mp {
                    continue;
                }
                // Root split.
                for k in (0..=nr.min(CURSOR_STACK - 2)).rev() {
                    w3.ki[k + 1] = w3.ki[k];
                    w3.pg[k + 1] = w3.pg[k];
                }
                w3.ki[0] = if usize::from(w3.ki[0]) >= nkeys { 1 } else { 0 };
                w3.pg[0] = root_pg;
                w3.snum += 1;
                w3.top += 1;
            }
            if w3.top >= top && w3.pg[top] == mp {
                if usize::from(w3.ki[top]) >= newindx && nflags & SPLIT_REPLACE == 0 {
                    w3.ki[top] += 1;
                }
                if usize::from(w3.ki[top]) >= nkeys {
                    w3.pg[top] = rp;
                    w3.ki[top] -= nkeys as u16;
                    w3.ki[..top].copy_from_slice(&m.ki[..top]);
                    w3.pg[..top].copy_from_slice(&m.pg[..top]);
                }
            } else if !did_split
                && w3.top >= ptop
                && w3.pg[ptop] == mc_pg_ptop
                && w3.ki[ptop] >= mc_ki_ptop
            {
                w3.ki[ptop] += 1;
            }
        }
        Ok(())
    }

    /// A page of this transaction by number.
    pub(crate) fn page_of_pgno(&self, pgno: u64) -> Result<PageRef<'_>> {
        self.core.page(pgno)
    }
}

/// The node at offset `off` of page bytes `src`.
fn node_at(src: &[u8], off: usize) -> Result<NodeRef<'_>> {
    NodeRef::new(src.get(off..).ok_or_else(corrupted)?).ok_or_else(corrupted)
}

/// The stored data part of a leaf node: the inline bytes, or the 8-byte
/// overflow page number of an `F_BIGDATA` node.
pub(crate) fn node_body<'a>(node: &NodeRef<'a>) -> Result<&'a [u8]> {
    if node.is_bigdata() {
        let k = node.key().ok_or_else(corrupted)?;
        let start = NODE_SIZE + k.len();
        let raw = node.raw(true).ok_or_else(corrupted)?;
        raw.get(start..start + 8).ok_or_else(corrupted)
    } else {
        node.inline_data().ok_or_else(corrupted)
    }
}
