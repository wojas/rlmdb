//! Committing a write transaction: `mdb_txn_commit` (mdb.c line 3791),
//! `mdb_page_flush` (line 3394) and `mdb_env_write_meta` (line 3935), and
//! LMDB 1.0's two-phase commit, which splits `_mdb_txn_commit` before the
//! meta page write (`mdb_txn_prepare`, mdb.c 1.0 line 4659; see
//! [`RwTxn::prepare`]).
//!
//! The order matters for crash safety and for concurrent readers, and is
//! C's:
//!
//! 1. store the `MDB_db` record of every named DB changed in this
//!    transaction in the main DB (`F_SUBDATA` nodes);
//! 2. save the free list (`RwTxn::freelist_save`);
//! 3. write all dirty pages with `pwritev`, in page order, up to
//!    `MDB_COMMIT_PAGES` (64) contiguous pages per call;
//! 4. `fdatasync` the data file (`F_FULLFSYNC` on macOS), unless `NO_SYNC`;
//! 5. write the tail of the meta page `txnid % 2` (from `mm_mapsize` on)
//!    through the `O_DSYNC` descriptor (unless `NO_SYNC`/`NO_META_SYNC`;
//!    on macOS an `F_FULLFSYNC` follows); only now does the new snapshot
//!    exist on disk. If the write or the sync fails, the old txnid is
//!    written back and the environment is unusable (`MDB_FATAL_ERROR`);
//! 6. publish the txnid in the lock file, so new readers pick it up
//!    (without one, in the state the file's `NO_LOCK` handles share);
//! 7. with `MDB_PREVSNAPSHOT`, share the exclusive lock and clear the flag
//!    (`Environment::end_prev_snapshot`).
//!
//! A commit that split the pages it freed over several free-list records
//! (feature `split-freelist`, see `freelist`) does steps 5 and 6 twice:
//! its meta page under its txnid `P`, synced also with `NO_META_SYNC`;
//! then the same snapshot once more under `P + 1` into the other meta
//! page, synced too, so that the two meta pages hold consecutive txnids
//! and both describe the committed state, as after two ordinary commits
//! ([`RwTxn::write_meta`]).
//!
//! Steps 1 to 4 are the prepare phase (`RwTxn::prepare0`, what
//! `mdb_txn_prepare` does), 5 to 7 the meta phase (`RwTxn::write_meta`,
//! what `mdb_txn_commit` of a prepared transaction does); a plain commit
//! runs both. Between them the transaction keeps the writer lock, and the
//! file holds what a commit that stopped before its meta page write
//! leaves: the new pages, which no meta page refers to.
//!
//! A transaction without dirty pages commits without writing anything
//! (`MDB_END_EMPTY_COMMIT`).

use crate::cmp::CmpFn;
use crate::consts::*;
use crate::core::cursor_put::PutData;
use crate::core::dbi::{Dbi, DB_DIRTY, DB_USRVALID, DB_VALID};
use crate::core::dirty::PageSlot;
use crate::core::env::Environment;
use crate::core::page_ops;
use crate::core::txn::Transaction;
use crate::core::txn_rw::{RwTxn, TXN_DIRTY, TXN_SPILLS};
use crate::core::wcursor::CRef;
use crate::error::{Error, MdbError, Result};
use crate::flags::DatabaseFlags;
use crate::format::layout::{Layout, META_MAPSIZE, META_SIZE};
use crate::format::meta::Meta;
use crate::format::node::F_SUBDATA;
use crate::format::page::{P_DIRTY, P_KEEP, P_LOOSE, P_OVERFLOW};
use crate::os;

/// `MDB_COMMIT_PAGES`: max number of pages to write in one `pwritev` call
/// (below `IOV_MAX` on every supported platform).
const COMMIT_PAGES: usize = 64;

/// `MAX_WRITE`: max bytes to write in one call.
const MAX_WRITE: usize = 0x4000_0000;

