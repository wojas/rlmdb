//! Page allocation and the free-page database: `mdb_page_alloc` (mdb.c
//! line 2231), `mdb_freelist_save` (line 3169), `mdb_page_loose` (line
//! 1934) and `mdb_ovpage_free` (line 5819).
//!
//! # How pages are recycled
//!
//! A write transaction never overwrites a page that a snapshot may still
//! read. Pages it stops using go on its free list (`mt_free_pgs`), which
//! commit stores in the free DB (DBI 0) under the transaction's own id:
//! `txnid -> [count, pgno...]` (descending). A later writer may reuse the
//! pages of record `T` once every reader's snapshot is at least `T`
//! (`mdb_find_oldest`). It reads such records in key order into
//! `me_pghead` (here `WriteState::pghead`, also descending) and remembers
//! the last one it consumed in `me_pglast`; the records themselves are only
//! deleted when the free list is saved at commit.
//!
//! Pages that were allocated *and* freed in the same transaction are
//! "loose" (`P_LOOSE`): they are still dirty and are reused first, without
//! touching the free DB.
//!
//! # Saving the free list
//!
//! Writing the free DB allocates and frees pages itself (copy-on-write of
//! the free DB's own pages, splits, deleted records), which changes the
//! very lists being written. `mdb_freelist_save` therefore loops until a
//! pass makes no more changes, reserving record space first
//! (`MDB_RESERVE`) and filling it in at the end. The port keeps C's exact
//! sequence of operations, which determines which pages end up where; see
//! the comments in [`RwTxn::freelist_save`].

use crate::consts::*;
use crate::core::cursor::{corrupted, cursor_first, cursor_next, cursor_set, Cursor, CursorOp};
use crate::core::cursor_put::PutData;
use crate::core::txn_rw::{RwTxn, TXN_ERROR};
use crate::core::wcursor::CRef;
use crate::error::{MdbError, Result};
use crate::format::get_u64;
use crate::format::max_free_1pg;
use crate::format::page::{P_DIRTY, P_LOOSE};
use crate::idl::Idl;

/// Reads a free DB key (a native `txnid_t`).
fn txnid_key(key: &[u8]) -> Result<u64> {
    get_u64(key, 0).ok_or_else(corrupted)
}

impl<'env> RwTxn<'env> {
    /// `mdb_page_alloc`: allocate `num` contiguous pages and return the
    /// first page number. The buffer is in the dirty list, zero-filled (a
    /// reused loose page keeps its old contents, as in C). Order of
    /// preference, as C:
    ///
    /// 1. a loose page (single pages only);
    /// 2. a run of `num` pages from `pghead`, preferring the tail (lowest
    ///    page numbers), reading more free DB records when needed: records
    ///    older than every reader, one at a time in key order, merged into
    ///    `pghead`; for multi-page runs at most `num * 60` records are read
    ///    while looking for a contiguous run;
    /// 3. new pages at the end of the file (`MDB_MAP_FULL` if the map is
    ///    too small).
    pub(crate) fn page_alloc(&mut self, num: usize) -> Result<u64> {
        self.page_alloc_init(num, None)
    }

    /// [`RwTxn::page_alloc`] of one page whose contents start as a copy of
    /// `src` (a whole page), for `mdb_page_touch`: a new buffer is made by
    /// copying `src` instead of zero-filling it first and copying after
    /// (the same bytes; the zero fill was a sizeable part of every write).
    pub(crate) fn page_alloc_copy(&mut self, src: &[u8]) -> Result<u64> {
        self.page_alloc_init(1, Some(src))
    }

