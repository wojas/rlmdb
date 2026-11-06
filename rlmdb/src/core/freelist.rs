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
//!
//! # Splitting the pages a transaction frees (`split-freelist`)
//!
//! Not in C LMDB 0.9 or 1.0: the opt-in feature
//! [`Feature::SplitFreelist`]. It follows the `MDB_SPLIT_FREELIST` patch
//! of the PowerDNS fork of LMDB (`mdb_freelist_save`,
//! `mdb_env_write_meta`), without its `mdb_txn_set_commit_txnid`: the
//! final txnid is [`PreparedTxn::id`](crate::PreparedTxn::id).
//!
//! C saves all pages a transaction freed as one record. Each page number
//! takes 8 bytes, so deleting 4 GB of data with 4 KiB pages (about a
//! million pages) needs a record of 8 MB, in one contiguous run of about
//! 2,000 overflow pages. The free list
//! rarely has such a run, so the file grows for it, and a transaction that
//! only deletes data can fail with `MDB_MAP_FULL`.
//!
//! With the feature, a transaction whose freed pages need more than one
//! page of record (`maxfree_1pg` page numbers) saves them in `n` records
//! of at most one page each, under the consecutive keys `T .. T + n - 1`,
//! where `T` is the transaction's own txnid. Its txnid then becomes
//! `P = T + (n & !1)`, the first txnid at or above the last key with the
//! parity of `T`, and the commit writes its snapshot into both meta
//! pages: under `P`, and once more under `P + 1` into the page that held
//! the snapshot before (`RwTxn::write_meta`). The format does not change
//! (the free DB maps txnids to page lists): C reads, reclaims and writes
//! such a free DB as any other, though no test runs C over these files.
//! Why it is safe:
//!
//! * C's reuse rule lets a writer `W` reuse a record once its key is
//!   below `W - 1` and below every reader's snapshot. Keeping key `W - 1`
//!   keeps the pages of the snapshot on the other meta page intact while
//!   `W` runs. After a split commit the next writer may reuse every split
//!   record, which list the pages of the snapshot before the commit. That
//!   is safe only because that snapshot is on no meta page any more: both
//!   pages hold the committed state, as after two ordinary commits. (The
//!   PowerDNS patch writes one meta page and leaves the old snapshot on
//!   the other; anything that reads the previous snapshot on purpose,
//!   `PREV_SNAPSHOT`, the tools' `-v`, `mdb_env_rollback`, could then read
//!   pages a later writer overwrote.)
//! * The keys are unique. Records this transaction loaded for reuse
//!   (`pghead`) are written back under keys of at most `pglast`, below
//!   `T`; every split key is at most `P`; later transactions get txnids
//!   above `P + 1`.
//! * All keys are at least `T`, so a reader of a snapshot before this
//!   commit keeps every split record from reuse, as always.
//! * `P` has the parity of `T`, so the first meta write goes to the page
//!   that the previous commit did not use, and the snapshot before stays
//!   intact until the commit is on disk; `P + 1` then replaces it. Both
//!   writes are synced also with `NO_META_SYNC`: a writer that takes over
//!   after this process dies may reuse the old snapshot's pages, which is
//!   safe only once the new snapshot is on disk (`NO_SYNC` promises
//!   nothing and stays as it is).
//! * The transaction's txnid changes to `P` only after the last put into
//!   the free DB, so the save reclaims only records below `T - 1`.
//!
//! A transaction whose freed pages fit one page saves them as C does, as
//! does one whose txnids would come within [`SPLIT_ID_MARGIN`] of
//! `u64::MAX` (once a save has split, it fails with `EOVERFLOW` instead
//! of falling back). `docs/split-freelist-details.md` has the full
//! account.

use crate::consts::*;
use crate::core::cursor::{corrupted, cursor_first, cursor_next, cursor_set, Cursor, CursorOp};
use crate::core::cursor_put::PutData;
use crate::core::dirty::PageSlot;
use crate::core::txn_rw::{RwTxn, TXN_ERROR};
use crate::core::wcursor::CRef;
use crate::error::{MdbError, Result};
use crate::feature::Feature;
use crate::format::get_u64;
use crate::format::layout::{Format, OverflowRef};
use crate::format::page::{P_DIRTY, P_LOOSE};
use crate::idl::Idl;