impl<'env> RwTxn<'env> {
    /// `_mdb_txn_commit` for a top-level transaction, up to (not including)
    /// `mdb_txn_end`: the prepare phase, then the meta phase.
    pub(crate) fn commit0(&mut self) -> Result<()> {
        if self.prepare0()? {
            self.write_meta()
        } else {
            Ok(())
        }
    }

    /// The prepare phase of `_mdb_txn_commit` for a top-level transaction:
    /// everything before `mdb_env_write_meta` (steps 1 to 4 of the module
    /// documentation). Returns `false` for a transaction without changes,
    /// which has nothing to write (C's `goto done`, an empty commit).
    pub(crate) fn prepare0(&mut self) -> Result<bool> {
        // (TXN_HAS_CHILD is still set if a nested transaction was leaked
        // with `mem::forget`: its pages are with the child, so nothing may
        // be written.)
        self.check_usable()?;
        // mdb_cursors_close: no user cursor can be alive (they borrow the
        // transaction), but handle-based ones may be.
        self.w.cursors.clear();

        if self.core.dirty.is_empty() && self.w.flags & (TXN_DIRTY | TXN_SPILLS) == 0 {
            return Ok(false);
        }

        self.update_db_records()?;
        self.freelist_save()?;
        self.w.pghead = None;

        self.page_flush()?;
        self.core.env.sync(false)?;
        Ok(true)
    }

    /// Update DB root pointers: write the record of each named DB this
    /// transaction changed into the main DB.
    fn update_db_records(&mut self) -> Result<()> {
        let n = self.core.dbs.get_mut().len();
        if n <= CORE_DBS as usize {
            return Ok(());
        }
        // C uses an untracked cursor; tracking it is equivalent here and
        // keeps it consistent after nested splits (KNOWN_C_BUGS.md row 11).
        let slot = self.open_slot(MAIN_DBI as usize, true);
        let rc = self.update_db_records_with(CRef::main(slot), n);
        self.close_slot(slot);
        rc
    }

    fn update_db_records_with(&mut self, mc: CRef, n: usize) -> Result<()> {
        for i in CORE_DBS as usize..n {
            let e = self.core.dbs.get_mut()[i];
            if e.state & DB_DIRTY == 0 {
                continue;
            }
            if e.seq != self.core.env.dbis.seq_of(i) {
                return Err(MdbError::BadDbi.into());
            }
            let name = self.core.env.dbis.name_of(i).ok_or(MdbError::BadDbi)?;
            let rec: [u8; 48] = e.db.to_bytes();
            self.put_at(mc, &name, PutData::bytes(&rec), u32::from(F_SUBDATA))?;
        }
        Ok(())
    }

    /// `mdb_page_flush(txn, 0)` at commit: write every dirty page to the
    /// data file, clearing `P_DIRTY` in each first. Loose pages (and
    /// `P_KEEP` ones, which lose the mark) stay dirty and in the list;
    /// written pages leave it.
    ///
    /// As in C, one pass over the list prepares the pages, then they are
    /// written; each page is reached through its slot, without a search.
    pub(crate) fn page_flush(&mut self) -> Result<()> {
        let l = self.layout();
        let ps = self.psize();
        let txnid = self.core.txnid;
        let mut runs = std::mem::take(&mut self.w.flush);
        runs.clear();
        self.core.dirty.for_each_mut(|pgno, slot, page| {
            let flags = page_ops::flags(l, page);
            // Don't flush this page yet.
            if flags & (P_LOOSE | P_KEEP) != 0 {
                page_ops::set_flags(l, page, flags & !P_KEEP);
            } else {
                runs.push((pgno, slot, prepare_page(l, ps, page, flags, txnid)));
            }
        });
        let rc = self.write_runs(&runs);
        self.w.flush = runs;
        rc
    }

