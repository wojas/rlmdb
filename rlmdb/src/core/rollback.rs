//! [`Environment::rollback`]: LMDB 1.0's `mdb_env_rollback` (mdb.c 1.0
//! line 4668), the undo of the last commit for two-phase commit protocols
//! (the first phase is [`RwTxn::prepare`](crate::core::RwTxn::prepare)).
//!
//! The two meta pages hold the two newest snapshots, transaction `N` on
//! page `N % 2` (`format::meta`). A rollback of `N` overwrites page `N % 2`
//! with a meta page that holds no snapshot, so the other page, `N - 1`,
//! is the newest again: for every reader and writer that comes later, C's
//! of either release or rlmdb's, the commit of `N` never happened. The
//! page written is C's, byte for byte: the tail `mdb_env_write_meta`
//! writes (from `mm_mapsize` on) of a transaction whose `MDB_db` records
//! are zero but for the page size and the environment flags kept in the
//! free DB's record (`mm_psize`, `mm_flags`), with `mm_last_pg` the
//! `(pgno_t)-1` of `mt_next_pgno - 1` with `mt_next_pgno` 0, `mm_txnid`
//! `N & 1` (C's `txn.mt_txnid = txnid & 1`, which selects the page) and
//! the map size of a commit; magic and version stay. Then `mti_txnid`
//! names `N - 1`.
//!
//! What rlmdb adds to C's checks (`KNOWN_C_BUGS.md` rows 62 and 63):
//! the previous meta page must hold a committed snapshot (after the
//! rollback of an odd `N`, C's own meta page has `mm_txnid` 1, which C's
//! check takes for a snapshot), and no reader may still use `N`.
//!
//! An environment opened with `MDB_PREVSNAPSHOT` that has not committed
//! yet presents `N - 1` already; the rollback of `N` writes the same page
//! and publishes the same txnid, and then ends the flag, as the first
//! commit would. C keeps the flag, whose rule (the older meta page) then
//! picks the page the rollback wrote (`KNOWN_C_BUGS.md` row 71).
//!
//! The order is chosen for readers that begin meanwhile (they take no
//! lock): first `mti_txnid` names `N - 1`, so new readers take that
//! snapshot, whose meta page is not touched; then the reader table is
//! checked for `N`; only then is page `N % 2` written. A reader stores the
//! txnid it read in its slot and reads `mti_txnid` again before it uses
//! the snapshot (ITS#7970, `LockRegion::publish_current_txnid`), and all
//! of these are sequentially consistent: a reader that ends up using `N`
//! stored it before `mti_txnid` changed, so the check sees it (and puts
//! `mti_txnid` back). C writes the page first and publishes `N & 1` on the
//! way (`mdb_env_write_meta`), which a reader beginning then takes for its
//! snapshot (`KNOWN_C_BUGS.md` row 64). The files and the lock file end up
//! as C leaves them.

use crate::consts::{MDB_MAGIC, NUM_METAS};
use crate::core::commit::write_meta_page;
use crate::core::env::Environment;
use crate::core::reader::ReaderTable;
use crate::core::txn_rw::WriterLock;
use crate::error::{Error, MdbError, Result};
use crate::format::meta::{pick_newest, Meta};
use crate::format::Db;

