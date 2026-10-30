//! Layout of the lock file: `MDB_txninfo` header plus the reader table.
//!
//! The lock file is shared memory between all processes using an
//! environment, so its layout must match the C build *on the same platform*
//! (mdb.c lines 668-790). It depends on how the platform implements the two
//! shared locks (reader table mutex `rmutex`, writer mutex `wmutex`):
//!
//! * **Linux/FreeBSD** (`MDB_USE_POSIX_MUTEX`): the `pthread_mutex_t`s live in
//!   the file. `MNAME_LEN == sizeof(pthread_mutex_t)`.
//! * **macOS** (`MDB_USE_POSIX_SEM`): the file stores the 32-byte *names* of
//!   two POSIX named semaphores. `MNAME_LEN == 32`.
//!
//! ```text
//! MDB_txninfo:
//!   mt1 (padded to a multiple of CACHELINE = 64):
//!     u32  mtb_magic        MDB_MAGIC
//!     u32  mtb_format       MDB_LOCK_FORMAT (0x10001 on Unix)
//!     [MNAME_LEN] rmutex or rmname
//!     u64  mtb_txnid        last committed txn (aligned to 8)
//!     u32  mtb_numreaders   high-water mark of used reader slots
//!   mt2 (MNAME_LEN padded to a multiple of 64):
//!     [MNAME_LEN] wmutex or wmname
//!   MDB_reader mti_readers[maxreaders]   (64 bytes each)
//!     u64  mr_txnid  (!0 when the slot is owned but not in a txn)
//!     i32  mr_pid    (0 when the slot is free)
//!     u64  mr_tid    (pthread_t, at offset 16)
//! ```
//!
//! The file size is `READERS_OFFSET + maxreaders * 64`
//! (`(maxreaders-1) * sizeof(MDB_reader) + sizeof(MDB_txninfo)`).
//!
//! Live access to the mapped lock file goes through `os` (atomics); this
//! module only knows offsets, plus a decoder for byte snapshots used in tests
//! and diagnostics.

use super::{get_u32, get_u64};
use crate::consts::{CACHELINE, MDB_LOCK_FORMAT};

const fn align_up(n: usize, a: usize) -> usize {
    n.div_ceil(a) * a
}

const fn max(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

/// `true` where the C build uses POSIX named semaphores (`MDB_USE_POSIX_SEM`).
pub const USES_NAMED_SEMAPHORES: bool = cfg!(any(target_os = "macos", target_os = "ios"));

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    /// `MNAME_LEN` with `MDB_USE_POSIX_SEM`.
    pub const MNAME_LEN: usize = 32;
    /// Alignment of the `mtb_rmname` member (a `char` array).
    pub const MUTEX_ALIGN: usize = 1;
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
mod platform {
    /// `MNAME_LEN` with `MDB_USE_POSIX_MUTEX`: `sizeof(pthread_mutex_t)`
    /// (40 on x86-64 glibc, 48 on aarch64 glibc).
    pub const MNAME_LEN: usize = core::mem::size_of::<libc::pthread_mutex_t>();
    /// `alignof(pthread_mutex_t)`.
    pub const MUTEX_ALIGN: usize = core::mem::align_of::<libc::pthread_mutex_t>();
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
)))]
compile_error!(
    "lmdb: unsupported platform; the lock file layout is only known for Linux, FreeBSD and macOS"
);

/// `MNAME_LEN`: size of the mutex (Linux) or semaphore name (macOS) field.
pub const MNAME_LEN: usize = platform::MNAME_LEN;

/// `MDB_LOCK_FORMAT`.
pub const LOCK_FORMAT: u32 = MDB_LOCK_FORMAT;

/// Offset of `mti_magic` (u32).
pub const TXNINFO_MAGIC: usize = 0;
/// Offset of `mti_format` (u32).
pub const TXNINFO_FORMAT: usize = 4;
/// Offset of `mti_rmutex` / `mti_rmname`.
pub const TXNINFO_RMUTEX: usize = align_up(8, platform::MUTEX_ALIGN);
/// Offset of `mti_txnid` (u64).
pub const TXNINFO_TXNID: usize = align_up(TXNINFO_RMUTEX + MNAME_LEN, 8);
/// Offset of `mti_numreaders` (u32).
pub const TXNINFO_NUMREADERS: usize = TXNINFO_TXNID + 8;
/// `sizeof(MDB_txbody)`.
const TXBODY_SIZE: usize = align_up(TXNINFO_NUMREADERS + 4, max(8, platform::MUTEX_ALIGN));
/// Size of the `mt1` union: `MDB_txbody` padded to a cache line multiple.
pub const TXNINFO_HEADER_SIZE: usize = align_up(TXBODY_SIZE, CACHELINE);
/// Offset of `mti_wmutex` / `mti_wmname` (start of the `mt2` union).
pub const TXNINFO_WMUTEX: usize = TXNINFO_HEADER_SIZE;
/// Offset of `mti_readers[0]`.
pub const READERS_OFFSET: usize = TXNINFO_WMUTEX + align_up(MNAME_LEN, CACHELINE);

