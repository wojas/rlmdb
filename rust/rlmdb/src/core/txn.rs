//! Read transactions: pinning a snapshot in the reader table.
//!
//! Beginning a read transaction (`mdb_txn_begin(MDB_RDONLY)` →
//! `mdb_txn_renew0`, mdb.c line 2758):
//!
//! 1. Claim a reader slot (`LockRegion::acquire_slot`). Like C with
//!    `MDB_NOTLS`, every transaction owns its slot; [`RoTxn::reset`] keeps it
//!    for a later [`InactiveTxn::renew`].
//! 2. Publish the last committed txnid (`mti_txnid`) in the slot, re-reading
//!    until stable. From then on writers will not reuse pages this snapshot
//!    needs.
//! 3. Read the meta page of that txnid (`txnid & 1`) and take the free/main
//!    DB records and `last_pgno` from it.
//!
//! Step 3 races with writers: a writer committing txn `N+2` rewrites the
//! meta page our txn `N` uses. C accepts that (very unlikely) torn read.
//! We additionally re-check `mti_txnid` after copying the meta: if it is
//! still `N`, no writer can have started rewriting that page, so the copy is
//! consistent; otherwise we retry with the newer snapshot.
//!
//! If the snapshot's pages extend past our map (another process grew the
//! database beyond our `map_size`), begin fails with `MDB_MAP_RESIZED`.

use std::cell::RefCell;
use std::sync::Arc;

use crate::cmp::{CmpFn, Comparator};
use crate::consts::*;
use crate::core::cursor::{self, Cursor, RoCursor};
use crate::core::dbi::{init_txn_dbs, Dbi, TxnDb, DB_USRVALID, DB_VALID};
use crate::core::dirty::{DirtyPages, Layer, PageSlot, SpillSet};
use crate::core::env::Environment;
use crate::core::Stat;
use crate::error::{Error, MdbError, Result};
use crate::flags::DatabaseFlags;
use crate::format::meta::Meta;
use crate::format::{Layout, PageRef};
use crate::os::DataMap;

/// Capacity of the page stacks of the temporary cursors of
/// [`Transaction::get`] (`mdb_get`); see [`TxnCore::get`]. Only trees of
/// very long keys on small pages get deeper: with 16-byte keys a tree of
/// 4 KiB pages has 4 levels at a million entries, with 511-byte keys
/// (7 per branch page) it passes 8 levels at a few hundred thousand.
const GET_STACK: usize = 8;

/// Operations shared by read and write transactions.
pub trait Transaction {
    /// The environment this transaction belongs to (`mdb_txn_env`).
    fn env(&self) -> &Environment;

    /// The transaction id (`mdb_txn_id`): for a read transaction, the id of
    /// the snapshot it reads.
    fn id(&self) -> u64;

    /// `mdb_dbi_open` without `MDB_CREATE`: the handle of an existing
    /// database (`None` = the main DB). `MdbError::NotFound` if it does not
    /// exist in this transaction's snapshot.
    fn open_db(&self, name: Option<&str>) -> Result<Dbi> {
        self.open_db_bytes(name.map(str::as_bytes))
    }

    /// [`Transaction::open_db`] for a name given as bytes: C database
    /// names are arbitrary bytes without NUL (`EINVAL` for a name with a
    /// NUL byte), not necessarily UTF-8.
    fn open_db_bytes(&self, name: Option<&[u8]>) -> Result<Dbi>;

