//! The lock file and its reader table, and the in-memory reader table of
//! an environment without a lock file.
//!
//! Readers do not lock anything while they read. They advertise the snapshot
//! they use in a slot of the shared reader table, so that writers know which
//! freed pages must not be reused yet (`mdb_find_oldest`). The table lives in
//! the lock file (`lock.mdb` or `<path>-lock`), memory-mapped by every
//! process using the environment.
//!
//! An environment without a lock file (`MDB_NOLOCK`, or read-only on a
//! read-only file system) has no shared table. C then does not track
//! readers at all: `mdb_find_oldest` returns the writer's txnid minus one,
//! so a write transaction may reuse pages a read transaction of the same
//! process still reads (`lmdb.h` makes that the caller's problem). Here the
//! environment keeps a `LocalReaders` table instead, which tracks the read
//! transactions of this process exactly as the lock file's table would; only
//! other processes are the caller's business, as in C
//! (`KNOWN_C_BUGS.md` #35). All `NO_LOCK` handles of one data file in the
//! process share one table (`registry::LocalShared`). `ReaderTable` is the
//! environment's choice of the two.
//!
//! C counterparts and behaviours kept exactly:
//!
//! * `LockRegion::open` = `mdb_env_setup_locks` (mdb.c line 4882) with
//!   `mdb_env_excl_lock` (line 4741): try an exclusive `fcntl` lock on byte 0;
//!   if we get it nobody else has the environment open, so we (re)initialise
//!   the table and the shared mutexes; otherwise we wait for a shared lock and
//!   validate magic and format. The exclusive opener downgrades to shared at
//!   the end of the open (`LockRegion::downgrade`, `mdb_env_share_locks`),
//!   publishing the current txnid in `mti_txnid` first.
//! * The lock file has the version of the environment's data format
//!   ([`LockLayout`]: version 1 for LMDB 0.9, version 2 for LMDB 1.0,
//!   `mdb_env_setup_locks` at mdb10.c line 5957). The protocol is the same in
//!   both; only the offsets, the `mti_format` word and, on macOS, how the
//!   semaphores are named differ. A table of the other version is
//!   `MDB_VERSION_MISMATCH` while someone else holds the file, and is
//!   re-initialised in our version by an exclusive opener, as C does.
//! * Reader liveness: each process holds an exclusive lock on byte `pid` of
//!   the lock file once it has started a read transaction
//!   (`mdb_reader_pid(Pidset)`); `LockRegion::reader_check` probes those
//!   locks with `F_GETLK` to clear slots of dead processes
//!   (`mdb_reader_check0`, line 10403). On Windows readers take no such
//!   lock (`MDB_PIDLOCK` is 0) and the check asks whether the process still
//!   exists (`os::pid_alive`), as C does there.
//! * Slot allocation (`LockRegion::acquire_slot`) happens under the reader
//!   mutex and follows the careful publication order of `mdb_txn_renew0`
//!   (line 2758): reset the slot, publish it in `mti_numreaders`, then claim
//!   it by writing our pid.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::consts::MDB_MAGIC;
use crate::core::registry::{LocalShared, Registration};
use crate::error::{Error, MdbError, Result};
use crate::format::lockfile::{
    LockLayout, READER_PID, READER_TID, READER_TID_SIZE, READER_TXNID, TXNINFO_FORMAT,
    TXNINFO_MAGIC,
};
use crate::os::shared_lock::{LockStatus, SharedLocks, SharedMutex};
use crate::os::{self, LockKind, LockMap};

/// One entry of [`Environment::reader_list`](crate::core::Environment::reader_list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderInfo {
    /// `mr_pid`: process owning the slot.
    pub pid: i32,
    /// `mr_tid`: thread that claimed the slot (`pthread_t` as an integer).
    pub thread: u64,
    /// `mr_txnid`: snapshot in use, or `None` if the slot is owned but idle.
    pub txnid: Option<u64>,
}

/// Formats reader entries exactly like `mdb_reader_list` prints them, so the
/// C API can pass the text to the user's callback unchanged.
pub fn format_reader_list(readers: &[ReaderInfo]) -> String {
    if readers.is_empty() {
        return "(no active readers)\n".to_owned();
    }
    let mut s = String::from("    pid     thread     txnid\n");
    for r in readers {
        match r.txnid {
            Some(t) => s.push_str(&format!("{:10} {:x} {}\n", r.pid, r.thread, t)),
            None => s.push_str(&format!("{:10} {:x} -\n", r.pid, r.thread)),
        }
    }
    s
}

/// Where the read transactions of an environment advertise their
/// snapshots: the lock file's table, shared with other processes, or,
/// without a lock file, an in-memory table of this process's readers.
pub(crate) enum ReaderTable {
    /// The lock file (`me_txns`).
    Shared(LockRegion),
    /// No lock file (`me_txns == NULL` in C, which tracks no readers then):
    /// the in-memory table, shared by every `NO_LOCK` handle of the file.
    Local(Arc<LocalShared>),
}

impl ReaderTable {
    /// `mdb_find_oldest`: the oldest snapshot still in use by a reader, or
    /// `txnid - 1` if there is none older. Without a lock file, the readers
    /// of this process.
    pub(crate) fn find_oldest(&self, txnid: u64) -> u64 {
        match self {
            ReaderTable::Shared(lock) => lock.find_oldest(txnid),
            ReaderTable::Local(local) => local.readers.find_oldest(txnid),
        }
    }
}

/// The open lock file: its mapping, the shared mutexes and our bookkeeping
/// (`me_lfd`, `me_txns`, `me_rmutex`/`me_wmutex`, `me_live_reader`,
/// `me_close_readers`).
pub(crate) struct LockRegion {
    /// Shared mutexes; an `Option` only so `Drop` can close them first.
    locks: Option<SharedLocks>,
    /// The mapped `MDB_txninfo`. Declared before `file` so it is unmapped
    /// before the descriptor (and with it our record locks) goes away.
    map: LockMap,
    file: File,
    /// Where the fields are: the lock-file version of the data format.
    layout: &'static LockLayout,
    /// Number of reader slots in the mapping (`me_maxreaders`).
    max_readers: u32,
    /// Our process id (`me_pid`).
    pid: i32,
    /// `true` while we still hold the exclusive lock on byte 0.
    exclusive: Mutex<bool>,
    /// `me_live_reader`: whether we hold the lock on byte `pid`.
    live_reader: Mutex<bool>,
    /// `me_close_readers`: highest `mti_numreaders` we have seen, i.e. how
    /// many slots `Drop` must scan for leftovers of this process.
    close_readers: AtomicU32,
}

