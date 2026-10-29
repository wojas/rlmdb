//! Write transactions: [`RwTxn`] and [`RwCursor`].
//!
//! Life cycle (C function, mdb.c line):
//!
//! * [`Environment::begin_rw_txn`] = `mdb_txn_begin` + the write branch of
//!   `mdb_txn_renew0` (line 2758): lock the shared writer mutex (after the
//!   in-process writer state, see `WriterState`), take the last committed
//!   txnid from the lock file (`mti_txnid`), read its meta page, and start
//!   txn `last + 1` from it.
//! * Changes go to owned page copies (`super::dirty`); every read of the
//!   transaction sees them because `TxnCore::page` looks there first.
//! * [`RwTxn::commit`] = `mdb_txn_commit` (line 3791, see `super::commit`):
//!   write named DB records into the main DB, save the free list, write the
//!   dirty pages, sync, write the meta page, publish the txnid.
//! * [`RwTxn::abort`] or dropping the transaction = `mdb_txn_abort` /
//!   `mdb_txn_end`: discard the dirty pages, close handles of databases
//!   created by the transaction, unlock.
//!
//! An `RwTxn` is `!Send`: on Linux the writer mutex is a pthread mutex owned
//! by the locking thread, so the transaction must end on the thread that
//! began it.
//!
//! Not implemented yet (stage 3): nested transactions, spilling dirty pages
//! to disk when the dirty list is full (a transaction that would need to
//! spill fails with `MDB_TXN_FULL` instead), `mdb_drop`.

use std::sync::MutexGuard;

use crate::consts::*;
use crate::core::cursor::C_SUB;
use crate::core::cursor_put::{PutData, MDB_NOSPILL};
use crate::core::dbi::{
    Dbi, DbiRegistry, DbiSlot, NamedDb, TxnDb, DB_DIRTY, DB_NEW, DB_USRVALID, DB_VALID,
};
use crate::core::env::{Environment, WriterState};
use crate::core::reader::LockRegion;
use crate::core::ro_cursor::{CursorOp, Entry, RoCursor};
use crate::core::txn::{Transaction, TxnCore};
use crate::core::wcursor::{read_cursors, write_back, Arena, CRef};
use crate::core::Stat;
use crate::error::{Error, MdbError, Result};
use crate::flags::{DatabaseFlags, WriteFlags};
use crate::format::layout::PAGE_HDR_SIZE;
use crate::format::Db;
use crate::idl::Idl;

/// `MDB_TXN_FINISHED`: the transaction ended.
pub(crate) const TXN_FINISHED: u32 = 0x01;
/// `MDB_TXN_ERROR`: an operation failed half-way; only abort is allowed.
pub(crate) const TXN_ERROR: u32 = 0x02;
/// `MDB_TXN_DIRTY`: must write a meta page even without dirty pages.
pub(crate) const TXN_DIRTY: u32 = 0x04;
/// `MDB_TXN_SPILLS`: the transaction (or a parent) spilled pages (stage 3).
pub(crate) const TXN_SPILLS: u32 = 0x08;
/// `MDB_TXN_HAS_CHILD`: a nested transaction is active (stage 3).
pub(crate) const TXN_HAS_CHILD: u32 = 0x10;
/// `MDB_TXN_BLOCKED`: most operations are illegal.
pub(crate) const TXN_BLOCKED: u32 = TXN_FINISHED | TXN_ERROR | TXN_HAS_CHILD;

/// The write-only part of `MDB_txn`, plus the free-page state C keeps in
/// `MDB_env` while a writer runs (`me_pghead`, `me_pglast`).
#[derive(Debug, Default)]
pub(crate) struct WriteState {
    /// `mt_flags` (`TXN_*`).
    pub(crate) flags: u32,
    /// `mt_free_pgs`: pages this transaction stopped using. Unsorted until
    /// `mdb_freelist_save` writes it under this transaction's id.
    pub(crate) free_pgs: Idl,
    /// `mt_loose_pgs`: dirty pages freed again in this transaction, reused
    /// first by `mdb_page_alloc` (last freed first). They stay in the dirty
    /// list, marked `P_LOOSE`.
    pub(crate) loose: Vec<u64>,
    /// `mt_dirty_room`: how many more pages may become dirty.
    pub(crate) dirty_room: usize,
    /// `me_pghead`: pages reclaimed from the free DB, descending; `None`
    /// until the first record is read (C's `NULL`, which some code tests).
    pub(crate) pghead: Option<Idl>,
    /// `me_pglast`: id of the last free DB record merged into `pghead`.
    pub(crate) pglast: u64,
    /// `me_pgoldest`, copied from the environment for the transaction.
    pub(crate) pg_oldest: u64,
    /// The cursors of this transaction (`mt_cursors`).
    pub(crate) cursors: Arena,
}