    /// The writing part of `mdb_page_flush` for the dirty pages `pgnos`
    /// (ascending), used by `mdb_page_spill`: clear `P_DIRTY` in each and
    /// finalise its header for the format, write them and drop them from
    /// the dirty list (see [`RwTxn::page_flush`]).
    pub(crate) fn write_pages(&mut self, pgnos: &[u64]) -> Result<()> {
        let l = self.layout();
        let ps = self.psize();
        let txnid = self.core.txnid;
        let mut runs = Vec::with_capacity(pgnos.len());
        for &pgno in pgnos {
            let slot = self.core.dirty.find(pgno).ok_or(MdbError::Corrupted)?;
            let page = self
                .core
                .dirty
                .at_mut(slot, pgno)
                .ok_or(MdbError::Corrupted)?;
            let flags = page_ops::flags(l, page);
            runs.push((pgno, slot, prepare_page(l, ps, page, flags, txnid)));
        }
        self.write_runs(&runs)
    }

    /// Writes the prepared dirty pages `runs` (page number, slot in the
    /// dirty list, bytes; ascending), contiguous runs batched into one
    /// `pwritev` of up to `MDB_COMMIT_PAGES` buffers (C's `iov` array,
    /// here on the stack too), then drops them from the dirty list
    /// (`mt_dirty_room` grows accordingly).
    fn write_runs(&mut self, runs: &[(u64, PageSlot, usize)]) -> Result<()> {
        let ps = self.psize();
        let env = self.core.env;
        let file = env.data_file();
        let dirty = &self.core.dirty;
        let mut batch: [&[u8]; COMMIT_PAGES] = [&[]; COMMIT_PAGES];
        let mut n = 0;
        let mut wpos = 0u64;
        let mut wsize = 0usize;
        let mut next_pos = u64::MAX;
        for &(pgno, slot, size) in runs {
            let pos = pgno * ps as u64;
            if pos != next_pos || n == COMMIT_PAGES || wsize + size > MAX_WRITE {
                write_batch(file, &batch[..n], wpos, wsize)?;
                env.invalidate_map(wpos, wsize);
                n = 0;
                wpos = pos;
                wsize = 0;
            }
            let buf = dirty.at(slot, pgno).ok_or(MdbError::Corrupted)?;
            batch[n] = buf.get(..size).ok_or(MdbError::Corrupted)?;
            n += 1;
            next_pos = pos + size as u64;
            wsize += size;
        }
        write_batch(file, &batch[..n], wpos, wsize)?;
        env.invalidate_map(wpos, wsize);

        // Drop the written pages from the dirty list (both lists are
        // ascending).
        let mut next = runs.iter().map(|w| w.0).peekable();
        self.core.dirty.retain(|pgno, _| {
            while next.peek().is_some_and(|&w| w < pgno) {
                next.next();
            }
            next.peek() != Some(&pgno)
        });
        self.w.dirty_room += runs.len();
        Ok(())
    }