impl LockRegion {
    /// Opens (creating if needed) and maps the lock file at `path`, with
    /// the layout `layout`, as `mdb_env_setup_locks` does. Returns
    /// `Ok(None)` for a read-only environment on a read-only file system
    /// (no lock file is used then).
    ///
    /// The file is claimed in `registration` before it is locked
    /// ([`Registration::claim_lock_file`]): if this process has it open as
    /// the lock file of another handle (under another name), the exclusive
    /// lock below would be granted (record locks do not conflict within a
    /// process) and the table re-initialised under that handle's readers.
    ///
    /// The exclusive opener writes a fresh table of `layout`'s version,
    /// whatever the file held. Everyone else validates the table: wrong
    /// magic is `MDB_INVALID`, a `mti_format` other than
    /// [`LockLayout::lock_format`] (the other version, or the same version
    /// built for another platform) is `MDB_VERSION_MISMATCH`.
    pub(crate) fn open(
        path: &Path,
        mode: u32,
        max_readers: u32,
        read_only: bool,
        layout: &'static LockLayout,
        registration: &mut Registration,
    ) -> Result<Option<LockRegion>> {
        let file = match os::env_file_options(
            OpenOptions::new().read(true).write(true).create(true),
            mode,
        )
        .open(path)
        {
            Ok(f) => registration.claim_lock_file(f)?,
            Err(e) if read_only && os::is_read_only_fs(&e) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // mdb_env_excl_lock: exclusive if possible, else wait for shared.
        let exclusive = match os::lock_byte(&file, 0, LockKind::Exclusive, false) {
            Ok(()) => true,
            Err(_) => {
                os::lock_byte(&file, 0, LockKind::Shared, true)?;
                false
            }
        };

        let size = os::file_size(&file)? as usize;
        let wanted = layout.file_size(max_readers);
        let (len, max_readers) = if size < wanted && exclusive {
            os::ftruncate(&file, wanted as u64)?;
            (wanted, max_readers)
        } else {
            // Use the table size of the existing file, whatever we asked for.
            let n = layout.readers_in_file(size);
            if n == 0 {
                return Err(MdbError::Invalid.into());
            }
            (size, u32::try_from(n).unwrap_or(u32::MAX))
        };

        let map = LockMap::map(&file, len)?;
        let locks = if exclusive {
            // Nobody else is using the table: (re)initialise it.
            let locks = SharedLocks::create(&map, &file, mode, layout)?;
            map.store_u32(TXNINFO_MAGIC, MDB_MAGIC);
            map.store_u32(TXNINFO_FORMAT, layout.lock_format);
            map.store_u64(layout.txnid, 0);
            map.store_u32(layout.num_readers, 0);
            locks
        } else {
            if map.load_u32(TXNINFO_MAGIC) != MDB_MAGIC {
                return Err(MdbError::Invalid.into());
            }
            if map.load_u32(TXNINFO_FORMAT) != layout.lock_format {
                return Err(MdbError::VersionMismatch.into());
            }
            SharedLocks::attach(&map, layout)?
        };

        Ok(Some(LockRegion {
            locks: Some(locks),
            map,
            file,
            layout,
            max_readers,
            pid: os::getpid(),
            exclusive: Mutex::new(exclusive),
            live_reader: Mutex::new(false),
            close_readers: AtomicU32::new(0),
        }))
    }

    /// `true` if we hold the exclusive lock (we initialised the table).
    pub(crate) fn is_exclusive(&self) -> bool {
        *self.exclusive.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `mdb_env_share_locks`: publish `txnid` as the last committed txn and
    /// downgrade our exclusive lock on byte 0 to a shared one, letting other
    /// openers in.
    pub(crate) fn downgrade(&self, txnid: u64) -> Result<()> {
        let mut excl = self.exclusive.lock().unwrap_or_else(|e| e.into_inner());
        self.map.store_u64(self.layout.txnid, txnid);
        os::downgrade_lock(&self.file)?;
        *excl = false;
        Ok(())
    }

    /// `me_maxreaders`: number of slots in the table.
    pub(crate) fn max_readers(&self) -> u32 {
        self.max_readers
    }

    fn locks(&self) -> &SharedLocks {
        // Only `Drop` takes the locks out.
        self.locks.as_ref().expect("lock region already closed")
    }

    /// Locks the writer mutex (`me_wmutex`) for a write transaction,
    /// blocking until other writers (threads or processes) are done.
    ///
    /// If the previous owner died holding it (robust mutexes, Linux), this
    /// repairs state as `mdb_mutex_failed` (mdb.c line 10463) does for the
    /// writer: publish the txnid of the newest meta page in `mti_txnid`
    /// (`newest_txnid`), so the next writer cannot overwrite pages that meta
    /// refers to, clear dead readers, and mark the mutex consistent.
    pub(crate) fn lock_writer(&self, newest_txnid: impl FnOnce() -> Result<u64>) -> Result<()> {
        let m = &self.locks().wmutex;
        match m.lock()? {
            LockStatus::Acquired => Ok(()),
            LockStatus::OwnerDied => {
                let repaired = newest_txnid()
                    .map(|t| self.set_txnid(t))
                    .and_then(|()| self.reader_check0(false))
                    .and_then(|_| m.mark_consistent().map_err(Error::from));
                if let Err(e) = repaired {
                    let _ = m.unlock();
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    /// Unlocks the writer mutex (`UNLOCK_MUTEX(env->me_wmutex)`). Must be
    /// called on the thread that locked it.
    pub(crate) fn unlock_writer(&self) {
        let _ = self.locks().wmutex.unlock();
    }

    // --- header fields ----------------------------------------------------

    /// `mti_txnid`: the last committed transaction.
    pub(crate) fn txnid(&self) -> u64 {
        self.map.load_u64(self.layout.txnid)
    }

    /// Sets `mti_txnid` (writers after commit, and dead-writer recovery).
    pub(crate) fn set_txnid(&self, txnid: u64) {
        self.map.store_u64(self.layout.txnid, txnid)
    }

    /// `mti_numreaders` as stored (may exceed our slot count if corrupt).
    pub(crate) fn raw_num_readers(&self) -> u32 {
        self.map.load_u32(self.layout.num_readers)
    }

    /// `mti_format` as stored.
    #[cfg(test)]
    fn lock_format(&self) -> u32 {
        self.map.load_u32(TXNINFO_FORMAT)
    }

    /// A copy of the header (`mt1` and `mt2`, everything before the reader
    /// table), read through the mapping. (Reading the file through another
    /// descriptor would drop this process's record locks when that
    /// descriptor is closed.)
    #[cfg(test)]
    fn header(&self) -> Option<crate::format::lockfile::TxnInfoSnapshot> {
        crate::format::lockfile::TxnInfoSnapshot::read(
            &self.map.read_bytes(0, self.layout.readers),
            self.layout,
        )
    }

    /// `mti_numreaders`, clamped to the slots we have mapped so that a
    /// corrupt value cannot make us read outside the table.
    fn num_readers(&self) -> usize {
        self.raw_num_readers().min(self.max_readers) as usize
    }

    // --- slot fields ------------------------------------------------------

    fn slot_pid(&self, i: usize) -> i32 {
        self.map.load_i32(self.layout.reader_offset(i) + READER_PID)
    }

    fn set_slot_pid(&self, i: usize, pid: i32) {
        self.map
            .store_i32(self.layout.reader_offset(i) + READER_PID, pid)
    }

    pub(crate) fn slot_txnid(&self, i: usize) -> u64 {
        self.map
            .load_u64(self.layout.reader_offset(i) + READER_TXNID)
    }

    pub(crate) fn set_slot_txnid(&self, i: usize, txnid: u64) {
        self.map
            .store_u64(self.layout.reader_offset(i) + READER_TXNID, txnid)
    }

    /// `mr_tid`: a `pthread_t` (8 bytes) on Unix, a `DWORD` on Windows.
    fn slot_tid(&self, i: usize) -> u64 {
        let off = self.layout.reader_offset(i) + READER_TID;
        match READER_TID_SIZE {
            4 => u64::from(self.map.load_u32(off)),
            _ => self.map.load_u64(off),
        }
    }

    fn set_slot_tid(&self, i: usize, tid: u64) {
        let off = self.layout.reader_offset(i) + READER_TID;
        match READER_TID_SIZE {
            4 => self.map.store_u32(off, tid as u32),
            _ => self.map.store_u64(off, tid),
        }
    }

    // --- the reader mutex ---------------------------------------------------

    /// Locks `me_rmutex`, repairing the table if its previous owner died
    /// (`LOCK_MUTEX` + `mdb_mutex_failed` for the reader mutex).
    fn lock_readers(&self) -> Result<ReaderTableGuard<'_>> {
        let m = &self.locks().rmutex;
        match m.lock()? {
            LockStatus::Acquired => Ok(ReaderTableGuard { mutex: m }),
            LockStatus::OwnerDied => {
                let repaired = self
                    .reader_check0(true)
                    .and_then(|_| m.mark_consistent().map_err(Error::from));
                match repaired {
                    Ok(()) => Ok(ReaderTableGuard { mutex: m }),
                    Err(e) => {
                        let _ = m.unlock();
                        Err(e)
                    }
                }
            }
        }
    }

    // --- reader slots -------------------------------------------------------

    /// Claims a free reader slot for this process, as `mdb_txn_renew0` does
    /// for a read transaction without a slot. Returns the slot index; the
    /// slot's txnid is `!0` (owned, not in a transaction).
    pub(crate) fn acquire_slot(&self) -> Result<usize> {
        {
            let mut live = self.live_reader.lock().unwrap_or_else(|e| e.into_inner());
            if !*live {
                // mdb_reader_pid(Pidset): advertise that this pid is alive.
                os::set_pid_lock(&self.file, self.pid)?;
                *live = true;
            }
        }

        let guard = self.lock_readers()?;
        let mut nr = self.num_readers();
        let i = (0..nr).find(|&i| self.slot_pid(i) == 0).unwrap_or(nr);
        if i >= self.max_readers as usize {
            drop(guard);
            return Err(MdbError::ReadersFull.into());
        }
        // Claim the slot carefully, since other code uses the table without
        // the mutex: first reset it, next publish it in mti_numreaders.
        // After that it is safe for an env close to touch it. Writing our
        // pid finally claims it.
        self.set_slot_pid(i, 0);
        self.set_slot_txnid(i, !0);
        self.set_slot_tid(i, os::thread_id());
        if i == nr {
            nr += 1;
            self.map.store_u32(self.layout.num_readers, nr as u32);
        }
        self.close_readers.fetch_max(nr as u32, Ordering::SeqCst);
        self.set_slot_pid(i, self.pid);
        drop(guard);
        Ok(i)
    }

    /// `true` if slot `i` is still ours and idle (`mr_pid == me_pid` and
    /// `mr_txnid == !0`); `mdb_txn_renew0` returns `MDB_BAD_RSLOT` otherwise.
    pub(crate) fn slot_is_idle_and_ours(&self, i: usize) -> bool {
        self.slot_pid(i) == self.pid && self.slot_txnid(i) == !0
    }

    /// Publishes the current `mti_txnid` in slot `i` and returns it,
    /// re-reading until both agree (ITS#7970: a writer may commit between
    /// our read and our store).
    pub(crate) fn publish_current_txnid(&self, i: usize) -> u64 {
        loop {
            let t = self.txnid();
            self.set_slot_txnid(i, t);
            if self.txnid() == t {
                return t;
            }
        }
    }

    /// Marks slot `i` as owned but not in a transaction (`mdb_txn_reset`).
    pub(crate) fn reset_slot(&self, i: usize) {
        self.set_slot_txnid(i, !0);
    }

    /// Releases slot `i` (`mdb_txn_end` with `MDB_END_SLOT`).
    pub(crate) fn release_slot(&self, i: usize) {
        self.set_slot_txnid(i, !0);
        self.set_slot_pid(i, 0);
    }

    /// `mdb_find_oldest`: the oldest snapshot still in use by any reader, or
    /// `txnid - 1` if there is none older.
    pub(crate) fn find_oldest(&self, txnid: u64) -> u64 {
        let mut oldest = txnid.wrapping_sub(1);
        for i in (0..self.num_readers()).rev() {
            if self.slot_pid(i) != 0 {
                oldest = oldest.min(self.slot_txnid(i));
            }
        }
        oldest
    }

    /// The occupied slots, as `mdb_reader_list` walks them.
    pub(crate) fn reader_list(&self) -> Vec<ReaderInfo> {
        (0..self.num_readers())
            .filter_map(|i| {
                let pid = self.slot_pid(i);
                if pid == 0 {
                    return None;
                }
                let txnid = self.slot_txnid(i);
                Some(ReaderInfo {
                    pid,
                    thread: self.slot_tid(i),
                    txnid: (txnid != !0).then_some(txnid),
                })
            })
            .collect()
    }

    /// `mdb_reader_check`: clears slots of processes that died without
    /// cleaning up. Returns the number of slots cleared.
    pub(crate) fn reader_check(&self) -> Result<usize> {
        self.reader_check0(false)
    }

    /// `mdb_reader_pid(Pidcheck)`: `false` only if the process is certainly
    /// dead (nobody holds the lock on byte `pid`; on Windows, the process
    /// does not exist any more).
    fn pid_alive(&self, pid: i32) -> bool {
        os::pid_alive(&self.file, pid)
    }

    /// `mdb_reader_check0`. `rlocked` is set if the caller holds the reader
    /// mutex already.
    fn reader_check0(&self, rlocked: bool) -> Result<usize> {
        let mut rdrs = self.num_readers();
        let mut checked: Vec<i32> = Vec::new();
        let mut count = 0;
        let mut i = 0;
        while i < rdrs {
            let pid = self.slot_pid(i);
            if pid != 0 && pid != self.pid {
                // mdb_pid_insert: look at each pid only once.
                if let Err(pos) = checked.binary_search(&pid) {
                    checked.insert(pos, pid);
                    if !self.pid_alive(pid) {
                        // Stale reader found.
                        let mut j = i;
                        let mut guard = None;
                        if !rlocked {
                            let m = &self.locks().rmutex;
                            match m.lock()? {
                                LockStatus::Acquired => {
                                    // Recheck: a new process may have reused the pid.
                                    if self.pid_alive(pid) {
                                        j = rdrs;
                                    }
                                }
                                LockStatus::OwnerDied => {
                                    // mdb_mutex_failed checks all readers itself.
                                    let repaired = self
                                        .reader_check0(true)
                                        .and_then(|_| m.mark_consistent().map_err(Error::from));
                                    if let Err(e) = repaired {
                                        let _ = m.unlock();
                                        return Err(e);
                                    }
                                    rdrs = 0;
                                }
                            }
                            guard = Some(ReaderTableGuard { mutex: m });
                        }
                        while j < rdrs {
                            if self.slot_pid(j) == pid {
                                self.set_slot_pid(j, 0);
                                count += 1;
                            }
                            j += 1;
                        }
                        drop(guard);
                    }
                }
            }
            i += 1;
        }
        Ok(count)
    }
}

impl Drop for LockRegion {
    /// The lock-file part of `mdb_env_close0`: clear any slot this process
    /// still owns, close the shared locks (removing semaphores if we are the
    /// last user), then unmap and close the file. Closing the file releases
    /// our `fcntl` locks.
    fn drop(&mut self) {
        let n = (self.close_readers.load(Ordering::SeqCst)).min(self.max_readers) as usize;
        for i in (0..n).rev() {
            if self.slot_pid(i) == self.pid {
                self.set_slot_pid(i, 0);
            }
        }
        if let Some(locks) = self.locks.take() {
            let last_user = self.layout.objects.removed_by_last_user()
                && os::lock_byte(&self.file, 0, LockKind::Exclusive, false).is_ok();
            locks.close(last_user);
        }
        // Windows releases the lock on byte 0 now rather than "not
        // necessarily at once" after the handle closes (mdb_env_close0).
        os::unlock_lock_file(&self.file);
    }
}

// ---------------------------------------------------------------------------
// The in-memory reader table
// ---------------------------------------------------------------------------

/// `mr_txnid` of an entry that pins no snapshot (`(txnid_t)-1`).
const IDLE: u64 = !0;

/// The reader table of an environment without a lock file: the in-memory
/// counterpart of the lock file's table, for the read transactions of this
/// process (see the module documentation), in all handles of the file.
///
/// Each read transaction owns an entry ([`LocalSlot`]) as long as it (or
/// the [`InactiveTxn`](crate::core::InactiveTxn) a reset leaves) lives, and
/// publishes its snapshot's txnid in it, as in a lock-file slot. Owning,
/// publishing, resetting and releasing are atomic operations on the entry;
/// the mutex only guards the list, which claiming an entry and
/// [`LocalReaders::find_oldest`] walk. Unlike the lock file's, the table has
/// no fixed size: C has no limit on read transactions without a lock file
/// either (no `MDB_READERS_FULL`).
#[derive(Debug, Default)]
pub(crate) struct LocalReaders {
    /// Every entry ever claimed; released ones are reused.
    entries: Mutex<Vec<Arc<LocalEntry>>>,
}

/// One entry of [`LocalReaders`] (an `MDB_reader` without pid and thread).
#[derive(Debug)]
struct LocalEntry {
    /// The snapshot its owner reads (`mr_txnid`), or [`IDLE`].
    txnid: AtomicU64,
    /// `true` while a transaction owns it (`mr_pid != 0`).
    owned: AtomicBool,
}

/// A read transaction's entry in [`LocalReaders`]. Dropping it releases the
/// entry.
#[derive(Debug)]
pub(crate) struct LocalSlot(Arc<LocalEntry>);

impl LocalReaders {
    /// Claims an entry for a read transaction, reusing a released one
    /// first (the slot search of `mdb_txn_renew0`). Its txnid is [`IDLE`].
    pub(crate) fn acquire(&self) -> LocalSlot {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Entries are only claimed under the mutex, so a free one stays
        // free until we take it. The acquire load pairs with the release
        // in `LocalSlot::drop`: the previous owner's last store (`IDLE`)
        // comes before ours.
        let entry = match entries.iter().find(|e| !e.owned.load(Ordering::Acquire)) {
            Some(e) => Arc::clone(e),
            None => {
                let e = Arc::new(LocalEntry {
                    txnid: AtomicU64::new(IDLE),
                    owned: AtomicBool::new(false),
                });
                entries.push(Arc::clone(&e));
                e
            }
        };
        entry.owned.store(true, Ordering::Relaxed);
        LocalSlot(entry)
    }

    /// `mdb_find_oldest` over this process's readers: the oldest snapshot
    /// one of them pins, or `txnid - 1` if there is none older.
    pub(crate) fn find_oldest(&self, txnid: u64) -> u64 {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Pairs with the fence in `LocalSlot::publish`: a reader whose
        // publication we miss sees the committed txnid move when it
        // re-checks it, and retries (`RoTxn::take_snapshot`).
        fence(Ordering::SeqCst);
        entries
            .iter()
            .map(|e| e.txnid.load(Ordering::SeqCst))
            .fold(txnid.wrapping_sub(1), u64::min)
    }

    /// The number of entries that pin a snapshot.
    pub(crate) fn active(&self) -> usize {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .filter(|e| e.txnid.load(Ordering::SeqCst) != IDLE)
            .count()
    }
}

impl LocalSlot {
    /// Pins snapshot `txnid` (`r->mr_txnid = txnid`). Writers that look at
    /// the table after this see it; see `RoTxn::take_snapshot` for those
    /// that looked before.
    pub(crate) fn publish(&self, txnid: u64) {
        self.0.txnid.store(txnid, Ordering::SeqCst);
        // Order the store before the caller's re-read of the committed
        // txnid.
        fence(Ordering::SeqCst);
    }

    /// Unpins the snapshot but keeps the entry (`mdb_txn_reset`).
    pub(crate) fn reset(&self) {
        self.0.txnid.store(IDLE, Ordering::SeqCst);
    }
}

impl Drop for LocalSlot {
    /// Unpins the snapshot and releases the entry (`mdb_txn_end` with
    /// `MDB_END_SLOT`).
    fn drop(&mut self) {
        self.0.txnid.store(IDLE, Ordering::SeqCst);
        self.0.owned.store(false, Ordering::Release);
    }
}

/// Holds the reader-table mutex; unlocks on drop (`UNLOCK_MUTEX`).
struct ReaderTableGuard<'a> {
    mutex: &'a SharedMutex,
}

impl Drop for ReaderTableGuard<'_> {
    fn drop(&mut self) {
        let _ = self.mutex.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_list_text_matches_c() {
        assert_eq!(format_reader_list(&[]), "(no active readers)\n");
        let s = format_reader_list(&[
            ReaderInfo {
                pid: 1234,
                thread: 0x7fff_1234,
                txnid: Some(42),
            },
            ReaderInfo {
                pid: 99,
                thread: 0xab,
                txnid: None,
            },
        ]);
        assert_eq!(
            s,
            "    pid     thread     txnid\n      1234 7fff1234 42\n        99 ab -\n"
        );
    }

    use crate::format::lockfile::{LOCK_V09, LOCK_V10};

    /// The lock file `path` (in a directory of its own), claimed in `reg`,
    /// which must outlive it.
    fn open(
        path: &Path,
        layout: &'static LockLayout,
        reg: &mut Registration,
    ) -> Result<LockRegion> {
        Ok(LockRegion::open(path, 0o644, 4, false, layout, reg)?.unwrap())
    }

    fn registration(lock_path: &Path) -> Registration {
        Registration::claim_path(lock_path.parent().unwrap(), false, false).unwrap()
    }

    #[test]
    fn fresh_table() {
        for layout in [&LOCK_V09, &LOCK_V10] {
            fresh_table_of(layout);
        }
    }

    fn fresh_table_of(layout: &'static LockLayout) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.mdb");
        let mut reg = registration(&path);
        let region = open(&path, layout, &mut reg).unwrap();
        assert!(region.is_exclusive());
        assert_eq!(region.max_readers(), 4);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            layout.file_size(4)
        );
        region.downgrade(7).unwrap();
        assert!(!region.is_exclusive());
        assert_eq!(region.txnid(), 7);

        let a = region.acquire_slot().unwrap();
        let b = region.acquire_slot().unwrap();
        assert_eq!((a, b), (0, 1));
        assert_eq!(region.raw_num_readers(), 2);
        assert!(region.slot_is_idle_and_ours(a));
        assert_eq!(region.publish_current_txnid(a), 7);
        assert_eq!(region.find_oldest(10), 7);
        let list = region.reader_list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].txnid, Some(7));
        assert_eq!(list[1].txnid, None);
        region.release_slot(a);
        // A released slot is reused before the table grows.
        assert_eq!(region.acquire_slot().unwrap(), 0);
        let _c = region.acquire_slot().unwrap();
        let _d = region.acquire_slot().unwrap();
        assert!(matches!(
            region.acquire_slot(),
            Err(Error::Mdb(MdbError::ReadersFull))
        ));
        // Our own slots are never "stale".
        assert_eq!(region.reader_check().unwrap(), 0);

        // The header is where the layout says, with the version's format
        // word.
        let h = region.header().unwrap();
        assert_eq!(h.magic, MDB_MAGIC);
        assert_eq!(h.format, layout.lock_format);
        assert_eq!(h.txnid, 7);
        assert_eq!(h.num_readers, 4);
        assert_eq!(h.mutex_id.is_some(), layout.objects.stores_hash());
        region.lock_writer(|| Ok(7)).unwrap();
        region.unlock_writer();
    }

    /// An exclusive opener re-initialises a table of the other version in
    /// its own. (Refusing it while another process holds the file needs a
    /// second process: `interop/tests/lockfile10.rs`.)
    #[test]
    fn exclusive_opener_reinitialises_other_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.mdb");
        for layout in [&LOCK_V09, &LOCK_V10, &LOCK_V09, &LOCK_V10] {
            let mut reg = registration(&path);
            let region = open(&path, layout, &mut reg).unwrap();
            assert!(region.is_exclusive());
            assert_eq!(region.lock_format(), layout.lock_format);
            region.downgrade(5).unwrap();
            let slot = region.acquire_slot().unwrap();
            assert_eq!(slot, 0, "numreaders was reset");
            assert_eq!(region.publish_current_txnid(slot), 5);
            let h = region.header().unwrap();
            assert_eq!(
                (h.format, h.txnid, h.num_readers),
                (layout.lock_format, 5, 1)
            );
            assert_eq!(region.reader_list().len(), 1);
        }
    }