/// `sizeof(MDB_reader)`: one cache line.
pub const READER_SIZE: usize = align_up(24, CACHELINE);
/// Offset of `mr_txnid` (u64) within a reader slot.
pub const READER_TXNID: usize = 0;
/// Offset of `mr_pid` (`pid_t`, i32) within a reader slot.
pub const READER_PID: usize = 8;
/// Offset of `mr_tid` (`pthread_t`, 8 bytes) within a reader slot.
pub const READER_TID: usize = 16;

/// Size of a lock file with room for `max_readers` slots (`rsize` in
/// `mdb_env_setup_locks`).
pub const fn lock_file_size(max_readers: u32) -> usize {
    READERS_OFFSET + max_readers as usize * READER_SIZE
}

/// Number of reader slots in an existing lock file of `size` bytes, as
/// `mdb_env_setup_locks` computes it for non-exclusive openers.
pub const fn readers_in_file(size: usize) -> usize {
    if size < READERS_OFFSET {
        0
    } else {
        (size - READERS_OFFSET) / READER_SIZE
    }
}

/// The offset of reader slot `i`.
pub const fn reader_offset(i: usize) -> usize {
    READERS_OFFSET + i * READER_SIZE
}

/// A decoded copy of a lock-file header (for diagnostics and tests; live
/// access goes through `os::LockMap`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnInfoSnapshot {
    /// `mti_magic`.
    pub magic: u32,
    /// `mti_format`.
    pub format: u32,
    /// `mti_txnid`.
    pub txnid: u64,
    /// `mti_numreaders`.
    pub num_readers: u32,
    /// `mti_rmname` (macOS only; empty on Linux).
    pub rmname: Vec<u8>,
    /// `mti_wmname` (macOS only; empty on Linux).
    pub wmname: Vec<u8>,
}

impl TxnInfoSnapshot {
    /// Decodes the header from a copy of the lock file's first bytes.
    pub fn read(buf: &[u8]) -> Option<TxnInfoSnapshot> {
        let name = |off: usize| -> Option<Vec<u8>> {
            if !USES_NAMED_SEMAPHORES {
                return Some(Vec::new());
            }
            let raw = buf.get(off..off + MNAME_LEN)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            Some(raw[..end].to_vec())
        };
        Some(TxnInfoSnapshot {
            magic: get_u32(buf, TXNINFO_MAGIC)?,
            format: get_u32(buf, TXNINFO_FORMAT)?,
            txnid: get_u64(buf, TXNINFO_TXNID)?,
            num_readers: get_u32(buf, TXNINFO_NUMREADERS)?,
            rmname: name(TXNINFO_RMUTEX)?,
            wmname: name(TXNINFO_WMUTEX)?,
        })
    }
}

/// A decoded copy of one reader slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderSnapshot {
    /// `mr_txnid`.
    pub txnid: u64,
    /// `mr_pid`.
    pub pid: i32,
    /// `mr_tid`.
    pub tid: u64,
}

impl ReaderSnapshot {
    /// Decodes slot `i` from a copy of the lock file.
    pub fn read(buf: &[u8], i: usize) -> Option<ReaderSnapshot> {
        let off = reader_offset(i);
        Some(ReaderSnapshot {
            txnid: get_u64(buf, off + READER_TXNID)?,
            pid: get_u32(buf, off + READER_PID)? as i32,
            tid: get_u64(buf, off + READER_TID)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_slot_layout() {
        assert_eq!(READER_SIZE, 64);
        assert_eq!(READERS_OFFSET % 64, 0);
        assert_eq!(lock_file_size(126), READERS_OFFSET + 126 * 64);
        assert_eq!(readers_in_file(lock_file_size(7)), 7);
        assert_eq!(readers_in_file(10), 0);
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn macos_layout() {
        // MDB_txbody = magic, format, rmname[32], txnid@40, numreaders@48 -> 52 -> 64.
        assert_eq!(TXNINFO_RMUTEX, 8);
        assert_eq!(TXNINFO_TXNID, 40);
        assert_eq!(TXNINFO_NUMREADERS, 48);
        assert_eq!(TXNINFO_HEADER_SIZE, 64);
        assert_eq!(TXNINFO_WMUTEX, 64);
        assert_eq!(READERS_OFFSET, 128);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn linux_x86_64_glibc_layout() {
        assert_eq!(MNAME_LEN, 40);
        assert_eq!(TXNINFO_TXNID, 48);
        assert_eq!(TXNINFO_NUMREADERS, 56);
        assert_eq!(TXNINFO_HEADER_SIZE, 64);
        assert_eq!(READERS_OFFSET, 128);
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    #[test]
    fn linux_aarch64_glibc_layout() {
        assert_eq!(MNAME_LEN, 48);
        assert_eq!(TXNINFO_TXNID, 56);
        assert_eq!(TXNINFO_HEADER_SIZE, 128);
        assert_eq!(READERS_OFFSET, 192);
    }
}