    /// `mdb_env_write_meta`: write the new meta page (`txnid % 2`) and
    /// publish the txnid. A failed write makes the environment unusable
    /// (`MDB_FATAL_ERROR`; later transactions fail with `MDB_PANIC`), after
    /// trying to put the old txnid back so the half-written page is not
    /// used. A failed sync after the write (macOS: the `F_FULLFSYNC` after
    /// the `O_DSYNC` write) is the same failure: the new page is in the
    /// file but the commit is not, so it gets the same treatment, where C
    /// returns the error and leaves the page (`KNOWN_C_BUGS.md` row 41). The
    /// txnid is published only after both succeeded.
    ///
    /// Without a lock file the txnid is published in the state the file's
    /// `NO_LOCK` handles share (`registry::LocalShared`). Readers copy the
    /// meta page of the txnid they find there and re-check it afterwards,
    /// as with `mti_txnid` (`RoTxn::take_snapshot`), so it must not name
    /// the page being rewritten: first it is set to the snapshot this
    /// transaction began from (on the other meta page), where it stays if
    /// the write or the sync fails. It already is that unless another
    /// process committed since this process last published a commit.
    ///
    /// # A commit that split the pages it freed
    ///
    /// The txnid is the one the transaction commits as: its own, or a
    /// higher one `P` of the same parity if the free-list save split the
    /// pages it freed (feature `split-freelist`, see the module
    /// documentation of `freelist`). Such a commit:
    ///
    /// 1. writes its meta page under `P` through the synchronous
    ///    descriptor, also with `NO_META_SYNC`;
    /// 2. publishes `P`;
    /// 3. writes the same snapshot once more, under `P + 1`, into the
    ///    other meta page (the one that held the snapshot it began from),
    ///    synced too;
    /// 4. publishes `P + 1`.
    ///
    /// The order matters: `P` is published before the other page is
    /// rewritten, so that a reader copying that page notices (it
    /// re-checks the published txnid). Both writes are synced, so that a
    /// writer that takes over after this process dies finds the new
    /// snapshot on disk before it may reuse the old snapshot's pages; if
    /// this process died between the writes, the old snapshot is still on
    /// the other page until that writer's commit
    /// (`docs/split-freelist-details.md`, section 5.2).
    ///
    /// If the second write fails, the commit stands (`P` is durable and
    /// published) and returns success. The page is not written back: if
    /// the duplicate reached the file it is as good as `P`, and it is
    /// published. The environment is marked unusable in this process, as
    /// after any failed meta write. The file is consistent either way, and
    /// the next writer's commit replaces the page
    /// (`docs/split-freelist-details.md`).
    pub(crate) fn write_meta(&mut self) -> Result<()> {
        let env = self.core.env;
        let txnid = self.core.txnid;
        let began_as = self.w.split_from.unwrap_or(txnid);
        let toggle = (txnid & 1) as usize;
        // Persist any increases of mapsize config. Meta page `toggle ^ 1`
        // is the one this transaction began from (it holds `began_as - 1`),
        // and only a writer changes meta pages, so its map size is still
        // the one read at begin.
        let mapsize = self.w.base_mapsize.max(env.map_size() as u64);
        let dbs = self.core.dbs.borrow();
        let meta = Meta {
            magic: MDB_MAGIC,
            version: env.layout().data_version,
            address: 0,
            map_size: mapsize,
            dbs: [dbs[FREE_DBI as usize].db, dbs[MAIN_DBI as usize].db],
            last_pgno: self.core.next_pgno - 1,
            txnid,
        };
        drop(dbs);
        // `metab`: what to write back if the write or the sync fails, the
        // new page with the old `mm_last_pg` and `mm_txnid`, as in C.
        let restore = env.meta_ids(toggle).map(|(last_pgno, txnid)| Meta {
            last_pgno,
            txnid,
            ..meta
        });
        if let Some(local) = env.local_shared() {
            local.set_txnid(began_as.wrapping_sub(1));
        }
        let split = self.w.split_from.is_some();
        write_meta_page(env, toggle, &meta, restore.as_ref(), split)?;
        env.publish_txnid(txnid);
        let mut published = txnid;
        if split {
            // The other meta page still holds the snapshot this transaction
            // began from; the split records list its pages, which the next
            // writer may reuse. Replace it with this snapshot under the
            // next txnid, so that nothing names it any more.
            debug_assert_eq!(
                env.meta_ids(toggle ^ 1).map(|(_, t)| t),
                Some(began_as.wrapping_sub(1))
            );
            let dup = Meta {
                txnid: txnid + 1,
                ..meta
            };
            if write_duplicate_meta(env, &dup) {
                env.publish_txnid(dup.txnid);
                published = dup.txnid;
            }
        }
        // C reaches this after a plain commit's meta page write and after a
        // prepared transaction's (`prepared:` in `_mdb_txn_commit`).
        env.end_prev_snapshot(published)
    }

    /// The txnid the commit of this transaction publishes: its txnid, or
    /// one more if the free-list save split the pages it freed (feature
    /// `split-freelist`: the second meta page's, see [`RwTxn::write_meta`]).
    pub(crate) fn commit_id(&self) -> u64 {
        self.core.txnid + u64::from(self.w.split_from.is_some())
    }

