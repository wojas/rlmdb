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

use crate::consts::*;
use crate::core::cursor::{self, Cursor, RoCursor};
use crate::core::dbi::{init_txn_dbs, Dbi, TxnDb, DB_USRVALID, DB_VALID};
use crate::core::env::Environment;
use crate::core::Stat;
use crate::error::{MdbError, Result};
use crate::flags::DatabaseFlags;
use crate::format::meta::Meta;
use crate::format::{overflow_pages_for, PageRef};

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
    fn open_db(&self, name: Option<&str>) -> Result<Dbi>;

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
/// `TxnCore::overflow_data`. A write transaction (later stage) plugs its
/// dirty-page store in there, so cursors work unchanged on both.
pub(crate) struct TxnCore<'env> {
    pub(crate) env: &'env Environment,
    /// `mt_txnid`.
    pub(crate) txnid: u64,
    /// `mt_next_pgno`: first page not in this snapshot.
    pub(crate) next_pgno: u64,
    /// `mt_dbs` + `mt_dbflags` (+ seqs and comparators), one per DBI slot.
    pub(crate) dbs: RefCell<Vec<TxnDb>>,
}

impl<'env> TxnCore<'env> {
    fn new(env: &'env Environment, dbs: Vec<TxnDb>) -> TxnCore<'env> {
        TxnCore {
            env,
            txnid: 0,
            next_pgno: 0,
            dbs: RefCell::new(dbs),
        }
    }

    /// Installs the snapshot described by `meta` (the tail of
    /// `mdb_txn_renew0`).
    fn set_snapshot(&mut self, txnid: u64, meta: &Meta) -> Result<()> {
        self.txnid = txnid;
        self.next_pgno = meta.last_pgno.wrapping_add(1);
        init_txn_dbs(&self.env.dbis, self.dbs.get_mut(), &meta.dbs);
        if self.env.max_pgno() < self.next_pgno {
            return Err(MdbError::MapResized.into());
        }
        Ok(())
    }

    /// `mdb_page_get`: page `pgno` of this snapshot. Meta pages are never
    /// returned (they change under readers), nor pages past the snapshot's
    /// end (`MDB_PAGE_NOTFOUND`).
    pub(crate) fn page(&self, pgno: u64) -> Result<PageRef<'_>> {
        if pgno < NUM_METAS as u64 {
            return Err(MdbError::Corrupted.into());
        }
        if pgno >= self.next_pgno {
            return Err(MdbError::PageNotFound.into());
        }
        let ps = self.env.page_size();
        let off = usize::try_from(pgno)
            .ok()
            .and_then(|p| p.checked_mul(ps))
            .ok_or(MdbError::PageNotFound)?;
        let bytes = self
            .env
            .data_map()
            .slice(off, ps)
            .ok_or(MdbError::PageNotFound)?;
        PageRef::new(bytes).ok_or_else(|| MdbError::Corrupted.into())
    }

    /// The data of an `F_BIGDATA` node: `len` bytes starting 16 bytes into
    /// the overflow run at `pgno` (`mdb_node_read`).
    pub(crate) fn overflow_data(&self, pgno: u64, len: usize) -> Result<&[u8]> {
        let ps = self.env.page_size();
        let npages = overflow_pages_for(len, ps) as u64;
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
        let bytes = self
            .env
            .data_map()
            .slice(off, npages as usize * ps)
            .ok_or(MdbError::PageNotFound)?;
        let page = PageRef::new(bytes).ok_or(MdbError::Corrupted)?;
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
    pub(crate) fn get(&self, dbi: Dbi, key: &[u8]) -> Result<Option<&[u8]>> {
        let entry = self.fresh_dbi(dbi, DB_USRVALID)?;
        let mut mc = Cursor::new(entry.db, entry.key_cmp, 0);
        let mut mx = cursor::new_xcursor(&entry);
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
            core: TxnCore::new(env, Vec::new()),
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

    fn open_db(&self, name: Option<&str>) -> Result<Dbi> {
        self.core.open_db(name)
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
            core: TxnCore::new(self.env, dbs),
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
