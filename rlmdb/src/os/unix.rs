//! Unix system calls used by the environment: memory maps, record locks,
//! positional I/O and syncing.
//!
//! C counterparts: `mdb_env_map` (mmap of the data file), the `mmap` in
//! `mdb_env_setup_locks` (lock file), `mdb_env_excl_lock`/`mdb_env_share_locks`
//! and `mdb_reader_pid` (fcntl locks), `MDB_FDATASYNC`, `GET_PAGESIZE`.

use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

/// The OS memory page size (`sysconf(_SC_PAGE_SIZE)`); 4096 on x86-64 Linux,
/// 16384 on Apple Silicon macOS.
pub fn os_page_size() -> usize {
    // SAFETY: sysconf has no preconditions.
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 {
        n as usize
    } else {
        4096
    }
}

/// The calling process id (`getpid()`), as stored in `mr_pid`.
pub fn getpid() -> i32 {
    std::process::id() as i32
}

/// The calling thread's `pthread_self()` as an integer, as stored in
/// `mr_tid` (only used for diagnostics such as `mdb_reader_list`).
pub fn thread_id() -> u64 {
    // SAFETY: pthread_self has no preconditions and always succeeds.
    let t = unsafe { libc::pthread_self() };
    t as usize as u64
}

/// `strerror(code)` of the C library.
pub fn strerror(code: i32) -> String {
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: buf is a valid writable buffer of the given length; the XSI
    // strerror_r (which the libc crate binds on both glibc and Darwin)
    // NUL-terminates on success.
    let rc = unsafe { libc::strerror_r(code, buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return format!("Unknown error: {code}");
    }
    // SAFETY: strerror_r succeeded, so buf holds a NUL-terminated string.
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
    s.to_string_lossy().into_owned()
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

// ---------------------------------------------------------------------------
// fcntl record locks
// ---------------------------------------------------------------------------

/// Kind of a byte-range lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// `F_RDLCK`: shared.
    Shared,
    /// `F_WRLCK`: exclusive.
    Exclusive,
}

fn flock_for(kind: libc::c_short, offset: u64) -> libc::flock {
    // SAFETY: flock is a plain C struct for which all-zero is valid.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = kind;
    fl.l_whence = libc::SEEK_SET as libc::c_short;
    fl.l_start = offset as libc::off_t;
    fl.l_len = 1;
    fl
}

/// Sets a one-byte `fcntl` lock at `offset` (`F_SETLK`, or `F_SETLKW` if
/// `wait`), retrying on `EINTR`. A conflicting lock yields the error
/// (`EAGAIN`/`EACCES`) for `F_SETLK`.
///
/// POSIX record locks belong to the process: re-locking a byte we already
/// hold converts the lock, and closing *any* descriptor of the file drops
/// all of the process's locks on it. This is why an environment must not be
/// opened twice in one process.
pub fn lock_byte(file: &File, offset: u64, kind: LockKind, wait: bool) -> io::Result<()> {
    let ty = match kind {
        LockKind::Shared => libc::F_RDLCK,
        LockKind::Exclusive => libc::F_WRLCK,
    } as libc::c_short;
    let fl = flock_for(ty, offset);
    let cmd = if wait { libc::F_SETLKW } else { libc::F_SETLK };
    loop {
        // SAFETY: the fd is valid for the lifetime of `file`; `fl` is a valid
        // flock struct that fcntl only reads for F_SETLK/F_SETLKW.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), cmd, &fl) };
        if rc == 0 {
            return Ok(());
        }
        let err = last_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Probes whether another process holds a lock on the byte at `offset`
/// (`F_GETLK` with an `F_WRLCK` request, as `mdb_reader_pid(Pidcheck)`).
/// Returns `Ok(true)` if a conflicting lock exists. Locks held by this
/// process never conflict and report `false`.
pub fn probe_lock(file: &File, offset: u64) -> io::Result<bool> {
    loop {
        let mut fl = flock_for(libc::F_WRLCK as libc::c_short, offset);
        // SAFETY: valid fd; `fl` is a valid, writable flock struct.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut fl) };
        if rc == 0 {
            return Ok(fl.l_type != libc::F_UNLCK as libc::c_short);
        }
        let err = last_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

// ---------------------------------------------------------------------------
// Positional I/O and syncing
// ---------------------------------------------------------------------------

/// `pread` the whole of `buf` at `offset`, returning how many bytes were
/// read (fewer only at end of file).
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match file.read_at(&mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// `pwrite` all of `buf` at `offset`.
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    file.write_all_at(buf, offset)
}

/// `pwritev`: write `bufs` back to back starting at `offset`, in one system
/// call where possible. Returns the number of bytes written by that call
/// (callers loop on short writes, as `mdb_page_flush` does).
#[allow(dead_code)] // used by the write path (stage 2)
pub fn pwritev(file: &File, bufs: &[&[u8]], offset: u64) -> io::Result<usize> {
    let iov: Vec<libc::iovec> = bufs
        .iter()
        .map(|b| libc::iovec {
            iov_base: b.as_ptr() as *mut libc::c_void,
            iov_len: b.len(),
        })
        .collect();
    loop {
        // SAFETY: every iovec points at a live, readable slice of the stated
        // length for the duration of the call; the fd is valid.
        let n = unsafe {
            libc::pwritev(
                file.as_raw_fd(),
                iov.as_ptr(),
                iov.len() as libc::c_int,
                offset as libc::off_t,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = last_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// `MDB_FDATASYNC`: `fdatasync` on Linux and FreeBSD, `fcntl(F_FULLFSYNC)`
/// on macOS (plain `fsync` does not flush the drive cache there), `fsync`
/// on OpenBSD, NetBSD and DragonFly, as C 0.9 picks for them (mdb.c lines
/// 136-149; FreeBSD has `fdatasync` from 11.1 on).
pub fn fdatasync(file: &File) -> io::Result<()> {
    loop {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        // SAFETY: valid fd; F_FULLFSYNC takes no argument.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
        // SAFETY: valid fd.
        let rc = unsafe { libc::fsync(file.as_raw_fd()) };
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )))]
        // SAFETY: valid fd.
        let rc = unsafe { libc::fdatasync(file.as_raw_fd()) };
        if rc == 0 {
            return Ok(());
        }
        let err = last_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// `fsync`.
#[allow(dead_code)] // used by the write path (stage 2)
pub fn fsync(file: &File) -> io::Result<()> {
    loop {
        // SAFETY: valid fd.
        let rc = unsafe { libc::fsync(file.as_raw_fd()) };
        if rc == 0 {
            return Ok(());
        }
        let err = last_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Asks the OS not to cache what is written through `file` (as
/// `mdb_fopen` does for `mdb_env_copy` targets): `F_NOCACHE` on macOS,
/// `O_DIRECT` elsewhere, nothing on OpenBSD (which has neither). Best
/// effort, failures are ignored like in C. With `O_DIRECT`, writes must
/// come from buffers aligned to the OS page size and have sizes that are
/// multiples of it.
pub fn set_nocache(file: &File) {
    let fd = file.as_raw_fd();
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    // SAFETY: valid fd; F_NOCACHE takes an int argument.
    unsafe {
        libc::fcntl(fd, libc::F_NOCACHE, 1);
    }
    #[cfg(target_os = "openbsd")]
    let _ = fd;
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "openbsd")))]
    // SAFETY: valid fd; F_GETFL/F_SETFL take no pointer arguments.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags != -1 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_DIRECT);
        }
    }
}

/// Closes `file`, reporting an error from `close` (which `File`'s `Drop`
/// ignores; `mdb_env_copy2` reports it).
pub fn close(file: File) -> io::Result<()> {
    use std::os::fd::IntoRawFd;
    let fd = file.into_raw_fd();
    // SAFETY: we own fd (taken from the File) and close it exactly once.
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `MDB_DSYNC`, the flag that makes writes through the meta-page
/// descriptor (`me_mfd`) synchronous: `O_DSYNC`, or `O_SYNC` where there is
/// none (DragonFly), as mdb.c (line 457) chooses.
#[cfg(not(target_os = "dragonfly"))]
pub const MDB_DSYNC: libc::c_int = libc::O_DSYNC;
/// `MDB_DSYNC`, the flag that makes writes through the meta-page
/// descriptor (`me_mfd`) synchronous: `O_DSYNC`, or `O_SYNC` where there is
/// none (DragonFly), as mdb.c (line 457) chooses.
#[cfg(target_os = "dragonfly")]
pub const MDB_DSYNC: libc::c_int = libc::O_SYNC;

/// `true` where data written to the file through a descriptor does not
/// show in a shared map of it until the map's copy is invalidated
/// ([`DataMap::invalidate`], C's `MDB_Invalidate`): OpenBSD, which has no
/// unified buffer cache (mdb.c lines 86-93).
pub const MAP_NEEDS_INVALIDATE: bool = cfg!(target_os = "openbsd");

/// `ftruncate`.
pub fn ftruncate(file: &File, len: u64) -> io::Result<()> {
    file.set_len(len)
}

/// The current size of the file (`fstat().st_size`).
pub fn file_size(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

// ---------------------------------------------------------------------------
// The data file map
// ---------------------------------------------------------------------------

/// A read-only, `MAP_SHARED` memory map of the data file (`me_map`).
///
/// The map may be longer than the file (`map_size` is the maximum database
/// size); touching a page beyond the end of the file would raise `SIGBUS`,
/// so [`DataMap::slice`] also checks against the file size, re-reading it
/// (one `fstat`) only when an access goes past the last size seen.
///
/// # Why handing out `&[u8]` into this map is sound
///
/// Other processes write the file while we read it. LMDB's MVCC invariant,
/// which the C implementation relies on just the same, is that a page
/// reachable from a snapshot is never modified while a reader slot pins that
/// snapshot: writers only reuse pages freed by transactions older than every
/// active reader (`mdb_find_oldest`). Without a lock file (`MDB_NOLOCK`) the
/// environment's in-memory reader table stands in for the slots of this
/// process's readers (C has none there); other processes then must not
/// write while we read, which `MDB_NOLOCK` makes the caller's job, as in C.
/// Slices handed out by `slice` are only
/// for such pages, and their lifetime is bounded by the read transaction
/// that pins the snapshot. The two meta pages *are* rewritten under readers;
/// they are only ever read through [`DataMap::read_volatile`] into a private
/// copy, and `slice` refuses them ([`DataMap::with_meta_pages`]).
///
/// What `slice` cannot check is that a page belongs to a pinned snapshot:
/// that is the contract of its (crate-internal) callers, `TxnCore::page`
/// and friends, which bound every page number by the snapshot's
/// `next_pgno`. Pages past the end of the file are refused, so a file that
/// is shorter than its meta pages claim yields errors, not `SIGBUS`. A file
/// that shrinks *while mapped* (truncated by another program, outside the
/// LMDB protocol) can still raise `SIGBUS` for pages seen before, as in C.
///
/// The MVCC invariant is upheld by the writers, so it also rests on what
/// they read: the free list in the data file and the reader table in the
/// lock file. Two kinds of damage break it, for C and for us alike, and
/// cannot be detected cheaply. A free-list record that lists a page still
/// reachable from a live snapshot (free-list records naming meta pages or
/// pages past the snapshot are refused, see `core::freelist`), and a lock
/// file into which another program wrote a too-high `mti_txnid` or cleared
/// a live reader slot, both make a writer (in this process or another)
/// rewrite a page that a read transaction may be looking at. So does a
/// process writing an `MDB_NOLOCK` environment that another process reads
/// (the misuse `lmdb.h` warns about). The page then changes under a live
/// `&[u8]`. Every access to page contents is a
/// bounds check against the slice's length, which does not change, so in
/// practice the reader sees wrong data or gets `MDB_CORRUPTED`. Formally
/// it is undefined behaviour in Rust, which is why it is listed with the
/// residual risks (`rlmdb-capi/README.md`, "Undefined behaviour that
/// remains").
pub struct DataMap {
    ptr: *mut u8,
    len: usize,
    /// End of the meta pages: `slice` refuses offsets below it.
    meta_end: usize,
    file: File,
    /// Last known size of the data file, to avoid `SIGBUS` past the end.
    known_file_size: AtomicU64,
}

// SAFETY: the map is read-only and the mapping itself is never changed after
// creation; concurrent reads from several threads are fine.
unsafe impl Send for DataMap {}
// SAFETY: see above; all methods take `&self` and only read.
unsafe impl Sync for DataMap {}

impl DataMap {
    /// Maps `len` bytes of `file` read-only and shared. `random` applies
    /// `madvise(MADV_RANDOM)` (`MDB_NORDAHEAD`). `file` is a duplicate of the
    /// data file handle, kept to refresh the file size.
    pub fn map_readonly(file: File, len: usize, random: bool) -> io::Result<DataMap> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // Before the mmap: from it until the `DataMap` that unmaps on drop
        // exists, nothing may fail (an early return would leak the map).
        // A file that grows meanwhile is seen by `refresh_size`.
        let size = file_size(&file)?;
        // SAFETY: a fresh mapping (addr = NULL) of a valid fd; the kernel
        // validates length and offset.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(last_error());
        }
        if random {
            // SAFETY: ptr/len describe the mapping we just created. Failure
            // is harmless (C ignores it too).
            unsafe {
                libc::madvise(ptr, len, libc::MADV_RANDOM);
            }
        }
        Ok(DataMap {
            ptr: ptr as *mut u8,
            len,
            meta_end: 0,
            file,
            known_file_size: AtomicU64::new(size),
        })
    }

    /// Declares the first `len` bytes of the map to be the meta pages,
    /// which writers rewrite under readers: [`DataMap::slice`] refuses
    /// them, only [`DataMap::read_volatile`] reads them.
    pub fn with_meta_pages(mut self, len: usize) -> DataMap {
        self.meta_end = len;
        self
    }

    /// Length of the mapping (`me_mapsize`).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the mapping is empty (never: `map_readonly` rejects 0).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `len` bytes at `offset`, or `None` if the range is outside the
    /// map, overlaps the meta pages or is beyond the end of the data file.
    ///
    /// Callers must only ask for pages of a snapshot they hold pinned (see
    /// the type documentation).
    #[inline]
    pub fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        if end > self.len || offset < self.meta_end {
            return None;
        }
        if end as u64 > self.known_file_size.load(Ordering::Relaxed) && !self.refresh_size(end) {
            return None;
        }
        // SAFETY: [offset, end) lies inside the live mapping (checked above)
        // and inside the file (so no SIGBUS), past the meta pages. The
        // mapping lives as long as `self`, and the bytes are not modified
        // while the caller's snapshot is pinned (type-level documentation).
        Some(unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) })
    }

    #[cold]
    fn refresh_size(&self, needed: usize) -> bool {
        match file_size(&self.file) {
            Ok(size) => {
                self.known_file_size.fetch_max(size, Ordering::Relaxed);
                size >= needed as u64
            }
            Err(_) => false,
        }
    }

    /// `MDB_Invalidate` (OpenBSD only, see [`MAP_NEEDS_INVALIDATE`]): after
    /// `len` bytes at `offset` of the file were written through a
    /// descriptor, `msync(MS_INVALIDATE)` drops the map's stale copy of
    /// them, so the next access reads them from the file. C passes its
    /// range as is, which `msync` refuses unless it starts on an OS page
    /// (LMDB 1.0 pages may be smaller); here it is widened to whole OS
    /// pages. Errors are ignored, as in C. The bytes of every page a
    /// snapshot can reach do not change (they are re-read as they were),
    /// so slices handed out stay valid.
    pub fn invalidate(&self, offset: u64, len: usize) {
        let os_page = os_page_size();
        let Ok(offset) = usize::try_from(offset) else {
            return;
        };
        let start = offset - offset % os_page;
        let end = offset.saturating_add(len).min(self.len);
        if start >= end {
            return;
        }
        // SAFETY: [start, end) lies inside our mapping and starts on an OS
        // page. MS_INVALIDATE changes no file contents; it only makes the
        // next access to these pages read them from the file again.
        unsafe {
            libc::msync(
                self.ptr.add(start) as *mut libc::c_void,
                end - start,
                libc::MS_INVALIDATE,
            );
        }
    }

    /// Copies `out.len()` bytes at `offset` with volatile reads, for data
    /// that other processes may be writing concurrently (the meta pages).
    /// Returns `false` if the range is not mapped or beyond the file.
    pub fn read_volatile(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(out.len()) else {
            return false;
        };
        if end > self.len
            || (end as u64 > self.known_file_size.load(Ordering::Relaxed)
                && !self.refresh_size(end))
        {
            return false;
        }
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: offset + i < end <= len, inside the mapping and file.
            // A volatile byte read cannot be torn or elided; concurrent
            // writes by other processes are expected and tolerated.
            *b = unsafe { ptr::read_volatile(self.ptr.add(offset + i)) };
        }
        true
    }
}