/// How far below `u64::MAX` the txnid of the writer after a split commit
/// must stay for the commit to split: above `u64::MAX - 16` C 1.0's
/// per-writer work id (`(txnid | 15) + 1`, mdb.c 1.0 line 3399) wraps to
/// 0, which would let that writer change committed pages in place; and
/// `u64::MAX` marks a free reader slot.
pub(crate) const SPLIT_ID_MARGIN: u64 = 16;

/// Whether a transaction with txnid `txnid` may split its freed pages over
/// `n` records: the writer after its commit, `txnid + (n | 1) + 1`, stays
/// [`SPLIT_ID_MARGIN`] below `u64::MAX`.
fn may_split(txnid: u64, n: usize) -> bool {
    (n as u64 | 1)
        .checked_add(1 + SPLIT_ID_MARGIN)
        .and_then(|ids| txnid.checked_add(ids))
        .is_some()
}

/// Reads a free DB key (a native `txnid_t`).
fn txnid_key(key: &[u8]) -> Result<u64> {
    get_u64(key, 0).ok_or_else(corrupted)
}

/// Reads free DB key `key` in a walk of the free DB, after key `prev` if
/// there was one. The keys are the ids of the transactions that freed the
/// pages: unique, so a walk meets them in ascending order. A key not larger
/// than the one before is a damaged tree, out of order or with branch pages
/// that share a child, whose records a walk would read once per path to it
/// (`KNOWN_C_BUGS.md` row 50): `MDB_CORRUPTED`.
pub(crate) fn next_txnid_key(prev: Option<u64>, key: &[u8]) -> Result<u64> {
    let id = txnid_key(key)?;
    match prev {
        Some(prev) if id <= prev => Err(corrupted()),
        _ => Ok(id),
    }
}