    fn page_alloc_init(&mut self, num: usize, init: Option<&[u8]>) -> Result<u64> {
        // If there are any loose pages, just use them.
        if num == 1 {
            if let Some(pgno) = self.w.loose.pop() {
                if let Some(src) = init {
                    self.dirty_mut(pgno)?
                        .get_mut(..src.len())
                        .ok_or_else(corrupted)?
                        .copy_from_slice(src);
                }
                return Ok(pgno);
            }
        }
        // If our dirty list is already full, we can't do anything.
        if self.w.dirty_room == 0 {
            self.w.flags |= TXN_ERROR;
            return Err(MdbError::TxnFull.into());
        }
        let found = match self.reclaim_run(num) {
            Ok(f) => f,
            Err(e) => {
                self.w.flags |= TXN_ERROR;
                return Err(e);
            }
        };
        let pgno = match found {
            Some(i) => {
                let mop = self.w.pghead.as_mut().ok_or_else(corrupted)?;
                mop.remove_run(i, num)
            }
            None => {
                // Use new pages from the map when nothing suitable in the
                // free DB.
                let pgno = self.core.next_pgno;
                if pgno + num as u64 >= self.core.max_pgno() {
                    self.w.flags |= TXN_ERROR;
                    return Err(MdbError::MapFull.into());
                }
                self.core.next_pgno = pgno + num as u64;
                pgno
            }
        };
        let mut buf = match init {
            Some(src) => {
                debug_assert_eq!(src.len(), self.psize() * num);
                self.core.dirty.alloc_copy(src)
            }
            None => self.core.dirty.alloc_zeroed(self.psize() * num),
        };
        crate::core::page_ops::set_pgno(&mut buf, pgno);
        // mdb_page_dirty
        if !self.core.dirty.insert(pgno, buf) {
            self.w.flags |= TXN_ERROR;
            return Err(MdbError::Corrupted.into());
        }
        self.w.dirty_room -= 1;
        Ok(pgno)
    }

    /// The search loop of `mdb_page_alloc`: find `num` contiguous pages in
    /// `pghead`, merging in more free DB records as long as they are older
    /// than every reader. Returns the 1-based C index of the run's lowest
    /// page in `pghead`.
    fn reclaim_run(&mut self, num: usize) -> Result<Option<usize>> {
        let n2 = num - 1;
        let mut retry = num as isize * 60;
        let core = &self.core;
        let w = &mut self.w;
        let free = core.dbs.borrow()[FREE_DBI as usize];
        let mut m2 = Cursor::new(free.db, free.key_cmp, 0);
        let mut op = CursorOp::First;
        let mut found_old = false;
        let mut oldest = 0u64;
        let mut last = 0u64;
        let mut mop_len = w.pghead.as_ref().map_or(0, Idl::len);
        loop {
            // Seek a big enough contiguous page range. Prefer pages at the
            // tail, just truncating the list.
            if mop_len > n2 {
                if let Some(i) = w.pghead.as_ref().and_then(|m| m.find_run(num)) {
                    return Ok(Some(i));
                }
                retry -= 1;
                if retry < 0 {
                    break;
                }
            }
            if op == CursorOp::First {
                // Prepare to fetch more and coalesce.
                last = w.pglast;
                oldest = w.pg_oldest;
                if last != 0 {
                    op = CursorOp::SetRange; // will look up last+1
                }
            }
            last += 1;
            // Do not fetch more if the record will be too recent.
            if oldest <= last {
                if !found_old {
                    oldest = find_oldest(core);
                    w.pg_oldest = oldest;
                    found_old = true;
                }
                if oldest <= last {
                    break;
                }
            }
            let kv = match op {
                CursorOp::First => cursor_first(core, &mut m2, None),
                CursorOp::SetRange => cursor_set(
                    core,
                    &mut m2,
                    None,
                    None,
                    &last.to_ne_bytes(),
                    None,
                    CursorOp::SetRange,
                    false,
                )
                .map(|(kv, _)| kv),
                _ => cursor_next(core, &mut m2, None, CursorOp::Next),
            };
            op = CursorOp::Next;
            let kv = match kv {
                Ok(kv) => kv,
                Err(e) if e.not_found() => break,
                Err(e) => return Err(e),
            };
            last = txnid_key(kv.key)?;
            if oldest <= last {
                if !found_old {
                    oldest = find_oldest(core);
                    w.pg_oldest = oldest;
                    found_old = true;
                }
                if oldest <= last {
                    break;
                }
            }
            let idl = Idl::from_bytes(kv.data).ok_or_else(corrupted)?;
            // C trusts the record. A meta page or a page past the snapshot
            // would be handed out and overwritten (or its offset overflow).
            if idl
                .as_slice()
                .iter()
                .any(|&p| p < NUM_METAS as u64 || p >= core.next_pgno)
            {
                return Err(corrupted());
            }
            w.pglast = last;
            // Merge in descending sorted order.
            let mop = w.pghead.get_or_insert_with(Idl::new);
            mop.xmerge(&idl);
            mop_len = mop.len();
        }
        Ok(None)
    }