impl Drop for DataMap {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe a mapping created in map_readonly that has
        // not been unmapped; no slices outlive `self` (they borrow it).
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

// ---------------------------------------------------------------------------
// The lock file map
// ---------------------------------------------------------------------------

/// A read/write `MAP_SHARED` map of the lock file (`me_txns`).
///
/// All accesses to the shared reader table go through atomic operations on
/// naturally aligned words, which is how the C code's `volatile` fields
/// behave on every supported CPU, and is well defined for memory shared
/// between processes. Offsets come from `format::lockfile`; an offset
/// outside the map or misaligned is a programming error and panics.
pub struct LockMap {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is shared memory designed for concurrent access from
// many threads and processes; every access is atomic (or happens while the
// caller has exclusive use, see `write_bytes`).
unsafe impl Send for LockMap {}
// SAFETY: as above.
unsafe impl Sync for LockMap {}

impl LockMap {
    /// Maps the first `len` bytes of the lock file read/write and shared.
    pub fn map(file: &File, len: usize) -> io::Result<LockMap> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: a fresh shared mapping of a valid fd opened read/write.
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(last_error());
        }
        Ok(LockMap {
            ptr: ptr as *mut u8,
            len,
        })
    }

    /// Length of the mapping.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if the mapping is empty (never: `map` rejects 0).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn checked_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        assert!(
            offset.checked_add(size).is_some_and(|e| e <= self.len) && offset % size == 0,
            "lock file access out of bounds or misaligned: {offset}+{size} of {}",
            self.len
        );
        // SAFETY: offset + size <= len (asserted), so the result stays
        // inside the mapping.
        unsafe { self.ptr.add(offset) }
    }

    #[inline]
    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        let p = self.checked_ptr(offset, 4) as *mut u32;
        // SAFETY: p is in bounds, 4-byte aligned (mmap is page aligned and
        // offset % 4 == 0), and valid for the lifetime of &self. All access
        // to this word is atomic.
        unsafe { AtomicU32::from_ptr(p) }
    }

    #[inline]
    fn atomic_i32(&self, offset: usize) -> &AtomicI32 {
        let p = self.checked_ptr(offset, 4) as *mut i32;
        // SAFETY: as for atomic_u32.
        unsafe { AtomicI32::from_ptr(p) }
    }

    #[inline]
    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        let p = self.checked_ptr(offset, 8) as *mut u64;
        // SAFETY: as for atomic_u32, with 8-byte alignment.
        unsafe { AtomicU64::from_ptr(p) }
    }

    /// Atomically loads the `u32` at `offset`.
    #[inline]
    pub fn load_u32(&self, offset: usize) -> u32 {
        self.atomic_u32(offset).load(Ordering::SeqCst)
    }

    /// Atomically stores the `u32` at `offset`.
    #[inline]
    pub fn store_u32(&self, offset: usize, v: u32) {
        self.atomic_u32(offset).store(v, Ordering::SeqCst)
    }

    /// Atomically loads the `i32` at `offset`.
    #[inline]
    pub fn load_i32(&self, offset: usize) -> i32 {
        self.atomic_i32(offset).load(Ordering::SeqCst)
    }

    /// Atomically stores the `i32` at `offset`.
    #[inline]
    pub fn store_i32(&self, offset: usize, v: i32) {
        self.atomic_i32(offset).store(v, Ordering::SeqCst)
    }

    /// Atomically loads the `u64` at `offset`.
    #[inline]
    pub fn load_u64(&self, offset: usize) -> u64 {
        self.atomic_u64(offset).load(Ordering::SeqCst)
    }

    /// Atomically stores the `u64` at `offset`.
    #[inline]
    pub fn store_u64(&self, offset: usize, v: u64) {
        self.atomic_u64(offset).store(v, Ordering::SeqCst)
    }

    /// Copies `len` bytes at `offset` (volatile byte reads), e.g. a
    /// semaphore name or the header. Used by the lock-file layouts that
    /// store semaphore names; dead code on the mutex layouts.
    #[cfg_attr(
        not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )),
        allow(dead_code)
    )]
    pub fn read_bytes(&self, offset: usize, len: usize) -> Vec<u8> {
        let p = self.checked_ptr(offset, 1);
        assert!(offset + len <= self.len, "lock file read out of bounds");
        (0..len)
            // SAFETY: offset + i < offset + len <= self.len.
            .map(|i| unsafe { ptr::read_volatile(p.add(i)) })
            .collect()
    }

    /// Writes bytes at `offset` (volatile byte writes). Only used while the
    /// caller holds the exclusive file lock and initialises the table.
    #[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
    pub fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        let p = self.checked_ptr(offset, 1);
        assert!(
            offset + bytes.len() <= self.len,
            "lock file write out of bounds"
        );
        for (i, b) in bytes.iter().enumerate() {
            // SAFETY: in bounds (asserted); nobody else uses the table while
            // we hold the exclusive lock.
            unsafe { ptr::write_volatile(p.add(i), *b) };
        }
    }

    /// Raw pointer to `size` bytes at `offset` with the given alignment, for
    /// the pthread mutexes that live in the lock file (Linux).
    #[cfg_attr(any(target_os = "macos", target_os = "ios"), allow(dead_code))]
    pub(crate) fn raw_ptr(&self, offset: usize, size: usize, align: usize) -> *mut u8 {
        assert!(offset % align == 0, "misaligned lock file object");
        assert!(offset + size <= self.len, "lock file object out of bounds");
        self.checked_ptr(offset, 1)
    }
}