    /// `mdb_txn_prepare` (LMDB 1.0's two-phase commit): write everything
    /// this transaction changed to the data file, synced as a commit
    /// syncs it, but not the meta page, which makes the changes the
    /// current snapshot; [`PreparedTxn::commit`] writes that, or
    /// [`PreparedTxn::abort`] (or dropping it) gives the changes up. The
    /// prepared transaction holds the writer lock until then, so no other
    /// writer, in this process or another, can commit in between: the
    /// meta page write is all that is left, and `lmdb.h` assumes it
    /// cannot fail once the data pages are written. Read transactions
    /// see the snapshot this transaction began from until the commit, as
    /// they would during a commit.
    ///
    /// This is the commit up to the meta page (see the module
    /// documentation of `commit`): the database records of changed named
    /// databases are stored, the free list is saved, the dirty pages are
    /// written and the data file is synced (unless
    /// [`NO_SYNC`](crate::EnvironmentFlags::NO_SYNC)); prepare and commit
    /// together write exactly what a [`commit`](RwTxn::commit) writes.
    /// Nothing changes for other processes before the commit: a crash or
    /// an abort in between leaves the data file as a commit that stopped
    /// before its meta page write leaves it: the new pages are in the
    /// file (and may have made it longer), but no meta page refers to
    /// them, and the next writer reuses them.
    ///
    /// The transaction is consumed: its cursors (which borrow it) are gone
    /// already, and nothing but [`PreparedTxn::commit`] and
    /// [`PreparedTxn::abort`] can use it any more. On failure the
    /// transaction is aborted, as a commit that fails is.
    ///
    /// A transaction with nothing to write (no change) ends here as an
    /// empty commit does: nothing is written, the writer lock is released,
    /// and the [`PreparedTxn`]'s commit and abort have nothing left to do.
    /// (C's `mdb_txn_prepare` does the same; its `mdb_txn_commit` then
    /// fails, `KNOWN_C_BUGS.md` row 61.)
    ///
    /// A nested transaction cannot be prepared, `EINVAL` (its changes are
    /// written with its top-level transaction; C's `mdb_txn_prepare`
    /// commits it into its parent then): it is aborted.
    ///
    /// ```
    /// # let dir = tempfile::tempdir().unwrap();
    /// # let env = rlmdb::Environment::builder().open(dir.path()).unwrap();
    /// # let db = rlmdb::Dbi::MAIN;
    /// use rlmdb::{Transaction, WriteFlags};
    /// # let mut txn = env.begin_rw_txn()?;
    /// # txn.put(db, b"key", b"old", WriteFlags::empty())?;
    /// # txn.commit()?;
    /// let mut txn = env.begin_rw_txn()?;
    /// txn.put(db, b"key", b"new", WriteFlags::empty())?;
    /// let prepared = txn.prepare()?;
    /// // The data pages are on disk; the other participants prepare too.
    /// // The ID the commit will publish:
    /// let id = prepared.id();
    /// prepared.commit()?;
    /// // If another participant fails now, the commit can be rolled back:
    /// env.rollback(id)?;
    /// assert_eq!(env.begin_ro_txn()?.get(db, b"key")?, Some(&b"old"[..]));
    /// # Ok::<(), rlmdb::Error>(())
    /// ```
    pub fn prepare(mut self) -> Result<PreparedTxn<'env>> {
        let id = self.core.txnid;
        if self.nested.is_some() {
            return Err(Error::einval());
        }
        match self.prepare0() {
            // The ID the commit publishes: the free-list save may have
            // raised it (feature `split-freelist`).
            Ok(true) => Ok(PreparedTxn {
                id: self.commit_id(),
                txn: Some(self),
            }),
            Ok(false) => {
                self.end(true);
                Ok(PreparedTxn { txn: None, id })
            }
            Err(e) => Err(e),
        }
    }
}

