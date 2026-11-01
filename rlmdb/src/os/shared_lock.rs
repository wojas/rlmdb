//! The two inter-process locks of an environment: the reader-table mutex
//! (`me_rmutex`, guards slot allocation) and the writer mutex (`me_wmutex`,
//! held for the whole of a write transaction).
//!
//! They must be the very objects a C process on the same platform uses, so
//! the implementation follows the C build:
//!
//! * **Linux/FreeBSD** (`MDB_USE_POSIX_MUTEX`, `MDB_USE_ROBUST`): two
//!   process-shared, robust `pthread_mutex_t`s stored *inside* the lock file
//!   at `mti_rmutex` / `mti_wmutex`. Only the exclusive opener initialises
//!   them (`mdb_env_setup_locks`, mdb.c line 4882). If a holder dies,
//!   `pthread_mutex_lock` returns `EOWNERDEAD` and the caller must repair
//!   state and call [`SharedMutex::mark_consistent`] (`mdb_mutex_failed`,
//!   line 10463).
//! * **macOS** (`MDB_USE_POSIX_SEM`): two POSIX named semaphores,
//!   `"/MDBr<hash>"` and `"/MDBw<hash>"`, where `<hash>` encodes an FNV-1a
//!   hash of `(st_dev, st_ino)` of the lock file. The exclusive opener
//!   `sem_unlink`s stale semaphores, then creates both with
//!   `O_CREAT|O_EXCL` and value 1; later openers `sem_open(name, 0)`. How
//!   the names reach them depends on the lock-file version
//!   ([`LockObjects`]): version 1 (LMDB 0.9) stores the names themselves at
//!   the mutex offsets (`mdb_hash_enc`, lines 4823-4880), version 2 (LMDB
//!   1.0) stores the 64-bit hash `mtb_mutexid` and every opener derives
//!   the names from the stored value (`mdb_env_mname_init`, `MUTEXNAME`,
//!   mdb10.c lines 5935-5945 and 6064-6095). Either way, what is in the
//!   file is read, never recomputed. Semaphores are not robust; C has the
//!   same limitation.
//! * **OpenBSD, NetBSD, DragonFly**: version 1 lock files as on macOS
//!   (C 0.9 selects `MDB_USE_POSIX_SEM` for every `BSD` but FreeBSD, mdb.c
//!   line 143), with names one character shorter on NetBSD
//!   (`MDB_SHORT_SEMNAMES`, line 5000). Version 2 lock files (LMDB 1.0)
//!   use a SysV semaphore set instead (`MDB_USE_SYSV_SEM`, mdb10.c lines
//!   192-198): the exclusive opener creates it with
//!   `semget(ftok(lockfile, 'M'), 2, mode | IPC_CREAT)`, sets both values
//!   to 1 and stores its id in `mtb_semid`; the others check it with
//!   `IPC_STAT` and `IPC_SET` (mdb10.c lines 6096-6110 and 6165-6173).
//!   Unlike C, the exclusive opener refuses an existing set of the key
//!   that is not ours to reset (`KNOWN_C_BUGS.md` #46).
//!   Every `semop` uses `SEM_UNDO`, so the kernel releases the semaphore
//!   of a process that dies holding it, and the flag in the lock file
//!   (`mti_rlocked`, `mti_wlocked`) that the dead holder left set makes
//!   the next holder report [`LockStatus::OwnerDied`] (`mdb_sem_wait`,
//!   mdb10.c line 470). The last user removes the set (`IPC_RMID`).
//!
//! A mutex locked here must be unlocked by the same thread on Linux (pthread
//! mutex ownership); the reader-table mutex is only held inside one
//! function call, and the writer will hold `wmutex` on the thread that began
//! the write transaction.

use std::fs::File;
use std::io;

use super::unix::LockMap;
use crate::format::lockfile::{LockLayout, LockObjects};

/// Outcome of a successful [`SharedMutex::lock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    /// The lock was acquired normally.
    Acquired,
    /// The lock was acquired, but its previous owner died while holding it
    /// (`EOWNERDEAD`). The protected data may need repair before
    /// [`SharedMutex::mark_consistent`] is called. Never reported by POSIX
    /// named semaphores (they are not robust), only by robust mutexes and
    /// SysV semaphores.
    #[cfg_attr(any(target_os = "macos", target_os = "ios"), allow(dead_code))]
    OwnerDied,
}

/// The pair of shared locks of an environment.
pub struct SharedLocks {
    /// `me_rmutex`: protects reader slot allocation.
    pub rmutex: SharedMutex,
    /// `me_wmutex`: serialises write transactions.
    pub wmutex: SharedMutex,
}

// ===========================================================================
// Linux / FreeBSD: robust process-shared pthread mutexes in the lock file.
// ===========================================================================

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
mod imp {
    use super::*;

    /// A robust, process-shared `pthread_mutex_t` living in the lock file.
    pub struct SharedMutex {
        mutex: *mut libc::pthread_mutex_t,
    }

    // SAFETY: a process-shared mutex is designed to be used from any thread
    // of any process; the pointer stays valid while the owning `LockMap`
    // lives (LockRegion closes its SharedLocks before its LockMap is
    // unmapped, see core::reader).
    unsafe impl Send for SharedMutex {}
    // SAFETY: as above; pthread mutex operations are thread-safe.
    unsafe impl Sync for SharedMutex {}

    fn mutex_at(map: &LockMap, offset: usize) -> *mut libc::pthread_mutex_t {
        map.raw_ptr(
            offset,
            std::mem::size_of::<libc::pthread_mutex_t>(),
            std::mem::align_of::<libc::pthread_mutex_t>(),
        ) as *mut libc::pthread_mutex_t
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    }

