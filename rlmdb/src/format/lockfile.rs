//! Layout of the lock file: `MDB_txninfo` header plus the reader table, in
//! both lock-file versions.
//!
//! The lock file is shared memory between all processes using an
//! environment, so its layout must match the C build *on the same platform*
//! and *of the same release*. Its version follows the data format: LMDB 0.9
//! (`MDB_DATA_VERSION 1`) uses `MDB_LOCK_VERSION 1` (mdb.c lines 668-790),
//! LMDB 1.0 (`MDB_DATA_VERSION 3`) uses `MDB_LOCK_VERSION 2` (mdb.c of
//! `mdb.RE/1.0` at 4865523, "mdb10.c" below, lines 865-1017). A
//! [`LockLayout`] describes one version on this platform; [`LockLayout::of`]
//! picks it for a data [`Format`], and the reader table code
//! (`core::reader`, `os::shared_lock`) takes every offset from it.
//!
//! The layout depends on how the platform implements the two shared locks
//! (reader table mutex `rmutex`, writer mutex `wmutex`), see [`LockObjects`]:
//!
//! * **Linux/FreeBSD** (`MDB_USE_POSIX_MUTEX`): the `pthread_mutex_t`s live in
//!   the file. `MNAME_LEN == sizeof(pthread_mutex_t)`.
//! * **macOS** (`MDB_USE_POSIX_SEM`): two POSIX named semaphores. Version 1
//!   stores their 32-byte *names* (`MNAME_LEN == 32`); version 2 stores a
//!   64-bit hash, `mtb_mutexid`, that the names are derived from, and has no
//!   second cache line for the writer lock.
//! * **OpenBSD, NetBSD, DragonFly** (mdb.c lines 143-149, mdb10.c lines
//!   192-203): version 1 as on macOS (named semaphores, `MNAME_LEN == 32`;
//!   NetBSD names are one character shorter). Version 2 uses a SysV
//!   semaphore set (`MDB_USE_SYSV_SEM`): its id `mtb_semid` and a flag
//!   `mtb_rlocked` in `mt1`, the flag `mt2_wlocked` in `mt2`
//!   (`MNAME_LEN == sizeof(int)`). FreeBSD is Linux's case, with an 8-byte
//!   `pthread_mutex_t` (a pointer; the mutex itself lives in a kernel object
//!   keyed by its address).
//!
//! ```text
//! Version 1 (LMDB 0.9), MDB_txninfo:
//!   mt1 (padded to a multiple of CACHELINE = 64):
//!     u32  mtb_magic        MDB_MAGIC
//!     u32  mtb_format       MDB_LOCK_FORMAT (0x10001 on Unix)
//!     [MNAME_LEN] mtb_rmutex | mtb_rmname
//!     u64  mtb_txnid        last committed txn (aligned to 8)
//!     u32  mtb_numreaders   high-water mark of used reader slots
//!   mt2 (MNAME_LEN padded to a multiple of 64):
//!     [MNAME_LEN] mt2_wmutex | mt2_wmname
//!   MDB_reader mti_readers[maxreaders]
//!
//! Version 2 (LMDB 1.0), MDB_txninfo:
//!   mt1 (padded to a multiple of 64):
//!     u32  mtb_magic        MDB_MAGIC
//!     u32  mtb_format       2 + (MDB_lock_desc << 12), per platform
//!     u64  mtb_txnid
//!     u32  mtb_numreaders
//!     pthread_mutex_t mtb_rmutex | u64 mtb_mutexid (aligned)
//!   mt2 (Linux only; sizeof(pthread_mutex_t) padded to a multiple of 64):
//!     pthread_mutex_t mt2_wmutex
//!   MDB_reader mti_readers[maxreaders]
//!
//! MDB_reader, both versions (64 bytes each):
//!   u64  mr_txnid  (!0 when the slot is owned but not in a txn)
//!   i32  mr_pid    (0 when the slot is free)
//!   u64  mr_tid    (pthread_t, at offset 16)
//! ```
//!
//! The numbers, measured with `layout_dump` and `layout_dump10` (interop
//! README; Linux x86-64 once, in an emulated container):
//!
//! | | macOS v1 | macOS v2 | Linux aarch64 v1 | v2 | Linux x86-64 v1 | v2 |
//! |---|---|---|---|---|---|---|
//! | `mti_format` | `0x10001` | `0x28570002` | `0x10001` | `0x2cd82002` | `0x10001` | `0x2c2f6002` |
//! | `mti_txnid`, `mti_numreaders` | 40, 48 | 8, 16 | 56, 64 | 8, 16 | 48, 56 | 8, 16 |
//! | reader lock: offset, size | 8, 32 | 24, 8 | 8, 48 | 24, 48 | 8, 40 | 24, 40 |
//! | writer lock | 64 | none | 128 | 128 | 64 | 64 |
//! | `mti_readers` | 128 | 64 | 192 | 192 | 128 | 128 |
//!
//! The BSDs, computed from the C declarations, not measured (64-bit; no
//! `layout_dump` has run there yet):
//!
//! | | FreeBSD v1 | FreeBSD v2 | Open/Net/DragonFly v1 | v2 (SysV) |
//! |---|---|---|---|---|
//! | `mti_format` | `0x10001` | `0x2986c002` | `0x10001` | `0x2889a002` |
//! | `mti_txnid`, `mti_numreaders` | 16, 24 | 8, 16 | 40, 48 | 8, 16 |
//! | reader lock: offset, size | 8, 8 | 24, 8 | 8, 32 | 20, 8 |
//! | writer lock | 64 | 64 | 64 | 64 |
//! | `mti_readers` | 128 | 128 | 128 | 128 |
//!
//! The file size is `readers + maxreaders * 64`
//! (`(maxreaders-1) * sizeof(MDB_reader) + sizeof(MDB_txninfo)`).
//!
//! Live access to the mapped lock file goes through `os` (atomics); this
//! module only knows offsets, plus decoders for byte snapshots used in tests
//! and diagnostics.

