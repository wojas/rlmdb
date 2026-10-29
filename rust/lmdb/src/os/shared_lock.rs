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
//! * **macOS** (`MDB_USE_POSIX_SEM`): two POSIX named semaphores whose names,
//!   `"/MDBr<hash>"` and `"/MDBw<hash>"`, are stored in the lock file at the
//!   same offsets. `<hash>` is `mdb_hash_enc` of `(st_dev, st_ino)` of the
//!   lock file (lines 4823-4880). The exclusive opener `sem_unlink`s stale
//!   semaphores, then creates both with `O_CREAT|O_EXCL` and value 1; later
//!   openers `sem_open(name, 0)` using the names *read from the file*.
//!   Semaphores are not robust; C has the same limitation.
//!
//! A mutex locked here must be unlocked by the same thread on Linux (pthread
//! mutex ownership); the reader-table mutex is only held inside one
//! function call, and the writer will hold `wmutex` on the thread that began
//! the write transaction.

use std::fs::File;
use std::io;

use super::unix::LockMap;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use crate::format::lockfile::MNAME_LEN;
use crate::format::lockfile::{TXNINFO_RMUTEX, TXNINFO_WMUTEX};

/// Outcome of a successful [`SharedMutex::lock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    /// The lock was acquired normally.
    Acquired,
    /// The lock was acquired, but its previous owner died while holding it
    /// (`EOWNERDEAD`). The protected data may need repair before
    /// [`SharedMutex::mark_consistent`] is called. Never reported by the
    /// semaphores used on macOS (they are not robust).
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

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
mod imp {
    use super::*;

    /// A robust, process-shared `pthread_mutex_t` living in the lock file.
    pub struct SharedMutex {
        mutex: *mut libc::pthread_mutex_t,
    }

    // SAFETY: a process-shared mutex is designed to be used from any thread
    // of any process; the pointer stays valid while the owning `LockMap`
    // lives (SharedLocks is always dropped before its map, see core::env).
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
        /// Initialises both mutexes in a lock file we hold exclusively
        /// (`mdb_env_setup_locks`, `*excl > 0` branch).
        pub fn create(map: &LockMap, _file: &File, _mode: u32) -> io::Result<SharedLocks> {
            let r = mutex_at(map, TXNINFO_RMUTEX);
            let w = mutex_at(map, TXNINFO_WMUTEX);
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
        pub fn attach(map: &LockMap) -> io::Result<SharedLocks> {
            Ok(SharedLocks {
                rmutex: SharedMutex {
                    mutex: mutex_at(map, TXNINFO_RMUTEX),
                },
                wmutex: SharedMutex {
                    mutex: mutex_at(map, TXNINFO_WMUTEX),
                },
            })
        }

        /// Releases our handles. Process-shared mutexes need no cleanup
        /// (C does not destroy them either).
        pub fn close(self, _last_user: bool) {}
    }
}

// ===========================================================================
// macOS: POSIX named semaphores, names stored in the lock file.
// ===========================================================================

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::fs::MetadataExt;

    /// A POSIX named semaphore with initial value 1, used as a mutex.
    pub struct SharedMutex {
        sem: *mut libc::sem_t,
        name: CString,
    }

    // SAFETY: semaphore handles may be used from any thread.
    unsafe impl Send for SharedMutex {}
    // SAFETY: sem_wait/sem_post are thread-safe.
    unsafe impl Sync for SharedMutex {}

    impl SharedMutex {
        /// Locks (waits on) the semaphore, retrying on `EINTR` like
        /// `mdb_sem_wait`.
        pub fn lock(&self) -> io::Result<LockStatus> {
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
        pub fn unlock(&self) -> io::Result<()> {
            // SAFETY: open semaphore handle.
            if unsafe { libc::sem_post(self.sem) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        /// Semaphores are not robust; nothing to do.
        pub fn mark_consistent(&self) -> io::Result<()> {
            Ok(())
        }

        fn open(name: CString, create_mode: Option<u32>) -> io::Result<SharedMutex> {
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
            Ok(SharedMutex { sem, name })
        }

        fn unlink(name: &CString) {
            // SAFETY: valid NUL-terminated name; failure (ENOENT) is fine.
            unsafe {
                libc::sem_unlink(name.as_ptr());
            }
        }
    }

    impl Drop for SharedMutex {
        fn drop(&mut self) {
            // SAFETY: the handle came from sem_open and is closed once.
            unsafe {
                libc::sem_close(self.sem);
            }
        }
    }

    /// `mdb_hash_val`: 64-bit FNV-1a over `bytes`, starting from `hval`.
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

    /// `mdb_pack85`: five base-85 digits of `l`, least significant first.
    fn pack85(mut l: u64, out: &mut Vec<u8>) {
        for _ in 0..5 {
            out.push(A85[(l % 85) as usize]);
            l /= 85;
        }
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

    /// The bytes C hashes: `struct { dev_t dev; ino_t ino; }`. On macOS
    /// `dev_t` is 4 bytes and `ino_t` 8, so the C struct has 4 bytes of
    /// padding that C leaves *uninitialised*; the resulting names are
    /// therefore not reproducible, which is why every opener after the
    /// first reads them from the lock file. We use zero padding.
    pub fn lock_file_id_bytes(dev: u64, ino: u64) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&(dev as i32).to_ne_bytes());
        b[8..16].copy_from_slice(&ino.to_ne_bytes());
        b
    }

    fn read_name(map: &LockMap, offset: usize) -> io::Result<CString> {
        let raw = map.read_bytes(offset, MNAME_LEN);
        let end = raw
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        CString::new(&raw[..end]).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }

    fn write_name(map: &LockMap, offset: usize, name: &CString) {
        map.write_bytes(offset, name.as_bytes_with_nul());
    }

    impl SharedLocks {
        /// Creates both semaphores for a lock file we hold exclusively and
        /// records their names in it (`mdb_env_setup_locks`, `*excl > 0`).
        pub fn create(map: &LockMap, file: &File, mode: u32) -> io::Result<SharedLocks> {
            let md = file.metadata()?;
            let enc = hash_enc(&lock_file_id_bytes(md.dev(), md.ino()));
            let rname = CString::new(format!("/MDBr{enc}"))
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            let wname = CString::new(format!("/MDBw{enc}"))
                .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
            write_name(map, TXNINFO_RMUTEX, &rname);
            write_name(map, TXNINFO_WMUTEX, &wname);
            // Clean up after a previous run, if needed.
            SharedMutex::unlink(&rname);
            SharedMutex::unlink(&wname);
            let rmutex = SharedMutex::open(rname, Some(mode))?;
            let wmutex = SharedMutex::open(wname, Some(mode))?;
            Ok(SharedLocks { rmutex, wmutex })
        }

        /// Opens the semaphores named in the lock file.
        pub fn attach(map: &LockMap) -> io::Result<SharedLocks> {
            let rmutex = SharedMutex::open(read_name(map, TXNINFO_RMUTEX)?, None)?;
            let wmutex = SharedMutex::open(read_name(map, TXNINFO_WMUTEX)?, None)?;
            Ok(SharedLocks { rmutex, wmutex })
        }

        /// Closes both semaphores; if we are the last user of the
        /// environment (`last_user`, i.e. we could take the exclusive file
        /// lock), also unlinks them, as `mdb_env_close0` does.
        pub fn close(self, last_user: bool) {
            let names = (self.rmutex.name.clone(), self.wmutex.name.clone());
            drop(self);
            if last_user {
                SharedMutex::unlink(&names.0);
                SharedMutex::unlink(&names.1);
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
    }
}

pub use imp::SharedMutex;