    /// `mdb_page_loose`: a page this cursor's tree no longer uses. If it is
    /// dirty (allocated in this transaction) and not a free DB page, keep it
    /// for reuse in this transaction; otherwise put it on the free list.
    pub(crate) fn page_loose(&mut self, mc: CRef, pgno: u64) -> Result<()> {
        let dirty = self.core.dirty.contains(pgno) && self.core.page(pgno)?.flags() & P_DIRTY != 0;
        if dirty && self.cur(mc).dbi != FREE_DBI as usize {
            self.w.loose.push(pgno);
            let page = self.dirty_mut(pgno)?;
            let f = crate::core::page_ops::flags(page);
            crate::core::page_ops::set_flags(page, f | P_LOOSE);
        } else {
            self.w.free_pgs.append(pgno);
        }
        Ok(())
    }

    /// The length of the overflow run at `pgno` (`mp_pages`), checked
    /// against the pages of the transaction: a corrupt count must not make
    /// the free list grow by billions of entries (C fails with `ENOMEM` at
    /// best).
    pub(crate) fn overflow_run_pages(&self, pgno: u64) -> Result<usize> {
        let page = self.core.page(pgno)?;
        let n = page.overflow_pages();
        if !page.is_overflow()
            || n == 0
            || pgno
                .checked_add(u64::from(n))
                .is_none_or(|end| end > self.core.next_pgno)
        {
            return Err(MdbError::Corrupted.into());
        }
        Ok(n as usize)
    }

    /// `mdb_ovpage_free`: free the overflow run starting at `pgno`. A run
    /// allocated in this transaction (dirty, or spilled by it) goes straight
    /// back to `pghead` (if the free DB is in use); otherwise it goes on the
    /// free list. Not in nested transactions: they would need to hide the
    /// run in their ancestors' dirty and spill lists.
    pub(crate) fn ovpage_free(&mut self, mc: CRef, pgno: u64) -> Result<()> {
        let ovpages = self.overflow_run_pages(pgno)?;
        let dirty = self.core.page(pgno)?.flags() & P_DIRTY != 0;
        let spilled = !dirty && self.core.spilled.contains(&pgno);
        if self.w.pghead.is_some() && !self.has_parent() && (dirty || spilled) {
            if spilled {
                // This page is no longer spilled.
                self.core.spilled.remove(&pgno);
            } else {
                // Remove from dirty list.
                if !self.core.dirty.remove(pgno) {
                    self.w.flags |= TXN_ERROR;
                    return Err(MdbError::Corrupted.into());
                }
                self.w.dirty_room += 1;
            }
            // Insert in me_pghead, keeping it descending.
            let mop = self.w.pghead.as_mut().ok_or_else(corrupted)?;
            mop.insert_run(pgno, ovpages);
        } else {
            self.w.free_pgs.append_range(pgno, ovpages);
        }
        let db = self.db_mut(mc);
        db.overflow_pages = db.overflow_pages.wrapping_sub(ovpages as u64);
        Ok(())
    }

    /// `mdb_freelist_save`: write the free list of this transaction and what
    /// is left of `pghead` into the free DB. Called by commit.
    ///
    /// The cursor is tracked (C uses an untracked one); see
    /// `KNOWN_C_BUGS.md` #11.
    pub(crate) fn freelist_save(&mut self) -> Result<()> {
        let slot = self.open_slot(FREE_DBI as usize, true);
        let rc = self.freelist_save0(CRef::main(slot));
        self.close_slot(slot);
        rc
    }