use super::layout::Format;
use super::{get_u32, get_u64};
use crate::consts::{CACHELINE, MDB_LOCK_FORMAT, MDB_LOCK_VERSION, MDB_PIDLOCK};

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

/// `LOG2_MOD(p2, n)` (mdb10.c line 779): `log2(p2) % n` for a power of two
/// `p2` and `0 < n < 8`, computed the way C does.
const fn log2_mod(p2: usize, n: u32) -> usize {
    7 - 86 / (p2 % ((1 << n) - 1) + 11)
}

/// `true` where the C 0.9 build uses POSIX named semaphores
/// (`MDB_USE_POSIX_SEM`). C 1.0 uses them only on macOS; the other BSDs
/// switch to SysV semaphores ([`LOCK_V10`]`.objects`).
pub const USES_NAMED_SEMAPHORES: bool = cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
));

/// How the two shared locks of an environment are kept in the lock file
/// (`MDB_USE_POSIX_MUTEX` or `MDB_USE_POSIX_SEM` in mdb.c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockObjects {
    /// Robust process-shared `pthread_mutex_t`s at [`LockLayout::rlock`]
    /// and [`LockLayout::wlock`] (Linux, FreeBSD; both versions).
    PthreadMutexes,
    /// POSIX named semaphores whose NUL-terminated names are stored at
    /// [`LockLayout::rlock`] and [`LockLayout::wlock`] (macOS, version 1:
    /// `mtb_rmname`, `mt2_wmname`). The names are read, never derived: C
    /// hashes a buffer with uninitialised padding to make them.
    SemaphoreNames,
    /// POSIX named semaphores named after the 64-bit hash `mtb_mutexid` at
    /// [`LockLayout::rlock`] (macOS, version 2): `"/MDB"`, `'r'` or `'w'`,
    /// then `mdb_pack85` of the hash (`MUTEXNAME`, mdb10.c lines
    /// 5920-5945). The hash is read from the file; only the exclusive
    /// opener computes it.
    SemaphoreHash,
    /// A SysV semaphore set of two (reader 0, writer 1), whose id
    /// `mtb_semid` (an `int`) is at [`LockLayout::rlock`], followed by the
    /// `int` flag `mtb_rlocked`; the writer's flag `mt2_wlocked` is at
    /// [`LockLayout::wlock`] (OpenBSD, NetBSD, DragonFly, version 2:
    /// `MDB_USE_SYSV_SEM`, mdb10.c lines 452-481 and 6096-6110). A flag is
    /// set while its semaphore is held; `SEM_UNDO` releases the semaphore
    /// of a process that dies holding it, and the flag it leaves set tells
    /// the next holder (`MDB_OWNERDEAD`). The id is read from the file;
    /// only the exclusive opener creates the set.
    SysVSemaphores,
}