impl Drop for LockMap {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe our mapping; nothing borrows it any more.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

// ---------------------------------------------------------------------------
// The parts of the interface Windows does differently (`windows.rs`): how
// files are opened and identified, the lock-file protocol steps, process
// liveness. On Unix they are the plain calls the engine used to make itself.
// ---------------------------------------------------------------------------

/// The descriptor type `copy_to_fd` takes (`mdb_filehandle_t` is an `int`).
pub type BorrowedFile<'a> = std::os::fd::BorrowedFd<'a>;

/// Sets the permissions `mode` for a file of an environment that `opts`
/// may create.
pub fn env_file_options(opts: &mut std::fs::OpenOptions, mode: u32) -> &mut std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(mode)
}

/// `MDB_O_META`: the descriptor for meta-page writes, which reach the disk
/// before the write returns ([`MDB_DSYNC`]).
pub fn sync_write_options(opts: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.custom_flags(MDB_DSYNC)
}

/// `MDB_O_COPY`: creates the target of `mdb_env_copy2` (`O_EXCL`, mode
/// `0666` before the umask); `nocache` then asks the OS not to cache it
/// ([`set_nocache`]).
pub fn open_copy_target(path: &std::path::Path, nocache: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o666)
        .open(path)?;
    if nocache {
        set_nocache(&file);
    }
    Ok(file)
}