    /// The reader and writer mutexes of a lock file of layout `layout`
    /// (`mti_rmutex`, `mti_wmutex`). `EINVAL` if the layout does not keep
    /// pthread mutexes, which only a layout of another platform could.
    fn mutexes(
        map: &LockMap,
        layout: &LockLayout,
    ) -> io::Result<(*mut libc::pthread_mutex_t, *mut libc::pthread_mutex_t)> {
        match (layout.objects, layout.wlock) {
            (LockObjects::PthreadMutexes, Some(w))
                if layout.rlock_size == std::mem::size_of::<libc::pthread_mutex_t>() =>
            {
                Ok((mutex_at(map, layout.rlock), mutex_at(map, w)))
            }
            _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }

    impl SharedMutex {
        /// Locks the mutex, blocking.
        pub fn lock(&self) -> io::Result<LockStatus> {
            // SAFETY: self.mutex points at an initialised process-shared
            // mutex inside the live lock-file mapping.
            let rc = unsafe { libc::pthread_mutex_lock(self.mutex) };
            match rc {
                0 => Ok(LockStatus::Acquired),
                libc::EOWNERDEAD => Ok(LockStatus::OwnerDied),
                e => Err(io::Error::from_raw_os_error(e)),
            }
        }

        /// Unlocks the mutex (must be called by the locking thread).
        pub fn unlock(&self) -> io::Result<()> {
            // SAFETY: as in `lock`; we hold the mutex.
            check(unsafe { libc::pthread_mutex_unlock(self.mutex) })
        }

        /// `pthread_mutex_consistent`, after repairing the state a dead
        /// owner left behind.
        pub fn mark_consistent(&self) -> io::Result<()> {
            // SAFETY: as in `lock`; we hold the mutex in EOWNERDEAD state.
            check(unsafe { libc::pthread_mutex_consistent(self.mutex) })
        }
    }

    impl SharedLocks {
        /// Initialises both mutexes in a lock file of layout `layout` that
        /// we hold exclusively (`mdb_env_setup_locks`, `*excl > 0` branch).
        pub fn create(
            map: &LockMap,
            _file: &File,
            _mode: u32,
            layout: &LockLayout,
        ) -> io::Result<SharedLocks> {
            let (r, w) = mutexes(map, layout)?;
            // SAFETY: r and w point at properly sized and aligned
            // pthread_mutex_t storage in the mapping. Nobody else uses the
            // lock file while we hold the exclusive fcntl lock, so
            // (re)initialising is race-free. Zeroing first matches C (needed
            // on Solaris, harmless elsewhere).
            unsafe {
                std::ptr::write_bytes(
                    r as *mut u8,
                    0,
                    std::mem::size_of::<libc::pthread_mutex_t>(),
                );
                std::ptr::write_bytes(
                    w as *mut u8,
                    0,
                    std::mem::size_of::<libc::pthread_mutex_t>(),
                );
                let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
                check(libc::pthread_mutexattr_init(&mut attr))?;
                let mut rc =
                    libc::pthread_mutexattr_setpshared(&mut attr, libc::PTHREAD_PROCESS_SHARED);
                if rc == 0 {
                    rc = libc::pthread_mutexattr_setrobust(&mut attr, libc::PTHREAD_MUTEX_ROBUST);
                }
                if rc == 0 {
                    rc = libc::pthread_mutex_init(r, &attr);
                }
                if rc == 0 {
                    rc = libc::pthread_mutex_init(w, &attr);
                }
                libc::pthread_mutexattr_destroy(&mut attr);
                check(rc)?;
            }
            Ok(SharedLocks {
                rmutex: SharedMutex { mutex: r },
                wmutex: SharedMutex { mutex: w },
            })
        }

        /// Uses the mutexes another process initialised.
        pub fn attach(map: &LockMap, layout: &LockLayout) -> io::Result<SharedLocks> {
            let (r, w) = mutexes(map, layout)?;
            Ok(SharedLocks {
                rmutex: SharedMutex { mutex: r },
                wmutex: SharedMutex { mutex: w },
            })
        }

        /// Releases our handles. Process-shared mutexes need no cleanup
        /// (C does not destroy them either).
        pub fn close(self, _last_user: bool) {}
    }
}

// ===========================================================================
// macOS, OpenBSD, NetBSD, DragonFly: POSIX named semaphores, named in (v1)
// or after (v2) the lock file; on the three BSDs, a SysV set for v2.
// ===========================================================================

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::fs::MetadataExt;

    /// One of the two shared locks: a POSIX named semaphore, or one
    /// semaphore of a SysV set (version 2 lock files on OpenBSD, NetBSD and
    /// DragonFly).
    pub struct SharedMutex(Lock);

    enum Lock {
        Named(NamedSemaphore),
        #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        SysV(super::sysv::SysVSemaphore),
    }

    impl SharedMutex {
        /// Locks the semaphore, blocking.
        pub fn lock(&self) -> io::Result<LockStatus> {
            match &self.0 {
                Lock::Named(s) => s.lock(),
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                Lock::SysV(s) => s.lock(),
            }
        }

        /// Unlocks the semaphore.
        pub fn unlock(&self) -> io::Result<()> {
            match &self.0 {
                Lock::Named(s) => s.unlock(),
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                Lock::SysV(s) => s.unlock(),
            }
        }