/// How the buffer of a newly allocated page starts (see
/// [`RwTxn::page_alloc_with`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum PageInit<'a> {
    /// Zero-filled (a reused loose page keeps its old contents, as in C).
    Zeroed,
    /// A copy of a whole page (`mdb_page_touch`, see
    /// [`RwTxn::page_alloc_copy`]).
    Copy(&'a [u8]),
    /// This data right after the page header and zeros everywhere else:
    /// the data of a new overflow run (`mdb_node_add`). The same bytes as
    /// zero-filling the run and copying the data in, but each written once
    /// (C's `mdb_page_malloc` zeroes only the last page of a run, which
    /// the data does not reach, and `mdb_node_add` copies the data). A
    /// reused loose page gets the data and keeps the rest of its old
    /// contents, as it does when zero-filling is asked for.
    Data(&'a [u8]),
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
        self.page_alloc_with(num, PageInit::Zeroed)
            .map(|(pgno, _)| pgno)
    }

    /// [`RwTxn::page_alloc`] of one page whose contents start as a copy of
    /// `src` (a whole page), for `mdb_page_touch`: a new buffer is made by
    /// copying `src` instead of zero-filling it first and copying after
    /// (the same bytes; the zero fill was a sizeable part of every write).
    /// Also returns the page's slot in the dirty list.
    pub(crate) fn page_alloc_copy(&mut self, src: &[u8]) -> Result<(u64, PageSlot)> {
        self.page_alloc_with(1, PageInit::Copy(src))
    }

    /// [`RwTxn::page_alloc`] with the buffer starting as `init` says.
    /// Returns the page number and its slot in the dirty list
    /// ([`PageSlot::NONE`] for a reused loose page that is not initialised
    /// here).
    pub(crate) fn page_alloc_with(
        &mut self,
        num: usize,
        init: PageInit<'_>,
    ) -> Result<(u64, PageSlot)> {
        // Counted first, whatever the outcome (see `TxnCore::walk_limit`).
        self.core.allocations = self.core.allocations.saturating_add(1);
        // If there are any loose pages, just use them.
        if num == 1 {
            if let Some(pgno) = self.w.loose.pop() {
                let (at, src) = match init {
                    PageInit::Zeroed => return Ok((pgno, PageSlot::NONE)),
                    PageInit::Copy(src) => (0, src),
                    PageInit::Data(data) => (self.layout().page_hdr, data),
                };
                let slot = self.core.dirty.find(pgno).ok_or_else(corrupted)?;
                self.core
                    .dirty
                    .at_mut(slot, pgno)
                    .and_then(|page| page.get_mut(at..at + src.len()))
                    .ok_or_else(corrupted)?
                    .copy_from_slice(src);
                return Ok((pgno, slot));
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
                // C 1.0 on Windows commits the pages in its view, so that
                // every process's view shows them (`os::windows`).
                #[cfg(windows)]
                {
                    let psize = self.psize();
                    if let Err(e) = self
                        .core
                        .map
                        .commit_pages(pgno as usize * psize, num * psize)
                    {
                        self.w.flags |= TXN_ERROR;
                        return Err(e.into());
                    }
                }
                self.core.next_pgno = pgno + num as u64;
                pgno
            }
        };
        let len = self.psize() * num;
        let mut buf = match init {
            PageInit::Zeroed => self.core.dirty.alloc_zeroed(len),
            PageInit::Copy(src) => {
                debug_assert_eq!(src.len(), len);
                self.core.dirty.alloc_copy(src)
            }
            PageInit::Data(data) => {
                let hdr = self.layout().page_hdr;
                match self.core.dirty.alloc_data(len, hdr, data) {
                    Some(buf) => buf,
                    None => {
                        self.w.flags |= TXN_ERROR;
                        return Err(corrupted());
                    }
                }
            }
        };
        crate::core::page_ops::set_pgno(&mut buf, pgno);
        // mdb_page_dirty
        let Some(slot) = self.core.dirty.insert(pgno, buf) else {
            self.w.flags |= TXN_ERROR;
            return Err(MdbError::Corrupted.into());
        };
        self.w.dirty_room -= 1;
        Ok((pgno, slot))
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
        let mut m2 = Cursor::new(free.db, free.key_cmp, 0, core.layout);
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
            let first = op == CursorOp::First;
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
            // C reads on whatever the key. `last` is one past the key read
            // before, or past `pglast` where the search started: a smaller
            // key is a damaged tree, and with branch pages that share a
            // child this loop would read its records once per path to it,
            // holding the writer lock (`KNOWN_C_BUGS.md` row 50).
            let prev = (!first).then(|| last - 1);
            last = next_txnid_key(prev, kv.key)?;
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
            let mut ids = Idl::encoded_ids(kv.data).ok_or_else(corrupted)?;
            // C trusts the record. A meta page or a page past the snapshot
            // would be handed out and overwritten (or its offset overflow).
            if ids.any(|p| p < NUM_METAS as u64 || p >= core.next_pgno) {
                return Err(corrupted());
            }
            w.pglast = last;
            // Merge in descending sorted order (straight from the record).
            let mop = w.pghead.get_or_insert_with(Idl::new);
            mop.xmerge_encoded(kv.data).ok_or_else(corrupted)?;
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
            let l = self.layout();
            let page = self.dirty_mut(pgno)?;
            let f = crate::core::page_ops::flags(l, page);
            crate::core::page_ops::set_flags(l, page, f | P_LOOSE);
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

    /// The length of the overflow run that reference `r` points to, as
    /// `mdb_drop0` frees it: 0.9 reads `mp_pages` from the run's first page
    /// ([`RwTxn::overflow_run_pages`]); 1.0 trusts the reference's
    /// `op_pages` and does not read the page. Either way the run must lie
    /// within the transaction's pages.
    pub(crate) fn dropped_run_pages(&self, r: OverflowRef) -> Result<usize> {
        match self.layout().format {
            Format::V09 => self.overflow_run_pages(r.pgno),
            Format::V10 => {
                if r.pages == 0
                    || r.pgno
                        .checked_add(r.pages)
                        .is_none_or(|end| end > self.core.next_pgno)
                {
                    return Err(MdbError::Corrupted.into());
                }
                usize::try_from(r.pages).map_err(|_| MdbError::Corrupted.into())
            }
        }
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
    /// With the feature `split-freelist` the pages this transaction freed
    /// may go into several records (see the module documentation); the
    /// transaction's txnid is then its first meta page's, `P`, and
    /// `WriteState::split_from` the one it began with.
    ///
    /// The cursor is tracked (C uses an untracked one); see
    /// `KNOWN_C_BUGS.md` row 11.
    pub(crate) fn freelist_save(&mut self) -> Result<()> {
        let split = self.core.env.has_feature(Feature::SplitFreelist);
        let txnid = self.core.txnid;
        let mut commit_id = txnid;
        let slot = self.open_slot(FREE_DBI as usize, true);
        let rc = self.freelist_save0(CRef::main(slot), split, &mut commit_id);
        self.close_slot(slot);
        rc?;
        // Nothing allocates from the free DB any more: the txnid becomes
        // `commit_id`.
        if commit_id != txnid {
            self.w.split_from = Some(txnid);
            self.core.txnid = commit_id;
        }
        Ok(())
    }

    /// The loop of `mdb_freelist_save`. `split`: the feature
    /// `split-freelist` is on; `commit_id` is the txnid of the first meta page, the
    /// transaction's own unless the pages it freed were split.
    fn freelist_save0(&mut self, mc: CRef, split: bool, commit_id: &mut u64) -> Result<()> {
        // `pghead` can grow and shrink during this call; `pglast` and
        // `free_pgs` can only grow. Page numbers cannot disappear from
        // `free_pgs`.
        let maxfree_1pg = self.layout().max_free_1pg(self.psize()) as isize;
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

            if split {
                if freecnt == 0 && !self.w.free_pgs.is_empty() {
                    // Touch the last page of the free DB before deciding:
                    // the pages that frees may make one record too big.
                    match self.page_search_modify(mc, None, crate::core::cursor::PS_LAST) {
                        Err(e) if !e.not_found() => return Err(e),
                        _ => {}
                    }
                }
                // Split only where one record would need more than a page
                // and the txnids stay clear of the end of the range. Write
                // all records again if the list grew since; split a single
                // record that grew past one page while it was written.
                let n = self.w.free_pgs.len().div_ceil(maxfree_1pg as usize);
                let started = *commit_id != txnid;
                if n > 1 && (freecnt < self.w.free_pgs.len() || !started) {
                    if may_split(txnid, n) {
                        (freecnt, *commit_id) =
                            self.save_split_free_pgs(mc, maxfree_1pg as usize)?;
                        continue;
                    }
                    // Once split, never save one record instead: the split
                    // records would stay and list the pages twice. (The
                    // PowerDNS patch does; unreachable: it needs the list
                    // to grow by records after the first pass, at the end
                    // of the txnid range.)
                    if started {
                        return Err(crate::error::Error::errno(libc::EOVERFLOW));
                    }
                }
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
                // C writes the list as it is. A page in it twice would be
                // handed out twice (`KNOWN_C_BUGS.md` row 50).
                if !self.w.free_pgs.is_sorted_desc() {
                    return Err(corrupted());
                }
                let loc = self.data_loc(mc)?;
                let ids = std::mem::take(&mut self.w.free_pgs);
                let written = self.loc_mut(loc).map(|out| ids.encode_into(out));
                self.w.free_pgs = ids;
                if !written? {
                    return Err(corrupted());
                }
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
            let mut ids = self.take_scratch();
            Idl::encode_append(&mop[mop_len - len..mop_len], &mut ids);
            let rc = self.put_at(mc, &id.to_ne_bytes(), PutData::bytes(&ids), MDB_CURRENT);
            self.give_scratch(ids);
            rc?;
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

impl RwTxn<'_> {
    /// Saves the pages this transaction freed in records of at most
    /// `maxfree` page numbers (one page each), under the keys from the
    /// transaction's txnid `T` up (feature `split-freelist`, see the module
    /// documentation). Returns how many pages were saved and the txnid of
    /// the first meta page, `T + (n & !1)` for `n` records (the commit
    /// publishes one more).
    ///
    /// The full records come first, so that a pass after the list grew
    /// rewrites them in place and only the last one grows. The lowest page
    /// numbers go under the lowest key: that record is read first, and
    /// `mdb_page_alloc` prefers low pages. A put may append to the list
    /// (when it touches a page of the free DB), never change the entries
    /// sorted before it, so each record is filled in right after its put;
    /// the caller comes back if the list grew.
    fn save_split_free_pgs(&mut self, mc: CRef, maxfree: usize) -> Result<(usize, u64)> {
        let txnid = self.core.txnid;
        self.w.free_pgs.sort();
        // As for a single record: a page in the list twice would be handed
        // out twice (`KNOWN_C_BUGS.md` row 50).
        if !self.w.free_pgs.is_sorted_desc() {
            return Err(corrupted());
        }
        let freecnt = self.w.free_pgs.len();
        let n = freecnt.div_ceil(maxfree) as u64;
        let mut id = txnid;
        let mut end = freecnt;
        while end > 0 {
            let len = end.min(maxfree);
            let size = (len + 1) * 8;
            self.put_at(mc, &id.to_ne_bytes(), PutData::reserve(size), MDB_RESERVE)?;
            let loc = self.data_loc(mc)?;
            let ids = std::mem::take(&mut self.w.free_pgs);
            let part = &ids.as_slice()[end - len..end];
            let written = self
                .loc_mut(loc)
                .map(|out| Idl::encode_slice_into(part, out));
            self.w.free_pgs = ids;
            if !written? {
                return Err(corrupted());
            }
            end -= len;
            id += 1;
        }
        let commit_id = txnid + (n & !1);
        debug_assert!(id - 1 <= commit_id && commit_id & 1 == txnid & 1);
        Ok((freecnt, commit_id))
    }
}

/// `mdb_find_oldest`: the oldest snapshot any reader uses, or our txnid
/// minus one if there is none older. Without a lock file C looks at no
/// reader at all; here the in-memory table of this process's readers
/// stands in for the lock file's (`KNOWN_C_BUGS.md` row 35).
fn find_oldest(core: &crate::core::txn::TxnCore<'_>) -> u64 {
    core.env.readers().find_oldest(core.txnid)
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

    /// Feature `split-freelist`: transactions that free and reclaim many
    /// pages at once, at small pages, with a reader pinning a snapshot for
    /// a while; the page audit (every page used once) after each commit,
    /// and the pinned snapshot's pages untouched by the later writers.
    #[test]
    fn split_free_pages_audit() {
        use crate::core::audit::{audit, page_use};
        use crate::feature::Feature;
        use crate::format::Format;
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::builder()
            .map_size(64 << 20)
            .format(Format::V10)
            .page_size(256)
            .flags(EnvironmentFlags::NO_SYNC)
            .enable(Feature::SplitFreelist)
            .open(dir.path())
            .unwrap();
        let mut splits = 0;
        let mut pinned = None;
        for round in 0..40u32 {
            let mut txn = env.begin_rw_txn().unwrap();
            let t = txn.id();
            for i in 0..300u32 {
                let k = (i * 7 + round * 13) % 500;
                if (i + round) % 3 == 0 {
                    txn.del(Dbi::MAIN, &k.to_be_bytes(), None).unwrap();
                } else {
                    let v = vec![round as u8; 20 + (i as usize * 37) % 700];
                    txn.put(Dbi::MAIN, &k.to_be_bytes(), &v, WriteFlags::empty())
                        .unwrap();
                }
            }
            let prepared = txn.prepare().unwrap();
            let f = prepared.id();
            prepared.commit().unwrap();
            if f != t {
                // The second meta page's txnid: the first one's is the last
                // split key or one above it.
                assert_eq!((f - t) % 2, 1);
                splits += 1;
                let recs = free_records(&env);
                assert!(recs.iter().filter(|(id, _)| *id >= t).count() > 1);
                assert!(recs.iter().all(|(id, _)| *id < f));
            }
            audit(&env);
            match round % 10 {
                2 => {
                    let txn = env.begin_ro_txn().unwrap();
                    let uses = page_use(&txn);
                    let bytes: Vec<(u64, Vec<u8>)> = uses
                        .tree
                        .iter()
                        .map(|&p| (p, page_bytes(&txn, p)))
                        .collect();
                    pinned = Some((txn, bytes));
                }
                8 => {
                    let (txn, bytes) = pinned.take().unwrap();
                    for (p, b) in bytes {
                        assert_eq!(page_bytes(&txn, p), b, "page {p}");
                    }
                }
                _ => {}
            }
        }
        assert!(splits >= 10, "{splits} split commits");
    }

    /// The first page-size bytes of page `p` in the snapshot of `txn`.
    fn page_bytes(txn: &crate::core::RoTxn<'_>, p: u64) -> Vec<u8> {
        let ps = txn.env().page_size();
        txn.core().page(p).unwrap().bytes()[..ps].to_vec()
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