/// The writer locks a write transaction holds: the in-process writer state
/// and, unless `MDB_NOLOCK`, the shared writer mutex. Dropping it unlocks.
struct WriterLock<'env> {
    state: MutexGuard<'env, WriterState>,
    shared: Option<&'env LockRegion>,
}

impl Drop for WriterLock<'_> {
    fn drop(&mut self) {
        if let Some(lock) = self.shared {
            lock.unlock_writer();
        }
    }
}

/// A read-write transaction (`MDB_txn` without `MDB_RDONLY`).
///
/// Only one exists per environment at a time, across all processes. Ends
/// with [`commit`](RwTxn::commit), [`abort`](RwTxn::abort) or drop (which
/// aborts). All read operations of [`Transaction`] work and see the
/// transaction's own changes.
///
/// It must end on the thread that began it, so it is not `Send`:
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<lmdb_core::RwTxn<'static>>();
/// ```
pub struct RwTxn<'env> {
    pub(crate) core: TxnCore<'env>,
    pub(crate) w: WriteState,
    lock: Option<WriterLock<'env>>,
}

impl std::fmt::Debug for RwTxn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RwTxn")
            .field("txnid", &self.core.txnid)
            .field("dirty_pages", &self.core.dirty.len())
            .field("flags", &self.w.flags)
            .finish()
    }
}

/// A cursor handle of a write transaction for the handle-based cursor API
/// ([`RwTxn::open_cursor`] & co., which the C API binds). Stale handles
/// (closed, or from another transaction) are rejected with `EINVAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CursorId {
    slot: u32,
    gen: u32,
}

/// Where the data of a put landed (for `MDB_RESERVE`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DataLoc {
    pub(crate) pgno: u64,
    pub(crate) start: usize,
    pub(crate) len: usize,
}