        /// Nothing to do: named semaphores are not robust, and a SysV
        /// semaphore's flag is already set again by the new holder
        /// (`mdb_mutex_consistent` is 0 with `MDB_USE_SYSV_SEM`).
        pub fn mark_consistent(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A POSIX named semaphore with initial value 1, used as a mutex.
    struct NamedSemaphore {
        sem: *mut libc::sem_t,
        name: CString,
    }

    // SAFETY: semaphore handles may be used from any thread.
    unsafe impl Send for NamedSemaphore {}
    // SAFETY: sem_wait/sem_post are thread-safe.
    unsafe impl Sync for NamedSemaphore {}

    impl NamedSemaphore {
        /// Locks (waits on) the semaphore, retrying on `EINTR` like
        /// `mdb_sem_wait`.
        fn lock(&self) -> io::Result<LockStatus> {
            loop {
                // SAFETY: self.sem is an open semaphore handle until drop.
                let rc = unsafe { libc::sem_wait(self.sem) };
                if rc == 0 {
                    return Ok(LockStatus::Acquired);
                }
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
        }

        /// Unlocks (posts) the semaphore.
        fn unlock(&self) -> io::Result<()> {
            // SAFETY: open semaphore handle.
            if unsafe { libc::sem_post(self.sem) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn open(name: CString, create_mode: Option<u32>) -> io::Result<NamedSemaphore> {
            // SAFETY: name is a valid NUL-terminated string. sem_open is
            // variadic: mode and value are passed as unsigned ints, as the C
            // default argument promotions require.
            let sem = unsafe {
                match create_mode {
                    Some(mode) => libc::sem_open(
                        name.as_ptr(),
                        libc::O_CREAT | libc::O_EXCL,
                        mode as libc::c_uint,
                        1 as libc::c_uint,
                    ),
                    None => libc::sem_open(name.as_ptr(), 0),
                }
            };
            if sem == libc::SEM_FAILED {
                return Err(io::Error::last_os_error());
            }
            Ok(NamedSemaphore { sem, name })
        }

        fn unlink(name: &CString) {
            // SAFETY: valid NUL-terminated name; failure (ENOENT) is fine.
            unsafe {
                libc::sem_unlink(name.as_ptr());
            }
        }
    }

    impl Drop for NamedSemaphore {
        fn drop(&mut self) {
            // SAFETY: the handle came from sem_open and is closed once.
            unsafe {
                libc::sem_close(self.sem);
            }
        }
    }

    /// `mdb_hash_val`: 64-bit FNV-1a over `bytes`, starting from `hval`
    /// (1.0's `mdb_hash` is the same function, starting from
    /// [`HASH_INIT`]).
    pub fn hash_val(bytes: &[u8], mut hval: u64) -> u64 {
        for &b in bytes {
            hval ^= u64::from(b);
            hval = hval
                .wrapping_add(hval << 1)
                .wrapping_add(hval << 4)
                .wrapping_add(hval << 5)
                .wrapping_add(hval << 7)
                .wrapping_add(hval << 8)
                .wrapping_add(hval << 40);
        }
        hval
    }

    /// `MDB_HASH_INIT`.
    pub const HASH_INIT: u64 = 0xcbf2_9ce4_8422_2325;

    const A85: &[u8; 85] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

    /// How many characters of `mdb_hash_enc`'s 10 a version 1 name keeps:
    /// NetBSD drops the last one, for a 14-character name
    /// (`MDB_SHORT_SEMNAMES`, mdb.c lines 4999-5013).
    const NAME_HASH_LEN: usize = if cfg!(target_os = "netbsd") { 9 } else { 10 };

    /// `mdb_pack85` of LMDB 0.9: five base-85 digits of `l`, least
    /// significant first.
    fn pack85(mut l: u64, out: &mut Vec<u8>) {
        for _ in 0..5 {
            out.push(A85[(l % 85) as usize]);
            l /= 85;
        }
    }

    /// `mdb_pack85` of LMDB 1.0 (mdb10.c line 5920): up to ten base-85
    /// digits of `l`, least significant first, stopping early when the
    /// rest is 0 (so a hash of 0 gives no digits at all).
    fn pack85_v10(mut l: u64, out: &mut Vec<u8>) {
        for _ in 0..10 {
            if l == 0 {
                break;
            }
            out.push(A85[(l % 85) as usize]);
            l /= 85;
        }
    }

    /// `MUTEXNAME(env, ch)` of LMDB 1.0 after `mdb_env_mname_init`: the
    /// semaphore name `"/MDB"`, `ch` (`b'r'` or `b'w'`), then
    /// `mdb_pack85` of the stored `mti_mutexid`.
    pub fn mutex_name_v10(mutex_id: u64, ch: u8) -> io::Result<CString> {
        let mut name = b"/MDB".to_vec();
        name.push(ch);
        pack85_v10(mutex_id, &mut name);
        CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }

    /// `mdb_hash_enc`: the 10-character encoding of the FNV-1a hash of
    /// `bytes` (`pack85(h)` then `pack85(h >> 32)`).
    pub fn hash_enc(bytes: &[u8]) -> String {
        let h = hash_val(bytes, HASH_INIT);
        let mut out = Vec::with_capacity(10);
        pack85(h, &mut out);
        pack85(h >> 32, &mut out);
        // All A85 characters are ASCII.
        String::from_utf8(out).unwrap_or_default()
    }

    /// The bytes C hashes: `struct { dev_t dev; ino_t ino; }`. `ino_t` has
    /// 8 bytes on every platform here, so `ino` is at offset 8; `dev_t` has
    /// 4 on macOS, OpenBSD and DragonFly (then 4 bytes of padding) and 8 on
    /// NetBSD. LMDB 0.9 leaves the padding *uninitialised*, so its names
    /// are not reproducible, which is why every opener after the first
    /// reads them from the lock file; LMDB 1.0 zeroes the struct first
    /// (mdb10.c line 6075) but still stores the hash rather than have
    /// others recompute it. We use zero padding.
    pub fn lock_file_id_bytes(dev: u64, ino: u64) -> [u8; 16] {
        const _: () = assert!(
            std::mem::size_of::<libc::dev_t>() <= 8 && std::mem::size_of::<libc::ino_t>() == 8
        );
        let mut b = [0u8; 16];
        let dev = (dev as libc::dev_t).to_ne_bytes();
        b[..dev.len()].copy_from_slice(&dev);
        b[8..16].copy_from_slice(&ino.to_ne_bytes());
        b
    }

    fn read_name(map: &LockMap, offset: usize, len: usize) -> io::Result<CString> {
        let raw = map.read_bytes(offset, len);
        let end = raw
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        CString::new(&raw[..end]).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }

    fn write_name(map: &LockMap, offset: usize, name: &CString) {
        map.write_bytes(offset, name.as_bytes_with_nul());
    }

    fn einval() -> io::Error {
        io::Error::from_raw_os_error(libc::EINVAL)
    }

    impl SharedLocks {
        /// Creates both semaphores for a lock file of layout `layout` that
        /// we hold exclusively, and records in it what the other openers
        /// need to find them (`mdb_env_setup_locks`, `*excl > 0`): the names
        /// (version 1), the hash they are derived from (version 2, macOS)
        /// or the id of the SysV set (version 2, the other BSDs).
        pub fn create(
            map: &LockMap,
            file: &File,
            mode: u32,
            layout: &LockLayout,
        ) -> io::Result<SharedLocks> {
            let md = file.metadata()?;
            let id = lock_file_id_bytes(md.dev(), md.ino());
            let (rname, wname) = match (layout.objects, layout.wlock) {
                (LockObjects::SemaphoreNames, Some(wlock)) => {
                    let enc = hash_enc(&id);
                    let enc = &enc[..NAME_HASH_LEN];
                    let rname = CString::new(format!("/MDBr{enc}")).map_err(|_| einval())?;
                    let wname = CString::new(format!("/MDBw{enc}")).map_err(|_| einval())?;
                    if rname.as_bytes_with_nul().len() > layout.rlock_size {
                        return Err(einval());
                    }
                    write_name(map, layout.rlock, &rname);
                    write_name(map, wlock, &wname);
                    (rname, wname)
                }
                (LockObjects::SemaphoreHash, _) if layout.rlock_size == 8 => {
                    let mutex_id = hash_val(&id, HASH_INIT);
                    map.store_u64(layout.rlock, mutex_id);
                    (
                        mutex_name_v10(mutex_id, b'r')?,
                        mutex_name_v10(mutex_id, b'w')?,
                    )
                }
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                (LockObjects::SysVSemaphores, Some(wlock)) if layout.rlock_size == 8 => {
                    let (r, w) = super::sysv::create(map, &md, mode, layout.rlock, wlock)?;
                    return Ok(SharedLocks::sysv(r, w));
                }
                _ => return Err(einval()),
            };
            // Clean up after a previous run, if needed.
            NamedSemaphore::unlink(&rname);
            NamedSemaphore::unlink(&wname);
            let rmutex = NamedSemaphore::open(rname, Some(mode))?;
            let wmutex = NamedSemaphore::open(wname, Some(mode))?;
            Ok(SharedLocks::named(rmutex, wmutex))
        }

        /// Opens the semaphores another process created: named in the
        /// lock file (version 1), named after the hash stored in it
        /// (version 2, macOS), or the SysV set whose id is stored in it
        /// (version 2, the other BSDs).
        pub fn attach(map: &LockMap, layout: &LockLayout) -> io::Result<SharedLocks> {
            let (rname, wname) = match (layout.objects, layout.wlock) {
                (LockObjects::SemaphoreNames, Some(wlock)) => (
                    read_name(map, layout.rlock, layout.rlock_size)?,
                    read_name(map, wlock, layout.rlock_size)?,
                ),
                (LockObjects::SemaphoreHash, _) if layout.rlock_size == 8 => {
                    let mutex_id = map.load_u64(layout.rlock);
                    (
                        mutex_name_v10(mutex_id, b'r')?,
                        mutex_name_v10(mutex_id, b'w')?,
                    )
                }
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                (LockObjects::SysVSemaphores, Some(wlock)) if layout.rlock_size == 8 => {
                    let (r, w) = super::sysv::attach(map, layout.rlock, wlock)?;
                    return Ok(SharedLocks::sysv(r, w));
                }
                _ => return Err(einval()),
            };
            let rmutex = NamedSemaphore::open(rname, None)?;
            let wmutex = NamedSemaphore::open(wname, None)?;
            Ok(SharedLocks::named(rmutex, wmutex))
        }

        fn named(rmutex: NamedSemaphore, wmutex: NamedSemaphore) -> SharedLocks {
            SharedLocks {
                rmutex: SharedMutex(Lock::Named(rmutex)),
                wmutex: SharedMutex(Lock::Named(wmutex)),
            }
        }

        #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        fn sysv(rmutex: super::sysv::SysVSemaphore, wmutex: super::sysv::SysVSemaphore) -> Self {
            SharedLocks {
                rmutex: SharedMutex(Lock::SysV(rmutex)),
                wmutex: SharedMutex(Lock::SysV(wmutex)),
            }
        }

        /// Closes both semaphores; if we are the last user of the
        /// environment (`last_user`, i.e. we could take the exclusive file
        /// lock), also removes them, as `mdb_env_close0` does: `sem_unlink`
        /// of the named ones, `IPC_RMID` of the SysV set.
        pub fn close(self, last_user: bool) {
            match (self.rmutex.0, self.wmutex.0) {
                (Lock::Named(r), Lock::Named(w)) => {
                    let names = (r.name.clone(), w.name.clone());
                    drop((r, w));
                    if last_user {
                        NamedSemaphore::unlink(&names.0);
                        NamedSemaphore::unlink(&names.1);
                    }
                }
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                (Lock::SysV(r), _) => {
                    if last_user {
                        r.remove();
                    }
                }
                // Never built: both locks are always of one kind.
                #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
                (Lock::Named(_), Lock::SysV(_)) => {}
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fnv1a_reference_values() {
            // Standard FNV-1a 64 test vectors.
            assert_eq!(hash_val(b"", HASH_INIT), 0xcbf2_9ce4_8422_2325);
            assert_eq!(hash_val(b"a", HASH_INIT), 0xaf63_dc4c_8601_ec8c);
            assert_eq!(hash_val(b"foobar", HASH_INIT), 0x8594_4171_f739_67e8);
        }

        #[test]
        fn encoding_shape() {
            let e = hash_enc(&lock_file_id_bytes(0x0100_0004, 123_456));
            assert_eq!(e.len(), 10);
            assert!(e.bytes().all(|c| A85.contains(&c)));
            // pack85 is base 85, least significant digit first.
            let mut out = Vec::new();
            pack85(85 * 2 + 1, &mut out);
            assert_eq!(out, b"12000");
        }

        /// The hashed struct: `dev` in the platform's `dev_t`, `ino` at 8.
        #[test]
        fn lock_file_id_layout() {
            let b = lock_file_id_bytes(0x0100_0004, 0x1122_3344_5566_7788);
            let dev = (0x0100_0004u64 as libc::dev_t).to_ne_bytes();
            assert_eq!(&b[..dev.len()], &dev[..]);
            assert!(b[dev.len()..8].iter().all(|&x| x == 0));
            assert_eq!(&b[8..], &0x1122_3344_5566_7788u64.to_ne_bytes()[..]);
        }

        /// A version 1 name fits `mtb_rmname` with its NUL: 15 characters,
        /// 14 on NetBSD (C's limit there).
        #[test]
        fn v09_name_length() {
            let enc = hash_enc(&lock_file_id_bytes(1, 2));
            let name = format!("/MDBr{}", &enc[..NAME_HASH_LEN]);
            let want = if cfg!(target_os = "netbsd") { 14 } else { 15 };
            assert_eq!(name.len(), want);
            assert!(name.len() < crate::format::lockfile::MNAME_LEN);
        }

        /// 1.0's names: `"/MDB"`, the lock letter, then the hash in up to
        /// ten base-85 digits, least significant first, without trailing
        /// zero digits (mdb10.c `mdb_pack85`, `MUTEXNAME`).
        #[test]
        fn v10_names() {
            let name = |id, ch| mutex_name_v10(id, ch).unwrap().into_bytes();
            assert_eq!(name(85 * 2 + 1, b'r'), b"/MDBr12");
            assert_eq!(name(85 * 2 + 1, b'w'), b"/MDBw12");
            assert_eq!(name(0, b'r'), b"/MDBr");
            assert_eq!(name(84, b'r'), b"/MDBr~");
            // 85^10 > 2^64: every 64-bit hash fits in ten digits, and the
            // largest needs all ten.
            let max = name(u64::MAX, b'w');
            assert_eq!(max.len(), 5 + 10);
            assert!(max[5..].iter().all(|c| A85.contains(c)));
            // The whole hash is encoded, not two 32-bit halves as in 0.9.
            let mut l = u64::MAX;
            let mut digits = Vec::new();
            while l != 0 {
                digits.push(A85[(l % 85) as usize]);
                l /= 85;
            }
            assert_eq!(&max[5..], &digits[..]);
        }
    }
}

// ===========================================================================
// OpenBSD, NetBSD, DragonFly, version 2: a SysV semaphore set.
// ===========================================================================

/// The SysV semaphore set of a version 2 lock file on OpenBSD, NetBSD and
/// DragonFly (`MDB_USE_SYSV_SEM`, mdb10.c lines 452-481, 6096-6110,
/// 6165-6182 and 6565-6574). Also built for the unit tests on macOS, which
/// has the same interface, so the protocol runs on a real kernel there.
#[cfg(any(
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    all(test, any(target_os = "macos", target_os = "ios"))
))]
mod sysv {
    use super::{LockMap, LockStatus};
    use libc::{c_int, c_short, c_ushort};
    use std::fs::Metadata;
    use std::io;
    use std::mem::size_of;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicI32, Ordering};

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    use libc::{sembuf, semctl, semget, semop, GETVAL, SEM_UNDO, SETALL};

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    use ffi::{sembuf, semctl, semget, semop, GETVAL, SEM_UNDO, SETALL};

    /// The SysV semaphore calls, which the `libc` crate does not bind for
    /// these BSDs. Prototypes and values from their `<sys/sem.h>` (the
    /// values are 4.4BSD's, the same on all three).
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    mod ffi {
        use libc::{c_int, c_short, c_ushort};

        /// `struct sembuf`.
        #[repr(C)]
        #[allow(non_camel_case_types)]
        pub struct sembuf {
            pub sem_num: c_ushort,
            pub sem_op: c_short,
            pub sem_flg: c_short,
        }

        /// `SEM_UNDO`: undo the operation when the process exits.
        pub const SEM_UNDO: c_int = 0o10000;
        /// `GETVAL`: the value of one semaphore.
        pub const GETVAL: c_int = 5;
        /// `SETALL`: set every value from `arg.array`.
        pub const SETALL: c_int = 9;

        extern "C" {
            pub fn semget(key: libc::key_t, nsems: c_int, semflg: c_int) -> c_int;
            // NetBSD's <sys/sem.h> renames it for the semid_ds with a
            // 64-bit time_t: `__RENAME(__semctl50)`.
            #[cfg_attr(target_os = "netbsd", link_name = "__semctl50")]
            pub fn semctl(semid: c_int, semnum: c_int, cmd: c_int, ...) -> c_int;
            #[cfg(not(target_os = "dragonfly"))]
            pub fn semop(semid: c_int, sops: *mut sembuf, nsops: libc::size_t) -> c_int;
            #[cfg(target_os = "dragonfly")]
            pub fn semop(semid: c_int, sops: *mut sembuf, nsops: libc::c_uint) -> c_int;
        }
    }

    /// The semaphore of the reader lock in the set (`me_rmutex->semnum`).
    const READER: c_ushort = 0;
    /// The semaphore of the writer lock in the set (`me_wmutex->semnum`).
    const WRITER: c_ushort = 1;

    /// Room for a `struct semid_ds`, which `IPC_STAT` fills and `IPC_SET`
    /// reads back; larger than it on every platform here (about 100 bytes).
    #[repr(C, align(8))]
    struct SemidDs([u8; 512]);

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const _: () = assert!(size_of::<libc::semid_ds>() <= size_of::<SemidDs>());

    /// One semaphore of the set, used as a mutex (`struct mdb_mutex`): the
    /// set's id, the semaphore's number, and the flag in the lock file
    /// (`mti_rlocked` or `mti_wlocked`) that is 1 while it is held.
    pub struct SysVSemaphore {
        semid: c_int,
        semnum: c_ushort,
        locked: *mut i32,
    }

    // SAFETY: a SysV semaphore may be used from any thread. `locked` points
    // into the lock-file mapping, which outlives this value (LockRegion
    // closes its SharedLocks before its LockMap is unmapped, as for the
    // pthread mutexes), and is only accessed atomically.
    unsafe impl Send for SysVSemaphore {}
    // SAFETY: as above; semop is thread-safe.
    unsafe impl Sync for SysVSemaphore {}

    impl SysVSemaphore {
        fn new(map: &LockMap, semid: c_int, semnum: c_ushort, flag: usize) -> SysVSemaphore {
            let locked = map.raw_ptr(flag, size_of::<i32>(), size_of::<i32>()) as *mut i32;
            SysVSemaphore {
                semid,
                semnum,
                locked,
            }
        }

        fn locked(&self) -> &AtomicI32 {
            // SAFETY: `raw_ptr` checked that the int is inside the mapping
            // and 4-byte aligned; the mapping is live (see the Send impl)
            // and every access to the flag is atomic.
            unsafe { AtomicI32::from_ptr(self.locked) }
        }

        /// One `semop` of `op` on our semaphore, with `SEM_UNDO`.
        fn semop(&self, op: c_short) -> io::Result<()> {
            let mut sb = sembuf {
                sem_num: self.semnum,
                sem_op: op,
                sem_flg: SEM_UNDO as c_short,
            };
            // SAFETY: `sb` is one valid sembuf, which semop only reads.
            if unsafe { semop(self.semid, &mut sb, 1) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        /// `mdb_sem_wait`: takes the semaphore (retrying on `EINTR`), then
        /// sets the flag. A flag that was already set was left by a
        /// holder that died (the kernel undid its `semop`): `OwnerDied`.
        pub fn lock(&self) -> io::Result<LockStatus> {
            loop {
                match self.semop(-1) {
                    Ok(()) => {
                        return Ok(if self.locked().swap(1, Ordering::SeqCst) != 0 {
                            LockStatus::OwnerDied
                        } else {
                            LockStatus::Acquired
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }

        /// `UNLOCK_MUTEX`: clears the flag, then releases the semaphore.
        pub fn unlock(&self) -> io::Result<()> {
            self.locked().store(0, Ordering::SeqCst);
            self.semop(1)
        }

        /// `semctl(IPC_RMID)`: removes the whole set (both semaphores).
        pub fn remove(&self) {
            // SAFETY: IPC_RMID takes no further argument; failure (the set
            // is gone already) is ignored, as in C.
            unsafe {
                semctl(self.semid, 0, libc::IPC_RMID);
            }
        }
    }

    /// `ftok(lockfile, 'M')` (mdb10.c line 6098), from the lock file's
    /// `fstat` rather than its path, which is not at hand here: the
    /// `ftok` of every BSD libc (macOS's included) is
    /// `(id & 0xff) << 24 | (st_dev & 0xff) << 16 | (st_ino & 0xffff)`.
    /// The key only picks the set the exclusive opener (re)initialises;
    /// everyone else uses the id stored in the lock file.
    pub fn ftok_key(md: &Metadata) -> libc::key_t {
        let key = (u32::from(b'M') << 24)
            | (((md.dev() & 0xff) as u32) << 16)
            | ((md.ino() & 0xffff) as u32);
        key as libc::key_t
    }

    /// The pair of locks on set `semid`, with their flags at `rlock +
    /// sizeof(int)` (`mti_rlocked`) and `wlock` (`mti_wlocked`).
    fn pair(
        map: &LockMap,
        semid: c_int,
        rlock: usize,
        wlock: usize,
    ) -> (SysVSemaphore, SysVSemaphore) {
        (
            SysVSemaphore::new(map, semid, READER, rlock + size_of::<c_int>()),
            SysVSemaphore::new(map, semid, WRITER, wlock),
        )
    }

    /// `IPC_STAT` of set `semid` into `buf`, then `IPC_SET` of it, which
    /// changes nothing but fails unless we are the set's owner or creator
    /// or root: C's check that an opener may read and change the set
    /// (mdb10.c lines 6168-6173).
    fn check_access(semid: c_int, buf: &mut SemidDs) -> io::Result<()> {
        for cmd in [libc::IPC_STAT, libc::IPC_SET] {
            // SAFETY: IPC_STAT writes and IPC_SET reads one struct
            // semid_ds through `arg.buf`, passed as the pointer (see
            // `create`); `buf` is larger than that struct and aligned.
            if unsafe { semctl(semid, 0, cmd, buf.0.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Checks that set `semid`, which `semget` returned for the lock
    /// file's key, is one [`create`] may reset. C 1.0 resets whatever set
    /// it gets (`KNOWN_C_BUGS.md` #46); the key is `ftok`'s, which keeps 8
    /// bits of the device and 16 of the inode, so another program's set
    /// can have it.
    ///
    /// * We pass [`check_access`], which every other opener must pass too
    ///   ([`attach`]). If we do not, the set is another user's: its error
    ///   (`EACCES`, `EPERM`).
    /// * The set has exactly two semaphores, else `EEXIST`. `semget` with
    ///   `nsems` 2 returns an existing set only if it has at least two,
    ///   and semaphore number 2 exists only in a larger one: `GETVAL` of
    ///   it fails with `EINVAL` in a set of two (POSIX). (`sem_nsems` would
    ///   say so too, but the `libc` crate has no `struct semid_ds` for
    ///   these BSDs.) `SETALL` of a larger set would read one value per
    ///   semaphore from our two-element array and overwrite the other
    ///   program's semaphores.
    ///
    /// A refused set is left as it is.
    fn check_reusable(semid: c_int) -> io::Result<()> {
        check_access(semid, &mut SemidDs([0; 512]))?;
        // SAFETY: GETVAL takes no further argument.
        if unsafe { semctl(semid, 2, GETVAL) } >= 0 {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EINVAL) {
            Ok(())
        } else {
            Err(e)
        }
    }

    /// Creates (or reuses) the set for a lock file that we hold
    /// exclusively and whose metadata is `md`, sets both semaphores to 1,
    /// and stores its id at `rlock` (`mti_semid`) and cleared flags
    /// (`mdb_env_setup_locks`, `*excl > 0`). An existing set of the key
    /// that is not ours to reset is refused ([`check_reusable`]).
    pub fn create(
        map: &LockMap,
        md: &Metadata,
        mode: u32,
        rlock: usize,
        wlock: usize,
    ) -> io::Result<(SysVSemaphore, SysVSemaphore)> {
        // SAFETY: semget takes plain integers.
        let semid = unsafe { semget(ftok_key(md), 2, (mode & 0o777) as c_int | libc::IPC_CREAT) };
        if semid < 0 {
            return Err(io::Error::last_os_error());
        }
        check_reusable(semid)?;
        let mut vals: [c_ushort; 2] = [1, 1];
        // SAFETY: SETALL reads one value per semaphore of the set from
        // `arg.array`, and `check_reusable` found two, as many as `vals`
        // holds. A set never changes size, and its id names no other set
        // before the kernel has reused its slot as many times as the id's
        // sequence number has values. The `union semun` argument is passed
        // as the pointer it holds: the C ABIs pass a pointer-sized union
        // like a pointer.
        if unsafe { semctl(semid, 0, SETALL, vals.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        map.store_i32(rlock, semid);
        map.store_i32(rlock + size_of::<c_int>(), 0);
        map.store_i32(wlock, 0);
        Ok(pair(map, semid, rlock, wlock))
    }

    /// Uses the set whose id another process stored at `rlock`, after
    /// checking, as C does, that we may read and change it
    /// ([`check_access`]).
    pub fn attach(
        map: &LockMap,
        rlock: usize,
        wlock: usize,
    ) -> io::Result<(SysVSemaphore, SysVSemaphore)> {
        let semid = map.load_i32(rlock);
        check_access(semid, &mut SemidDs([0; 512]))?;
        Ok(pair(map, semid, rlock, wlock))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::ffi::CString;
        use std::fs::File;
        use std::os::unix::ffi::OsStrExt;

        /// Where the tests put the set: `mtb_semid` at 20, `mt2_wlocked`
        /// at 64, as in the version 2 layout of these BSDs.
        const RLOCK: usize = 20;
        const WLOCK: usize = 64;

        /// Removes the set when a test ends, even by panicking.
        struct Remove(c_int);

        impl Drop for Remove {
            fn drop(&mut self) {
                // SAFETY: IPC_RMID takes no further argument.
                unsafe {
                    semctl(self.0, 0, libc::IPC_RMID);
                }
            }
        }

        fn value(semid: c_int, semnum: c_ushort) -> c_int {
            // SAFETY: GETVAL takes no further argument.
            unsafe { semctl(semid, c_int::from(semnum), GETVAL) }
        }

        /// A lock file whose `ftok` key had no set, and a set of `nsems`
        /// semaphores that we then create at that key, as another program
        /// whose key collides with the lock file's would.
        fn file_with_set(nsems: c_int) -> (File, c_int) {
            // Files whose key is taken stay open, so that the next one
            // gets another inode.
            let mut taken = Vec::new();
            for _ in 0..64 {
                let f = tempfile::tempfile().unwrap();
                f.set_len(4096).unwrap();
                let key = ftok_key(&f.metadata().unwrap());
                // SAFETY: semget takes plain integers.
                let id = unsafe { semget(key, nsems, libc::IPC_CREAT | libc::IPC_EXCL | 0o600) };
                if id >= 0 {
                    return (f, id);
                }
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::EEXIST)
                );
                taken.push(f);
            }
            panic!("no free SysV key for {} files", taken.len());
        }

        fn set_all(semid: c_int, vals: &mut [c_ushort]) {
            // SAFETY: SETALL reads one value per semaphore from `vals`,
            // which the callers make as long as the set.
            assert_eq!(unsafe { semctl(semid, 0, SETALL, vals.as_mut_ptr()) }, 0);
        }

        /// Another program's set at the lock file's key with more
        /// semaphores than ours: `create` refuses it and changes nothing
        /// (C 1.0's `SETALL` would read three values from its two-element
        /// array).
        #[test]
        fn create_refuses_larger_set() {
            let (f, id) = file_with_set(3);
            let _remove = Remove(id);
            set_all(id, &mut [11, 12, 13]);
            let map = LockMap::map(&f, 4096).unwrap();
            let err = create(&map, &f.metadata().unwrap(), 0o600, RLOCK, WLOCK)
                .err()
                .unwrap();
            assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
            assert_eq!([value(id, 0), value(id, 1), value(id, 2)], [11, 12, 13]);
            assert_eq!(map.load_i32(RLOCK), 0);
        }

        /// A set of two at the key (as a process that died with the
        /// environment open leaves it) is ours: `create` takes it over and
        /// resets both semaphores.
        #[test]
        fn create_reuses_set_of_two() {
            let (f, id) = file_with_set(2);
            let _remove = Remove(id);
            set_all(id, &mut [0, 0]);
            let map = LockMap::map(&f, 4096).unwrap();
            let (r, w) = create(&map, &f.metadata().unwrap(), 0o600, RLOCK, WLOCK).unwrap();
            assert_eq!((r.semid, w.semid, map.load_i32(RLOCK)), (id, id, id));
            assert_eq!((value(id, READER), value(id, WRITER)), (1, 1));
            // The size `check_reusable` probed for is the one IPC_STAT
            // reports.
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                // SAFETY: semid_ds is a plain C struct; all-zero is valid.
                let mut ds: libc::semid_ds = unsafe { std::mem::zeroed() };
                // SAFETY: IPC_STAT writes one semid_ds through `arg.buf`.
                assert_eq!(unsafe { semctl(id, 0, libc::IPC_STAT, &mut ds) }, 0);
                assert_eq!({ ds.sem_nsems }, 2);
            }
        }

        #[test]
        fn ftok_matches_libc() {
            let f = tempfile::NamedTempFile::new().unwrap();
            let path = CString::new(f.path().as_os_str().as_bytes()).unwrap();
            // SAFETY: a valid NUL-terminated path.
            let want = unsafe { libc::ftok(path.as_ptr(), c_int::from(b'M')) };
            assert_ne!(want, -1);
            assert_eq!(ftok_key(&f.as_file().metadata().unwrap()), want);
        }

        /// Lock, unlock, a second handle on the set, and the robustness:
        /// a process that dies holding the writer lock leaves it free and
        /// its flag set, and the next holder is told.
        #[test]
        fn protocol() {
            let f = tempfile::tempfile().unwrap();
            f.set_len(4096).unwrap();
            let map = LockMap::map(&f, 4096).unwrap();
            let (r, w) = create(&map, &f.metadata().unwrap(), 0o600, RLOCK, WLOCK).unwrap();
            let _remove = Remove(r.semid);
            assert_eq!(map.load_i32(RLOCK), r.semid);
            assert_eq!((value(r.semid, READER), value(r.semid, WRITER)), (1, 1));

            assert_eq!(r.lock().unwrap(), LockStatus::Acquired);
            assert_eq!(map.load_i32(RLOCK + 4), 1);
            assert_eq!(value(r.semid, READER), 0);
            r.unlock().unwrap();
            assert_eq!(map.load_i32(RLOCK + 4), 0);
            assert_eq!(value(r.semid, READER), 1);

            // Another opener finds the set through the stored id.
            let (_r2, w2) = attach(&map, RLOCK, WLOCK).unwrap();
            assert_eq!(w2.semid, w.semid);
            assert_eq!(w2.lock().unwrap(), LockStatus::Acquired);
            assert_eq!(map.load_i32(WLOCK), 1);
            w2.unlock().unwrap();

            // A child takes the writer lock and exits without releasing it.
            // SAFETY: the child only calls semop (through `lock`, which
            // does not allocate on success) and _exit.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let ok = w.lock().is_ok();
                // SAFETY: ends the child without running the parent's
                // destructors or atexit handlers.
                unsafe { libc::_exit(if ok { 0 } else { 1 }) };
            }
            let mut status = 0;
            // SAFETY: waits for our own child.
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
            // SEM_UNDO gave the semaphore back; the flag is still set.
            assert_eq!(value(w.semid, WRITER), 1);
            assert_eq!(map.load_i32(WLOCK), 1);
            assert_eq!(w.lock().unwrap(), LockStatus::OwnerDied);
            w.unlock().unwrap();
            assert_eq!(w.lock().unwrap(), LockStatus::Acquired);
            w.unlock().unwrap();

            // The last user removes the set.
            r.remove();
            assert_eq!(value(r.semid, READER), -1);
            assert!(attach(&map, RLOCK, WLOCK).is_err());
        }
    }
}

pub use imp::SharedMutex;
