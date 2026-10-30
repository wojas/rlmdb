//! Database handles: the environment's DBI table and each transaction's view
//! of it.
//!
//! C keeps three parallel per-environment arrays indexed by `MDB_dbi`:
//! `me_dbxs` (name and comparators), `me_dbflags` (persistent flags plus
//! `MDB_VALID`) and `me_dbiseqs` (a sequence number bumped whenever a slot is
//! closed or reused). Each transaction has its own `mt_dbs` (the `MDB_db`
//! records of its snapshot) and `mt_dbflags` (`DB_VALID`, `DB_STALE`, …).
//! Slot 0 is the free-page DB, slot 1 the main DB; their records come from
//! the meta page. Named databases live in the main DB as `F_SUBDATA` nodes.
//!
//! Here the environment table is a `DbiRegistry` (`RwLock` + atomic
//! sequence numbers, so handles may be opened from any thread), and the
//! per-transaction view is a vector of `TxnDb` filled lazily:
//!
//! * A handle's record is *stale* (`DB_STALE`) until the transaction first
//!   uses it; it is then re-read from the transaction's own snapshot of the
//!   main DB (the refresh in `mdb_page_search`, mdb.c line 5757), because
//!   another transaction may have changed the named DB since the handle was
//!   opened. If the DB's flags changed meanwhile (dropped and recreated), use
//!   fails with `MDB_INCOMPATIBLE`; if the DB no longer exists in this
//!   snapshot, with `MDB_BAD_DBI`.
//! * A handle closed with [`Environment::close_db`](crate::core::Environment::close_db)
//!   has its sequence number bumped; transactions that cached it then get
//!   `MDB_BAD_DBI` (`TXN_DBI_CHANGED`).
//!
//! Deviation from C, documented: a handle opened inside a read transaction
//! is published to the environment immediately and stays valid after the
//! transaction ends, whether it commits or aborts. (C discards handles
//! opened by an aborted read transaction; keeping them is strictly more
//! permissive and cannot affect other processes.)

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::RwLock;

use crate::cmp::{CmpFn, Comparator};
use crate::consts::*;
use crate::core::cursor::Cursor;
use crate::core::txn::TxnCore;
use crate::core::Stat;
use crate::error::{Error, MdbError, Result};
use crate::flags::DatabaseFlags;
use crate::format::Db;

/// A database handle (`MDB_dbi`). `Copy`, borrows nothing; validity is
/// checked on every use.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct Dbi(pub(crate) u32);

impl Dbi {
    /// The free-page database (`FREE_DBI`). Read-only transactions may open
    /// a cursor on it; [`Transaction::get`](crate::core::Transaction::get)
    /// refuses it as C does.
    pub const FREE: Dbi = Dbi(FREE_DBI);
    /// The main (unnamed) database (`MAIN_DBI`).
    pub const MAIN: Dbi = Dbi(MAIN_DBI);

    /// The C handle value.
    pub fn to_raw(self) -> u32 {
        self.0
    }

    /// Wraps a C handle value. Invalid values are rejected when used.
    pub fn from_raw(dbi: u32) -> Dbi {
        Dbi(dbi)
    }
}

/// `mt_dbflags` bit: the DB was written in this transaction (`DB_DIRTY`);
/// its record goes back into the main DB at commit.
pub(crate) const DB_DIRTY: u8 = 0x01;
/// `mt_dbflags` bit: the DB record is older than the transaction.
pub(crate) const DB_STALE: u8 = 0x02;
/// `mt_dbflags` bit: the handle was created by this write transaction
/// (`DB_NEW`); it becomes valid for others at commit and is closed by an
/// abort.
pub(crate) const DB_NEW: u8 = 0x04;
/// `mt_dbflags` bit: the handle is valid (`DB_VALID`).
pub(crate) const DB_VALID: u8 = 0x08;
/// `mt_dbflags` bit: valid for user operations (not set for `FREE_DBI`).
pub(crate) const DB_USRVALID: u8 = 0x10;
/// `mx_dbflag` bit: the DB is a `MDB_DUPSORT` sub-DB (`DB_DUPDATA`).
pub(crate) const DB_DUPDATA: u8 = 0x20;