impl LockObjects {
    /// `true` if the objects outlive the processes using them, so the last
    /// one to close the environment removes them (`mdb_env_close0`):
    /// `sem_unlink` of the named semaphores, `IPC_RMID` of the SysV set.
    /// Process-shared mutexes live in the lock file and need nothing.
    pub const fn removed_by_last_user(self) -> bool {
        matches!(
            self,
            LockObjects::SemaphoreNames | LockObjects::SemaphoreHash | LockObjects::SysVSemaphores
        )
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use super::LockObjects;

    /// `MNAME_LEN` of version 1 with `MDB_USE_POSIX_SEM`: the space for a
    /// semaphore name.
    pub const MNAME_LEN: usize = 32;
    /// Alignment of the version 1 `mtb_rmname` member (a `char` array).
    pub const MUTEX_ALIGN: usize = 1;
    /// The shared locks of version 1.
    pub const OBJECTS_V1: LockObjects = LockObjects::SemaphoreNames;
    /// The shared locks of version 2.
    pub const OBJECTS_V2: LockObjects = LockObjects::SemaphoreHash;
    /// Size and alignment of the version 2 reader lock field,
    /// `mdb_hash_t mtb_mutexid` (`unsigned long long`).
    pub const RLOCK_V2: (usize, usize) = (
        core::mem::size_of::<core::ffi::c_ulonglong>(),
        core::mem::align_of::<core::ffi::c_ulonglong>(),
    );
    /// `MDB_LOCK_TYPE` of version 2 (mdb10.c line 980).
    pub const LOCK_TYPE_V2: usize = super::lock_type_posix_sem(RLOCK_V2.1);
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
mod platform {
    use super::LockObjects;

    /// `MNAME_LEN` with `MDB_USE_POSIX_MUTEX`: `sizeof(pthread_mutex_t)`
    /// (40 on x86-64 glibc, 48 on aarch64 glibc, 8 on FreeBSD).
    pub const MNAME_LEN: usize = core::mem::size_of::<libc::pthread_mutex_t>();
    /// `alignof(pthread_mutex_t)`.
    pub const MUTEX_ALIGN: usize = core::mem::align_of::<libc::pthread_mutex_t>();
    /// The shared locks of version 1.
    pub const OBJECTS_V1: LockObjects = LockObjects::PthreadMutexes;
    /// The shared locks of version 2.
    pub const OBJECTS_V2: LockObjects = LockObjects::PthreadMutexes;
    /// Size and alignment of the version 2 reader lock field, `mtb_rmutex`.
    pub const RLOCK_V2: (usize, usize) = (MNAME_LEN, MUTEX_ALIGN);
    /// `MDB_LOCK_TYPE` of version 2 (mdb10.c line 987).
    pub const LOCK_TYPE_V2: usize = super::lock_type_posix_mutex(MNAME_LEN, MUTEX_ALIGN);
}

#[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
mod platform {
    use super::LockObjects;

    /// `MNAME_LEN` of version 1 with `MDB_USE_POSIX_SEM`, which C 0.9
    /// selects for every BSD but FreeBSD (`defined(BSD)`, mdb.c line 143).
    pub const MNAME_LEN: usize = 32;
    /// Alignment of the version 1 `mtb_rmname` member (a `char` array).
    pub const MUTEX_ALIGN: usize = 1;
    /// The shared locks of version 1.
    pub const OBJECTS_V1: LockObjects = LockObjects::SemaphoreNames;
    /// The shared locks of version 2: C 1.0 selects `MDB_USE_SYSV_SEM`
    /// for these BSDs (mdb10.c lines 192-198).
    pub const OBJECTS_V2: LockObjects = LockObjects::SysVSemaphores;
    /// Size and alignment of the version 2 reader lock fields,
    /// `int mtb_semid` and `int mtb_rlocked`.
    pub const RLOCK_V2: (usize, usize) = (
        2 * core::mem::size_of::<core::ffi::c_int>(),
        core::mem::align_of::<core::ffi::c_int>(),
    );
    /// `MDB_LOCK_TYPE` of version 2 with `MDB_USE_SYSV_SEM` (mdb10.c line
    /// 982).
    pub const LOCK_TYPE_V2: usize = 8;
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
compile_error!(
    "lmdb: unsupported platform; the lock file layout is only known for Linux, macOS and the BSDs"
);

// --- version 1 (LMDB 0.9) -------------------------------------------------

/// `MNAME_LEN` of version 1: size of the mutex (Linux) or semaphore name
/// (macOS) fields.
pub const MNAME_LEN: usize = platform::MNAME_LEN;

/// `MDB_LOCK_FORMAT` of version 1. Engine code uses
/// [`LockLayout::lock_format`].
pub const LOCK_FORMAT: u32 = MDB_LOCK_FORMAT;

/// Offset of `mti_magic` (u32), in both versions.
pub const TXNINFO_MAGIC: usize = 0;
/// Offset of `mti_format` (u32), in both versions.
pub const TXNINFO_FORMAT: usize = 4;
/// Offset of `mti_rmutex` / `mti_rmname` (version 1).
pub const TXNINFO_RMUTEX: usize = align_up(8, platform::MUTEX_ALIGN);
/// Offset of `mti_txnid` (u64) (version 1).
pub const TXNINFO_TXNID: usize = align_up(TXNINFO_RMUTEX + MNAME_LEN, 8);
/// Offset of `mti_numreaders` (u32) (version 1).
pub const TXNINFO_NUMREADERS: usize = TXNINFO_TXNID + 8;
/// `sizeof(MDB_txbody)` (version 1).
const TXBODY_SIZE: usize = align_up(TXNINFO_NUMREADERS + 4, max(8, platform::MUTEX_ALIGN));
/// Size of the `mt1` union: `MDB_txbody` padded to a cache line multiple
/// (version 1).
pub const TXNINFO_HEADER_SIZE: usize = align_up(TXBODY_SIZE, CACHELINE);
/// Offset of `mti_wmutex` / `mti_wmname`, the start of the `mt2` union
/// (version 1).
pub const TXNINFO_WMUTEX: usize = TXNINFO_HEADER_SIZE;
/// Offset of `mti_readers[0]` (version 1). Engine code uses
/// [`LockLayout::readers`].
pub const READERS_OFFSET: usize = TXNINFO_WMUTEX + align_up(MNAME_LEN, CACHELINE);

// --- the reader slot, both versions ----------------------------------------

/// `sizeof(MDB_reader)`: one cache line.
pub const READER_SIZE: usize = align_up(24, CACHELINE);
/// Offset of `mr_txnid` (u64) within a reader slot.
pub const READER_TXNID: usize = 0;
/// Offset of `mr_pid` (`pid_t`, i32) within a reader slot.
pub const READER_PID: usize = 8;
/// Offset of `mr_tid` (`pthread_t`, 8 bytes) within a reader slot.
pub const READER_TID: usize = 16;

// --- version 2 (LMDB 1.0) -------------------------------------------------

/// `MDB_LOCK_VERSION` of LMDB 1.0 (mdb10.c line 699).
const LOCK_VERSION_V2: u32 = 2;
/// `MDB_LOCK_VERSION_BITS` (mdb10.c line 703): the low bits of
/// `MDB_LOCK_FORMAT` that hold the version; `MDB_lock_desc` is above them.
const LOCK_VERSION_BITS: u32 = 12;

/// Offset of `mti_txnid` (version 2).
const V2_TXNID: usize = 8;
/// Offset of `mti_numreaders` (version 2).
const V2_NUMREADERS: usize = 16;
/// Offset of `mtb_rmutex` / `mtb_mutexid` / `mtb_semid`, the last
/// member(s) (version 2).
const V2_RLOCK: usize = align_up(V2_NUMREADERS + 4, platform::RLOCK_V2.1);
/// `sizeof(MDB_txbody)` (version 2).
const V2_TXBODY: usize = align_up(
    V2_RLOCK + platform::RLOCK_V2.0,
    max(8, platform::RLOCK_V2.1),
);
/// Size of the `mt1` union (version 2).
const V2_HEADER: usize = align_up(V2_TXBODY, CACHELINE);
/// `true` if version 2 has the `mt2` union for the writer lock: with
/// pthread mutexes and SysV semaphores, not with named semaphores
/// (`#if !(defined(_WIN32) || defined(MDB_USE_POSIX_SEM))`, mdb10.c line
/// 953).
const V2_HAS_MT2: bool = matches!(
    platform::OBJECTS_V2,
    LockObjects::PthreadMutexes | LockObjects::SysVSemaphores
);
/// `MNAME_LEN` of version 2, the size of the member of `mt2`:
/// `sizeof(int)` (`mt2_wlocked`) with SysV semaphores, else
/// `sizeof(pthread_mutex_t)` (mdb10.c lines 535-539).
const V2_MNAME_LEN: usize = match platform::OBJECTS_V2 {
    LockObjects::SysVSemaphores => core::mem::size_of::<core::ffi::c_int>(),
    _ => MNAME_LEN,
};
/// Offset of `mti_readers[0]` (version 2): there is no `mt2` with named
/// semaphores.
const V2_READERS: usize = if V2_HAS_MT2 {
    V2_HEADER + align_up(V2_MNAME_LEN, CACHELINE)
} else {
    V2_HEADER
};

/// `MDB_LOCK_TYPE` with `MDB_USE_POSIX_SEM` (mdb10.c line 980):
/// `4 + ALIGNOF2(mdb_hash_t)/8 % 2`. (Every platform's value is tested.)
#[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
const fn lock_type_posix_sem(hash_align: usize) -> usize {
    4 + hash_align / 8 % 2
}

/// `MDB_LOCK_TYPE` with `MDB_USE_POSIX_MUTEX` (mdb10.c line 987): only the
/// size and alignment of `pthread_mutex_t` are checked,
/// `10 + LOG2_MOD(ALIGNOF2(pthread_mutex_t), 5) + sizeof(pthread_mutex_t) / 4 % 22 * 5`.
#[cfg_attr(
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ),
    allow(dead_code)
)]
const fn lock_type_posix_mutex(size: usize, align: usize) -> usize {
    10 + log2_mod(align, 5) + size / 4 % 22 * 5
}

