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