    // --- readers pin their pages, with or without a lock file -------------

    use std::collections::BTreeMap;

    use crate::core::audit::{page_use, PageUse};
    use crate::core::txn_rw::TXN_SPILLS;
    use crate::core::{Dbi, Environment, RoTxn, RwTxn, Transaction};
    use crate::flags::{EnvironmentFlags, WriteFlags};

    /// Keys of the scenarios below.
    const KEYS: u32 = 300;

    fn scenario_env(dir: &Path, flags: EnvironmentFlags) -> Environment {
        Environment::builder()
            .map_size(256 << 20)
            .flags(flags | EnvironmentFlags::NO_SYNC)
            .open(dir)
            .unwrap()
    }

    /// Value of key `i` in round `round`: every seventh an overflow run.
    fn value(i: u32, round: u32, page_size: usize) -> Vec<u8> {
        let len = if i % 7 == 0 { 2 * page_size } else { 100 };
        let mut v = vec![(i ^ round) as u8; len];
        v[..4].copy_from_slice(&round.to_le_bytes());
        v
    }

    /// Rewrites every key in `txn`, which frees every page of the tree.
    fn rewrite(txn: &mut RwTxn<'_>, round: u32) {
        rewrite_keys(txn, round, 0..KEYS);
    }

    /// Rewrites the keys `keys` in `txn`.
    fn rewrite_keys(txn: &mut RwTxn<'_>, round: u32, keys: std::ops::Range<u32>) {
        let ps = txn.psize();
        for i in keys {
            txn.put(
                Dbi::MAIN,
                &i.to_be_bytes(),
                &value(i, round, ps),
                WriteFlags::empty(),
            )
            .unwrap();
        }
    }