    /// `mdb_set_compare(txn, db, cmp)`: order the keys of `db` with `cmp`
    /// (`None` = the default order again). As in C the setting belongs to
    /// the environment's handle; unlike [`Environment::set_compare`] it
    /// also takes effect in this transaction at once. Use it before
    /// anything is read or written with the handle, and use the same order
    /// in every process (`lmdb.h`). `EINVAL` for the free DB or an invalid
    /// handle.
    fn set_compare(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()>;

    /// `mdb_set_dupsort(txn, db, cmp)`: as [`Transaction::set_compare`] for
    /// the order of duplicate data items (`DUP_SORT` databases). Cursors
    /// opened earlier keep the order they started with, as in C.
    fn set_dupsort(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()>;

    /// The comparators this transaction uses for `db`: the key order and,
    /// for `DUP_SORT` databases, the duplicate order (what `mdb_cmp` and
    /// `mdb_dcmp` call).
    fn comparators(&self, db: Dbi) -> Result<(Comparator, Option<Comparator>)>;

    /// `mdb_get`: the value stored under `key`, or `None`. For `DUP_SORT`
    /// databases, the first data item of the key.
    fn get<'txn>(&'txn self, db: Dbi, key: &[u8]) -> Result<Option<&'txn [u8]>>;

    /// `mdb_cursor_open`: a cursor over `db`.
    fn cursor<'txn>(&'txn self, db: Dbi) -> Result<RoCursor<'txn>>;

    /// `mdb_dbi_flags`: the persistent flags of `db`.
    fn db_flags(&self, db: Dbi) -> Result<DatabaseFlags>;

    /// `mdb_stat`: statistics of `db` in this transaction.
    fn stat(&self, db: Dbi) -> Result<Stat>;
}

/// The state every transaction has: its snapshot and its view of the DBI
/// table (`MDB_txn` minus the write-only parts).
///
/// All page access goes through `TxnCore::page` and
/// `TxnCore::overflow_data`. A write transaction keeps its dirty pages in
/// `dirty`, which both consult first (`mdb_page_get`), so cursors work
/// unchanged on read and write transactions. A read transaction has no
/// written pages, so both go straight to the map.
pub(crate) struct TxnCore<'env> {
    pub(crate) env: &'env Environment,
    /// `true` for a read transaction (`MDB_RDONLY`): `dirty`, `spilled`
    /// and `parents` stay empty, and page lookups skip them.
    read_only: bool,
    /// The environment's page size (`me_psize`), used on every lookup.
    page_size: usize,
    /// The environment's page layout, given to every page this
    /// transaction hands out.
    pub(crate) layout: &'static Layout,
    /// The data map current when the transaction began. Kept alive by the
    /// transaction so that [`Environment::set_map_size`] cannot invalidate
    /// the slices it hands out.
    pub(crate) map: Arc<DataMap>,
    /// `mt_txnid`.
    pub(crate) txnid: u64,
    /// `mt_next_pgno`: first page not in this snapshot (write transactions:
    /// first page never allocated).
    pub(crate) next_pgno: u64,
    /// `mt_dbs` + `mt_dbflags` (+ seqs and comparators), one per DBI slot.
    pub(crate) dbs: RefCell<Vec<TxnDb>>,
    /// `mt_u.dirty_list`: pages written by a write transaction. Always
    /// empty for read transactions.
    pub(crate) dirty: DirtyPages,
    /// `mt_spill_pgs`: pages this write transaction wrote to the data file
    /// before commit to make room in the dirty list (`mdb_page_spill`).
    pub(crate) spilled: SpillSet,
    /// For a nested transaction: the dirty and spilled pages of its parent,
    /// grandparent, … (nearest first), consulted after its own.
    pub(crate) parents: Vec<Layer>,
}

/// Where `mdb_page_get` finds a page of a write transaction, and at which
/// `level` (1 = this transaction, 2 = its parent, …).
pub(crate) enum PageSource<'a> {
    /// In the dirty list of the transaction at `level` (the whole run for
    /// overflow pages).
    Dirty(&'a [u8], usize),
    /// Spilled by the transaction at `level`: read it from the map.
    Spilled(usize),
    /// Not written by this transaction or an ancestor (level 0): the map.
    Clean,
}