impl<'env> RwTxn<'env> {
    /// `mdb_txn_begin(env, NULL, 0)` (see the module documentation).
    pub(crate) fn begin(env: &'env Environment) -> Result<RwTxn<'env>> {
        if env.is_read_only() {
            return Err(Error::errno(libc::EACCES));
        }
        let state = env.lock_writer_state();
        let shared = env.lock_region();
        if let Some(lock) = shared {
            lock.lock_writer(|| env.pick_meta().map(|m| m.txnid))?;
        }
        // From here on, dropping `lock` unlocks again.
        let lock = WriterLock { state, shared };
        let (last, meta) = match shared {
            Some(l) => {
                let t = l.txnid();
                (t, env.read_meta((t & 1) as usize)?)
            }
            None => {
                let m = env.pick_meta()?;
                (m.txnid, m)
            }
        };
        let mut core = TxnCore::new(env, Vec::new());
        core.set_snapshot(last.wrapping_add(1), &meta)?;
        let pg_oldest = lock.state.pg_oldest;
        Ok(RwTxn {
            core,
            w: WriteState {
                dirty_room: MDB_IDL_UM_MAX,
                pg_oldest,
                ..WriteState::default()
            },
            lock: Some(lock),
        })
    }

    /// `mdb_txn_end` for a top-level write transaction: export (`keep`) or
    /// close the handles of databases this transaction created
    /// (`mdb_dbis_update`), drop the dirty pages and cursors, unlock.
    fn end(&mut self, keep: bool) {
        if self.w.flags & TXN_FINISHED != 0 {
            return;
        }
        let registry = &self.core.env.dbis;
        for (i, e) in self
            .core
            .dbs
            .get_mut()
            .iter()
            .enumerate()
            .skip(CORE_DBS as usize)
        {
            if e.state & DB_NEW != 0 {
                registry.finish_new(i, keep, e.db.flags);
            }
        }
        self.w.cursors.clear();
        self.core.dirty.clear();
        self.w.free_pgs.clear();
        self.w.loose.clear();
        self.w.pghead = None;
        self.w.pglast = 0;
        self.w.flags = TXN_FINISHED;
        if let Some(mut lock) = self.lock.take() {
            lock.state.pg_oldest = self.w.pg_oldest;
        }
    }

    /// `mdb_txn_abort`: discard all changes. Same as dropping the
    /// transaction.
    pub fn abort(self) {}

    /// `mdb_txn_commit`: make all changes durable (unless `NO_SYNC`) and
    /// visible to new transactions. On failure the transaction is aborted
    /// and nothing changes on disk that a reader could see.
    pub fn commit(mut self) -> Result<()> {
        let rc = self.commit0();
        self.end(rc.is_ok());
        rc
    }

    /// `MDB_TXN_BLOCKED` → `MDB_BAD_TXN`.
    pub(crate) fn check_usable(&self) -> Result<()> {
        if self.w.flags & TXN_BLOCKED != 0 {
            return Err(MdbError::BadTxn.into());
        }
        Ok(())
    }

    /// `TXN_DBI_EXIST(txn, dbi, DB_USRVALID)` for a write operation, then
    /// the stale-record refresh of `mdb_cursor_init`. Returns the index.
    fn user_dbi(&self, db: Dbi) -> Result<usize> {
        self.core.dbi_entry(db, DB_USRVALID)?;
        self.check_usable()?;
        self.core.fresh_dbi(db, DB_USRVALID)?;
        Ok(db.0 as usize)
    }

    // --- databases ---------------------------------------------------------------

    /// `mdb_dbi_open` (mdb.c line 9955) in a write transaction: the handle
    /// of database `name` (`None` = the main DB), created with the
    /// persistent bits of `flags` if it does not exist and `flags`
    /// contains [`DatabaseFlags::CREATE`].
    ///
    /// Errors as C: `MDB_NOTFOUND` (does not exist, no `CREATE`),
    /// `MDB_DBS_FULL`, `MDB_INCOMPATIBLE` (the name holds plain data, or
    /// the main DB is `DUP_SORT`/`INTEGER_KEY` so it cannot hold names),
    /// `EINVAL` for unknown flags. For the main DB, persistent flags are
    /// added to its record (and committed).
    ///
    /// A database created here becomes visible to other transactions'
    /// handles at commit; an abort closes its handle. A handle of an
    /// existing database is valid immediately and stays valid (see
    /// [`crate::core::dbi`]).
    pub fn create_db(&mut self, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi> {
        let flags = flags.bits();
        if flags & !VALID_FLAGS != 0 {
            return Err(Error::einval());
        }
        self.check_usable()?;
        let Some(name) = name else {
            self.update_main_flags(flags);
            return Ok(Dbi::MAIN);
        };
        let name = name.as_bytes();
        let env = self.core.env;
        // The registry stays locked until the slot is registered. Nothing
        // below touches the registry: the main DB is never stale.
        let mut slots = env.dbis.write();
        let mut unused = None;
        for (i, slot) in slots.iter().enumerate().skip(CORE_DBS as usize) {
            match &slot.name {
                None => {
                    unused.get_or_insert(i);
                }
                Some(n) if &**n == name => return Ok(Dbi(i as u32)),
                Some(_) => {}
            }
        }
        if unused.is_none() && slots.len() >= env.dbis.capacity() {
            return Err(MdbError::DbsFull.into());
        }
        let main = self.core.dbs.borrow()[MAIN_DBI as usize];
        if u32::from(main.db.flags) & (MDB_DUPSORT | MDB_INTEGERKEY) != 0 {
            return Err(if flags & MDB_CREATE != 0 {
                MdbError::Incompatible
            } else {
                MdbError::NotFound
            }
            .into());
        }
        let found = match self.core.find_named_db(name) {
            Ok(NamedDb::Found(db)) => Some(db),
            Ok(NamedDb::Missing) => None,
            Err(e) if e.not_found() => None,
            Err(e) => return Err(e),
        };
        let (db, state) = match found {
            Some(db) => (db, DB_VALID | DB_USRVALID),
            None => {
                if flags & MDB_CREATE == 0 {
                    return Err(MdbError::NotFound.into());
                }
                let db = Db::empty((flags & PERSISTENT_FLAGS) as u16);
                let slot = self.open_slot(MAIN_DBI as usize, true);
                let rc = self.put_at(
                    CRef::main(slot),
                    name,
                    PutData::bytes(&db.to_bytes()),
                    u32::from(crate::format::node::F_SUBDATA),
                );
                self.close_slot(slot);
                rc?;
                (db, DB_NEW | DB_VALID | DB_USRVALID | DB_DIRTY)
            }
        };
        // Got info, register DBI in this txn and the environment.
        let entry = DbiSlot {
            name: Some(name.into()),
            flags: (u32::from(db.flags) & PERSISTENT_FLAGS) as u16,
            valid: state & DB_NEW == 0,
            custom_cmp: None,
            custom_dcmp: None,
        };
        let i = match unused {
            Some(i) => {
                slots[i] = entry;
                i
            }
            None => {
                slots.push(entry);
                slots.len() - 1
            }
        };
        let seq = env.dbis.bump_seq(i);
        let (key_cmp, dup_cmp) = DbiRegistry::comparators(&slots[i], i, db.flags);
        drop(slots);
        self.core.dbs.get_mut()[i] = TxnDb {
            db,
            state,
            seq,
            key_cmp,
            dup_cmp,
        };
        Ok(Dbi(i as u32))
    }

    /// The main-DB branch of `mdb_dbi_open`: add persistent flags to the
    /// main DB's record (`MDB_TXN_DIRTY` makes sure they get committed) and
    /// pick its comparators again (`mdb_default_cmp`).
    fn update_main_flags(&mut self, flags: u32) {
        let f2 = (flags & PERSISTENT_FLAGS) as u16;
        let main = MAIN_DBI as usize;
        let e = &mut self.core.dbs.get_mut()[main];
        if e.db.flags | f2 != e.db.flags {
            e.db.flags |= f2;
            self.w.flags |= TXN_DIRTY;
        }
        let new_flags = e.db.flags;
        let slots = self.core.env.dbis.read();
        let (key_cmp, dup_cmp) = DbiRegistry::comparators(&slots[main], main, new_flags);
        drop(slots);
        let e = &mut self.core.dbs.get_mut()[main];
        e.key_cmp = key_cmp;
        e.dup_cmp = dup_cmp;
    }

    // --- writes --------------------------------------------------------------------

    /// `mdb_put`: store `data` under `key` in `db`.
    ///
    /// Flags: [`WriteFlags::NO_OVERWRITE`] (`MdbError::KeyExist` if the key
    /// exists), [`WriteFlags::NO_DUP_DATA`] (`KeyExist` if the pair exists,
    /// `DUP_SORT` only), [`WriteFlags::APPEND`] / [`WriteFlags::APPEND_DUP`]
    /// (the key/data sorts after all existing ones, else `KeyExist`). Other
    /// flags give `EINVAL`; use [`RwTxn::reserve`] for `MDB_RESERVE`.
    ///
    /// Size limits as C: keys 1..=511 bytes, `DUP_SORT` data 1..=511 bytes
    /// (and equal sizes for `DUP_FIXED`), otherwise `MdbError::BadValSize`.
    pub fn put(&mut self, db: Dbi, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()> {
        if flags.bits() & MDB_RESERVE != 0 {
            return Err(Error::einval());
        }
        self.mdb_put(db, key, PutData::bytes(data), flags.bits())
            .map(|_| ())
    }

    /// `mdb_put` with `MDB_RESERVE`: make room for `len` bytes of data under
    /// `key` and return them for the caller to fill in. The slice is valid
    /// until the next change in this transaction (enforced by the borrow).
    ///
    /// Not for `DUP_SORT` databases (`EINVAL`), as documented for C.
    pub fn reserve(
        &mut self,
        db: Dbi,
        key: &[u8],
        len: usize,
        flags: WriteFlags,
    ) -> Result<&mut [u8]> {
        let loc = self.mdb_put(db, key, PutData::reserve(len), flags.bits() | MDB_RESERVE)?;
        let loc = loc.ok_or_else(|| Error::from(MdbError::Corrupted))?;
        self.loc_mut(loc)
    }

    /// `mdb_put` (mdb.c line 9231): a temporary tracked cursor and
    /// `mdb_cursor_put`. Returns where the data went for `MDB_RESERVE`.
    fn mdb_put(
        &mut self,
        db: Dbi,
        key: &[u8],
        data: PutData<'_>,
        flags: u32,
    ) -> Result<Option<DataLoc>> {
        self.core.dbi_entry(db, DB_USRVALID)?;
        if flags & !(MDB_NOOVERWRITE | MDB_NODUPDATA | MDB_RESERVE | MDB_APPEND | MDB_APPENDDUP)
            != 0
        {
            return Err(Error::einval());
        }
        let dbi = self.user_dbi(db)?;
        self.check_reserve(dbi, flags)?;
        let slot = self.open_slot(dbi, true);
        let c = CRef::main(slot);
        let rc = self.put_at(c, key, data, flags);
        let loc = match rc {
            Ok(_) if flags & MDB_RESERVE != 0 => self.data_loc(c).map(Some),
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        };
        self.close_slot(slot);
        loc
    }

    /// `MDB_RESERVE` is documented as not allowed with `MDB_DUPSORT`; C
    /// does not check and misbehaves, we refuse it.
    fn check_reserve(&self, dbi: usize, flags: u32) -> Result<()> {
        if flags & MDB_RESERVE != 0
            && u32::from(self.core.dbs.borrow()[dbi].db.flags) & MDB_DUPSORT != 0
        {
            return Err(Error::einval());
        }
        Ok(())
    }

    /// The location of the data of the node under cursor `c` (after a put,
    /// the item just written): inline in a dirty leaf, or on its overflow
    /// pages.
    pub(crate) fn data_loc(&self, c: CRef) -> Result<DataLoc> {
        let page = self.top_page(c)?;
        let ki = self.cur(c).ki_top();
        let node = page.node(ki).ok_or(MdbError::Corrupted)?;
        if node.is_bigdata() {
            return Ok(DataLoc {
                pgno: node.overflow_pgno().ok_or(MdbError::Corrupted)?,
                start: PAGE_HDR_SIZE,
                len: node.data_size(),
            });
        }
        let range =
            crate::core::page_ops::node_data_range(page.bytes(), ki).ok_or(MdbError::Corrupted)?;
        Ok(DataLoc {
            pgno: self.cur(c).pg_top(),
            start: range.start,
            len: range.len(),
        })
    }

    /// The bytes at a [`DataLoc`] (in a dirty page).
    pub(crate) fn loc_mut(&mut self, loc: DataLoc) -> Result<&mut [u8]> {
        self.dirty_mut(loc.pgno)?
            .get_mut(loc.start..loc.start + loc.len)
            .ok_or_else(|| MdbError::Corrupted.into())
    }

    /// `mdb_del`: delete `key` from `db`; for a `DUP_SORT` database only the
    /// pair `key`/`data` if `data` is given, else all of the key's items.
    /// Returns `false` if there was nothing to delete (`MDB_NOTFOUND`).
    pub fn del(&mut self, db: Dbi, key: &[u8], data: Option<&[u8]>) -> Result<bool> {
        self.core.dbi_entry(db, DB_USRVALID)?;
        let dbi = self.user_dbi(db)?;
        let dupsort = u32::from(self.core.dbs.borrow()[dbi].db.flags) & MDB_DUPSORT != 0;
        // Must ignore any data unless DUP_SORT.
        let data = if dupsort { data } else { None };
        match self.del0(dbi, key, data, 0) {
            Ok(()) => Ok(true),
            Err(e) if e.not_found() => Ok(false),
            Err(e) => Err(e),
        }
    }

    // --- cursors ------------------------------------------------------------------

    /// `mdb_cursor_open` in a write transaction: a cursor over `db` that
    /// can write. It borrows the transaction mutably; use the handle-based
    /// [`RwTxn::open_cursor`] API for several cursors at once.
    pub fn cursor_mut(&mut self, db: Dbi) -> Result<RwCursor<'_>> {
        let id = self.open_cursor(db)?;
        Ok(RwCursor {
            txn: self,
            id,
            dbi: db,
        })
    }

    /// `mdb_cursor_open`: a tracked cursor on `db`, named by a handle. The
    /// cursor stays valid across all changes made through this transaction
    /// (it is adjusted as C adjusts `mt_cursors`) until
    /// [`RwTxn::close_cursor`] or the end of the transaction. `EINVAL` for
    /// the free DB, as in C.
    pub fn open_cursor(&mut self, db: Dbi) -> Result<CursorId> {
        self.core.dbi_entry(db, DB_VALID)?;
        self.check_usable()?;
        if db == Dbi::FREE {
            return Err(Error::einval());
        }
        self.core.fresh_dbi(db, DB_VALID)?;
        let slot = self.open_slot(db.0 as usize, true);
        let gen = self.w.cursors.slot(slot).map_or(0, |s| s.gen);
        Ok(CursorId {
            slot: slot as u32,
            gen,
        })
    }

    /// `mdb_cursor_close`.
    pub fn close_cursor(&mut self, id: CursorId) {
        if let Ok(slot) = self.cursor_slot(id) {
            self.close_slot(slot);
        }
    }

    fn cursor_slot(&self, id: CursorId) -> Result<usize> {
        match self.w.cursors.slot(id.slot as usize) {
            Some(s) if s.gen == id.gen && s.tracked => Ok(id.slot as usize),
            _ => Err(Error::einval()),
        }
    }

    /// `mdb_cursor_get` on a handle (see [`RwCursor::get`]).
    pub fn cursor_get(
        &mut self,
        id: CursorId,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Option<Entry<'_>>> {
        let slot = self.cursor_slot(id)?;
        self.check_usable()?;
        let c = CRef::main(slot);
        let dbi = self.cur(c).dbi;
        let dcmp = self.core.dbs.borrow()[dbi].dup_cmp;
        let core = &self.core;
        let (mc, mx) = read_cursors(core, &self.w.cursors, c)?;
        let mut ro = RoCursor::from_parts(core, Dbi(dbi as u32), mc, mx, dcmp);
        let res = ro.get(key, data, op);
        let (mc, mx) = ro.parts();
        write_back(&mut self.w.cursors, c, mc, mx);
        res
    }

    /// `mdb_cursor_put` on a handle (see [`RwCursor::put`]).
    pub fn cursor_put(
        &mut self,
        id: CursorId,
        key: &[u8],
        data: &[u8],
        flags: WriteFlags,
    ) -> Result<()> {
        if flags.bits() & (MDB_RESERVE | MDB_MULTIPLE) != 0 {
            return Err(Error::einval());
        }
        let slot = self.cursor_slot(id)?;
        self.cursor_put_checked(slot, key, PutData::bytes(data), flags.bits())
            .map(|_| ())
    }

    /// `mdb_cursor_put` with `MDB_RESERVE` on a handle.
    pub fn cursor_reserve(
        &mut self,
        id: CursorId,
        key: &[u8],
        len: usize,
        flags: WriteFlags,
    ) -> Result<&mut [u8]> {
        if flags.bits() & MDB_MULTIPLE != 0 {
            return Err(Error::einval());
        }
        let slot = self.cursor_slot(id)?;
        let dbi = self.cur(CRef::main(slot)).dbi;
        self.check_reserve(dbi, MDB_RESERVE)?;
        self.cursor_put_checked(slot, key, PutData::reserve(len), flags.bits() | MDB_RESERVE)?;
        let loc = self.data_loc(CRef::main(slot))?;
        self.loc_mut(loc)
    }

    /// `mdb_cursor_put` with `MDB_MULTIPLE` on a handle: store `count`
    /// items of `size` bytes each, packed in `items`, as duplicates of
    /// `key` (`DUP_FIXED` only). Returns how many were stored.
    pub fn cursor_put_multiple(
        &mut self,
        id: CursorId,
        key: &[u8],
        items: &[u8],
        size: usize,
        flags: WriteFlags,
    ) -> Result<usize> {
        if flags.bits() & MDB_RESERVE != 0 {
            return Err(Error::einval());
        }
        let count = items.len().checked_div(size).unwrap_or(0);
        let slot = self.cursor_slot(id)?;
        self.cursor_put_checked(
            slot,
            key,
            PutData::multiple(items, size, count),
            flags.bits() | MDB_MULTIPLE,
        )
    }

    fn cursor_put_checked(
        &mut self,
        slot: usize,
        key: &[u8],
        data: PutData<'_>,
        flags: u32,
    ) -> Result<usize> {
        let valid = MDB_NOOVERWRITE
            | MDB_NODUPDATA
            | MDB_CURRENT
            | MDB_RESERVE
            | MDB_APPEND
            | MDB_APPENDDUP
            | MDB_MULTIPLE;
        if flags & !valid != 0 {
            return Err(Error::einval());
        }
        self.put_at(CRef::main(slot), key, data, flags & !MDB_NOSPILL)
    }

    /// `mdb_cursor_del` on a handle (see [`RwCursor::del`]).
    pub fn cursor_del(&mut self, id: CursorId, flags: WriteFlags) -> Result<()> {
        let slot = self.cursor_slot(id)?;
        if flags.bits() & !MDB_NODUPDATA != 0 {
            return Err(Error::einval());
        }
        self.del_at(CRef::main(slot), flags.bits())
    }

    /// `mdb_cursor_count` on a handle.
    pub fn cursor_count(&mut self, id: CursorId) -> Result<u64> {
        let slot = self.cursor_slot(id)?;
        self.check_usable()?;
        let c = CRef::main(slot);
        let dbi = self.cur(c).dbi;
        let dcmp = self.core.dbs.borrow()[dbi].dup_cmp;
        let core = &self.core;
        let (mc, mx) = read_cursors(core, &self.w.cursors, c)?;
        let mut ro = RoCursor::from_parts(core, Dbi(dbi as u32), mc, mx, dcmp);
        let res = ro.count();
        let (mc, mx) = ro.parts();
        write_back(&mut self.w.cursors, c, mc, mx);
        res
    }

    /// `mdb_cursor_dbi` of a handle.
    pub fn cursor_dbi(&self, id: CursorId) -> Result<Dbi> {
        let slot = self.cursor_slot(id)?;
        Ok(Dbi(self.cur(CRef::main(slot)).dbi as u32))
    }

    /// `true` if `c` is a sub-cursor (for debug assertions).
    #[allow(dead_code)]
    pub(crate) fn is_sub(&self, c: CRef) -> bool {
        self.cur(c).flags & C_SUB != 0
    }
}

impl Drop for RwTxn<'_> {
    /// `mdb_txn_abort` unless committed.
    fn drop(&mut self) {
        self.end(false);
    }
}

impl Transaction for RwTxn<'_> {
    fn env(&self) -> &Environment {
        self.core.env
    }