/// One slot of the environment's DBI table (`me_dbxs[i]` + `me_dbflags[i]`).
#[derive(Debug, Clone, Default)]
pub(crate) struct DbiSlot {
    /// `md_name`; `None` for the core DBs and for free slots.
    pub(crate) name: Option<Box<[u8]>>,
    /// Persistent flags (`me_dbflags & PERSISTENT_FLAGS`).
    pub(crate) flags: u16,
    /// `MDB_VALID`. A slot reserved by a write transaction for a database it
    /// created stays invalid for other transactions until the commit.
    pub(crate) valid: bool,
    /// Key comparator installed with `set_compare`.
    pub(crate) custom_cmp: Option<CmpFn>,
    /// Data comparator installed with `set_dupsort`.
    pub(crate) custom_dcmp: Option<CmpFn>,
}

/// The environment's DBI table.
pub(crate) struct DbiRegistry {
    /// `me_dbxs`/`me_dbflags`; `len()` is `me_numdbs`.
    slots: RwLock<Vec<DbiSlot>>,
    /// `me_dbiseqs`, one per possible slot (`me_maxdbs`).
    seqs: Box<[AtomicU32]>,
}

impl DbiRegistry {
    /// A table with room for `max_dbs` named databases plus the two core DBs.
    pub(crate) fn new(max_dbs: u32) -> DbiRegistry {
        let total = max_dbs as usize + CORE_DBS as usize;
        let core = DbiSlot {
            valid: true,
            ..DbiSlot::default()
        };
        DbiRegistry {
            slots: RwLock::new(vec![core.clone(), core]),
            seqs: (0..total).map(|_| AtomicU32::new(0)).collect(),
        }
    }

    /// `me_maxdbs`: total number of slots including the core DBs.
    pub(crate) fn capacity(&self) -> usize {
        self.seqs.len()
    }

    fn seq(&self, i: usize) -> u32 {
        self.seqs[i].load(Ordering::SeqCst)
    }

    /// `me_dbiseqs[i]`.
    pub(crate) fn seq_of(&self, i: usize) -> u32 {
        self.seqs
            .get(i)
            .map_or(u32::MAX, |s| s.load(Ordering::SeqCst))
    }

    /// `++me_dbiseqs[i]`: the new sequence number of a (re)used slot.
    pub(crate) fn bump_seq(&self, i: usize) -> u32 {
        self.seqs[i].fetch_add(1, Ordering::SeqCst) + 1
    }

    /// `me_dbxs[i].md_name`.
    pub(crate) fn name_of(&self, i: usize) -> Option<Box<[u8]>> {
        self.read().get(i).and_then(|s| s.name.clone())
    }

    pub(crate) fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<DbiSlot>> {
        self.slots.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn write(&self) -> std::sync::RwLockWriteGuard<'_, Vec<DbiSlot>> {
        self.slots.write().unwrap_or_else(|e| e.into_inner())
    }

    /// `mdb_dbis_update` for one `DB_NEW` slot: publish it with `flags`
    /// (commit) or close it (abort).
    pub(crate) fn finish_new(&self, i: usize, keep: bool, flags: u16) {
        if keep {
            let mut slots = self.write();
            if let Some(slot) = slots.get_mut(i) {
                if slot.name.is_some() {
                    slot.flags = (u32::from(flags) & PERSISTENT_FLAGS) as u16;
                    slot.valid = true;
                }
            }
        } else {
            self.close(Dbi(i as u32));
        }
    }