impl Environment {
    /// `mdb_env_rollback` (LMDB 1.0): undo the last commit, transaction
    /// `txnid`, so that the snapshot before it is the current one again.
    /// This exists for two-phase commit protocols: when the local
    /// transaction has committed (after [`RwTxn::prepare`] and
    /// [`PreparedTxn::commit`]) but another participant that had prepared
    /// fails to commit, the local commit is taken back. `txnid` is the
    /// committed transaction's ID ([`PreparedTxn::id`], or
    /// [`Transaction::id`](crate::Transaction::id) of the `RwTxn`).
    ///
    /// As `lmdb.h` warns: it may not be called twice in a row, and no other
    /// operation, by any process, may happen on the environment between
    /// the commit of `txnid` and this call. In particular nothing may have
    /// read the new snapshot or written after it: once rolled back, its
    /// pages are free again (pages it took from the free list are on the
    /// free list of the previous snapshot, pages it added at the end of
    /// the file lie past that snapshot's last page), and the next writer
    /// overwrites them; the next commit gets `txnid` again. Handles of
    /// databases the rolled-back transaction created stay open but name a
    /// database that does not exist (as after another process deleted it).
    ///
    /// The meta page of `txnid` is overwritten with one that holds no
    /// snapshot (C's bytes, see the module documentation of `rollback`),
    /// synced as a commit's meta page is (not with
    /// [`NO_SYNC`](crate::EnvironmentFlags::NO_SYNC) or
    /// [`NO_META_SYNC`](crate::EnvironmentFlags::NO_META_SYNC)); then new
    /// read transactions take the previous snapshot. The previous meta
    /// page and the data pages are not touched.
    ///
    /// It takes the writer lock for its duration, like a write
    /// transaction: it waits while another process writes, and while an
    /// [`RwTxn`] or a [`PreparedTxn`] of this environment is open on
    /// another thread. One open on the calling thread deadlocks it, as a
    /// second [`Environment::begin_rw_txn`] would; roll back after the
    /// commit, not from within another transaction.
    ///
    /// Errors (`MdbError::CantRollback` changes nothing):
    ///
    /// * `MdbError::CantRollback` if the newest snapshot is not `txnid`
    ///   (another transaction committed since, or `txnid` is wrong), or
    ///   there is no snapshot before it (the first commit of a new
    ///   environment, or the previous one was rolled back: a second
    ///   rollback in a row). C's check is that the previous meta page has
    ///   a txnid other than 0; a rollback leaves `txnid & 1` there, so after
    ///   the rollback of an odd txnid C would roll back again and leave no
    ///   snapshot at all (`KNOWN_C_BUGS.md` row 62). Here the previous meta
    ///   page must also hold a snapshot a commit wrote.
    /// * `MdbError::CantRollback` if the previous meta page holds the same
    ///   snapshot as the newest one (the same trees and last page): the
    ///   rollback would change no data. That is how a commit that split
    ///   its free pages (feature
    ///   [`split-freelist`](crate::Feature::SplitFreelist)) leaves the
    ///   meta pages: it cannot be undone, and this refusal keeps a
    ///   two-phase commit from taking a rollback for an undo (C 1.0 rolls
    ///   back to the same data and reports success). A program that needs
    ///   the undo aborts the prepared transaction when
    ///   [`PreparedTxn::id`] is higher than the transaction's ID was.
    /// * `MdbError::CantRollback` if a read transaction (of any process
    ///   using the lock file, or without one of this process) still reads
    ///   `txnid`: rolling back under it would let the next writer
    ///   overwrite pages it reads. C does not check this (`lmdb.h` forbids
    ///   the situation; `KNOWN_C_BUGS.md` row 63). The reader slot of a
    ///   process that died counts until [`Environment::reader_check`]
    ///   clears it.
    /// * `EACCES` for a read-only environment (C fails writing the page,
    ///   with `EBADF`, and makes the environment unusable).
    /// * An I/O error if the meta page cannot be written or synced: then
    ///   the old meta page is written back (C writes back only its txnid
    ///   and last page, over the zeroed records: `KNOWN_C_BUGS.md` row 64)
    ///   and, as after a failed commit, every later transaction of this
    ///   process fails with [`MdbError::Panic`].
    ///
    /// Without a lock file ([`NO_LOCK`](crate::EnvironmentFlags::NO_LOCK))
    /// the writer lock and the reader check cover this process only, as
    /// for write transactions.
    ///
    /// With [`PREV_SNAPSHOT`](crate::EnvironmentFlags::PREV_SNAPSHOT),
    /// before the environment's first commit, it reads the previous
    /// snapshot and reports its txnid ([`Environment::info`]), one less
    /// than the newest commit: the meta pages hold consecutive txnids, so
    /// the txnid to roll back is the reported one plus one. The rollback
    /// is the one without the flag (the same checks, the same page written,
    /// the previous txnid published), after which the flag ends as after
    /// the first commit: the exclusive lock on the lock file is shared, and
    /// the environment reports and reads the snapshot that is now the
    /// newest, the one it read already. A rollback of the reported txnid
    /// fails, as it is not the newest commit. C keeps the flag, which then
    /// picks the page the rollback wrote, without a snapshot, until the
    /// next commit (`KNOWN_C_BUGS.md` row 71).
    ///
    /// [`RwTxn`]: crate::RwTxn
    /// [`RwTxn::prepare`]: crate::RwTxn::prepare
    /// [`PreparedTxn`]: crate::PreparedTxn
    /// [`PreparedTxn::id`]: crate::PreparedTxn::id
    /// [`PreparedTxn::commit`]: crate::PreparedTxn::commit
    pub fn rollback(&self, txnid: u64) -> Result<()> {
        if self.is_read_only() {
            return Err(Error::errno(libc::EACCES));
        }
        let _lock = WriterLock::acquire(self)?;
        let metas = [self.read_meta(0)?, self.read_meta(1)?];
        let newest = pick_newest(&metas);
        let (current, previous) = (metas[newest], metas[newest ^ 1]);
        // C's check.
        if previous.txnid == 0 || current.txnid != txnid {
            return Err(MdbError::CantRollback.into());
        }
        // Not in C: the previous page must be a snapshot (not the page a
        // rollback wrote, row 62), and the newest one where its txnid puts
        // it (a damaged file could hold equal txnids, and the page written
        // would then be the previous one).
        if !holds_snapshot(&previous) || newest != (txnid & 1) as usize {
            return Err(MdbError::CantRollback.into());
        }
        // Not in C: a commit that split the pages it freed (feature
        // `split-freelist`, `RwTxn::write_meta`) wrote its snapshot into
        // both meta pages. Rolling it back would publish the previous page
        // and leave the data as it is: not an undo, which a two-phase
        // commit would take for one. (C's rollback does just that.) Rolling
        // back any commit that changed nothing is refused the same way.
        if previous.dbs == current.dbs && previous.last_pgno == current.last_pgno {
            return Err(MdbError::CantRollback.into());
        }
        // New readers take the previous snapshot from here on; then no
        // reader may be left on `txnid` (see the module documentation). A
        // failure puts back what was published: `txnid`, or with
        // PREV_SNAPSHOT the previous one already.
        let published = self.published_txnid();
        self.publish_txnid(previous.txnid);
        if self.readers().pins(txnid) {
            self.publish_txnid(published);
            return Err(MdbError::CantRollback.into());
        }
        let free = current.dbs[0];
        let rolled_back = Meta {
            magic: MDB_MAGIC,
            version: self.layout().data_version,
            address: 0,
            // mdb_env_write_meta: the other page's map size, or this
            // process's if that is larger.
            map_size: previous.map_size.max(self.map_size() as u64),
            dbs: [
                Db {
                    pad: free.pad,
                    flags: free.flags,
                    ..Db::default()
                },
                Db::default(),
            ],
            // mt_next_pgno - 1 with mt_next_pgno 0.
            last_pgno: u64::MAX,
            txnid: txnid & 1,
        };
        if let Err(e) = write_meta_page(self, newest, &rolled_back, Some(&current), false) {
            self.publish_txnid(published);
            return Err(e);
        }
        // With PREV_SNAPSHOT the environment stepped back from `txnid`
        // already, and now it is gone: the flag has done its work. End it
        // as the first commit does (share the lock, report the newest meta
        // page); C keeps it (row 71).
        self.end_prev_snapshot(previous.txnid)
    }

    /// The last committed txnid that new read transactions take
    /// (`mti_txnid`, or without a lock file the state the file's handles in
    /// this process share).
    fn published_txnid(&self) -> u64 {
        match self.readers() {
            ReaderTable::Shared(lock) => lock.txnid(),
            ReaderTable::Local(local) => local.txnid(),
        }
    }

    /// Sets the last committed txnid that new read transactions take
    /// (`mti_txnid`, or without a lock file the state the file's handles in
    /// this process share). The caller holds the writer lock.
    pub(crate) fn publish_txnid(&self, txnid: u64) {
        match self.readers() {
            ReaderTable::Shared(lock) => lock.set_txnid(txnid),
            ReaderTable::Local(local) => local.set_txnid(txnid),
        }
    }
}

/// Whether `meta` holds a snapshot a commit wrote: its last page covers
/// the meta pages and is not the `(pgno_t)-1` of a page a rollback wrote
/// (the condition under which `TxnCore::set_snapshot` takes a snapshot,
/// `KNOWN_C_BUGS.md` row 49).
fn holds_snapshot(meta: &Meta) -> bool {
    meta.last_pgno >= NUM_METAS as u64 - 1 && meta.last_pgno != u64::MAX
}