/// What `MDB_lock_desc` (mdb10.c lines 1004-1017) is computed from.
#[derive(Debug, Clone, Copy)]
struct LockDescInputs {
    cacheline: usize,
    pid_size: usize,
    pthread_size: usize,
    txbody_size: usize,
    lock_type: usize,
    txnid_size: usize,
    reader_size: usize,
    pidlock: bool,
}

/// `MDB_lock_desc`, term by term as in mdb10.c.
const fn lock_desc(d: LockDescInputs) -> u32 {
    let cacheline_term = if d.cacheline == 64 {
        0
    } else {
        1 + log2_mod(d.cacheline >> (d.cacheline > 64) as usize, 5)
    };
    (cacheline_term
        + 6 * (d.pid_size / 4 % 3)
        + 18 * (d.pthread_size / 4 % 5)
        + 90 * (d.txbody_size / d.cacheline % 3)
        + 270 * (d.lock_type % 120)
        + (((d.txnid_size == 8) as usize) << 15)
        + (((d.reader_size > d.cacheline) as usize) << 16)
        + ((d.pidlock as usize) << 17)) as u32
}

/// `MDB_LOCK_FORMAT` of LMDB 1.0: the version in the low
/// `MDB_LOCK_VERSION_BITS`, `MDB_lock_desc` above (mdb10.c line 969).
const fn lock_format_v2(d: LockDescInputs) -> u32 {
    LOCK_VERSION_V2 % (1 << LOCK_VERSION_BITS) + (lock_desc(d) << LOCK_VERSION_BITS)
}