    /// `mdb_dbi_close`: forget a named handle. Core handles and unknown
    /// handles are ignored, as in C.
    pub(crate) fn close(&self, dbi: Dbi) {
        let i = dbi.0 as usize;
        if i < CORE_DBS as usize || i >= self.capacity() {
            return;
        }
        let mut slots = self.write();
        if let Some(slot) = slots.get_mut(i) {
            if slot.name.is_some() {
                *slot = DbiSlot::default();
                self.seqs[i].fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Reopens a handle closed by `drop_db` in a nested transaction that
    /// aborted, in its old slot, if that is still free. Returns the slot's
    /// new sequence number.
    pub(crate) fn reopen(&self, i: usize, slot: DbiSlot) -> Option<u32> {
        let mut slots = self.write();
        let dst = slots.get_mut(i).filter(|s| s.name.is_none())?;
        *dst = slot;
        Some(self.bump_seq(i))
    }

    /// `mdb_set_compare` / `mdb_set_dupsort`: install a custom comparator.
    pub(crate) fn set_comparator(&self, dbi: Dbi, cmp: Option<CmpFn>, dup: bool) -> Result<()> {
        let mut slots = self.write();
        let slot = slots
            .get_mut(dbi.0 as usize)
            .filter(|s| s.valid && dbi.0 != FREE_DBI)
            .ok_or_else(Error::einval)?;
        if dup {
            slot.custom_dcmp = cmp;
        } else {
            slot.custom_cmp = cmp;
        }
        Ok(())
    }

    /// The comparators to use for slot `i` given the DB's flags.
    pub(crate) fn comparators(
        slot: &DbiSlot,
        i: usize,
        flags: u16,
    ) -> (Comparator, Option<Comparator>) {
        if i == FREE_DBI as usize {
            // me_dbxs[FREE_DBI].md_cmp = mdb_cmp_long (aligned MDB_INTEGERKEY)
            return (Comparator::Long, None);
        }
        let flags = u32::from(flags);
        let key = slot
            .custom_cmp
            .map(Comparator::Custom)
            .unwrap_or_else(|| Comparator::default_key(flags));
        let dup = if flags & MDB_DUPSORT != 0 {
            slot.custom_dcmp
                .map(Comparator::Custom)
                .or_else(|| Comparator::default_dup(flags))
        } else {
            None
        };
        (key, dup)
    }
}

/// A transaction's view of one DBI slot (`mt_dbs[i]`, `mt_dbflags[i]`,
/// `mt_dbiseqs[i]`, plus the comparators from `mt_dbxs[i]`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TxnDb {
    /// The `MDB_db` record (only meaningful once not `DB_STALE`).
    pub(crate) db: Db,
    /// `mt_dbflags`; 0 means "not looked at yet in this transaction".
    pub(crate) state: u8,
    /// Sequence number of the slot when this view was taken.
    pub(crate) seq: u32,
    /// Key comparator.
    pub(crate) key_cmp: Comparator,
    /// Data comparator (`MDB_DUPSORT` only).
    pub(crate) dup_cmp: Option<Comparator>,
    /// [`name_id`] of the database's name (0 for the core DBs): a slot may
    /// be closed and reused for another name, and a nested transaction may
    /// register a name again in a new slot generation; this tells whether
    /// a view and a registration are about the same database.
    pub(crate) name_id: u64,
}

/// A fingerprint of a database name (FNV-1a, never 0).
pub(crate) fn name_id(name: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in name {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h | 1
}

impl TxnDb {
    /// An entry not yet looked at in this transaction.
    pub(crate) const UNKNOWN: TxnDb = TxnDb {
        db: Db {
            pad: 0,
            flags: 0,
            depth: 0,
            branch_pages: 0,
            leaf_pages: 0,
            overflow_pages: 0,
            entries: 0,
            root: P_INVALID,
        },
        state: 0,
        seq: 0,
        key_cmp: Comparator::Memn,
        dup_cmp: None,
        name_id: 0,
    };
}

/// Fills `dbs` for a transaction starting on snapshot `core_dbs`, as
/// `mdb_txn_renew0` does: the two core records come from the meta page,
/// every other slot will be looked at lazily.
pub(crate) fn init_txn_dbs(registry: &DbiRegistry, dbs: &mut Vec<TxnDb>, core_dbs: &[Db; 2]) {
    dbs.clear();
    dbs.resize(registry.capacity(), TxnDb::UNKNOWN);
    let slots = registry.read();
    for i in 0..CORE_DBS as usize {
        let (key_cmp, dup_cmp) = DbiRegistry::comparators(&slots[i], i, core_dbs[i].flags);
        dbs[i] = TxnDb {
            db: core_dbs[i],
            state: if i == FREE_DBI as usize {
                DB_VALID
            } else {
                DB_VALID | DB_USRVALID
            },
            seq: registry.seq(i),
            key_cmp,
            dup_cmp,
            name_id: 0,
        };
    }
}

/// Database names are C strings: arbitrary bytes without NUL. A name with
/// a NUL byte could not be opened by a C process (`EINVAL`).
pub(crate) fn check_db_name(name: &[u8]) -> Result<()> {
    if name.contains(&0) {
        return Err(Error::einval());
    }
    Ok(())
}

/// Outcome of looking a name up in the main DB.
pub(crate) enum NamedDb {
    /// An `F_SUBDATA` node with this record.
    Found(Db),
    /// No exact match.
    Missing,
}

impl TxnCore<'_> {
    /// `TXN_DBI_EXIST(txn, dbi, validity)` plus `TXN_DBI_CHANGED`: returns
    /// this transaction's entry for `dbi`, taking a (stale) view of the
    /// environment slot the first time. `EINVAL` if the handle is not valid,
    /// `MDB_BAD_DBI` if it was closed or reused since.
    pub(crate) fn dbi_entry(&self, dbi: Dbi, validity: u8) -> Result<TxnDb> {
        let i = dbi.0 as usize;
        let registry = &self.env.dbis;
        let mut entry = *self.dbs.borrow().get(i).ok_or_else(Error::einval)?;
        if entry.state == 0 {
            // First use in this transaction: take a stale view of the slot.
            let slots = registry.read();
            let slot = slots.get(i).filter(|s| s.valid).ok_or_else(Error::einval)?;
            let (key_cmp, dup_cmp) = DbiRegistry::comparators(slot, i, slot.flags);
            entry = TxnDb {
                db: Db::empty(slot.flags),
                state: DB_VALID | DB_USRVALID | DB_STALE,
                seq: registry.seq(i),
                key_cmp,
                dup_cmp,
                name_id: slot.name.as_deref().map_or(0, name_id),
            };
            drop(slots);
            self.dbs.borrow_mut()[i] = entry;
        }
        if entry.state & validity == 0 {
            return Err(Error::einval());
        }
        if entry.seq != registry.seq(i) {
            return Err(MdbError::BadDbi.into());
        }
        Ok(entry)
    }

    /// As [`Self::dbi_entry`], with a stale record refreshed from this
    /// transaction's snapshot of the main DB.
    pub(crate) fn fresh_dbi(&self, dbi: Dbi, validity: u8) -> Result<TxnDb> {
        let entry = self.dbi_entry(dbi, validity)?;
        self.refreshed(dbi, entry)
    }

    /// The refresh of [`Self::fresh_dbi`], for an `entry` that
    /// [`Self::dbi_entry`] just returned.
    pub(crate) fn refreshed(&self, dbi: Dbi, entry: TxnDb) -> Result<TxnDb> {
        if entry.state & DB_STALE == 0 {
            return Ok(entry);
        }
        self.refresh_stale(dbi.0 as usize, entry)
    }

    /// The `DB_STALE` branch of `mdb_page_search`: re-read a named DB's
    /// record from the main DB of this snapshot.
    fn refresh_stale(&self, i: usize, mut entry: TxnDb) -> Result<TxnDb> {
        let name = {
            let slots = self.env.dbis.read();
            if entry.seq != self.env.dbis.seq(i) {
                return Err(MdbError::BadDbi.into());
            }
            slots
                .get(i)
                .and_then(|s| s.name.clone())
                .ok_or(MdbError::BadDbi)?
        };
        let db = match self.find_named_db(&name)? {
            NamedDb::Found(db) => db,
            NamedDb::Missing => return Err(MdbError::BadDbi.into()),
        };
        // The txn may not know this DBI, or another process may have dropped
        // and recreated the DB with other flags.
        if u32::from(entry.db.flags) & PERSISTENT_FLAGS != u32::from(db.flags) {
            return Err(MdbError::Incompatible.into());
        }
        entry.db = db;
        entry.state &= !DB_STALE;
        self.dbs.borrow_mut()[i] = entry;
        Ok(entry)
    }

    /// Searches the main DB for `name` and returns its `MDB_db` record if it
    /// is a named DB. `MDB_NOTFOUND` if the main DB is empty; `Missing` if
    /// there is no exact match; `MDB_INCOMPATIBLE` if the key holds plain
    /// data instead of a DB record.
    pub(crate) fn find_named_db(&self, name: &[u8]) -> Result<NamedDb> {
        // mdb_cursor_set: a zero-length key (DB name) is invalid.
        if name.is_empty() {
            return Err(MdbError::BadValSize.into());
        }
        let main = self.dbs.borrow()[MAIN_DBI as usize];
        let mut mc = Cursor::new(main.db, main.key_cmp, 0);
        mc.page_search(self, Some(name), 0)?;
        let (found, exact) = mc.node_search(name)?;
        if !found || !exact {
            return Ok(NamedDb::Missing);
        }
        let leaf = mc.current_node()?;
        if leaf.flags() & (crate::format::node::F_DUPDATA | crate::format::node::F_SUBDATA)
            != crate::format::node::F_SUBDATA
        {
            return Err(MdbError::Incompatible.into());
        }
        let data = crate::core::cursor::node_read(self, leaf)?;
        Ok(NamedDb::Found(Db::read(data).ok_or(MdbError::Corrupted)?))
    }

    /// `mdb_dbi_open` without `MDB_CREATE`: the handle of an existing
    /// database (`None` = the main DB).
    pub(crate) fn open_db(&self, name: Option<&[u8]>) -> Result<Dbi> {
        let Some(name) = name else {
            return Ok(Dbi::MAIN);
        };
        check_db_name(name)?;
        let registry = &self.env.dbis;
        let mut slots = registry.write();

        // Is the DB already open?
        let mut unused = None;
        for (i, slot) in slots.iter().enumerate().skip(CORE_DBS as usize) {
            match &slot.name {
                None => {
                    unused.get_or_insert(i);
                }
                Some(n) if &**n == name => {
                    drop(slots);
                    self.adopt_registration(i, name);
                    return Ok(Dbi(i as u32));
                }
                Some(_) => {}
            }
        }
        if let Some(dbi) = self.reopen_live_view(&mut slots, name) {
            return Ok(dbi);
        }
        // If no free slot and max hit, fail.
        if unused.is_none() && slots.len() >= registry.capacity() {
            return Err(MdbError::DbsFull.into());
        }
        // Cannot mix named databases with some main DB flags.
        let main = self.dbs.borrow()[MAIN_DBI as usize];
        if u32::from(main.db.flags) & (MDB_DUPSORT | MDB_INTEGERKEY) != 0 {
            return Err(MdbError::NotFound.into());
        }
        let db = match self.find_named_db(name)? {
            NamedDb::Found(db) => db,
            NamedDb::Missing => return Err(MdbError::NotFound.into()),
        };

        // Register the handle in the environment and in this transaction.
        let slot = DbiSlot {
            name: Some(name.into()),
            flags: (u32::from(db.flags) & PERSISTENT_FLAGS) as u16,
            valid: true,
            custom_cmp: None,
            custom_dcmp: None,
        };
        let i = match unused {
            Some(i) => {
                slots[i] = slot;
                i
            }
            None => {
                slots.push(slot);
                slots.len() - 1
            }
        };
        let seq = registry.seqs[i].fetch_add(1, Ordering::SeqCst) + 1;
        let (key_cmp, dup_cmp) = DbiRegistry::comparators(&slots[i], i, db.flags);
        drop(slots);
        self.dbs.borrow_mut()[i] = TxnDb {
            db,
            state: DB_VALID | DB_USRVALID,
            seq,
            key_cmp,
            dup_cmp,
            name_id: name_id(name),
        };
        Ok(Dbi(i as u32))
    }

    /// `name` is not open in the environment, but this transaction may
    /// still have a live view of it in a slot whose handle was closed under
    /// it (`drop_db` in a nested transaction, or `Environment::close_db`).
    /// Opening it again must keep that view: within a write transaction the
    /// DB record in the main DB is only written back at commit, so reading
    /// it would lose this transaction's changes to the database (C does,
    /// KNOWN_C_BUGS.md #19). Registers the view's slot again and returns
    /// its handle.
    pub(crate) fn reopen_live_view(&self, slots: &mut Vec<DbiSlot>, name: &[u8]) -> Option<Dbi> {
        let id = name_id(name);
        let mut dbs = self.dbs.borrow_mut();
        let i = (CORE_DBS as usize..dbs.len()).find(|&i| {
            let e = &dbs[i];
            e.state & DB_VALID != 0
                && e.name_id == id
                && slots.get(i).is_none_or(|s| s.name.is_none())
        })?;
        let e = &mut dbs[i];
        let slot = DbiSlot {
            name: Some(name.into()),
            flags: (u32::from(e.db.flags) & PERSISTENT_FLAGS) as u16,
            valid: e.state & DB_NEW == 0,
            custom_cmp: None,
            custom_dcmp: None,
        };
        if i < slots.len() {
            slots[i] = slot;
        } else if i == slots.len() {
            slots.push(slot);
        } else {
            return None;
        }
        e.seq = self.env.dbis.bump_seq(i);
        Some(Dbi(i as u32))
    }

    /// `name` is open in the environment in slot `i`. If this transaction
    /// has a usable view of the same database from an earlier generation
    /// of the slot (the handle was closed by `drop_db` in a nested
    /// transaction that aborted, or registered again by one), it carries
    /// over to the current one; C gets the same effect from the nested
    /// transaction updating its parent's `mt_dbiseqs`. A view of a
    /// database this transaction dropped, or of another name, is
    /// forgotten: the handle is looked at afresh when it is used. Returns
    /// `false` in that case.
    pub(crate) fn adopt_registration(&self, i: usize, name: &[u8]) -> bool {
        let seq = self.env.dbis.seq_of(i);
        let mut dbs = self.dbs.borrow_mut();
        let e = &mut dbs[i];
        if e.state == 0 || e.seq == seq {
            return true;
        }
        if e.state & DB_VALID != 0 && e.name_id == name_id(name) {
            e.seq = seq;
            return true;
        }
        *e = TxnDb::UNKNOWN;
        false
    }

    /// `mdb_set_compare` / `mdb_set_dupsort` called with a transaction:
    /// install `cmp` for the handle (environment-wide, as C's `me_dbxs`)
    /// and use it in this transaction from now on.
    pub(crate) fn set_comparator(&self, db: Dbi, cmp: Option<CmpFn>, dup: bool) -> Result<()> {
        let entry = self.dbi_entry(db, DB_USRVALID)?;
        let i = db.0 as usize;
        let registry = &self.env.dbis;
        let mut slots = registry.write();
        let slot = slots.get_mut(i).ok_or_else(Error::einval)?;
        if dup {
            slot.custom_dcmp = cmp;
        } else {
            slot.custom_cmp = cmp;
        }
        let (key_cmp, dup_cmp) = DbiRegistry::comparators(slot, i, entry.db.flags);
        drop(slots);
        let mut dbs = self.dbs.borrow_mut();
        dbs[i].key_cmp = key_cmp;
        dbs[i].dup_cmp = dup_cmp;
        Ok(())
    }

    /// The key and duplicate-data comparators this transaction uses for
    /// `db` (`mdb_cmp` / `mdb_dcmp`).
    pub(crate) fn comparators(&self, db: Dbi) -> Result<(Comparator, Option<Comparator>)> {
        let entry = self.dbi_entry(db, DB_VALID)?;
        Ok((entry.key_cmp, entry.dup_cmp))
    }

    /// `mdb_dbi_flags`: the persistent flags of a user DB.
    pub(crate) fn db_flags(&self, dbi: Dbi) -> Result<DatabaseFlags> {
        let entry = self.dbi_entry(dbi, DB_USRVALID)?;
        Ok(DatabaseFlags::from_bits_retain(
            u32::from(entry.db.flags) & PERSISTENT_FLAGS,
        ))
    }

    /// `mdb_stat` for a database of this transaction.
    pub(crate) fn db_stat(&self, dbi: Dbi) -> Result<Stat> {
        let entry = self.fresh_dbi(dbi, DB_VALID)?;
        Ok(Stat::from_db(self.env.page_size(), &entry.db))
    }
}

#[cfg(test)]
mod tests {
    use crate::core::{Environment, Transaction};
    use crate::error::{Error, MdbError};
    use crate::flags::{DatabaseFlags, EnvironmentFlags};

    fn code(e: Error) -> i32 {
        e.to_err_code()
    }

    /// Names are validated as C validates keys in `mdb_dbi_open`: the
    /// empty name fails in `mdb_cursor_set` with `MDB_BAD_VALSIZE` (open
    /// and create); a name longer than the maximum key size is simply not
    /// found when opening and fails the record's put with
    /// `MDB_BAD_VALSIZE` when creating.
    #[test]
    fn empty_and_too_long_names() {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment::builder()
            .max_dbs(4)
            .flags(EnvironmentFlags::NO_SYNC)
            .open(dir.path())
            .unwrap();
        let bad = MdbError::BadValSize.to_err_code();
        let long = vec![b'n'; 512];
        let max = vec![b'm'; 511];
        for populated in [false, true] {
            let mut txn = env.begin_rw_txn().unwrap();
            assert_eq!(code(txn.open_db(Some("")).unwrap_err()), bad);
            assert_eq!(code(txn.open_db_bytes(Some(b"")).unwrap_err()), bad);
            assert_eq!(
                code(txn.create_db(Some(""), DatabaseFlags::CREATE).unwrap_err()),
                bad
            );
            assert_eq!(
                code(
                    txn.create_db_bytes(Some(b""), DatabaseFlags::CREATE)
                        .unwrap_err()
                ),
                bad
            );
            assert!(txn.open_db_bytes(Some(&long)).unwrap_err().not_found());
            assert_eq!(
                code(
                    txn.create_db_bytes(Some(&long), DatabaseFlags::CREATE)
                        .unwrap_err()
                ),
                bad
            );
            // The longest valid name works.
            if populated {
                txn.create_db_bytes(Some(&max), DatabaseFlags::CREATE)
                    .unwrap();
            }
            txn.commit().unwrap();
            let ro = env.begin_ro_txn().unwrap();
            assert_eq!(code(ro.open_db(Some("")).unwrap_err()), bad);
            assert_eq!(code(ro.open_db_bytes(Some(b"")).unwrap_err()), bad);
            assert!(ro.open_db_bytes(Some(&long)).unwrap_err().not_found());
            assert_eq!(ro.open_db_bytes(Some(&max)).is_ok(), populated);
        }
        // NUL bytes cannot be part of a C name.
        let mut txn = env.begin_rw_txn().unwrap();
        let e = txn
            .create_db_bytes(Some(b"a\0b"), DatabaseFlags::CREATE)
            .unwrap_err();
        assert_eq!(code(e), libc::EINVAL);
        // Non-UTF-8 names are fine.
        let db = txn
            .create_db_bytes(Some(b"\xff\xfe"), DatabaseFlags::CREATE)
            .unwrap();
        txn.commit().unwrap();
        let ro = env.begin_ro_txn().unwrap();
        assert_eq!(ro.open_db_bytes(Some(b"\xff\xfe")).unwrap(), db);
    }
}