    fn id(&self) -> u64 {
        self.core.txnid
    }

    fn open_db(&self, name: Option<&str>) -> Result<Dbi> {
        self.check_usable()?;
        self.core.open_db(name)
    }

    fn get<'txn>(&'txn self, db: Dbi, key: &[u8]) -> Result<Option<&'txn [u8]>> {
        self.core.dbi_entry(db, DB_USRVALID)?;
        self.check_usable()?;
        self.core.get(db, key)
    }

    /// `mdb_cursor_open` for reading only. `EINVAL` for the free DB, which
    /// C does not let write transactions open cursors on.
    fn cursor<'txn>(&'txn self, db: Dbi) -> Result<RoCursor<'txn>> {
        self.core.dbi_entry(db, DB_VALID)?;
        self.check_usable()?;
        if db == Dbi::FREE {
            return Err(Error::einval());
        }
        self.core.cursor(db)
    }

    fn db_flags(&self, db: Dbi) -> Result<DatabaseFlags> {
        self.core.db_flags(db)
    }

    fn stat(&self, db: Dbi) -> Result<Stat> {
        self.core.dbi_entry(db, DB_VALID)?;
        self.check_usable()?;
        self.core.db_stat(db)
    }
}

/// A cursor of a write transaction (`MDB_cursor` opened in a write txn).
///
/// It borrows the transaction mutably, so while it exists the transaction
/// can only be changed through it. Values returned by [`get`](RwCursor::get)
/// borrow the cursor: they may live in dirty pages that the next
/// [`put`](RwCursor::put) or [`del`](RwCursor::del) moves.
///
/// Closing (dropping) the cursor removes it from the transaction.
pub struct RwCursor<'txn> {
    txn: &'txn mut dyn CursorHost,
    id: CursorId,
    dbi: Dbi,
}