impl<'env> TxnCore<'env> {
    pub(crate) fn new(env: &'env Environment, dbs: Vec<TxnDb>) -> TxnCore<'env> {
        TxnCore {
            env,
            read_only: false,
            page_size: env.page_size(),
            layout: env.layout(),
            map: env.data_map(),
            txnid: 0,
            next_pgno: 0,
            dbs: RefCell::new(dbs),
            dirty: DirtyPages::new(),
            spilled: SpillSet::new(),
            parents: Vec::new(),
        }
    }

    /// [`TxnCore::new`] for a read transaction.
    fn new_read_only(env: &'env Environment, dbs: Vec<TxnDb>) -> TxnCore<'env> {
        TxnCore {
            read_only: true,
            ..TxnCore::new(env, dbs)
        }
    }

    /// The `mt_parent` walk of `mdb_page_get`: at each level the spill list
    /// is checked before the dirty list, so a page this transaction spilled
    /// is read from the map even if a parent still has an older dirty copy.
    #[inline]
    pub(crate) fn locate(&self, pgno: u64) -> PageSource<'_> {
        self.locate_in(pgno, true)
    }

    /// [`TxnCore::locate`], searching this transaction's own dirty list
    /// only if `own_dirty`.
    #[inline]
    fn locate_in(&self, pgno: u64, own_dirty: bool) -> PageSource<'_> {
        if !self.spilled.is_empty() && self.spilled.contains(&pgno) {
            return PageSource::Spilled(1);
        }
        if own_dirty {
            if let Some(buf) = self.dirty.get(pgno) {
                return PageSource::Dirty(buf, 1);
            }
        }
        for (i, layer) in self.parents.iter().enumerate() {
            if !layer.spilled.is_empty() && layer.spilled.contains(&pgno) {
                return PageSource::Spilled(i + 2);
            }
            if let Some(buf) = layer.dirty.get(pgno) {
                return PageSource::Dirty(buf, i + 2);
            }
        }
        PageSource::Clean
    }

    /// The `level` output of `mdb_page_get` (see [`PageSource`]).
    pub(crate) fn page_level(&self, pgno: u64) -> usize {
        match self.locate(pgno) {
            PageSource::Dirty(_, level) | PageSource::Spilled(level) => level,
            PageSource::Clean => 0,
        }
    }

    /// The written buffer of `pgno` (dirty here or in an ancestor), if any.
    #[inline]
    fn dirty_buf(&self, pgno: u64) -> Option<&[u8]> {
        match self.locate(pgno) {
            PageSource::Dirty(buf, _) => Some(buf),
            _ => None,
        }
    }

    /// `me_maxpg` for this transaction's map: the number of pages it holds.
    pub(crate) fn max_pgno(&self) -> u64 {
        (self.map.len() / self.env.page_size()) as u64
    }

    /// Installs the snapshot described by `meta` (the tail of
    /// `mdb_txn_renew0`).
    pub(crate) fn set_snapshot(&mut self, txnid: u64, meta: &Meta) -> Result<()> {
        self.txnid = txnid;
        self.next_pgno = meta.last_pgno.wrapping_add(1);
        init_txn_dbs(&self.env.dbis, self.dbs.get_mut(), &meta.dbs);
        if self.env.has_fatal_error() {
            return Err(MdbError::Panic.into());
        }
        if self.max_pgno() < self.next_pgno {
            return Err(MdbError::MapResized.into());
        }
        Ok(())
    }