    fn freelist_save0(&mut self, mc: CRef) -> Result<()> {
        // `pghead` can grow and shrink during this call; `pglast` and
        // `free_pgs` can only grow. Page numbers cannot disappear from
        // `free_pgs`.
        let maxfree_1pg = max_free_1pg(self.psize()) as isize;
        let mut more = 1isize;
        let mut pglast = 0u64;
        let mut head_id = 0u64;
        let mut freecnt = 0usize;
        let mut head_room = 0isize;
        let mut total_room = 0isize;
        let txnid = self.core.txnid;

        if self.w.pghead.is_some() {
            // Make sure first page of freeDB is touched and on freelist.
            match self.page_search_modify(mc, None, crate::core::cursor::PS_FIRST) {
                Err(e) if !e.not_found() => return Err(e),
                _ => {}
            }
        }

        if self.w.pghead.is_none() && !self.w.loose.is_empty() {
            // Put loose page numbers in free_pgs, since we may be unable to
            // return them to pghead. They must also leave the dirty list.
            for &pgno in self.w.loose.iter().rev() {
                self.w.free_pgs.append(pgno);
                self.core.dirty.remove(pgno);
            }
            self.w.loose.clear();
        }

        loop {
            // Come back here after each put in case the free list changed.

            // If using records from freeDB which we have not yet deleted,
            // delete them and any we reserved for pghead.
            while pglast < self.w.pglast {
                let key = self.read_op(mc, |txn, c, _| {
                    cursor_first(txn, c, None).and_then(|kv| txnid_key(kv.key))
                })?;
                pglast = key;
                head_id = key;
                total_room = 0;
                head_room = 0;
                self.del_at(mc, 0)?;
            }

            // Save the IDL of pages freed by this txn, to a single record.
            if freecnt < self.w.free_pgs.len() {
                if freecnt == 0 {
                    // Make sure last page of freeDB is touched and on freelist.
                    match self.page_search_modify(mc, None, crate::core::cursor::PS_LAST) {
                        Err(e) if !e.not_found() => return Err(e),
                        _ => {}
                    }
                }
                let key = txnid.to_ne_bytes();
                loop {
                    freecnt = self.w.free_pgs.len();
                    let size = (freecnt + 1) * 8;
                    self.put_at(mc, &key, PutData::reserve(size), MDB_RESERVE)?;
                    // Retry if free_pgs grew during the put.
                    if freecnt >= self.w.free_pgs.len() {
                        break;
                    }
                }
                self.w.free_pgs.sort();
                let bytes = self.w.free_pgs.to_bytes();
                let loc = self.data_loc(mc)?;
                self.loc_mut(loc)?.copy_from_slice(&bytes);
                continue;
            }

            let mop_len = self.w.pghead.as_ref().map_or(0, Idl::len) + self.w.loose.len();
            let mop_len = mop_len as isize;

            // Reserve records for pghead. Split it if multi-page, to avoid
            // searching freeDB for a page range. Use keys in range
            // [1, pglast]: smaller than txnid of oldest reader.
            if total_room >= mop_len {
                if total_room == mop_len || {
                    more -= 1;
                    more < 0
                } {
                    break;
                }
            } else if head_room >= maxfree_1pg && head_id > 1 {
                // Keep current record (overflow page), add a new one.
                head_id -= 1;
                head_room = 0;
            }
            // (Re)write {key = head_id, IDL length = head_room}.
            total_room -= head_room;
            head_room = mop_len - total_room;
            if head_room > maxfree_1pg && head_id > 1 {
                // Overflow multi-page for part of pghead.
                head_room /= head_id as isize; // amortize page sizes
                head_room += maxfree_1pg - head_room % (maxfree_1pg + 1);
            } else if head_room < 0 {
                // Rare case, not bothering to delete this record.
                head_room = 0;
            }
            let size = (head_room as usize + 1) * 8;
            self.put_at(
                mc,
                &head_id.to_ne_bytes(),
                PutData::reserve(size),
                MDB_RESERVE,
            )?;
            // The IDL is initially empty. (C zeroes the count, or all of it
            // where `malloc` did not; we zero all of it.)
            let loc = self.data_loc(mc)?;
            self.loc_mut(loc)?.fill(0);
            total_room += head_room;
        }

        // Return loose page numbers to pghead, though usually none are left
        // at this point. The pages themselves remain in the dirty list.
        if !self.w.loose.is_empty() {
            let mut loose = Idl::from_vec(std::mem::take(&mut self.w.loose));
            loose.sort();
            self.w.pghead.get_or_insert_with(Idl::new).xmerge(&loose);
        }

        // Fill in the reserved pghead records.
        let mop: Vec<u64> = self
            .w
            .pghead
            .as_ref()
            .map_or_else(Vec::new, |m| m.as_slice().to_vec());
        let mut mop_len = mop.len();
        if mop_len == 0 {
            return Ok(());
        }
        let mut rec = self.read_op(mc, |txn, c, _| {
            cursor_first(txn, c, None).and_then(|kv| Ok((txnid_key(kv.key)?, kv.data.len())))
        });
        loop {
            let (id, dsize) = rec?;
            let mut len = (dsize / 8).wrapping_sub(1) as isize;
            if len < 0 || id > self.w.pglast {
                return Err(MdbError::Corrupted.into());
            }
            if len as usize > mop_len {
                len = mop_len as isize;
            }
            let len = len as usize;
            // The record holds the `len` lowest remaining page numbers.
            let ids = Idl::from_vec(mop[mop_len - len..mop_len].to_vec());
            self.put_at(
                mc,
                &id.to_ne_bytes(),
                PutData::bytes(&ids.to_bytes()),
                MDB_CURRENT,
            )?;
            mop_len -= len;
            if mop_len == 0 {
                return Ok(());
            }
            rec = self.read_op(mc, |txn, c, _| {
                cursor_next(txn, c, None, CursorOp::Next)
                    .and_then(|kv| Ok((txnid_key(kv.key)?, kv.data.len())))
            });
        }
    }
}