/// `true` if `e`, from opening `_path`, says the file does not exist
/// (`ENOENT`, also for a missing directory; a file where a directory should
/// be is `ENOTDIR`).
pub fn is_missing(e: &io::Error, _path: &std::path::Path) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// `true` if `e` says the file system is read-only (`EROFS`).
pub fn is_read_only_fs(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EROFS)
}

/// The identity of an open file: device and inode (`st_dev`, `st_ino`).
pub fn file_id(file: &File) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// Maps the data file of an environment (of any format) read-only:
/// [`DataMap::map_readonly`]. (Windows maps 0.9 and 1.0 files the way their
/// C releases do there.)
pub fn map_data(
    file: File,
    len: usize,
    random: bool,
    _format: crate::format::layout::Format,
) -> io::Result<DataMap> {
    DataMap::map_readonly(file, len, random)
}

/// `mdb_env_share_locks`: turns our exclusive lock on byte 0 of the lock
/// file into a shared one (an `F_SETLK` converts it).
pub fn downgrade_lock(file: &File) -> io::Result<()> {
    lock_byte(file, 0, LockKind::Shared, false)
}

/// Nothing to do before the lock file is closed: closing it releases the
/// process's record locks.
pub fn unlock_lock_file(_file: &File) {}

/// `mdb_reader_pid(Pidset)`: the exclusive lock on byte `pid` of the lock
/// file that advertises this process as alive.
pub fn set_pid_lock(file: &File, pid: i32) -> io::Result<()> {
    lock_byte(file, pid as u64, LockKind::Exclusive, false)
}