/// The inputs of `MDB_lock_desc` for this platform.
const LOCK_DESC_V2: LockDescInputs = LockDescInputs {
    cacheline: CACHELINE,
    pid_size: core::mem::size_of::<libc::pid_t>(),
    pthread_size: core::mem::size_of::<libc::pthread_t>(),
    txbody_size: V2_TXBODY,
    lock_type: platform::LOCK_TYPE_V2,
    txnid_size: 8,
    reader_size: READER_SIZE,
    pidlock: MDB_PIDLOCK != 0,
};

// --- the descriptor -------------------------------------------------------------

/// Where the fields of one lock-file version are on this platform. One
/// `static` per version ([`LOCK_V09`], [`LOCK_V10`]); [`LockLayout::of`]
/// picks the one of a data [`Format`]. `mti_magic` and `mti_format` are at
/// [`TXNINFO_MAGIC`] and [`TXNINFO_FORMAT`], and a reader slot is laid out
/// the same way ([`READER_SIZE`], [`READER_TXNID`], …), in both versions.
#[derive(Debug, PartialEq, Eq)]
pub struct LockLayout {
    /// The data format whose lock file this is.
    pub format: Format,
    /// `MDB_LOCK_VERSION`: 1 or 2.
    pub version: u32,
    /// `MDB_LOCK_FORMAT`: the value of `mti_format`. Version 2 encodes the
    /// platform in it, so it is a per-platform constant.
    pub lock_format: u32,
    /// How the shared locks are kept.
    pub objects: LockObjects,
    /// Offset of `mti_txnid` (u64).
    pub txnid: usize,
    /// Offset of `mti_numreaders` (u32).
    pub num_readers: usize,
    /// Offset of the reader lock field: `mti_rmutex` (Linux, FreeBSD),
    /// `mti_rmname` (named semaphores, version 1), `mti_mutexid` (macOS,
    /// version 2) or `mti_semid` followed by `mti_rlocked` (the other BSDs,
    /// version 2).
    pub rlock: usize,
    /// Size of the reader lock field.
    pub rlock_size: usize,
    /// Offset of the writer lock field `mti_wmutex` / `mti_wmname` /
    /// `mti_wlocked` (the `mt2` union); `None` where there is none (macOS,
    /// version 2).
    pub wlock: Option<usize>,
    /// `sizeof(MDB_txbody)`.
    pub txbody_size: usize,
    /// Size of the `mt1` union: `MDB_txbody` padded to a cache line
    /// multiple.
    pub header_size: usize,
    /// Offset of `mti_readers[0]`.
    pub readers: usize,
}

/// The version 1 lock file of LMDB 0.9 on this platform.
pub static LOCK_V09: LockLayout = LockLayout {
    format: Format::V09,
    version: MDB_LOCK_VERSION,
    lock_format: MDB_LOCK_FORMAT,
    objects: platform::OBJECTS_V1,
    txnid: TXNINFO_TXNID,
    num_readers: TXNINFO_NUMREADERS,
    rlock: TXNINFO_RMUTEX,
    rlock_size: MNAME_LEN,
    wlock: Some(TXNINFO_WMUTEX),
    txbody_size: TXBODY_SIZE,
    header_size: TXNINFO_HEADER_SIZE,
    readers: READERS_OFFSET,
};

/// The version 2 lock file of LMDB 1.0 on this platform.
pub static LOCK_V10: LockLayout = LockLayout {
    format: Format::V10,
    version: LOCK_VERSION_V2,
    lock_format: lock_format_v2(LOCK_DESC_V2),
    objects: platform::OBJECTS_V2,
    txnid: V2_TXNID,
    num_readers: V2_NUMREADERS,
    rlock: V2_RLOCK,
    rlock_size: platform::RLOCK_V2.0,
    wlock: if V2_HAS_MT2 { Some(V2_HEADER) } else { None },
    txbody_size: V2_TXBODY,
    header_size: V2_HEADER,
    readers: V2_READERS,
};