    /// One write transaction of [`rewrite`].
    fn commit_round(env: &Environment, round: u32) {
        let mut txn = env.begin_rw_txn().unwrap();
        rewrite(&mut txn, round);
        txn.commit().unwrap();
    }

    /// What a reader saw when it began: the bytes of every page of its
    /// snapshot, its page use, and some values it holds.
    struct Pinned<'t> {
        pages: BTreeMap<u64, Vec<u8>>,
        uses: PageUse,
        held: Vec<(&'t [u8], Vec<u8>)>,
    }

    fn pin<'t>(txn: &'t RoTxn<'_>) -> Pinned<'t> {
        let uses = page_use(txn);
        let ps = txn.env().page_size();
        let pages = uses
            .tree
            .iter()
            .map(|&p| (p, mapped(txn, p, ps).to_vec()))
            .collect();
        let held = (0..KEYS)
            .step_by(13)
            .map(|i| {
                let v = txn.get(Dbi::MAIN, &i.to_be_bytes()).unwrap().unwrap();
                (v, v.to_vec())
            })
            .collect();
        Pinned { pages, uses, held }
    }

    /// Page `p` as the reader's map holds it now.
    fn mapped<'t>(txn: &'t RoTxn<'_>, p: u64, ps: usize) -> &'t [u8] {
        txn.core().map.slice(p as usize * ps, ps).unwrap()
    }