    /// `mdb_page_get`: page `pgno` of this snapshot: a dirty page of this
    /// transaction or an ancestor, else the mapped page. Meta pages are
    /// never returned (they change under readers), nor pages past the
    /// snapshot's end (`MDB_PAGE_NOTFOUND`).
    #[inline]
    pub(crate) fn page(&self, pgno: u64) -> Result<PageRef<'_>> {
        if self.read_only {
            self.mapped_page(pgno)
        } else {
            self.written_or_mapped_page(pgno)
        }
    }

    /// [`TxnCore::page`] for a page of a write cursor, whose slot in this
    /// transaction's dirty list may be known (`slot`, a hint checked
    /// against `pgno`; see `super::dirty`). A page found there is the one
    /// `mdb_page_get` returns: a page in a transaction's own dirty list is
    /// never in its spill set.
    #[inline]
    pub(crate) fn page_by_slot(&self, pgno: u64, slot: PageSlot) -> Result<PageRef<'_>> {
        match self.dirty.at(slot, pgno) {
            Some(buf) => PageRef::with_layout(&buf[..self.page_size], self.layout)
                .ok_or_else(|| MdbError::Corrupted.into()),
            None => self.page(pgno),
        }
    }

    /// [`TxnCore::page`] in a write transaction for a page the caller
    /// already looked for in this transaction's own dirty list, and did not
    /// find: the same page, without searching that list again.
    pub(crate) fn page_not_dirty_here(&self, pgno: u64) -> Result<PageRef<'_>> {
        match self.locate_in(pgno, false) {
            PageSource::Dirty(buf, _) => PageRef::with_layout(&buf[..self.page_size], self.layout)
                .ok_or_else(|| MdbError::Corrupted.into()),
            _ => self.mapped_page(pgno),
        }
    }

    /// [`TxnCore::page`] for a write transaction.
    fn written_or_mapped_page(&self, pgno: u64) -> Result<PageRef<'_>> {
        if let Some(buf) = self.dirty_buf(pgno) {
            return PageRef::with_layout(&buf[..self.page_size], self.layout)
                .ok_or_else(|| MdbError::Corrupted.into());
        }
        self.mapped_page(pgno)
    }

    /// The mapped page `pgno` of the snapshot: the end of `mdb_page_get`.
    #[inline]
    fn mapped_page(&self, pgno: u64) -> Result<PageRef<'_>> {
        if pgno < NUM_METAS as u64 {
            return Err(MdbError::Corrupted.into());
        }
        if pgno >= self.next_pgno {
            return Err(MdbError::PageNotFound.into());
        }
        let ps = self.page_size;
        let off = usize::try_from(pgno)
            .ok()
            .and_then(|p| p.checked_mul(ps))
            .ok_or(MdbError::PageNotFound)?;
        let bytes = self.map.slice(off, ps).ok_or(MdbError::PageNotFound)?;
        PageRef::with_layout(bytes, self.layout).ok_or_else(|| MdbError::Corrupted.into())
    }

    /// The data of an `F_BIGDATA` node: `len` bytes starting right after
    /// the page header of the overflow run at `pgno` (`mdb_node_read`).
    pub(crate) fn overflow_data(&self, pgno: u64, len: usize) -> Result<&[u8]> {
        let ps = self.page_size;
        let npages = self.layout.overflow_pages_for(len, ps) as u64;
        let written = if self.read_only {
            None
        } else {
            self.dirty_buf(pgno)
        };
        let bytes = match written {
            Some(buf) => buf,
            None => {
                if pgno < NUM_METAS as u64 {
                    return Err(MdbError::Corrupted.into());
                }
                if pgno
                    .checked_add(npages)
                    .is_none_or(|end| end > self.next_pgno)
                {
                    return Err(MdbError::PageNotFound.into());
                }
                let off = (pgno as usize)
                    .checked_mul(ps)
                    .ok_or(MdbError::PageNotFound)?;
                self.map
                    .slice(off, npages as usize * ps)
                    .ok_or(MdbError::PageNotFound)?
            }
        };
        let page = PageRef::with_layout(bytes, self.layout).ok_or(MdbError::Corrupted)?;
        if !page.is_overflow() {
            return Err(MdbError::Corrupted.into());
        }
        page.overflow_data(len)
            .ok_or_else(|| MdbError::Corrupted.into())
    }

    /// `mdb_cursor_open`.
    pub(crate) fn cursor(&self, dbi: Dbi) -> Result<RoCursor<'_>> {
        let entry = self.fresh_dbi(dbi, DB_VALID)?;
        Ok(RoCursor::new(self, dbi, &entry))
    }

    /// `mdb_get`: a `MDB_SET` on a temporary cursor.
    ///
    /// For a tree of at most [`GET_STACK`] levels (`md_depth`) the cursor,
    /// and the sub-cursor of a `MDB_DUPSORT` database, get stacks of that
    /// size instead of C's [`CURSOR_STACK`], which spares filling the
    /// unused slots on every call (see [`Cursor`]). A tree deeper than its
    /// record says (a corrupt one), or a sub-DB of duplicates deeper than
    /// the short stack, makes that search fail with `MDB_CURSOR_FULL`; the
    /// lookup is then repeated with full stacks. That gives exactly C's
    /// result: the search only reads the snapshot.
    pub(crate) fn get(&self, dbi: Dbi, key: &[u8]) -> Result<Option<&[u8]>> {
        let entry = self.fresh_dbi(dbi, DB_USRVALID)?;
        if usize::from(entry.db.depth) <= GET_STACK {
            match self.get_with_stack::<GET_STACK>(&entry, key) {
                Err(Error::Mdb(MdbError::CursorFull)) => {}
                rc => return rc,
            }
        }
        self.get_deep(&entry, key)
    }

    /// [`TxnCore::get`] with full stacks.
    #[cold]
    #[inline(never)]
    fn get_deep(&self, entry: &TxnDb, key: &[u8]) -> Result<Option<&[u8]>> {
        self.get_with_stack::<CURSOR_STACK>(entry, key)
    }

    /// The lookup of [`TxnCore::get`] with cursors of `N` levels.
    #[inline]
    fn get_with_stack<const N: usize>(&self, entry: &TxnDb, key: &[u8]) -> Result<Option<&[u8]>> {
        let mut mc = Cursor::<N>::unpositioned(entry.db, entry.key_cmp, 0, self.layout);
        let mut mx = cursor::new_xcursor::<N>(entry, self.layout);
        match cursor::cursor_set(
            self,
            &mut mc,
            mx.as_mut(),
            entry.dup_cmp,
            key,
            None,
            cursor::CursorOp::Set,
            true,
        ) {
            Ok((kv, _)) => Ok(Some(kv.data)),
            Err(e) if e.not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// A read-only transaction (`MDB_txn` with `MDB_RDONLY`).
///
/// Pins one snapshot until it is dropped, [`aborted`](RoTxn::abort),
/// [`committed`](RoTxn::commit) or [`reset`](RoTxn::reset). Not tied to a
/// thread: it may be sent to another thread (`Send`).
pub struct RoTxn<'env> {
    core: TxnCore<'env>,
    /// Our reader slot, if the environment has a lock file.
    slot: Option<usize>,
}

impl std::fmt::Debug for RoTxn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoTxn")
            .field("txnid", &self.core.txnid)
            .field("slot", &self.slot)
            .finish()
    }
}