impl LockLayout {
    /// The lock-file layout that goes with the data format `format`.
    #[inline(always)]
    pub const fn of(format: Format) -> &'static LockLayout {
        match format {
            Format::V09 => &LOCK_V09,
            Format::V10 => &LOCK_V10,
        }
    }

    /// Size of a lock file with room for `max_readers` slots (`rsize` in
    /// `mdb_env_setup_locks`).
    pub const fn file_size(&self, max_readers: u32) -> usize {
        self.readers + max_readers as usize * READER_SIZE
    }

    /// Number of reader slots in an existing lock file of `size` bytes, as
    /// `mdb_env_setup_locks` computes it for non-exclusive openers.
    pub const fn readers_in_file(&self, size: usize) -> usize {
        if size < self.readers {
            0
        } else {
            (size - self.readers) / READER_SIZE
        }
    }

    /// The offset of reader slot `i`.
    #[inline]
    pub const fn reader_offset(&self, i: usize) -> usize {
        self.readers + i * READER_SIZE
    }
}

// --- snapshots ---------------------------------------------------------------

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
    /// `mti_rmname` ([`LockObjects::SemaphoreNames`] only; empty otherwise).
    pub rmname: Vec<u8>,
    /// `mti_wmname` ([`LockObjects::SemaphoreNames`] only; empty otherwise).
    pub wmname: Vec<u8>,
    /// `mti_mutexid` ([`LockObjects::SemaphoreHash`] only): the hash the
    /// semaphore names are derived from.
    pub mutex_id: Option<u64>,
}