    /// The reader's bytes did not change and none of its pages was reused.
    /// `freed` is the page use right after the first commit since the
    /// reader began, which freed (nearly) all of its pages; such a page must
    /// stay in the free record that freed it for as long as the reader
    /// lives (`now`), and any other page of the snapshot must still be in a
    /// tree, unchanged, or in a free record newer than the snapshot.
    fn check_pinned(txn: &RoTxn<'_>, pinned: &Pinned<'_>, freed: &PageUse, now: &PageUse) {
        let ps = txn.env().page_size();
        let id = txn.id();
        for (&p, bytes) in &pinned.pages {
            assert!(
                mapped(txn, p, ps) == &bytes[..],
                "page {p} of the reader's snapshot {id} changed under it"
            );
            match freed.free.get(&p) {
                Some(&rec) if rec > id => assert_eq!(
                    now.free.get(&p),
                    Some(&rec),
                    "page {p} of snapshot {id}, freed by txn {rec}, was reused"
                ),
                _ if now.tree.contains(&p) => {}
                _ => assert!(
                    now.free.get(&p).is_some_and(|&rec| rec > id),
                    "page {p} of snapshot {id} was reused"
                ),
            }
        }
        for (v, copy) in &pinned.held {
            assert_eq!(v, copy, "a value the reader holds changed");
        }
    }