/// `mdb_find_oldest`: the oldest snapshot any reader uses, or our txnid
/// minus one if there is none older.
fn find_oldest(core: &crate::core::txn::TxnCore<'_>) -> u64 {
    let txnid = core.txnid;
    match core.env.lock_region() {
        Some(lock) => lock.find_oldest(txnid),
        None => txnid.wrapping_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{Dbi, Environment, Transaction};
    use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};
    use crate::idl::Idl;

    fn env(dir: &std::path::Path) -> Environment {
        Environment::builder()
            .map_size(64 << 20)
            .flags(EnvironmentFlags::NO_SYNC)
            .open(dir)
            .unwrap()
    }

    /// The free DB records of the newest snapshot: (txnid, pages).
    fn free_records(env: &Environment) -> Vec<(u64, Idl)> {
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(Dbi::FREE).unwrap();
        let mut out = Vec::new();
        while let Some((k, v)) = c.next().unwrap() {
            let id = u64::from_ne_bytes(k.try_into().unwrap());
            out.push((id, Idl::from_bytes(v).unwrap()));
        }
        out
    }

    #[test]
    fn freed_pages_are_saved_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
        for i in 0..500u32 {
            txn.put(db, &i.to_be_bytes(), &[1; 100], WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
        let used = env.info().unwrap().last_pgno;

        // Rewriting everything frees every page of the old tree; they are
        // saved under the new txnid, sorted descending, without duplicates.
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..500u32 {
            txn.put(db, &i.to_be_bytes(), &[2; 100], WriteFlags::empty())
                .unwrap();
        }
        let id = txn.id();
        txn.commit().unwrap();
        let recs = free_records(&env);
        let (rid, idl) = recs.iter().find(|(r, _)| *r == id).unwrap();
        assert_eq!(*rid, id);
        assert!(idl.is_sorted_desc() && !idl.is_empty());
        assert!(idl.as_slice().iter().all(|&p| p >= 2 && p <= used as u64));

        // With no reader in the way, later transactions take those pages
        // instead of growing the file.
        for round in 0..20u8 {
            let mut txn = env.begin_rw_txn().unwrap();
            for i in 0..500u32 {
                txn.put(db, &i.to_be_bytes(), &[round; 100], WriteFlags::empty())
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        // (Pages freed by txn N become reusable in txn N+2, as in C, so the
        // steady state holds about three trees plus the free DB.)
        let grown = env.info().unwrap().last_pgno;
        assert!(grown <= used * 4, "file grew from {used} to {grown} pages");
        // Every page is either in the tree, free, or a meta page.
        let txn = env.begin_ro_txn().unwrap();
        let st = txn.stat(db).unwrap();
        let fst = txn.stat(Dbi::FREE).unwrap();
        let free: usize = free_records(&env).iter().map(|(_, l)| l.len()).sum();
        let in_trees = st.branch_pages
            + st.leaf_pages
            + st.overflow_pages
            + fst.branch_pages
            + fst.leaf_pages
            + fst.overflow_pages;
        assert_eq!(2 + in_trees + free, grown + 1);
    }

    #[test]
    fn loose_pages_are_reused_within_a_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
        // Grow and shrink the tree repeatedly in one transaction: merged
        // pages become loose and are allocated again.
        for round in 0..5u32 {
            for i in 0..2000u32 {
                txn.put(db, &i.to_be_bytes(), &[3; 200], WriteFlags::empty())
                    .unwrap();
            }
            for i in 0..2000u32 {
                assert!(
                    txn.del(db, &i.to_be_bytes(), None).unwrap(),
                    "round {round}"
                );
            }
        }
        let next = txn.core.next_pgno;
        txn.commit().unwrap();
        // One round's worth of pages, not five.
        let pages_per_round = 2000 * 216 / env.page_size() + 10;
        assert!((next as usize) < 2 + 2 * pages_per_round, "{next}");
    }
}