impl TxnInfoSnapshot {
    /// Decodes the header of a lock file of layout `layout` from a copy of
    /// its first bytes. `None` if `buf` is too short.
    pub fn read(buf: &[u8], layout: &LockLayout) -> Option<TxnInfoSnapshot> {
        let name = |off: Option<usize>| -> Option<Vec<u8>> {
            let Some(off) = off else {
                return Some(Vec::new());
            };
            let raw = buf.get(off..off.checked_add(layout.rlock_size)?)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            Some(raw[..end].to_vec())
        };
        let names = layout.objects == LockObjects::SemaphoreNames;
        Some(TxnInfoSnapshot {
            magic: get_u32(buf, TXNINFO_MAGIC)?,
            format: get_u32(buf, TXNINFO_FORMAT)?,
            txnid: get_u64(buf, layout.txnid)?,
            num_readers: get_u32(buf, layout.num_readers)?,
            rmname: name(names.then_some(layout.rlock))?,
            wmname: name(layout.wlock.filter(|_| names))?,
            mutex_id: match layout.objects {
                LockObjects::SemaphoreHash => Some(get_u64(buf, layout.rlock)?),
                _ => None,
            },
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
    /// Decodes slot `i` from a copy of a lock file of layout `layout`.
    /// `None` if `buf` is too short.
    pub fn read(buf: &[u8], layout: &LockLayout, i: usize) -> Option<ReaderSnapshot> {
        let off = i.checked_mul(READER_SIZE)?.checked_add(layout.readers)?;
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
        for l in [&LOCK_V09, &LOCK_V10] {
            assert_eq!(l.readers % 64, 0);
            assert_eq!(l.file_size(126), l.readers + 126 * 64);
            assert_eq!(l.readers_in_file(l.file_size(7)), 7);
            assert_eq!(l.readers_in_file(10), 0);
            assert_eq!(l.reader_offset(2), l.readers + 128);
            assert_eq!(l.header_size % 64, 0);
            assert!(l.txbody_size <= l.header_size);
            assert!(l.rlock + l.rlock_size <= l.txbody_size);
            assert!(l.num_readers + 4 <= l.txbody_size);
        }
    }

    #[test]
    fn selection_follows_the_data_format() {
        assert_eq!(LockLayout::of(Format::V09), &LOCK_V09);
        assert_eq!(LockLayout::of(Format::V10), &LOCK_V10);
        assert_eq!(LOCK_V09.format, Format::V09);
        assert_eq!(LOCK_V10.format, Format::V10);
        assert_eq!((LOCK_V09.version, LOCK_V10.version), (1, 2));
        assert_eq!(LOCK_V09.lock_format, 0x10001);
        assert_eq!(LOCK_V10.lock_format & 0xfff, 2);
    }

    /// The version 1 constants are the descriptor's values.
    #[test]
    fn v09_constants_agree() {
        let l = &LOCK_V09;
        assert_eq!(l.lock_format, LOCK_FORMAT);
        assert_eq!(l.txnid, TXNINFO_TXNID);
        assert_eq!(l.num_readers, TXNINFO_NUMREADERS);
        assert_eq!(l.rlock, TXNINFO_RMUTEX);
        assert_eq!(l.rlock_size, MNAME_LEN);
        assert_eq!(l.wlock, Some(TXNINFO_WMUTEX));
        assert_eq!(l.header_size, TXNINFO_HEADER_SIZE);
        assert_eq!(l.readers, READERS_OFFSET);
    }

    #[test]
    fn log2_mod_matches_c() {
        // LOG2_MOD(p2, 5) is log2(p2) % 5 for powers of two.
        for (p2, want) in [(1, 0), (2, 1), (4, 2), (8, 3), (16, 4), (32, 0), (64, 1)] {
            assert_eq!(log2_mod(p2, 5), want, "{p2}");
        }
    }

    fn inputs(txbody_size: usize, lock_type: usize) -> LockDescInputs {
        LockDescInputs {
            cacheline: 64,
            pid_size: 4,
            pthread_size: 8,
            txbody_size,
            lock_type,
            txnid_size: 8,
            reader_size: 64,
            pidlock: true,
        }
    }

    /// `MDB_LOCK_FORMAT` of version 2 for the three platforms, whatever the
    /// host: the values `layout_dump10` prints there (interop README).
    #[test]
    fn lock_format_v2_per_platform() {
        // macOS arm64: named semaphores, 8-byte aligned hash, MDB_txbody 32.
        let sem = lock_type_posix_sem(8);
        assert_eq!(sem, 5);
        assert_eq!(lock_desc(inputs(32, sem)), 165_232);
        assert_eq!(lock_format_v2(inputs(32, sem)), 0x2857_0002);
        // Linux aarch64 glibc: 48-byte mutex, MDB_txbody 72.
        let aarch64 = lock_type_posix_mutex(48, 8);
        assert_eq!(aarch64, 73);
        assert_eq!(lock_desc(inputs(72, aarch64)), 183_682);
        assert_eq!(lock_format_v2(inputs(72, aarch64)), 0x2cd8_2002);
        // Linux x86-64 glibc: 40-byte mutex, MDB_txbody 64.
        let x86_64 = lock_type_posix_mutex(40, 8);
        assert_eq!(x86_64, 63);
        assert_eq!(lock_format_v2(inputs(64, x86_64)), 0x2c2f_6002);
        // The BSDs, from the C declarations (no layout_dump10 has run
        // there): FreeBSD's 8-byte pthread_mutex_t, MDB_txbody 32; the
        // other BSDs' SysV semaphores (MDB_LOCK_TYPE 8), MDB_txbody 32.
        let freebsd = lock_type_posix_mutex(8, 8);
        assert_eq!(freebsd, 23);
        assert_eq!(lock_format_v2(inputs(32, freebsd)), 0x2986_c002);
        assert_eq!(lock_format_v2(inputs(32, 8)), 0x2889_a002);
        // The CACHELINE term is 0 only for 64-byte cache lines.
        let mut other = inputs(32, sem);
        other.cacheline = 128;
        assert_ne!(lock_desc(other), lock_desc(inputs(32, sem)));
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn macos_layout() {
        // Version 1: magic, format, rmname[32], txnid@40, numreaders@48 -> 52 -> 56.
        let l = &LOCK_V09;
        assert_eq!(l.objects, LockObjects::SemaphoreNames);
        assert_eq!((l.rlock, l.rlock_size), (8, 32));
        assert_eq!((l.txnid, l.num_readers), (40, 48));
        assert_eq!(l.txbody_size, 56);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
        // Version 2: magic, format, txnid@8, numreaders@16, mutexid@24 -> 32;
        // no mt2.
        let l = &LOCK_V10;
        assert_eq!(l.objects, LockObjects::SemaphoreHash);
        assert_eq!(l.lock_format, 0x2857_0002);
        assert_eq!((l.txnid, l.num_readers), (8, 16));
        assert_eq!((l.rlock, l.rlock_size), (24, 8));
        assert_eq!(l.txbody_size, 32);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, None);
        assert_eq!(l.readers, 64);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    #[test]
    fn linux_x86_64_glibc_layout() {
        let l = &LOCK_V09;
        assert_eq!(MNAME_LEN, 40);
        assert_eq!((l.rlock, l.rlock_size), (8, 40));
        assert_eq!((l.txnid, l.num_readers), (48, 56));
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
        let l = &LOCK_V10;
        assert_eq!(l.objects, LockObjects::PthreadMutexes);
        assert_eq!(l.lock_format, 0x2c2f_6002);
        assert_eq!((l.txnid, l.num_readers), (8, 16));
        assert_eq!((l.rlock, l.rlock_size), (24, 40));
        assert_eq!(l.txbody_size, 64);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
    }

    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    #[test]
    fn linux_aarch64_glibc_layout() {
        let l = &LOCK_V09;
        assert_eq!(MNAME_LEN, 48);
        assert_eq!((l.rlock, l.rlock_size), (8, 48));
        assert_eq!((l.txnid, l.num_readers), (56, 64));
        assert_eq!(l.header_size, 128);
        assert_eq!(l.wlock, Some(128));
        assert_eq!(l.readers, 192);
        let l = &LOCK_V10;
        assert_eq!(l.objects, LockObjects::PthreadMutexes);
        assert_eq!(l.lock_format, 0x2cd8_2002);
        assert_eq!((l.txnid, l.num_readers), (8, 16));
        assert_eq!((l.rlock, l.rlock_size), (24, 48));
        assert_eq!(l.txbody_size, 72);
        assert_eq!(l.header_size, 128);
        assert_eq!(l.wlock, Some(128));
        assert_eq!(l.readers, 192);
    }

    /// FreeBSD: Linux's layout with an 8-byte `pthread_mutex_t` (computed
    /// from the C declarations, not measured).
    #[cfg(all(target_os = "freebsd", target_pointer_width = "64"))]
    #[test]
    fn freebsd_layout() {
        let l = &LOCK_V09;
        assert_eq!(l.objects, LockObjects::PthreadMutexes);
        assert_eq!(MNAME_LEN, 8);
        assert_eq!((l.rlock, l.rlock_size), (8, 8));
        assert_eq!((l.txnid, l.num_readers), (16, 24));
        assert_eq!(l.txbody_size, 32);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
        let l = &LOCK_V10;
        assert_eq!(l.objects, LockObjects::PthreadMutexes);
        assert_eq!(l.lock_format, 0x2986_c002);
        assert_eq!((l.txnid, l.num_readers), (8, 16));
        assert_eq!((l.rlock, l.rlock_size), (24, 8));
        assert_eq!(l.txbody_size, 32);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
    }

    /// OpenBSD, NetBSD, DragonFly: version 1 as on macOS, version 2 with
    /// SysV semaphores (computed from the C declarations, not measured).
    #[cfg(any(target_os = "openbsd", target_os = "netbsd", target_os = "dragonfly"))]
    #[test]
    fn bsd_semaphore_layout() {
        let l = &LOCK_V09;
        assert_eq!(l.objects, LockObjects::SemaphoreNames);
        assert_eq!((l.rlock, l.rlock_size), (8, 32));
        assert_eq!((l.txnid, l.num_readers), (40, 48));
        assert_eq!(l.txbody_size, 56);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
        // Version 2: magic, format, txnid@8, numreaders@16, semid@20,
        // rlocked@24 -> 28 -> 32; mt2 holds wlocked.
        let l = &LOCK_V10;
        assert_eq!(l.objects, LockObjects::SysVSemaphores);
        assert_eq!(l.lock_format, 0x2889_a002);
        assert_eq!((l.txnid, l.num_readers), (8, 16));
        assert_eq!((l.rlock, l.rlock_size), (20, 8));
        assert_eq!(l.txbody_size, 32);
        assert_eq!(l.header_size, 64);
        assert_eq!(l.wlock, Some(64));
        assert_eq!(l.readers, 128);
    }

    /// Who removes the shared lock objects when the last user closes.
    #[test]
    fn removed_by_last_user() {
        assert!(!LockObjects::PthreadMutexes.removed_by_last_user());
        assert!(LockObjects::SemaphoreNames.removed_by_last_user());
        assert!(LockObjects::SemaphoreHash.removed_by_last_user());
        assert!(LockObjects::SysVSemaphores.removed_by_last_user());
    }

    #[test]
    fn snapshots() {
        for l in [&LOCK_V09, &LOCK_V10] {
            let mut buf = vec![0u8; l.file_size(2)];
            buf[0..4].copy_from_slice(&0xBEEF_C0DEu32.to_ne_bytes());
            buf[4..8].copy_from_slice(&l.lock_format.to_ne_bytes());
            buf[l.txnid..l.txnid + 8].copy_from_slice(&42u64.to_ne_bytes());
            buf[l.num_readers..l.num_readers + 4].copy_from_slice(&2u32.to_ne_bytes());
            if l.objects == LockObjects::SemaphoreHash {
                buf[l.rlock..l.rlock + 8].copy_from_slice(&0x1234u64.to_ne_bytes());
            }
            if l.objects == LockObjects::SemaphoreNames {
                buf[l.rlock..l.rlock + 4].copy_from_slice(b"/MDB");
            }
            let off = l.reader_offset(1);
            buf[off..off + 8].copy_from_slice(&7u64.to_ne_bytes());
            buf[off + 8..off + 12].copy_from_slice(&99i32.to_ne_bytes());
            buf[off + 16..off + 24].copy_from_slice(&5u64.to_ne_bytes());

            let h = TxnInfoSnapshot::read(&buf, l).unwrap();
            assert_eq!(h.magic, 0xBEEF_C0DE);
            assert_eq!(h.format, l.lock_format);
            assert_eq!(h.txnid, 42);
            assert_eq!(h.num_readers, 2);
            assert_eq!(
                h.mutex_id,
                (l.objects == LockObjects::SemaphoreHash).then_some(0x1234)
            );
            let names = l.objects == LockObjects::SemaphoreNames;
            assert_eq!(h.rmname, if names { &b"/MDB"[..] } else { &[] });
            assert!(h.wmname.is_empty());
            let r = ReaderSnapshot::read(&buf, l, 1).unwrap();
            assert_eq!((r.txnid, r.pid, r.tid), (7, 99, 5));
            assert_eq!(ReaderSnapshot::read(&buf, l, 2), None);
            assert_eq!(ReaderSnapshot::read(&buf, l, usize::MAX), None);
            assert_eq!(TxnInfoSnapshot::read(&buf[..l.num_readers], l), None);
        }
    }
}