    /// The first commit after a reader began freed most of its pages (so
    /// [`check_pinned`] tests something).
    fn assert_freed_most(pinned: &Pinned<'_>, id: u64, freed: &PageUse) {
        let n = pinned
            .pages
            .keys()
            .filter(|p| freed.free.get(p).is_some_and(|&rec| rec > id))
            .count();
        assert!(n * 2 > pinned.pages.len(), "{n} of {}", pinned.pages.len());
    }

    /// The pages that were free in `before` and have been reused since:
    /// they are in a tree in `after`, or in a newer free record (reused,
    /// then freed again).
    fn reused<'a>(before: &'a PageUse, after: &'a PageUse) -> impl Iterator<Item = u64> + 'a {
        before.free.iter().filter_map(|(&p, &rec)| {
            let again = after.tree.contains(&p) || after.free.get(&p).is_some_and(|&r| r > rec);
            again.then_some(p)
        })
    }

    fn newest(env: &Environment) -> PageUse {
        page_use(&env.begin_ro_txn().unwrap())
    }

    /// A reader pins its snapshot while a dozen commits rewrite every page;
    /// then it ends and its pages are reused. Returns the newest page use.
    fn long_reader(flags: EnvironmentFlags) -> PageUse {
        long_reader_with(flags, false)
    }