impl<'env> RoTxn<'env> {
    /// `mdb_txn_begin(env, NULL, MDB_RDONLY)`.
    pub(crate) fn begin(env: &'env Environment) -> Result<RoTxn<'env>> {
        let slot = match env.lock_region() {
            Some(lock) => Some(lock.acquire_slot()?),
            None => None,
        };
        // Built before the snapshot is taken so that Drop releases the slot
        // if anything below fails.
        let mut txn = RoTxn {
            core: TxnCore::new_read_only(env, Vec::new()),
            slot,
        };
        txn.take_snapshot()?;
        Ok(txn)
    }

    /// The read-only branch of `mdb_txn_renew0`.
    fn take_snapshot(&mut self) -> Result<()> {
        let env = self.core.env;
        let (txnid, meta) = match (env.lock_region(), self.slot) {
            (Some(lock), Some(slot)) => loop {
                let txnid = lock.publish_current_txnid(slot);
                if txnid == 0 && env.is_read_only() {
                    let meta = env.pick_meta()?;
                    lock.set_slot_txnid(slot, meta.txnid);
                    break (meta.txnid, meta);
                }
                let meta = env.read_meta((txnid & 1) as usize)?;
                // If no commit happened while we copied the meta page, no
                // writer can have been rewriting it (see module docs).
                if lock.txnid() == txnid {
                    break (txnid, meta);
                }
            },
            _ => {
                let meta = env.pick_meta()?;
                (meta.txnid, meta)
            }
        };
        self.core.set_snapshot(txnid, &meta)
    }

    /// `mdb_txn_abort`: end the transaction and release its reader slot.
    /// Same as dropping it.
    pub fn abort(self) {}

    /// `mdb_txn_commit` for a read-only transaction: ends it. Handles opened
    /// in it stay valid (they always do, see [`crate::core::dbi`]).
    pub fn commit(self) -> Result<()> {
        Ok(())
    }

    /// `mdb_txn_reset`: release the snapshot but keep the reader slot, so
    /// that [`InactiveTxn::renew`] is cheap.
    pub fn reset(mut self) -> InactiveTxn<'env> {
        let slot = self.slot.take();
        if let (Some(lock), Some(i)) = (self.core.env.lock_region(), slot) {
            lock.reset_slot(i);
        }
        let dbs = std::mem::take(self.core.dbs.get_mut());
        InactiveTxn {
            env: self.core.env,
            slot,
            dbs,
        }
    }

