//! Operating system interface: the **only** module of the crate that contains
//! `unsafe` code.
//!
//! It wraps exactly the system facilities `mdb.c` uses on Unix:
//!
//! * [`unix`]: memory maps of the data file (read-only, `MAP_SHARED`) and of
//!   the lock file (read/write, `MAP_SHARED`), `fcntl` byte-range locks,
//!   `pread`/`pwrite`/`pwritev`, `fdatasync`/`F_FULLFSYNC`, page size, pid and
//!   thread id, `strerror`.
//! * [`shared_lock`]: the two inter-process locks guarding the reader table
//!   and the writer, implemented the way the C build on the same platform does
//!   it (robust process-shared pthread mutexes on Linux/FreeBSD, POSIX named
//!   semaphores on macOS; on OpenBSD, NetBSD and DragonFly named semaphores
//!   for LMDB 0.9 lock files and a SysV semaphore set for LMDB 1.0 ones).
//!
//! On Windows, `windows.rs` provides the same public surface (`DataMap`,
//! `LockMap`, `lock_byte`, `SharedMutex`, …) after mdb.c's `_WIN32` paths:
//! file mappings, `LockFileEx` on the lock file, named mutexes; its module
//! documentation lists every Windows-specific decision.
//!
//! Every `unsafe` block carries a `// SAFETY:` comment. Higher layers only see
//! safe types: byte slices with lifetimes, atomics on the lock file, and
//! `io::Result`s.
//!
//! # `MDB_WRITEMAP` on OpenBSD: refused
//!
//! rlmdb never writes through a writable map: with `MDB_WRITEMAP` it writes
//! pages with `pwrite`, as without the flag, and keeps the limits the flag
//! sets through the API (no nested transactions). Where the OS has one page
//! cache for a file's maps and its descriptors (Linux, macOS, the other
//! BSDs, Windows) that is safe next to a C process that does use the map.
//! OpenBSD keeps a file's mapped pages apart from what `pwrite` and `pread`
//! see ([`MAP_NEEDS_INVALIDATE`]; `KNOWN_C_BUGS.md` row 52 shows the effect
//! on the meta page): a process that accepted the flag but writes with
//! `pwrite` would be a process without `MDB_WRITEMAP` mixed with C
//! processes using it, which `lmdb.h` forbids ("Do not mix processes with
//! and without MDB_WRITEMAP on the same environment"). So on OpenBSD, until
//! rlmdb implements a writable map, an open for writing with the flag fails
//! with `ENOTSUP` ([`check_write_map`]); a read-only open drops the flag
//! silently, as C does, and writes nothing. `ENOTSUP`, not C's `EINVAL` for
//! a bad flag, so that a program can tell "not supported here" from a
//! wrong flag, as the unimplemented LMDB 1.0 functions of the C API do.

#![allow(unsafe_code)]

#[cfg(unix)]
pub mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
pub mod shared_lock;

// Windows (MSVC and MinGW): `windows.rs` also holds the shared locks, as
// `windows::shared_lock`, re-exported here as `os::shared_lock`.
#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(not(any(unix, windows)))]
compile_error!("lmdb: only Unix and Windows are implemented");

/// For tests that read an environment's file through a descriptor while
/// processes have it mapped: where the OS keeps a map's dirty pages apart
/// from the file until they are synced (OpenBSD, [`MAP_NEEDS_INVALIDATE`]),
/// writes them to it (`msync(MS_SYNC)` through a map of the whole file), so
/// that `read` sees what was stored through the maps; nothing elsewhere.
/// Also for a write through a descriptor to such a file: done first, the
/// invalidation after the write ([`invalidate_file`]) cannot write a dirty
/// page back over it. `file` must be open for reading. Not part of the API.
#[doc(hidden)]
pub fn flush_file_maps(file: &std::fs::File) -> std::io::Result<()> {
    #[cfg(target_os = "openbsd")]
    {
        unix::msync_file(file, libc::MS_SYNC)
    }
    #[cfg(not(target_os = "openbsd"))]
    {
        let _ = file;
        Ok(())
    }
}

/// For tests that write to an environment's file through a descriptor
/// behind its back, as another process could: drops the cached copy of
/// `len` bytes at `offset` of `file` (of the whole file, in fact) where a
/// map does not see such writes by itself (OpenBSD,
/// [`MAP_NEEDS_INVALIDATE`]), as an LMDB process does after its own writes
/// (`MDB_Invalidate`); nothing elsewhere. `file` must be open for reading
/// (the invalidation goes through a read-only map of it). Not part of the
/// API.
#[doc(hidden)]
pub fn invalidate_file(file: &std::fs::File, offset: u64, len: usize) -> std::io::Result<()> {
    let _ = offset;
    if len == 0 {
        return Ok(());
    }
    #[cfg(target_os = "openbsd")]
    {
        unix::msync_file(file, libc::MS_INVALIDATE)
    }
    #[cfg(not(target_os = "openbsd"))]
    {
        let _ = file;
        Ok(())
    }
}

/// `MDB_WRITEMAP` on an open for writing: refused with `ENOTSUP` on
/// OpenBSD, which keeps a file's maps apart from its descriptors, until
/// rlmdb implements a writable map (see the module documentation).
#[cfg(target_os = "openbsd")]
pub fn check_write_map() -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(libc::ENOTSUP))
}

/// `MDB_WRITEMAP` on an open for writing: accepted (and its writable map
/// not used) where maps and descriptors share one page cache, that is
/// everywhere but OpenBSD (see the module documentation).
#[cfg(not(target_os = "openbsd"))]
pub fn check_write_map() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The `MDB_WRITEMAP` policy: refused on OpenBSD, accepted elsewhere.
    #[test]
    fn write_map_policy() {
        let checked = super::check_write_map();
        if cfg!(target_os = "openbsd") {
            assert_eq!(checked.unwrap_err().raw_os_error(), Some(libc::ENOTSUP));
        } else {
            checked.unwrap();
        }
    }
}