    /// [`long_reader`], with the writes in a second handle of the file if
    /// `two_handles` (`NO_LOCK` only): its writers keep the pages of the
    /// first handle's reader as if it were their own.
    fn long_reader_with(flags: EnvironmentFlags, two_handles: bool) -> PageUse {
        let dir = tempfile::tempdir().unwrap();
        let env = scenario_env(dir.path(), flags);
        let second;
        let writer = if two_handles {
            second = scenario_env(dir.path(), flags);
            &second
        } else {
            &env
        };
        for round in 0..3 {
            commit_round(writer, round);
        }
        let reader = env.begin_ro_txn().unwrap();
        let pinned = pin(&reader);
        commit_round(writer, 3);
        let freed = newest(writer);
        assert_freed_most(&pinned, reader.id(), &freed);
        for round in 4..15 {
            commit_round(writer, round);
        }
        let now = newest(writer);
        check_pinned(&reader, &pinned, &freed, &now);
        // The writers did reuse pages meanwhile (the free ones of the
        // reader's snapshot).
        assert!(
            reused(&pinned.uses, &now).next().is_some(),
            "no page was reused while the reader was live"
        );
        let reader_pages: Vec<u64> = pinned.pages.keys().copied().collect();
        drop(pinned);
        drop(reader);
        // Once it has ended, its pages are reused.
        for round in 15..18 {
            commit_round(writer, round);
        }
        let after = newest(writer);
        assert!(
            reused(&now, &after).any(|p| reader_pages.contains(&p)),
            "the ended reader's pages were not reused"
        );
        crate::core::audit::audit(writer);
        after
    }

    #[test]
    fn no_lock_reader_pins_its_pages() {
        let no_lock = long_reader(EnvironmentFlags::NO_LOCK);
        // With a lock file the writers make the same decisions.
        let locked = long_reader(EnvironmentFlags::empty());
        assert_eq!(no_lock, locked);
    }

    /// The reader in one `NO_LOCK` handle, the writers in another: the same
    /// decisions as one handle with a lock file.
    #[test]
    fn no_lock_handles_of_one_file_pin_each_others_pages() {
        let two = long_reader_with(EnvironmentFlags::NO_LOCK, true);
        let locked = long_reader(EnvironmentFlags::empty());
        assert_eq!(two, locked);
    }

    /// A handle closed (on another thread) while another handle's reader
    /// is live leaves the shared table alone: later handles' writers keep
    /// the reader's pages.
    #[test]
    fn no_lock_closing_a_handle_keeps_the_other_handles_readers() {
        let dir = tempfile::tempdir().unwrap();
        let a = scenario_env(dir.path(), EnvironmentFlags::NO_LOCK);
        let b = scenario_env(dir.path(), EnvironmentFlags::NO_LOCK);
        for round in 0..3 {
            commit_round(&a, round);
        }
        let reader = b.begin_ro_txn().unwrap();
        let pinned = pin(&reader);
        // A reader of A's that ends with it.
        let txn = a.begin_ro_txn().unwrap();
        commit_round(&a, 3);
        let freed = newest(&a);
        assert_freed_most(&pinned, reader.id(), &freed);
        drop(txn);
        std::thread::spawn(move || drop(a)).join().unwrap();
        assert_eq!(b.in_memory_readers(), Some(1));
        let c = scenario_env(dir.path(), EnvironmentFlags::NO_LOCK);
        assert_eq!(c.in_memory_readers(), Some(1));
        for round in 4..15 {
            commit_round(&c, round);
        }
        check_pinned(&reader, &pinned, &freed, &newest(&c));
        drop(pinned);
        drop(reader);
        assert_eq!(c.in_memory_readers(), Some(0));
        crate::core::audit::audit(&c);
    }

    /// One round of [`spilling_reader`]: half of the keys in a write
    /// transaction, the other half in a nested one, both spilling. Returns
    /// whether they spilled.
    fn nested_round(env: &Environment, round: u32) -> bool {
        let mut txn = env.begin_rw_txn().unwrap();
        rewrite_keys(&mut txn, round, 0..KEYS / 2);
        let mut spilled = txn.w.flags & TXN_SPILLS != 0;
        let mut child = txn.begin_nested_txn().unwrap();
        let before = child.core.spilled.len();
        rewrite_keys(&mut child, round, KEYS / 2..KEYS);
        spilled &= child.core.spilled.len() > before;
        child.commit().unwrap();
        txn.commit().unwrap();
        spilled
    }

    /// [`long_reader`] with write transactions that spill, in nested
    /// transactions too: spilled pages go to the file before the commit, so
    /// they must not be pages the reader reads either (`mdb_page_spill`
    /// writes pages `mdb_page_alloc` took, which asked `mdb_find_oldest`).
    fn spilling_reader(flags: EnvironmentFlags) -> PageUse {
        let dir = tempfile::tempdir().unwrap();
        let env = scenario_env(dir.path(), flags);
        // (An overflow run is one entry of the dirty list.)
        env.set_dirty_limit(16);
        for round in 0..3 {
            assert!(nested_round(&env, round));
        }
        let reader = env.begin_ro_txn().unwrap();
        let pinned = pin(&reader);
        assert!(nested_round(&env, 3));
        let freed = newest(&env);
        assert_freed_most(&pinned, reader.id(), &freed);
        for round in 4..15 {
            assert!(nested_round(&env, round), "round {round} did not spill");
        }
        let now = newest(&env);
        check_pinned(&reader, &pinned, &freed, &now);
        assert!(reused(&pinned.uses, &now).next().is_some());
        let reader_pages: Vec<u64> = pinned.pages.keys().copied().collect();
        drop(pinned);
        drop(reader);
        for round in 15..18 {
            nested_round(&env, round);
        }
        let after = newest(&env);
        assert!(reused(&now, &after).any(|p| reader_pages.contains(&p)));
        crate::core::audit::audit(&env);
        after
    }