/// `mdb_reader_pid(Pidcheck)`: `false` only if the process is certainly
/// dead (nobody holds the lock on byte `pid`).
pub fn pid_alive(file: &File, pid: i32) -> bool {
    probe_lock(file, pid as u64).unwrap_or(true)
}

/// For tests on OpenBSD ([`flush_file_maps`](super::flush_file_maps),
/// [`invalidate_file`](super::invalidate_file)): `msync(flags)` over a
/// temporary read-only map of the whole of `file`, through the caller's
/// descriptor. No second descriptor: closing one would release every
/// `fcntl` lock the process holds on the file (the environment's shared
/// lock on a lock file among them).
#[cfg(target_os = "openbsd")]
pub fn msync_file(file: &File, flags: libc::c_int) -> io::Result<()> {
    let len =
        usize::try_from(file_size(file)?).map_err(|_| io::Error::from_raw_os_error(libc::EFBIG))?;
    if len == 0 {
        return Ok(());
    }
    // SAFETY: a fresh read-only shared mapping of a valid descriptor,
    // unmapped below; msync changes no file contents.
    unsafe {
        let ptr = libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(last_error());
        }
        let rc = libc::msync(ptr, len, flags);
        let err = last_error();
        libc::munmap(ptr, len);
        if rc != 0 {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn page_size_is_power_of_two() {
        let p = os_page_size();
        assert!(p >= 4096 && p.is_power_of_two());
    }

    #[test]
    fn strerror_text() {
        assert_eq!(strerror(libc::ENOENT), "No such file or directory");
    }

    #[test]
    fn data_map_bounds() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[7u8; 8192]).unwrap();
        let m = DataMap::map_readonly(f.try_clone().unwrap(), 1 << 20, true)
            .unwrap()
            .with_meta_pages(2048);
        assert_eq!(m.slice(4096, 4096).unwrap()[0], 7);
        // The meta pages are only read with volatile copies.
        assert!(m.slice(0, 16).is_none() && m.slice(2047, 1).is_none());
        assert!(m.slice(2048, 1).is_some());
        // Inside the map but past the end of the file: refused, no SIGBUS.
        assert!(m.slice(8192, 4096).is_none());
        assert!(m.slice(1 << 20, 1).is_none());
        assert!(m.slice(usize::MAX, 2).is_none());
        // The file grows: the new page becomes visible.
        f.write_all(&[9u8; 4096]).unwrap();
        assert_eq!(m.slice(8192, 4096).unwrap()[4095], 9);
        let mut buf = [0u8; 4];
        assert!(m.read_volatile(8190, &mut buf));
        assert_eq!(buf, [7, 7, 9, 9]);
    }

    /// `invalidate` (only called on OpenBSD) accepts any range: pages
    /// written through the descriptor read back through the map, and ranges
    /// that are unaligned or reach past the map do no harm.
    #[test]
    fn data_map_invalidate() {
        let f = tempfile::tempfile().unwrap();
        f.set_len(3 * 4096).unwrap();
        let m = DataMap::map_readonly(f.try_clone().unwrap(), 1 << 20, false).unwrap();
        assert_eq!(m.slice(4096, 1).unwrap()[0], 0);
        write_all_at(&f, &[5u8; 4096], 4096).unwrap();
        m.invalidate(4096, 4096);
        m.invalidate(4100, 10);
        m.invalidate((1 << 20) - 1, 4096);
        m.invalidate(u64::MAX, 1);
        m.invalidate(0, 0);
        assert_eq!(m.slice(4096, 4096).unwrap(), &[5u8; 4096][..]);
    }

    #[test]
    fn lock_map_atomics() {
        let f = tempfile::tempfile().unwrap();
        f.set_len(4096).unwrap();
        let m = LockMap::map(&f, 4096).unwrap();
        m.store_u64(64, 0x1122_3344_5566_7788);
        m.store_i32(72, -5);
        assert_eq!(m.load_u64(64), 0x1122_3344_5566_7788);
        assert_eq!(m.load_i32(72), -5);
        m.write_bytes(8, b"/MDBr");
        assert_eq!(m.read_bytes(8, 5), b"/MDBr");
        // The stores reach the file. Not on OpenBSD, whose page cache is
        // not the buffer cache: a store through the map shows in `read`
        // only after an `msync`, and the library never reads the lock
        // file through its descriptor (C does not either).
        if !cfg!(target_os = "openbsd") {
            let mut back = [0u8; 8];
            read_at(&f, &mut back, 64).unwrap();
            assert_eq!(u64::from_ne_bytes(back), 0x1122_3344_5566_7788);
        }
    }

    #[test]
    fn record_locks() {
        let f = tempfile::tempfile().unwrap();
        lock_byte(&f, 0, LockKind::Exclusive, false).unwrap();
        // Our own locks never conflict with ourselves.
        assert!(!probe_lock(&f, 0).unwrap());
        lock_byte(&f, 0, LockKind::Shared, false).unwrap();
    }
}