/// A write transaction whose changes are written to the data file but not
/// committed yet: [`RwTxn::prepare`], the first phase of LMDB 1.0's
/// two-phase commit (`mdb_txn_prepare`). The second phase is
/// [`commit`](PreparedTxn::commit) or [`abort`](PreparedTxn::abort);
/// dropping it aborts.
///
/// It holds the writer lock of the environment until it ends, as the
/// [`RwTxn`] did: no other write transaction, of this process or another,
/// begins before, and [`Environment::rollback`] and
/// [`Environment::set_map_size`] wait or fail as they do while a write
/// transaction is open. Like an [`RwTxn`], it must end on the thread that
/// began it, so it is not `Send`:
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<rlmdb::PreparedTxn<'static>>();
/// ```
///
/// Nothing can be read or written through it (C: `MDB_TXN_PREPARE` makes
/// every other operation fail with `MDB_BAD_TXN`), and it cannot be
/// prepared again (C: `MDB_TXN_PENDING`): it has no such methods.
#[derive(Debug)]
pub struct PreparedTxn<'env> {
    /// The transaction after its prepare phase; `None` if the prepare
    /// ended it (it had nothing to write).
    txn: Option<RwTxn<'env>>,
    /// The txnid its commit publishes (the one it began with if it has
    /// nothing to write).
    id: u64,
}

impl PreparedTxn<'_> {
    /// The transaction's ID (`mdb_txn_id`): the txnid the commit publishes
    /// (the ID [`Environment::rollback`] takes, where the commit can be
    /// rolled back). A transaction with nothing to write publishes
    /// nothing; this is the ID it had.
    ///
    /// This is the final ID. With the feature
    /// [`split-freelist`](crate::Feature::SplitFreelist) it may be higher
    /// than the [`Transaction::id`](crate::Transaction::id) the transaction
    /// had before the prepare: a transaction that freed many pages saves
    /// them in several free-list records, one ID each, and commits under
    /// a higher ID. Only the prepare decides this, so a program that needs
    /// the ID its commit publishes (to tell its own commits from those of
    /// other writers) prepares and then commits, instead of committing at
    /// once. Such a commit cannot be undone with
    /// [`Environment::rollback`] afterwards; a program that needs that
    /// aborts the prepared transaction when the ID is higher than it was.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// `mdb_txn_commit` of a prepared transaction: the meta phase. Writes
    /// the meta page (synced unless
    /// [`NO_SYNC`](crate::EnvironmentFlags::NO_SYNC) or
    /// [`NO_META_SYNC`](crate::EnvironmentFlags::NO_META_SYNC)), which
    /// makes the changes the current snapshot, publishes the txnid to
    /// readers and releases the writer lock. Databases created in the
    /// transaction become visible to other transactions, as with
    /// [`RwTxn::commit`].
    ///
    /// If the write (or the sync after it) fails, the error is returned and
    /// the transaction is aborted. The meta page is then put back as far
    /// as the disk allows (its old txnid, as C does), the data file keeps
    /// the new pages, which nothing refers to, and the environment is
    /// unusable in this process: every later transaction fails with
    /// [`MdbError::Panic`] (`MDB_FATAL_ERROR`) until it is opened again.
    ///
    /// A transaction that split the pages it freed (feature
    /// [`split-freelist`](crate::Feature::SplitFreelist)) writes two meta
    /// pages, both synced also with `NO_META_SYNC`, and publishes
    /// [`id`](PreparedTxn::id) with the second. If the second write fails,
    /// the commit stands under the first txnid and returns success; the
    /// environment is unusable in this process all the same (see
    /// `RwTxn::write_meta` and `docs/split-freelist-details.md`).
    pub fn commit(mut self) -> Result<()> {
        let Some(mut txn) = self.txn.take() else {
            return Ok(());
        };
        let rc = txn.write_meta();
        txn.end(rc.is_ok());
        rc
    }

    /// `mdb_txn_abort` of a prepared transaction: give the changes up. The
    /// meta page is not written, so the file holds what a crash between
    /// the phases leaves (see [`RwTxn::prepare`]), and the next writer
    /// reuses the pages. Same as dropping it.
    pub fn abort(self) {}

    /// Whether the prepare ended the transaction (it had nothing to
    /// write): then commit and abort do nothing. Not part of the API: the
    /// C API's handle answers as C's ended transaction does.
    #[doc(hidden)]
    pub fn ended(&self) -> bool {
        self.txn.is_none()
    }

    /// The transaction's checks of handle `db`, as an operation that
    /// needs a user database (`user`, C's `DB_USRVALID`) or any valid one
    /// (`DB_VALID`) makes them before it looks at the transaction's state:
    /// `EINVAL` if it names no database of the transaction (also once the
    /// prepare ended it), `MdbError::BadDbi` if it was closed since. Not
    /// part of the API: C's operations on a prepared transaction make
    /// these checks before they fail with `MDB_BAD_TXN`.
    #[doc(hidden)]
    pub fn check_dbi(&self, db: Dbi, user: bool) -> Result<()> {
        let txn = self.txn.as_ref().ok_or_else(Error::einval)?;
        let validity = if user { DB_USRVALID } else { DB_VALID };
        txn.core.dbi_entry(db, validity).map(drop)
    }

    /// [`Transaction::db_flags`] of the transaction (`EINVAL` once the
    /// prepare ended it). Not part of the API: C's `mdb_dbi_flags`,
    /// `mdb_cmp` and `mdb_dcmp` still work on a prepared transaction.
    #[doc(hidden)]
    pub fn db_flags(&self, db: Dbi) -> Result<DatabaseFlags> {
        let txn = self.txn.as_ref().ok_or_else(Error::einval)?;
        txn.db_flags(db)
    }

    /// [`Transaction::set_compare`] (`dup`: [`Transaction::set_dupsort`])
    /// of the transaction (`EINVAL` once the prepare ended it). Not part
    /// of the API: C's `mdb_set_compare` and `mdb_set_dupsort` still work
    /// on a prepared transaction (they set the handle's function).
    #[doc(hidden)]
    pub fn set_comparator(&self, db: Dbi, cmp: Option<CmpFn>, dup: bool) -> Result<()> {
        let txn = self.txn.as_ref().ok_or_else(Error::einval)?;
        if dup {
            txn.set_dupsort(db, cmp)
        } else {
            txn.set_compare(db, cmp)
        }
    }
}