    #[test]
    fn no_lock_reader_pins_its_pages_against_spills_and_nested_txns() {
        let no_lock = spilling_reader(EnvironmentFlags::NO_LOCK);
        let locked = spilling_reader(EnvironmentFlags::empty());
        assert_eq!(no_lock, locked);
    }

    /// The oldest snapshot the writer of `env` would have to keep, as its
    /// next write transaction (txnid `next`) sees it.
    fn oldest(env: &Environment, next: u64) -> u64 {
        env.readers().find_oldest(next)
    }

    /// Entries of the in-memory table (claimed or not).
    fn local_entries(env: &Environment) -> usize {
        match env.readers() {
            ReaderTable::Local(local) => local.readers.entries.lock().unwrap().len(),
            ReaderTable::Shared(_) => panic!("environment has a lock file"),
        }
    }

    #[test]
    fn no_lock_begin_reset_renew_and_end_update_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let env = scenario_env(dir.path(), EnvironmentFlags::NO_LOCK);
        for round in 0..3 {
            commit_round(&env, round);
        }
        let next = 10;
        assert_eq!(oldest(&env, next), next - 1);

        let a = env.begin_ro_txn().unwrap();
        let id = a.id();
        assert_eq!(oldest(&env, next), id);
        assert_eq!(a.reader_slot(), None);
        commit_round(&env, 3);
        let b = env.begin_ro_txn().unwrap();
        assert_eq!(b.id(), id + 1);
        assert_eq!(env.in_memory_readers(), Some(2));
        assert_eq!(oldest(&env, next), id);

        // A reset transaction pins nothing; renewed, it pins the newest
        // snapshot.
        let inactive = a.reset();
        assert_eq!(env.in_memory_readers(), Some(1));
        assert_eq!(oldest(&env, next), id + 1);
        commit_round(&env, 4);
        let a = inactive.renew().unwrap();
        assert_eq!(a.id(), id + 2);
        assert_eq!(oldest(&env, next), id + 1);

        // Abort, commit and drop on another thread unregister.
        b.abort();
        assert_eq!(oldest(&env, next), id + 2);
        std::thread::scope(|s| {
            s.spawn(move || drop(a)).join().unwrap();
        });
        assert_eq!(env.in_memory_readers(), Some(0));
        assert_eq!(oldest(&env, next), next - 1);
        let c = env.begin_ro_txn().unwrap();
        assert_eq!(oldest(&env, next), id + 2);
        c.commit().unwrap();
        // An inactive transaction that is dropped releases its entry.
        drop(env.begin_ro_txn().unwrap().reset());
        assert_eq!(env.in_memory_readers(), Some(0));
        assert_eq!(oldest(&env, next), next - 1);
        // Released entries are reused.
        assert_eq!(local_entries(&env), 2);
        for _ in 0..10 {
            let t = env.begin_ro_txn().unwrap();
            let u = env.begin_ro_txn().unwrap();
            drop((t, u));
        }
        assert_eq!(local_entries(&env), 2);
        // No limit, as in C without a lock file (no MDB_READERS_FULL).
        let many: Vec<_> = (0..300).map(|_| env.begin_ro_txn().unwrap()).collect();
        assert_eq!(env.in_memory_readers(), Some(300));
        drop(many);
        assert_eq!(env.in_memory_readers(), Some(0));
    }

    /// Readers on several threads hold values while a writer commits: no
    /// value changes under a reader, also when a reader begins while
    /// commits happen (the publication race of `RoTxn::take_snapshot`).
    #[test]
    fn no_lock_readers_on_other_threads_keep_their_snapshots() {
        use std::sync::atomic::AtomicU64;

        let dir = tempfile::tempdir().unwrap();
        let env = scenario_env(dir.path(), EnvironmentFlags::NO_LOCK);
        let keys = 64u32;
        let val = |i: u32, round: u64| -> Vec<u8> {
            let mut v = vec![i as u8; 200];
            v[..8].copy_from_slice(&round.to_le_bytes());
            v
        };
        let write = |round: u64| {
            let mut txn = env.begin_rw_txn().unwrap();
            for i in 0..keys {
                txn.put(
                    Dbi::MAIN,
                    &i.to_be_bytes(),
                    &val(i, round),
                    WriteFlags::empty(),
                )
                .unwrap();
            }
            txn.commit().unwrap();
        };
        write(0);
        let commits = AtomicU64::new(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let mut snapshots = 0;
                    while !done.load(Ordering::SeqCst) || snapshots < 3 {
                        let txn = env.begin_ro_txn().unwrap();
                        let held: Vec<&[u8]> = (0..keys)
                            .map(|i| txn.get(Dbi::MAIN, &i.to_be_bytes()).unwrap().unwrap())
                            .collect();
                        let copies: Vec<Vec<u8>> = held.iter().map(|v| v.to_vec()).collect();
                        let round = u64::from_le_bytes(copies[0][..8].try_into().unwrap());
                        for (i, v) in copies.iter().enumerate() {
                            assert_eq!(v, &val(i as u32, round), "torn snapshot");
                        }
                        // Hold the values across a few commits.
                        let seen = commits.load(Ordering::SeqCst);
                        while commits.load(Ordering::SeqCst) < seen + 3
                            && !done.load(Ordering::SeqCst)
                        {
                            std::thread::yield_now();
                        }
                        for (v, c) in held.iter().zip(&copies) {
                            assert_eq!(v, c, "a value changed under its reader");
                        }
                        snapshots += 1;
                    }
                });
            }
            for round in 1..300 {
                write(round);
                commits.fetch_add(1, Ordering::SeqCst);
            }
            done.store(true, Ordering::SeqCst);
        });
        assert_eq!(env.in_memory_readers(), Some(0));
        crate::core::audit::audit(&env);
    }
}