impl std::fmt::Debug for RwCursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RwCursor")
            .field("dbi", &self.dbi)
            .field("id", &self.id)
            .finish()
    }
}

/// What [`RwCursor`] needs from its transaction (lets the cursor carry one
/// lifetime instead of the transaction's two).
trait CursorHost {
    fn get(
        &mut self,
        id: CursorId,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Option<Entry<'_>>>;
    fn put(&mut self, id: CursorId, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()>;
    fn reserve(
        &mut self,
        id: CursorId,
        key: &[u8],
        len: usize,
        flags: WriteFlags,
    ) -> Result<&mut [u8]>;
    fn put_multiple(
        &mut self,
        id: CursorId,
        key: &[u8],
        items: &[u8],
        size: usize,
        flags: WriteFlags,
    ) -> Result<usize>;
    fn del(&mut self, id: CursorId, flags: WriteFlags) -> Result<()>;
    fn count(&mut self, id: CursorId) -> Result<u64>;
    fn close(&mut self, id: CursorId);
}

impl CursorHost for RwTxn<'_> {
    fn get(
        &mut self,
        id: CursorId,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Option<Entry<'_>>> {
        self.cursor_get(id, key, data, op)
    }

    fn put(&mut self, id: CursorId, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()> {
        self.cursor_put(id, key, data, flags)
    }

    fn reserve(
        &mut self,
        id: CursorId,
        key: &[u8],
        len: usize,
        flags: WriteFlags,
    ) -> Result<&mut [u8]> {
        self.cursor_reserve(id, key, len, flags)
    }

    fn put_multiple(
        &mut self,
        id: CursorId,
        key: &[u8],
        items: &[u8],
        size: usize,
        flags: WriteFlags,
    ) -> Result<usize> {
        self.cursor_put_multiple(id, key, items, size, flags)
    }

    fn del(&mut self, id: CursorId, flags: WriteFlags) -> Result<()> {
        self.cursor_del(id, flags)
    }

    fn count(&mut self, id: CursorId) -> Result<u64> {
        self.cursor_count(id)
    }

    fn close(&mut self, id: CursorId) {
        self.close_cursor(id)
    }
}

impl RwCursor<'_> {
    /// The database this cursor works on (`mdb_cursor_dbi`).
    pub fn dbi(&self) -> Dbi {
        self.dbi
    }

    /// `mdb_cursor_get`: perform `op` (see [`RoCursor::get`]).
    pub fn get(
        &mut self,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Option<Entry<'_>>> {
        self.txn.get(self.id, key, data, op)
    }

    /// `mdb_cursor_put`: store `data` under `key` and leave the cursor on
    /// the new item. Flags as [`RwTxn::put`], plus
    /// [`WriteFlags::CURRENT`]: replace the item at the cursor position
    /// (the key must match; for `DUP_SORT` the new data must sort like the
    /// old).
    pub fn put(&mut self, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()> {
        self.txn.put(self.id, key, data, flags)
    }

    /// `mdb_cursor_put` with `MDB_RESERVE` (see [`RwTxn::reserve`]).
    pub fn reserve(&mut self, key: &[u8], len: usize, flags: WriteFlags) -> Result<&mut [u8]> {
        self.txn.reserve(self.id, key, len, flags)
    }

    /// `mdb_cursor_put` with `MDB_MULTIPLE`: store the `size`-byte items
    /// packed in `items` as duplicates of `key` (`DUP_FIXED` only). Returns
    /// how many were stored.
    pub fn put_multiple(
        &mut self,
        key: &[u8],
        items: &[u8],
        size: usize,
        flags: WriteFlags,
    ) -> Result<usize> {
        self.txn.put_multiple(self.id, key, items, size, flags)
    }

    /// `mdb_cursor_del`: delete the item at the cursor position; with
    /// [`WriteFlags::NO_DUP_DATA`] all duplicates of the current key.
    pub fn del(&mut self, flags: WriteFlags) -> Result<()> {
        self.txn.del(self.id, flags)
    }

    /// `mdb_cursor_count` (see [`RoCursor::count`]).
    pub fn count(&mut self) -> Result<u64> {
        self.txn.count(self.id)
    }

    /// `MDB_FIRST`.
    pub fn first(&mut self) -> Result<Option<Entry<'_>>> {
        self.get(None, None, CursorOp::First)
    }

    /// `MDB_LAST`.
    pub fn last(&mut self) -> Result<Option<Entry<'_>>> {
        self.get(None, None, CursorOp::Last)
    }

    /// `MDB_NEXT`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Entry<'_>>> {
        self.get(None, None, CursorOp::Next)
    }

    /// `MDB_PREV`.
    pub fn prev(&mut self) -> Result<Option<Entry<'_>>> {
        self.get(None, None, CursorOp::Prev)
    }

    /// `MDB_SET_KEY`: position at `key` (exact match).
    pub fn set_key(&mut self, key: &[u8]) -> Result<Option<Entry<'_>>> {
        self.get(Some(key), None, CursorOp::SetKey)
    }

    /// `MDB_SET_RANGE`: position at the first key `>= key`.
    pub fn set_range(&mut self, key: &[u8]) -> Result<Option<Entry<'_>>> {
        self.get(Some(key), None, CursorOp::SetRange)
    }

    /// `MDB_GET_CURRENT`.
    pub fn get_current(&mut self) -> Result<Option<Entry<'_>>> {
        self.get(None, None, CursorOp::GetCurrent)
    }
}

impl Drop for RwCursor<'_> {
    /// `mdb_cursor_close`.
    fn drop(&mut self) {
        self.txn.close(self.id);
    }
}