/// `mdb_env_write_meta` for the meta page `toggle` (0 or 1), whose new
/// contents are `meta` (used by the commit and by
/// [`Environment::rollback`]): writes its tail, from `mm_mapsize` on,
/// through the `O_DSYNC` descriptor unless the environment has `NO_SYNC`
/// or, without `force_sync`, `NO_META_SYNC` (macOS: then an
/// `F_FULLFSYNC` too). `force_sync` is for a commit that split its free
/// pages (feature `split-freelist`, see `freelist`). If the write or
/// that sync fails, `restore` (if any) is written over it with the plain
/// descriptor, so that the page in the page cache is not used, the
/// environment is marked unusable (`MDB_FATAL_ERROR`), and the error is
/// returned. Publishing a txnid is the caller's business.
pub(crate) fn write_meta_page(
    env: &Environment,
    toggle: usize,
    meta: &Meta,
    restore: Option<&Meta>,
    force_sync: bool,
) -> Result<()> {
    let ps = env.page_size();
    let flags = env.raw_flags();
    let off = (toggle * ps + env.layout().meta_offset() + META_MAPSIZE) as u64;
    let mut buf = [0u8; META_SIZE];
    meta.write(&mut buf);
    // Write to the SYNC fd unless NO_SYNC/NO_META_SYNC. (The meta fd goes
    // to the same file, but writing to it also syncs to disk.)
    let no_sync = if force_sync {
        MDB_NOSYNC
    } else {
        MDB_NOSYNC | MDB_NOMETASYNC
    };
    let sync_fd = flags & no_sync == 0;
    let fd = match (sync_fd, env.meta_file()) {
        (true, Some(f)) => f,
        _ => env.data_file(),
    };
    let written = os::write_all_at(fd, &buf[META_MAPSIZE..], off).and_then(|()| {
        if cfg!(any(target_os = "macos", target_os = "ios")) && sync_fd {
            // O_DSYNC does not flush the drive cache on macOS.
            if let Some(f) = env.meta_file() {
                os::fdatasync(f)?;
            }
        }
        env.injected_meta_sync_error()
    });
    if let Err(e) = written {
        // On a failure, the page cache still contains the new data.
        // Write some old data back, to prevent it from being used.
        if let Some(old) = restore {
            old.write(&mut buf);
            let _ = os::write_all_at(env.data_file(), &buf[META_MAPSIZE..], off);
        }
        env.set_fatal_error();
        return Err(e.into());
    }
    env.invalidate_map((toggle * ps) as u64, ps);
    Ok(())
}

