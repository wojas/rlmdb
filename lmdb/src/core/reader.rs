//! The lock file and its reader table.
//!
//! Readers do not lock anything while they read. They advertise the snapshot
//! they use in a slot of the shared reader table, so that writers know which
//! freed pages must not be reused yet (`mdb_find_oldest`). The table lives in
//! the lock file (`lock.mdb` or `<path>-lock`), memory-mapped by every
//! process using the environment.
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
//! * Reader liveness: each process holds an exclusive lock on byte `pid` of
//!   the lock file once it has started a read transaction
//!   (`mdb_reader_pid(Pidset)`); `LockRegion::reader_check` probes those
//!   locks with `F_GETLK` to clear slots of dead processes
//!   (`mdb_reader_check0`, line 10403).
//! * Slot allocation (`LockRegion::acquire_slot`) happens under the reader
//!   mutex and follows the careful publication order of `mdb_txn_renew0`
//!   (line 2758): reset the slot, publish it in `mti_numreaders`, then claim
//!   it by writing our pid.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::consts::MDB_MAGIC;
use crate::error::{Error, MdbError, Result};
use crate::format::lockfile::{self, *};
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
    /// Opens (creating if needed) and maps the lock file at `path`, as
    /// `mdb_env_setup_locks` does. Returns `Ok(None)` for a read-only
    /// environment on a read-only file system (no lock file is used then).
    pub(crate) fn open(
        path: &Path,
        mode: u32,
        max_readers: u32,
        read_only: bool,
    ) -> Result<Option<LockRegion>> {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(mode)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if read_only && e.raw_os_error() == Some(libc::EROFS) => return Ok(None),
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
        let wanted = lock_file_size(max_readers);
        let (len, max_readers) = if size < wanted && exclusive {
            os::ftruncate(&file, wanted as u64)?;
            (wanted, max_readers)
        } else {
            // Use the table size of the existing file, whatever we asked for.
            let n = readers_in_file(size);
            if n == 0 {
                return Err(MdbError::Invalid.into());
            }
            (size, u32::try_from(n).unwrap_or(u32::MAX))
        };

        let map = LockMap::map(&file, len)?;
        let locks = if exclusive {
            // Nobody else is using the table: (re)initialise it.
            let locks = SharedLocks::create(&map, &file, mode)?;
            map.store_u32(TXNINFO_MAGIC, MDB_MAGIC);
            map.store_u32(TXNINFO_FORMAT, LOCK_FORMAT);
            map.store_u64(TXNINFO_TXNID, 0);
            map.store_u32(TXNINFO_NUMREADERS, 0);
            locks
        } else {
            if map.load_u32(TXNINFO_MAGIC) != MDB_MAGIC {
                return Err(MdbError::Invalid.into());
            }
            if map.load_u32(TXNINFO_FORMAT) != LOCK_FORMAT {
                return Err(MdbError::VersionMismatch.into());
            }
            SharedLocks::attach(&map)?
        };

        Ok(Some(LockRegion {
            locks: Some(locks),
            map,
            file,
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
        self.map.store_u64(TXNINFO_TXNID, txnid);
        os::lock_byte(&self.file, 0, LockKind::Shared, false)?;
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

    /// The writer mutex (`me_wmutex`), for write transactions.
    #[allow(dead_code)] // used by the write path (stage 2)
    pub(crate) fn writer_mutex(&self) -> &SharedMutex {
        &self.locks().wmutex
    }

    // --- header fields ----------------------------------------------------

    /// `mti_txnid`: the last committed transaction.
    pub(crate) fn txnid(&self) -> u64 {
        self.map.load_u64(TXNINFO_TXNID)
    }

    /// Sets `mti_txnid` (writers after commit, and dead-writer recovery).
    #[allow(dead_code)] // used by the write path (stage 2)
    pub(crate) fn set_txnid(&self, txnid: u64) {
        self.map.store_u64(TXNINFO_TXNID, txnid)
    }

    /// `mti_numreaders` as stored (may exceed our slot count if corrupt).
    pub(crate) fn raw_num_readers(&self) -> u32 {
        self.map.load_u32(TXNINFO_NUMREADERS)
    }

    /// `mti_numreaders`, clamped to the slots we have mapped so that a
    /// corrupt value cannot make us read outside the table.
    fn num_readers(&self) -> usize {
        self.raw_num_readers().min(self.max_readers) as usize
    }

    // --- slot fields ------------------------------------------------------

    fn slot_pid(&self, i: usize) -> i32 {
        self.map.load_i32(reader_offset(i) + READER_PID)
    }

    fn set_slot_pid(&self, i: usize, pid: i32) {
        self.map.store_i32(reader_offset(i) + READER_PID, pid)
    }

    pub(crate) fn slot_txnid(&self, i: usize) -> u64 {
        self.map.load_u64(reader_offset(i) + READER_TXNID)
    }

    pub(crate) fn set_slot_txnid(&self, i: usize, txnid: u64) {
        self.map.store_u64(reader_offset(i) + READER_TXNID, txnid)
    }

    fn slot_tid(&self, i: usize) -> u64 {
        self.map.load_u64(reader_offset(i) + READER_TID)
    }

    fn set_slot_tid(&self, i: usize, tid: u64) {
        self.map.store_u64(reader_offset(i) + READER_TID, tid)
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
                os::lock_byte(&self.file, self.pid as u64, LockKind::Exclusive, false)?;
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
            self.map.store_u32(TXNINFO_NUMREADERS, nr as u32);
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
    #[allow(dead_code)] // used by the write path (stage 2)
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
    /// dead (nobody holds the lock on byte `pid`).
    fn pid_alive(&self, pid: i32) -> bool {
        os::probe_lock(&self.file, pid as u64).unwrap_or(true)
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
    /// still owns, close the shared locks (unlinking the semaphores on macOS
    /// if we are the last user), then unmap and close the file. Closing the
    /// file releases our `fcntl` locks.
    fn drop(&mut self) {
        let n = (self.close_readers.load(Ordering::SeqCst)).min(self.max_readers) as usize;
        for i in (0..n).rev() {
            if self.slot_pid(i) == self.pid {
                self.set_slot_pid(i, 0);
            }
        }
        if let Some(locks) = self.locks.take() {
            let last_user = lockfile::USES_NAMED_SEMAPHORES
                && os::lock_byte(&self.file, 0, LockKind::Exclusive, false).is_ok();
            locks.close(last_user);
        }
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

    #[test]
    fn fresh_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.mdb");
        let region = LockRegion::open(&path, 0o644, 4, false).unwrap().unwrap();
        assert!(region.is_exclusive());
        assert_eq!(region.max_readers(), 4);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            lock_file_size(4)
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
    }
}