    /// The transaction's snapshot state.
    pub(crate) fn core(&self) -> &TxnCore<'env> {
        &self.core
    }

    /// The reader slot index this transaction occupies, if any.
    pub fn reader_slot(&self) -> Option<usize> {
        self.slot
    }
}

impl Drop for RoTxn<'_> {
    /// `mdb_txn_end(MDB_END_ABORT|MDB_END_SLOT)`: clear the slot's txnid and
    /// give the slot back.
    fn drop(&mut self) {
        if let (Some(lock), Some(i)) = (self.core.env.lock_region(), self.slot) {
            lock.release_slot(i);
        }
    }
}

impl Transaction for RoTxn<'_> {
    fn env(&self) -> &Environment {
        self.core.env
    }

    fn id(&self) -> u64 {
        self.core.txnid
    }

    fn open_db_bytes(&self, name: Option<&[u8]>) -> Result<Dbi> {
        self.core.open_db(name)
    }

    fn set_compare(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()> {
        self.core.set_comparator(db, cmp, false)
    }

    fn set_dupsort(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()> {
        self.core.set_comparator(db, cmp, true)
    }

    fn comparators(&self, db: Dbi) -> Result<(Comparator, Option<Comparator>)> {
        self.core.comparators(db)
    }

    fn get<'txn>(&'txn self, db: Dbi, key: &[u8]) -> Result<Option<&'txn [u8]>> {
        self.core.get(db, key)
    }

    fn cursor<'txn>(&'txn self, db: Dbi) -> Result<RoCursor<'txn>> {
        self.core.cursor(db)
    }

    fn db_flags(&self, db: Dbi) -> Result<DatabaseFlags> {
        self.core.db_flags(db)
    }

    fn stat(&self, db: Dbi) -> Result<Stat> {
        self.core.db_stat(db)
    }
}

/// A reset read transaction (`mdb_txn_reset`): owns a reader slot but no
/// snapshot. [`renew`](InactiveTxn::renew) it to read the latest snapshot.
pub struct InactiveTxn<'env> {
    env: &'env Environment,
    slot: Option<usize>,
    dbs: Vec<TxnDb>,
}

impl std::fmt::Debug for InactiveTxn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InactiveTxn")
            .field("slot", &self.slot)
            .finish()
    }
}

impl<'env> InactiveTxn<'env> {
    /// `mdb_txn_renew`: start reading the latest snapshot again, reusing the
    /// reader slot. `MdbError::BadRslot` if the slot was taken from us (e.g.
    /// cleared by a reader check in another process).
    pub fn renew(mut self) -> Result<RoTxn<'env>> {
        let slot = self.slot.take();
        let dbs = std::mem::take(&mut self.dbs);
        let mut txn = RoTxn {
            core: TxnCore::new_read_only(self.env, dbs),
            slot,
        };
        if let (Some(lock), Some(i)) = (self.env.lock_region(), slot) {
            if !lock.slot_is_idle_and_ours(i) {
                // Not ours any more: do not release it on drop.
                txn.slot = None;
                return Err(MdbError::BadRslot.into());
            }
        }
        txn.take_snapshot()?;
        Ok(txn)
    }

    /// End for good, releasing the reader slot. Same as dropping it.
    pub fn abort(self) {}
}

impl Drop for InactiveTxn<'_> {
    fn drop(&mut self) {
        if let (Some(lock), Some(i)) = (self.env.lock_region(), self.slot) {
            lock.release_slot(i);
        }
    }
}