/// Writes `meta`, the snapshot of the newest meta page once more under the
/// next txnid, into meta page `meta.txnid & 1`, through the synchronous
/// descriptor unless the environment has `NO_SYNC` (macOS: an
/// `F_FULLFSYNC` too): the second meta page of a commit that split the
/// pages it freed (feature `split-freelist`, [`RwTxn::write_meta`]).
/// Returns whether the page holds it afterwards. The caller has published `meta.txnid - 1`, the newest
/// snapshot, so no reader copies this page unchecked while it is written.
///
/// Unlike [`write_meta_page`], a failure writes nothing back: the page
/// then holds either the old snapshot or this one, both valid, and this
/// one is the better content (the old snapshot's pages may be reused). If
/// the page reads back with `meta.txnid`, the write reached the file and
/// the caller may publish it. Either way the environment is marked
/// unusable in this process (`MDB_FATAL_ERROR`), as after any failed meta
/// page write: the disk failed a write.
pub(crate) fn write_duplicate_meta(env: &Environment, meta: &Meta) -> bool {
    let ps = env.page_size();
    let toggle = (meta.txnid & 1) as usize;
    let off = (toggle * ps + env.layout().meta_offset() + META_MAPSIZE) as u64;
    let mut buf = [0u8; META_SIZE];
    meta.write(&mut buf);
    let sync_fd = env.raw_flags() & MDB_NOSYNC == 0;
    let fd = match (sync_fd, env.meta_file()) {
        (true, Some(f)) => f,
        _ => env.data_file(),
    };
    let written = os::write_all_at(fd, &buf[META_MAPSIZE..], off).and_then(|()| {
        if cfg!(any(target_os = "macos", target_os = "ios")) && sync_fd {
            if let Some(f) = env.meta_file() {
                os::fdatasync(f)?;
            }
        }
        env.injected_meta_sync_error()
    });
    env.invalidate_map((toggle * ps) as u64, ps);
    match written {
        Ok(()) => true,
        Err(_) => {
            env.set_fatal_error();
            env.meta_ids(toggle).is_some_and(|(_, t)| t == meta.txnid)
        }
    }
}

/// Prepares dirty page `page` (flags `flags`, pages of `ps` bytes) for
/// writing as `mdb_page_flush` does: clears `P_DIRTY` and finalises the
/// header for the format
/// ([`Layout::stamp_clean_page`](crate::format::Layout::stamp_clean_page)).
/// Returns how many bytes to write (the whole run for overflow pages).
fn prepare_page(l: &Layout, ps: usize, page: &mut [u8], flags: u16, txnid: u64) -> usize {
    page_ops::set_flags(l, page, flags & !P_DIRTY);
    l.stamp_clean_page(page, txnid);
    let npages = if flags & P_OVERFLOW != 0 {
        page_ops::overflow_pages(l, page).unwrap_or(1) as usize
    } else {
        1
    };
    npages * ps
}

/// Writes one batch of contiguous pages at `pos` (`pwritev`, or `pwrite`
/// for a single buffer). A short write is `EIO`, as in C.
fn write_batch(file: &std::fs::File, bufs: &[&[u8]], pos: u64, size: usize) -> Result<()> {
    match bufs.len() {
        0 => Ok(()),
        1 => Ok(os::write_all_at(file, bufs[0], pos)?),
        _ => {
            let n = os::pwritev(file, bufs, pos)?;
            if n != size {
                return Err(crate::error::Error::errno(libc::EIO));
            }
            Ok(())
        }
    }
}
