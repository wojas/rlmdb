//! Windows system calls used by the environment: memory maps, byte-range
//! locks on the lock file, named mutexes, positional I/O and syncing.
//!
//! This is the Windows counterpart of `unix.rs` and `shared_lock.rs`, with
//! the same public surface, written after the `_WIN32` paths of mdb.c
//! (LMDB 0.9.36, `tests/lmdb-c/0.9`) and, for the version 2 lock file, of
//! LMDB 1.0 (`tests/lmdb-c/1.0/mdb.c`, "mdb10.c" below). Both 64-bit
//! toolchains, `x86_64-pc-windows-msvc` and `x86_64-pc-windows-gnu`
//! (MinGW), lay out C's structures alike, so one set of decisions covers
//! both. Every Windows-specific decision is listed here.
//!
//! # The data file map (`mdb_env_map`)
//!
//! [`map_data`] maps the data file as the C release of its format does on
//! Windows; both views are read-only (`PAGE_READONLY`).
//!
//! * **LMDB 0.9** (mdb.c line 4110): `CreateFileMappingW` and
//!   `MapViewOfFile(FILE_MAP_READ)` of the whole map size; the mapping
//!   handle is closed at once (the view keeps the section alive). Windows
//!   maps at most the length of the file, so C 0.9 sets the data file of a
//!   writable environment to the map size before mapping it
//!   (`SetEndOfFile`): on Windows a 0.9 data file is as long as its map, not
//!   as its used pages. [`DataMap::map_readonly`] does the same when the
//!   handle is writable. This matters for interoperation, not only for
//!   sizes: a C 0.9 process with a read-only environment maps the file as
//!   it is and reads pages past its view unchecked, so the file must
//!   already have them.
//! * **LMDB 1.0** (mdb10.c line 5076): a section created with `SEC_RESERVE`
//!   over the file as it is (`NtCreateSection`) and a view that reserves the
//!   map size (`NtMapViewOfSection` with `MEM_RESERVE`). The file grows with
//!   the writes ([`DataMap::map_reserved`]). Writing past the end of the
//!   file does not make the pages readable in such a view: all sections of
//!   a file share one segment, which covers the first MiB of the file and
//!   past that only what was committed or extended (measured on Windows
//!   10; an access beyond faults). C 1.0's `mdb_page_alloc` therefore
//!   commits every page a write transaction takes from the end of the map
//!   in its view (`VirtualAlloc(MEM_COMMIT)`, mdb10.c line 2876), which
//!   extends the file by those pages and the shared segment, so that the
//!   views of all processes show them before the commit that writes them
//!   is published. rlmdb does the same ([`DataMap::commit_pages`]): a
//!   writer that did not would leave C 1.0 processes faulting on its
//!   pages. As a second line of defence the map keeps its section and
//!   extends it to the file's size (`NtExtendSection`, never past it)
//!   before it hands out a page beyond the size it last knew. C 1.0 closes
//!   the section after mapping. (Unit tests
//!   `reserved_map_shows_the_file_growing`,
//!   `reserved_map_follows_the_file_past_the_first_mib` and
//!   `committed_pages_show_in_other_views`.)
//! * A read-only handle maps the file as it is (C maps with size 0, "whatever
//!   exists", for `MDB_RDONLY`); Windows refuses a reserved view of a
//!   read-only section. Pages added later are refused by [`DataMap::slice`]
//!   (an error, not a fault), and when a snapshot needs them the
//!   environment maps the file again (`Environment::renew_short_map`,
//!   which on Unix never applies: there the map is always the map size).
//! * A 0.9 file is only ever grown. C 0.9 also shrinks it when the map size
//!   shrinks, which fails (`ERROR_USER_MAPPED_FILE`) while another process,
//!   or here a read transaction of this one holding an older map, still
//!   maps more; the pages past the new map size are unused either way.
//! * `MDB_NORDAHEAD` has no effect (C ignores it on Windows too).
//!
//! # The lock file
//!
//! * Mapped read/write with `CreateFileMappingW(PAGE_READWRITE)` over the
//!   whole file and `MapViewOfFile(FILE_MAP_WRITE)` (mdb.c line 4937).
//! * Exclusive and shared use is a lock on byte 0, as on Unix, with
//!   `LockFileEx`. Windows locks belong to a *handle*, not to the process,
//!   and are mandatory for `ReadFile`/`WriteFile` through other handles
//!   (not for mapped views, which is all LMDB uses). `mdb_env_excl_lock`:
//!   an exclusive lock without waiting (`LockFile`), else a shared one,
//!   waiting. `mdb_env_share_locks`: a shared lock through the same handle,
//!   which may overlap our own exclusive one, then `UnlockFile`, which
//!   removes the exclusive lock first ([`downgrade_lock`]). Before the lock
//!   file is closed its lock is released explicitly ([`unlock_lock_file`]):
//!   "Windows would have unlocked it after closing anyway, but not
//!   necessarily at once" (`mdb_env_close0`).
//! * Readers take no pid locks (`MDB_PIDLOCK` is 0, so version 1's
//!   `mti_format` is `0x1`). `mdb_reader_pid(Pidcheck)` asks whether the
//!   process exists ([`pid_alive`]): a pid is dead when `OpenProcess` fails
//!   with `ERROR_INVALID_PARAMETER` or when the process has exited. C opens
//!   the process with `PROCESS_QUERY_LIMITED_INFORMATION` alone, so its
//!   `WaitForSingleObject` always fails and a process that exited while
//!   someone still holds a handle to it counts as alive; this opens it with
//!   `SYNCHRONIZE` too (`KNOWN_C_BUGS.md` #38).
//! * The reader slot's `mr_tid` is a `DWORD` (`GetCurrentThreadId`) at
//!   offset 12, `mr_pid` the `int` from `GetCurrentProcessId`
//!   (`format::lockfile::READER_TID`).
//!
//! # The two shared locks (`mdb_env_setup_locks`, mdb.c line 4955)
//!
//! * Named kernel mutexes, created by the exclusive opener with
//!   `CreateMutexA` and a NULL DACL (C's `mdb_all_sa`: any account may open
//!   them), opened by everyone else with `OpenMutexA(SYNCHRONIZE)`.
//! * Their names come from the identity of the lock file,
//!   `struct { DWORD volume; DWORD nhigh; DWORD nlow; }` from
//!   `GetFileInformationByHandle` (12 bytes, no padding, so unlike macOS's
//!   C 0.9 names they are reproducible). Version 1: `"Global\MDBr"` and
//!   `"Global\MDBw"` followed by `mdb_hash_enc` of it, where `mdb_pack85`
//!   takes an `unsigned long`, which is 32 bits on Windows, so each half of
//!   the 64-bit hash gives five digits of its own 32 bits; the names are
//!   stored in the lock file (`mti_rmname`, `mti_wmname`) and read by the
//!   other openers. Version 2: the 64-bit hash is stored (`mti_mutexid`),
//!   and every opener names the mutexes `"Global\MDB"`, `'r'` or `'w'`,
//!   then up to ten base-85 digits of the stored hash (`MUTEXNAME`).
//! * A mutex whose owner died is *abandoned*: the next `WaitForSingleObject`
//!   returns `WAIT_ABANDONED` and owns it (`MDB_OWNERDEAD`). That is
//!   [`LockStatus::OwnerDied`], so the engine repairs state as after a dead
//!   owner of a robust pthread mutex; marking it consistent is a no-op
//!   (`mdb_mutex_consistent` is 0 on Windows).
//! * Windows mutexes belong to the locking *thread* and are recursive. The
//!   engine locks each on one thread and unlocks it there (a write
//!   transaction is `!Send`), and never twice on a thread: a second write
//!   transaction of the process waits for the environment's in-process
//!   writer state first.
//! * The kernel destroys a named mutex with its last handle, so there is
//!   nothing to unlink when an environment closes.
//!
//! # Files and I/O
//!
//! * Environment files are opened with `FILE_SHARE_READ | FILE_SHARE_WRITE`
//!   and no `FILE_SHARE_DELETE`, as `mdb_fopen` does: the files of an open
//!   environment cannot be deleted or renamed. The Unix permission `mode`
//!   is ignored, as in C.
//! * The meta pages are written through a second handle opened with
//!   `FILE_FLAG_WRITE_THROUGH` (C's `MDB_O_META`, where Unix uses
//!   `O_DSYNC`).
//! * A copy target (`mdb_env_copy2`) is created with `CREATE_NEW`, no
//!   sharing and `FILE_FLAG_WRITE_THROUGH` (`MDB_O_COPY`), and with
//!   `FILE_FLAG_NO_BUFFERING` when the page size is at least the OS page
//!   size, the condition under which Unix asks for `O_DIRECT`. C 0.9 always
//!   asks for it, which LMDB 1.0's pages below the sector size would break.
//! * `pread`/`pwrite` are `ReadFile`/`WriteFile` with an offset in an
//!   `OVERLAPPED` (`FileExt::seek_read`/`seek_write`; they also move the
//!   handle's file pointer, which nothing here relies on). `pwritev` writes
//!   its buffers one after the other: C's `mdb_page_flush` writes page by
//!   page on Windows too. `fdatasync` and `fsync` are `FlushFileBuffers`.
//! * A read-only file system is `ERROR_WRITE_PROTECT` (C's
//!   `MDB_ERRCODE_ROFS`).
//! * The file identity (the registry's "device and inode") is the volume
//!   serial number and the 64-bit file index.
//! * `os_page_size` is `GetSystemInfo`'s `dwPageSize` (4 KiB on x86-64), the
//!   default page size of new environments, as `GET_PAGESIZE`.
//! * `strerror` follows `mdb_strerror` on Windows: the C runtime's
//!   `strerror` for the `errno` values LMDB itself returns (`ENOENT`, `EIO`,
//!   `ENOMEM`, `EACCES`, `EBUSY`, `EINVAL`, `ENOSPC`), `FormatMessageA`'s
//!   system text (with its trailing `"\r\n"`) for every other code, which
//!   on Windows is a `GetLastError` value.
//!
//! # Not ported
//!
//! C's thread-local reader slots and their TLS callback (`mdb_tls_callback`)
//! do not exist here: rlmdb always has `MDB_NOTLS` semantics.
//!
//! The atomic and volatile accessors of [`DataMap`] and [`LockMap`] repeat
//! those of `unix.rs`; only how the maps are made and released differs.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, IntoRawHandle};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use windows_sys::Wdk::Foundation::NtClose;
use windows_sys::Wdk::Storage::FileSystem::NtCreateSection;
use windows_sys::Wdk::System::Memory::{NtMapViewOfSection, ViewUnmap};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_WRITE_PROTECT,
    HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Foundation::{RtlNtStatusToDosError, STATUS_ACCESS_DENIED};
use windows_sys::Win32::Security::{
    InitializeSecurityDescriptor, SetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, LockFileEx, UnlockFile, BY_HANDLE_FILE_INFORMATION,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE,
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, SYNCHRONIZE,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FormatMessageA, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY, PAGE_READWRITE,
};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, MEM_COMMIT, MEM_RESERVE, SECTION_EXTEND_SIZE, SECTION_MAP_READ, SEC_RESERVE,
};

// `NtExtendSection` (ntdll), which windows-sys does not declare: grows a
// section to `new_size` (in: the size wanted; out: the size it has).
#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtExtendSection(section: HANDLE, new_size: *mut i64) -> i32;
}
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{
    CreateMutexA, GetCurrentProcess, GetCurrentThreadId, OpenProcess, ReleaseMutex,
    WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::System::WindowsProgramming::OpenMutexA;
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::format::layout::Format;
use crate::format::lockfile::{LockLayout, LockObjects};

/// The OS memory page size (`GetSystemInfo().dwPageSize`, 4096 on x86-64).
pub fn os_page_size() -> usize {
    let mut si = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo only fills the struct we pass; it cannot fail.
    unsafe { GetSystemInfo(&mut si) };
    if si.dwPageSize > 0 {
        si.dwPageSize as usize
    } else {
        4096
    }
}

/// The calling process id (`GetCurrentProcessId()`), as stored in `mr_pid`.
pub fn getpid() -> i32 {
    std::process::id() as i32
}

/// The calling thread's id (`GetCurrentThreadId()`), as stored in the
/// `DWORD` `mr_tid` (only used for diagnostics such as `mdb_reader_list`).
pub fn thread_id() -> u64 {
    // SAFETY: GetCurrentThreadId has no preconditions and always succeeds.
    u64::from(unsafe { GetCurrentThreadId() })
}

/// `mdb_strerror` of C on Windows for a code that is not an LMDB error: the
/// C runtime's text for the `errno` values LMDB uses, the system's message
/// for everything else.
pub fn strerror(code: i32) -> String {
    match code {
        libc::ENOENT
        | libc::EIO
        | libc::ENOMEM
        | libc::EACCES
        | libc::EBUSY
        | libc::EINVAL
        | libc::ENOSPC => crt_strerror(code),
        _ => format_message(code as u32),
    }
}

/// The C runtime's `strerror(code)`.
fn crt_strerror(code: i32) -> String {
    // SAFETY: the C runtime's strerror returns a NUL-terminated string in a
    // buffer of the calling thread (or a static table), valid until its
    // next call on this thread; it is copied at once.
    let p = unsafe { libc::strerror(code) };
    if p.is_null() {
        return format!("Unknown error: {code}");
    }
    // SAFETY: p is a non-null NUL-terminated string (see above).
    let s = unsafe { CStr::from_ptr(p) };
    s.to_string_lossy().into_owned()
}

/// `FormatMessageA(FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS)`
/// into a 1024-byte buffer, as `mdb_strerror` does; empty if there is no
/// message.
fn format_message(code: u32) -> String {
    let mut buf = [0u8; 1024];
    // SAFETY: buf is writable for its stated length; with FROM_SYSTEM and
    // IGNORE_INSERTS the function reads neither a source nor arguments.
    let n = unsafe {
        FormatMessageA(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            ptr::null(),
        )
    };
    let n = (n as usize).min(buf.len());
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn handle(file: &File) -> HANDLE {
    file.as_raw_handle()
}

// ---------------------------------------------------------------------------
// Byte-range locks on the lock file
// ---------------------------------------------------------------------------

/// Kind of a byte-range lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    /// A shared lock (`LockFileEx` without `LOCKFILE_EXCLUSIVE_LOCK`).
    Shared,
    /// An exclusive lock.
    Exclusive,
}

/// An `OVERLAPPED` that names `offset`, for the locking functions.
fn overlapped_at(offset: u64) -> OVERLAPPED {
    let mut ov = OVERLAPPED::default();
    ov.Anonymous.Anonymous.Offset = offset as u32;
    ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    ov
}

/// Locks the byte at `offset` with `LockFileEx`, waiting for it if `wait`
/// (else failing with `ERROR_LOCK_VIOLATION` if another handle holds a
/// conflicting lock).
///
/// Windows locks belong to the handle: another handle of the same file in
/// this process conflicts like another process would. A handle may add a
/// shared lock to its own exclusive one, not the reverse.
pub fn lock_byte(file: &File, offset: u64, kind: LockKind, wait: bool) -> io::Result<()> {
    let mut flags = 0;
    if kind == LockKind::Exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK;
    }
    if !wait {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    let mut ov = overlapped_at(offset);
    // SAFETY: the handle is valid for the lifetime of `file`; `ov` lives
    // across the call. The handle is synchronous (not opened with
    // FILE_FLAG_OVERLAPPED), so LockFileEx returns only once it is done.
    if unsafe { LockFileEx(handle(file), flags, 0, 1, 0, &mut ov) } != 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `UnlockFile` of the byte at `offset`: removes one lock of this handle
/// there, the exclusive one first if it holds both.
fn unlock_byte(file: &File, offset: u64) -> io::Result<()> {
    // SAFETY: the handle is valid for the lifetime of `file`.
    let ok = unsafe { UnlockFile(handle(file), offset as u32, (offset >> 32) as u32, 1, 0) };
    if ok != 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `mdb_env_share_locks`: turns our exclusive lock on byte 0 of the lock
/// file into a shared one. First a shared lock, which may overlap our own
/// exclusive lock; the unlock then removes the exclusive one (C ignores
/// its result too).
pub fn downgrade_lock(file: &File) -> io::Result<()> {
    lock_byte(file, 0, LockKind::Shared, true)?;
    let _ = unlock_byte(file, 0);
    Ok(())
}

/// Releases our lock on byte 0 of the lock file before it is closed, as
/// `mdb_env_close0` does: Windows would release it with the handle, "but
/// not necessarily at once".
pub fn unlock_lock_file(file: &File) {
    let _ = unlock_byte(file, 0);
}

/// `mdb_reader_pid(Pidset)`: nothing on Windows (`MDB_PIDLOCK` is 0).
pub fn set_pid_lock(_file: &File, _pid: i32) -> io::Result<()> {
    Ok(())
}

/// `mdb_reader_pid(Pidcheck)`: `false` only if process `pid` is certainly
/// gone: `OpenProcess` says there is no such process
/// (`ERROR_INVALID_PARAMETER`), or the process has exited. Any other
/// failure (access denied, …) counts as alive. See the module
/// documentation for the `SYNCHRONIZE` right C does not ask for.
pub fn pid_alive(_file: &File, pid: i32) -> bool {
    let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: OpenProcess has no memory preconditions.
    let h = unsafe { OpenProcess(access, 0, pid as u32) };
    if h.is_null() {
        // SAFETY: GetLastError has no preconditions.
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    // SAFETY: h is a process handle we own, with SYNCHRONIZE access; it is
    // closed exactly once.
    let rc = unsafe {
        let rc = WaitForSingleObject(h, 0);
        CloseHandle(h);
        rc
    };
    rc != WAIT_OBJECT_0
}

// ---------------------------------------------------------------------------
// Opening files
// ---------------------------------------------------------------------------

/// The handle type `copy_to_fd` takes (`mdb_filehandle_t` is a `HANDLE`).
pub type BorrowedFile<'a> = std::os::windows::io::BorrowedHandle<'a>;

/// `mdb_fopen`'s sharing for the files of an environment (data file and
/// lock file): `FILE_SHARE_READ | FILE_SHARE_WRITE`. `mode`, the Unix
/// permissions, is ignored as in C.
pub fn env_file_options(opts: &mut OpenOptions, _mode: u32) -> &mut OpenOptions {
    opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
}

/// `MDB_O_META`: the handle for meta-page writes, which reach the disk
/// before the write returns (`FILE_FLAG_WRITE_THROUGH`).
pub fn sync_write_options(opts: &mut OpenOptions) -> &mut OpenOptions {
    opts.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
}

/// `MDB_O_COPY`: creates the target of `mdb_env_copy2`, which must not
/// exist, unshared and written through; `nocache` also bypasses the cache
/// (`FILE_FLAG_NO_BUFFERING`: every write must then be a multiple of the
/// sector size from a sector-aligned buffer, which page-aligned buffers of
/// whole pages of at least the OS page size are).
pub fn open_copy_target(path: &Path, nocache: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_WRITE_THROUGH;
    if nocache {
        flags |= FILE_FLAG_NO_BUFFERING;
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .custom_flags(flags)
        .open(path)
}

/// `true` if `e`, from opening `path`, says the file does not exist, as
/// Unix's `ENOENT` does: `ERROR_FILE_NOT_FOUND`, or `ERROR_PATH_NOT_FOUND`
/// for a directory that does not exist. Windows says the latter also when a
/// file stands where a directory should be, which Unix reports as
/// `ENOTDIR`, an error: that is not "missing".
pub fn is_missing(e: &io::Error, path: &Path) -> bool {
    match e.raw_os_error() {
        Some(2) => true,
        Some(3) => !path.ancestors().skip(1).any(|p| p.is_file()),
        _ => e.kind() == io::ErrorKind::NotFound,
    }
}

/// `true` if `e` says the file system is read-only (`ERROR_WRITE_PROTECT`).
pub fn is_read_only_fs(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_WRITE_PROTECT as i32)
}

fn file_information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid handle; info is a writable struct of the right type.
    if unsafe { GetFileInformationByHandle(handle(file), &mut info) } != 0 {
        Ok(info)
    } else {
        Err(last_error())
    }
}

/// The identity of an open file, the registry's "device and inode": the
/// volume serial number and the 64-bit file index.
pub fn file_id(file: &File) -> io::Result<(u64, u64)> {
    let info = file_information(file)?;
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((u64::from(info.dwVolumeSerialNumber), index))
}

// ---------------------------------------------------------------------------
// Positional I/O and syncing
// ---------------------------------------------------------------------------

/// `pread` the whole of `buf` at `offset`, returning how many bytes were
/// read (fewer only at end of file).
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match file.seek_read(&mut buf[done..], offset + done as u64) {
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
    let mut done = 0;
    while done < buf.len() {
        match file.seek_write(&buf[done..], offset + done as u64) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `pwritev`: write `bufs` back to back starting at `offset`. Windows has
/// gather writes only for unbuffered handles, so each buffer is one write,
/// as in C's `mdb_page_flush` on Windows. Returns the number of bytes
/// written.
#[allow(dead_code)] // used by the write path (stage 2)
pub fn pwritev(file: &File, bufs: &[&[u8]], offset: u64) -> io::Result<usize> {
    let mut pos = offset;
    let mut total = 0;
    for b in bufs {
        write_all_at(file, b, pos)?;
        pos += b.len() as u64;
        total += b.len();
    }
    Ok(total)
}

/// `MDB_FDATASYNC`: `FlushFileBuffers`.
pub fn fdatasync(file: &File) -> io::Result<()> {
    file.sync_data()
}

/// `fsync`: `FlushFileBuffers`.
#[allow(dead_code)] // used by the write path (stage 2)
pub fn fsync(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Closes `file`, reporting an error from `CloseHandle` (which `File`'s
/// `Drop` ignores; `mdb_env_copy2` reports it).
pub fn close(file: File) -> io::Result<()> {
    let h = file.into_raw_handle();
    // SAFETY: we own the handle (taken from the File) and close it once.
    if unsafe { CloseHandle(h) } != 0 {
        Ok(())
    } else {
        Err(last_error())
    }
}

/// `SetEndOfFile` at `len`.
pub fn ftruncate(file: &File, len: u64) -> io::Result<()> {
    file.set_len(len)
}

/// The current size of the file (`GetFileSizeEx`).
pub fn file_size(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

// ---------------------------------------------------------------------------
// The data file map
// ---------------------------------------------------------------------------

/// Maps `len` bytes of `file` at offset 0 with protection `protect` and
/// view access `access`. The file must be at least `len` bytes long.
fn map_view(file: &File, len: usize, protect: u32, access: u32) -> io::Result<*mut u8> {
    let size = len as u64;
    // SAFETY: a valid file handle; no security attributes, no name. The
    // section is as long as `len`, which the caller checked the file has.
    let mapping = unsafe {
        CreateFileMappingW(
            handle(file),
            ptr::null(),
            protect,
            (size >> 32) as u32,
            size as u32,
            ptr::null(),
        )
    };
    if mapping.is_null() {
        return Err(last_error());
    }
    // SAFETY: `mapping` is the section just created; the view keeps it
    // alive after we close our handle to it, as C does.
    let view = unsafe { MapViewOfFile(mapping, access, 0, 0, len) };
    let err = last_error();
    // SAFETY: we own `mapping` and close it exactly once.
    unsafe { CloseHandle(mapping) };
    if view.Value.is_null() {
        return Err(err);
    }
    Ok(view.Value as *mut u8)
}

fn unmap_view(ptr: *mut u8) {
    // SAFETY: ptr is the base of a view created by map_view that has not
    // been unmapped; the caller guarantees nothing borrows it any more.
    unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
            Value: ptr as *mut core::ffi::c_void,
        });
    }
}

/// A read-only map of the data file (`me_map`).
///
/// Why handing out `&[u8]` into this map is sound is the same as for the
/// Unix map (see the documentation of `DataMap` in `unix.rs`): LMDB's MVCC
/// invariant keeps the pages of a pinned snapshot unchanged, the meta
/// pages are only read with volatile copies, and every access is checked
/// against the map and the file size. A page past the end of the file
/// would raise an access violation here instead of `SIGBUS`.
///
/// The map is as long as the map size where the file handle is writable
/// (for 0.9 the file is extended to it first, as C 0.9 does; for 1.0 the
/// view reserves it and the file grows into it), else as long as the file
/// (see the module documentation).
pub struct DataMap {
    ptr: *mut u8,
    len: usize,
    /// End of the meta pages: `slice` refuses offsets below it.
    meta_end: usize,
    file: File,
    /// Last known size of the data file, to avoid faults past the end; with
    /// a `section`, also the size the view is known to show.
    known_file_size: AtomicU64,
    /// The section of a reserved view (LMDB 1.0, writable), extended as the
    /// file grows; null for the other maps.
    section: HANDLE,
}

// SAFETY: the map is read-only and its view never moves after creation
// (extending a reserved view's section, which the kernel serialises, only
// makes more of it readable); concurrent reads from several threads are
// fine, and the section handle may be used from any thread.
unsafe impl Send for DataMap {}
// SAFETY: see above; all methods take `&self` and only read.
unsafe impl Sync for DataMap {}

/// Maps the data file of an environment of `format` read-only, as the C
/// release of that format does on Windows (see the module documentation):
/// 0.9 extends the file to the map size first
/// ([`DataMap::map_readonly`]), 1.0 lets it grow with the writes
/// ([`DataMap::map_reserved`]).
pub fn map_data(file: File, len: usize, random: bool, format: Format) -> io::Result<DataMap> {
    match format {
        Format::V09 => DataMap::map_readonly(file, len, random),
        Format::V10 => DataMap::map_reserved(file, len),
    }
}

/// `false`: Windows keeps a file's views and its `ReadFile`/`WriteFile`
/// data coherent (one cache manager), so no map needs invalidating after
/// a write (C's `MDB_Invalidate` is a no-op there).
pub const MAP_NEEDS_INVALIDATE: bool = false;

/// The Win32 error of an `NTSTATUS` (C 1.0's `mdb_nt2win32`).
fn nt_error(status: i32) -> io::Error {
    // SAFETY: RtlNtStatusToDosError only translates a number.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

impl DataMap {
    /// `mdb_env_map` of LMDB 1.0 (mdb10.c line 5076): a section created
    /// with `SEC_RESERVE` over the file as it is, and a view of it that
    /// reserves `len` bytes of address space (`MEM_RESERVE`). Pages written
    /// past the end of the file become visible in the view as the file
    /// grows, so the file is never extended to the map size. Through a
    /// read-only handle (C: `MDB_RDONLY`) the view covers the file as it is.
    pub fn map_reserved(file: File, len: usize) -> io::Result<DataMap> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let size = file_size(&file)?;
        let create = |protect: u32, access: u32| -> Result<HANDLE, i32> {
            let mut section: HANDLE = ptr::null_mut();
            // SAFETY: `section` is a writable out-pointer; no object
            // attributes and no maximum size (the file's size); the file
            // handle is valid for the lifetime of `file`.
            let st = unsafe {
                NtCreateSection(
                    &mut section,
                    access,
                    ptr::null(),
                    ptr::null(),
                    protect,
                    SEC_RESERVE,
                    handle(&file),
                )
            };
            if st < 0 {
                Err(st)
            } else {
                Ok(section)
            }
        };
        // C asks for a writable section for a writable environment (the
        // view itself is read-only), a read-only one for MDB_RDONLY.
        let (section, writable) =
            match create(PAGE_READWRITE, SECTION_MAP_READ | SECTION_EXTEND_SIZE) {
                Ok(s) => (s, true),
                Err(STATUS_ACCESS_DENIED) => (
                    create(PAGE_READONLY, SECTION_MAP_READ).map_err(nt_error)?,
                    false,
                ),
                Err(st) => return Err(nt_error(st)),
            };
        let map = |reserve: bool| -> Result<(*mut u8, usize), i32> {
            let mut base: *mut core::ffi::c_void = ptr::null_mut();
            let mut view = if reserve { len } else { 0 };
            // SAFETY: `section` is the section just created and still
            // open; `base` and `view` are writable out-parameters (no
            // address asked for; the view size, 0 for all of the section).
            let st = unsafe {
                NtMapViewOfSection(
                    section,
                    GetCurrentProcess(),
                    &mut base,
                    0,
                    0,
                    ptr::null_mut(),
                    &mut view,
                    ViewUnmap,
                    if reserve { MEM_RESERVE } else { 0 },
                    PAGE_READONLY,
                )
            };
            if st < 0 {
                Err(st)
            } else {
                Ok((base as *mut u8, if reserve { len.min(view) } else { view }))
            }
        };
        // A writable environment reserves the map size, as C 1.0 does. A
        // read-only one does too where Windows allows it, so that it sees
        // the pages other processes add (C 1.0 maps what exists then, and
        // so does this if the reservation is refused).
        let mapped = map(true).or_else(|st| if writable { Err(st) } else { map(false) });
        // A writable map keeps its section to extend it (`refresh_size`);
        // otherwise the view keeps the section alive, as in C.
        let keep = writable && mapped.is_ok();
        if !keep {
            // SAFETY: we own `section` and close it once.
            unsafe { NtClose(section) };
        }
        let (ptr, len) = mapped.map_err(nt_error)?;
        Ok(DataMap {
            ptr,
            len,
            meta_end: 0,
            file,
            known_file_size: AtomicU64::new(size),
            section: if keep { section } else { ptr::null_mut() },
        })
    }

    /// Maps `len` bytes of `file` read-only (`mdb_env_map`). If the file is
    /// shorter and `file` is writable, it is extended to `len` first;
    /// through a read-only handle the map covers the file as it is.
    /// `random` (`MDB_NORDAHEAD`) has no effect on Windows. `file` is a
    /// duplicate of the data file handle, kept to refresh the file size.
    pub fn map_readonly(file: File, len: usize, random: bool) -> io::Result<DataMap> {
        let _ = random;
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut size = file_size(&file)?;
        if size < len as u64 {
            match file.set_len(len as u64) {
                Ok(()) => size = len as u64,
                // A read-only handle: map what exists.
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {}
                Err(e) => return Err(e),
            }
        }
        let map_len = usize::try_from(size).map_or(len, |s| s.min(len));
        if map_len == 0 {
            // Windows cannot map an empty file (C fails the same way).
            return Err(io::Error::from_raw_os_error(
                windows_sys::Win32::Foundation::ERROR_FILE_INVALID as i32,
            ));
        }
        let ptr = map_view(&file, map_len, PAGE_READONLY, FILE_MAP_READ)?;
        Ok(DataMap {
            ptr,
            len: map_len,
            meta_end: 0,
            file,
            known_file_size: AtomicU64::new(size),
            section: ptr::null_mut(),
        })
    }

    /// Declares the first `len` bytes of the map to be the meta pages,
    /// which writers rewrite under readers: [`DataMap::slice`] refuses
    /// them, only [`DataMap::read_volatile`] reads them.
    pub fn with_meta_pages(mut self, len: usize) -> DataMap {
        self.meta_end = len;
        self
    }

    /// Length of the mapping.
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
        // SAFETY: [offset, end) lies inside the live view (checked above)
        // and inside the file, past the meta pages; with a reserved view
        // also inside its section, which `refresh_size` extended to the
        // file size it reports. The view lives as long as `self`, and the
        // bytes are not modified while the caller's snapshot is pinned
        // (type-level documentation).
        Some(unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) })
    }

    #[cold]
    fn refresh_size(&self, needed: usize) -> bool {
        match file_size(&self.file) {
            Ok(size) => {
                let size = if self.section.is_null() {
                    size
                } else {
                    self.extend_section(size)
                };
                self.known_file_size.fetch_max(size, Ordering::Relaxed);
                size >= needed as u64
            }
            Err(_) => false,
        }
    }

    /// Extends the section of a reserved view to `file_size` (at most to the
    /// view's length) and returns the size the view now shows, or the size
    /// known before if that fails. Never makes the section larger than the
    /// file, so the file is not changed.
    fn extend_section(&self, file_size: u64) -> u64 {
        let want = file_size.min(self.len as u64);
        let mut size = want as i64;
        // SAFETY: `section` is the section of this view, open until drop
        // and created with SECTION_EXTEND_SIZE; `size` is a writable i64.
        let st = unsafe { NtExtendSection(self.section, &mut size) };
        if st < 0 {
            return self.known_file_size.load(Ordering::Relaxed);
        }
        want.min(size as u64)
    }

    /// `MDB_Invalidate`: nothing to do on Windows
    /// ([`MAP_NEEDS_INVALIDATE`]).
    pub fn invalidate(&self, _offset: u64, _len: usize) {}

    /// Commits `len` bytes at `offset` of a reserved view, the pages a
    /// write transaction takes from the end of the map (C 1.0's
    /// `mdb_page_alloc`, mdb10.c line 2876: `VirtualAlloc(MEM_COMMIT,
    /// PAGE_READONLY)`). That extends the file by these pages and the
    /// segment the file's sections share, so they show in every process's
    /// view once written (see the module documentation). A no-op for the
    /// other maps (LMDB 0.9: the file already has the map size).
    pub fn commit_pages(&self, offset: usize, len: usize) -> io::Result<()> {
        if self.section.is_null() {
            return Ok(());
        }
        match offset.checked_add(len) {
            Some(end) if end <= self.len => {}
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        }
        // SAFETY: [offset, offset + len) lies inside the view, which lives
        // as long as `self`; committing pages of a reserved view of a
        // file-backed section only makes them readable (it changes no
        // bytes that a slice handed out could see).
        let p = unsafe {
            VirtualAlloc(
                self.ptr.add(offset) as *const core::ffi::c_void,
                len,
                MEM_COMMIT,
                PAGE_READONLY,
            )
        };
        if p.is_null() {
            return Err(last_error());
        }
        Ok(())
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
            // SAFETY: offset + i < end <= len, inside the view and the
            // file (and the section, see `refresh_size`). A volatile byte
            // read cannot be torn or elided; concurrent writes by other
            // processes are expected.
            *b = unsafe { ptr::read_volatile(self.ptr.add(offset + i)) };
        }
        true
    }
}

impl Drop for DataMap {
    fn drop(&mut self) {
        // No slices outlive `self` (they borrow it).
        unmap_view(self.ptr);
        if !self.section.is_null() {
            // SAFETY: the section of this map, closed once, after its view.
            unsafe { NtClose(self.section) };
        }
    }
}

// ---------------------------------------------------------------------------
// The lock file map
// ---------------------------------------------------------------------------

/// A read/write map of the lock file (`me_txns`).
///
/// All accesses to the shared reader table are atomic operations on
/// naturally aligned words (views start on a 64 KiB boundary), as on Unix.
/// Offsets come from `format::lockfile`; an offset outside the map or
/// misaligned is a programming error and panics.
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
    /// Maps the first `len` bytes of the lock file read/write, which must
    /// be at least that long.
    pub fn map(file: &File, len: usize) -> io::Result<LockMap> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let ptr = map_view(file, len, PAGE_READWRITE, FILE_MAP_WRITE)?;
        Ok(LockMap { ptr, len })
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
        // SAFETY: p is in bounds, 4-byte aligned (the view is aligned to the
        // allocation granularity and offset % 4 == 0), and valid for the
        // lifetime of &self. All access to this word is atomic.
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

    /// Copies `len` bytes at `offset` (volatile byte reads), e.g. a mutex
    /// name or the header.
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
}

impl Drop for LockMap {
    fn drop(&mut self) {
        // Nothing borrows the mapping any more.
        unmap_view(self.ptr);
    }
}

// ---------------------------------------------------------------------------
// The two shared locks: named mutexes
// ---------------------------------------------------------------------------

/// Outcome of a successful [`SharedMutex::lock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    /// The lock was acquired normally.
    Acquired,
    /// The lock was acquired, but its previous owner died while holding it
    /// (`WAIT_ABANDONED`, C's `MDB_OWNERDEAD`). The protected data may need
    /// repair before [`SharedMutex::mark_consistent`] is called.
    OwnerDied,
}

/// The pair of shared locks of an environment.
pub struct SharedLocks {
    /// `me_rmutex`: protects reader slot allocation.
    pub rmutex: SharedMutex,
    /// `me_wmutex`: serialises write transactions.
    pub wmutex: SharedMutex,
}

/// A named kernel mutex (`CreateMutexA` / `OpenMutexA`).
pub struct SharedMutex {
    handle: HANDLE,
}

// SAFETY: a mutex handle may be used from any thread; ownership of the
// mutex itself is per thread, which the callers respect (module docs).
unsafe impl Send for SharedMutex {}
// SAFETY: WaitForSingleObject and ReleaseMutex are thread-safe.
unsafe impl Sync for SharedMutex {}

impl SharedMutex {
    /// Locks the mutex, blocking (`WaitForSingleObject(INFINITE)`).
    pub fn lock(&self) -> io::Result<LockStatus> {
        // SAFETY: self.handle is an open mutex handle until drop.
        match unsafe { WaitForSingleObject(self.handle, INFINITE) } {
            WAIT_OBJECT_0 => Ok(LockStatus::Acquired),
            WAIT_ABANDONED => Ok(LockStatus::OwnerDied),
            _ => Err(last_error()),
        }
    }

    /// Unlocks the mutex (`ReleaseMutex`; must be called by the locking
    /// thread).
    pub fn unlock(&self) -> io::Result<()> {
        // SAFETY: open mutex handle.
        if unsafe { ReleaseMutex(self.handle) } != 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    /// Nothing to do: the waiter owns an abandoned mutex normally
    /// (`mdb_mutex_consistent` is 0 on Windows).
    pub fn mark_consistent(&self) -> io::Result<()> {
        Ok(())
    }

    /// Creates (or opens, if it exists) the mutex `name`, accessible to
    /// every account (a NULL DACL, C's `mdb_all_sa`).
    fn create(name: &CStr) -> io::Result<SharedMutex> {
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = &mut sd as *mut SECURITY_DESCRIPTOR as PSECURITY_DESCRIPTOR;
        // SAFETY: psd points at a writable SECURITY_DESCRIPTOR that lives
        // until the end of this function; a present but NULL DACL grants
        // everyone access. `sa` refers to it during CreateMutexA only, and
        // `name` is NUL-terminated.
        let h = unsafe {
            if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION) == 0
                || SetSecurityDescriptorDacl(psd, 1, ptr::null(), 0) == 0
            {
                return Err(last_error());
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd,
                bInheritHandle: 0,
            };
            CreateMutexA(&sa, 0, name.as_ptr().cast())
        };
        if h.is_null() {
            return Err(last_error());
        }
        Ok(SharedMutex { handle: h })
    }

    /// Opens the existing mutex `name` (`OpenMutexA(SYNCHRONIZE)`, enough
    /// to wait on and release it).
    fn open(name: &CStr) -> io::Result<SharedMutex> {
        // SAFETY: name is NUL-terminated.
        let h = unsafe { OpenMutexA(SYNCHRONIZE, 0, name.as_ptr().cast()) };
        if h.is_null() {
            return Err(last_error());
        }
        Ok(SharedMutex { handle: h })
    }
}

impl Drop for SharedMutex {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateMutexA/OpenMutexA and is
        // closed once. The kernel destroys the mutex with its last handle.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// `MUTEXNAME_PREFIX` of LMDB 1.0 on Windows, also the start of 0.9's names.
const MUTEX_PREFIX: &str = "Global\\MDB";

/// `mdb_hash_val` (0.9) and `mdb_hash` (1.0): 64-bit FNV-1a over `bytes`,
/// starting from `hval`. (`shared_lock.rs` has the same function for
/// macOS.)
fn hash_val(bytes: &[u8], mut hval: u64) -> u64 {
    for &b in bytes {
        hval ^= u64::from(b);
        hval = hval.wrapping_mul(0x0100_0000_01b3);
    }
    hval
}

/// `MDB_HASH_INIT`.
const HASH_INIT: u64 = 0xcbf2_9ce4_8422_2325;

const A85: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

/// `mdb_pack85` of LMDB 0.9 on Windows: five base-85 digits of `l`, least
/// significant first. Its parameter is an `unsigned long`, 32 bits on
/// Windows.
fn pack85_v09(mut l: u32, out: &mut Vec<u8>) {
    for _ in 0..5 {
        out.push(A85[(l % 85) as usize]);
        l /= 85;
    }
}

/// `mdb_hash_enc` of LMDB 0.9 on Windows: `pack85(h)` then `pack85(h >> 32)`,
/// each truncated to 32 bits.
fn hash_enc_v09(bytes: &[u8]) -> String {
    let h = hash_val(bytes, HASH_INIT);
    let mut out = Vec::with_capacity(10);
    pack85_v09(h as u32, &mut out);
    pack85_v09((h >> 32) as u32, &mut out);
    // All A85 characters are ASCII.
    String::from_utf8(out).unwrap_or_default()
}

/// `MUTEXNAME(env, ch)` of LMDB 1.0 on Windows: `"Global\MDB"`, `ch`, then
/// up to ten base-85 digits of `mutex_id`, least significant first,
/// stopping when the rest is 0 (`mdb_pack85`, mdb10.c line 5920).
fn mutex_name_v10(mutex_id: u64, ch: u8) -> io::Result<CString> {
    let mut name = MUTEX_PREFIX.as_bytes().to_vec();
    name.push(ch);
    let mut l = mutex_id;
    for _ in 0..10 {
        if l == 0 {
            break;
        }
        name.push(A85[(l % 85) as usize]);
        l /= 85;
    }
    CString::new(name).map_err(|_| einval())
}

/// The bytes C hashes: `struct { DWORD volume; DWORD nhigh; DWORD nlow; }`
/// of the lock file.
fn lock_file_id_bytes(info: &BY_HANDLE_FILE_INFORMATION) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0..4].copy_from_slice(&info.dwVolumeSerialNumber.to_ne_bytes());
    b[4..8].copy_from_slice(&info.nFileIndexHigh.to_ne_bytes());
    b[8..12].copy_from_slice(&info.nFileIndexLow.to_ne_bytes());
    b
}

fn read_name(map: &LockMap, offset: usize, len: usize) -> io::Result<CString> {
    let raw = map.read_bytes(offset, len);
    let end = raw.iter().position(|&b| b == 0).ok_or_else(einval)?;
    CString::new(&raw[..end]).map_err(|_| einval())
}

fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

impl SharedLocks {
    /// Creates both mutexes for a lock file of layout `layout` that we hold
    /// exclusively, and records in it what the other openers need to find
    /// them (`mdb_env_setup_locks`, `*excl > 0`): the names (version 1) or
    /// the hash they are derived from (version 2). `mode` is not used.
    pub fn create(
        map: &LockMap,
        file: &File,
        _mode: u32,
        layout: &LockLayout,
    ) -> io::Result<SharedLocks> {
        let id = lock_file_id_bytes(&file_information(file)?);
        let (rname, wname) = match (layout.objects, layout.wlock) {
            (LockObjects::MutexNames, Some(wlock)) => {
                let enc = hash_enc_v09(&id);
                let rname = CString::new(format!("{MUTEX_PREFIX}r{enc}")).map_err(|_| einval())?;
                let wname = CString::new(format!("{MUTEX_PREFIX}w{enc}")).map_err(|_| einval())?;
                if rname.as_bytes_with_nul().len() > layout.rlock_size {
                    return Err(einval());
                }
                // sprintf: the name and its NUL; the rest of the field keeps
                // whatever was there, as in C.
                map.write_bytes(layout.rlock, rname.as_bytes_with_nul());
                map.write_bytes(wlock, wname.as_bytes_with_nul());
                (rname, wname)
            }
            (LockObjects::MutexHash, _) if layout.rlock_size == 8 => {
                let mutex_id = hash_val(&id, HASH_INIT);
                map.store_u64(layout.rlock, mutex_id);
                (
                    mutex_name_v10(mutex_id, b'r')?,
                    mutex_name_v10(mutex_id, b'w')?,
                )
            }
            _ => return Err(einval()),
        };
        let rmutex = SharedMutex::create(&rname)?;
        let wmutex = SharedMutex::create(&wname)?;
        Ok(SharedLocks { rmutex, wmutex })
    }

    /// Opens the mutexes another process created: named in the lock file
    /// (version 1), or named after the hash stored in it (version 2).
    pub fn attach(map: &LockMap, layout: &LockLayout) -> io::Result<SharedLocks> {
        let (rname, wname) = match (layout.objects, layout.wlock) {
            (LockObjects::MutexNames, Some(wlock)) => (
                read_name(map, layout.rlock, layout.rlock_size)?,
                read_name(map, wlock, layout.rlock_size)?,
            ),
            (LockObjects::MutexHash, _) if layout.rlock_size == 8 => {
                let mutex_id = map.load_u64(layout.rlock);
                (
                    mutex_name_v10(mutex_id, b'r')?,
                    mutex_name_v10(mutex_id, b'w')?,
                )
            }
            _ => return Err(einval()),
        };
        let rmutex = SharedMutex::open(&rname)?;
        let wmutex = SharedMutex::open(&wname)?;
        Ok(SharedLocks { rmutex, wmutex })
    }

    /// Closes both mutex handles. Nothing is unlinked: the kernel destroys
    /// a named mutex with its last handle (`last_user` is not used).
    pub fn close(self, _last_user: bool) {}
}

/// The shared-lock types under the path the engine uses on every platform
/// (`os::shared_lock`, `shared_lock.rs` on Unix).
pub mod shared_lock {
    pub use super::{LockStatus, SharedLocks, SharedMutex};
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
        // The C runtime's text for LMDB's errno values, the system's for
        // Windows error codes.
        assert_eq!(strerror(libc::ENOENT), "No such file or directory");
        assert_eq!(strerror(libc::EINVAL), "Invalid argument");
        // 5 is EIO here, as in C, not ERROR_ACCESS_DENIED.
        assert_eq!(strerror(ERROR_ACCESS_DENIED as i32), "Input/output error");
        // ERROR_SHARING_VIOLATION: the system's message, as FormatMessage
        // gives it.
        let text = strerror(32);
        assert!(text.len() > 10 && text.ends_with("\r\n"), "{text:?}");
    }

    #[test]
    fn fnv1a_and_names() {
        // Standard FNV-1a 64 test vectors.
        assert_eq!(hash_val(b"", HASH_INIT), 0xcbf2_9ce4_8422_2325);
        assert_eq!(hash_val(b"a", HASH_INIT), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash_val(b"foobar", HASH_INIT), 0x8594_4171_f739_67e8);
        // 0.9: two halves of five digits, each of 32 bits only.
        let mut out = Vec::new();
        pack85_v09(85 * 2 + 1, &mut out);
        assert_eq!(out, b"12000");
        let enc = hash_enc_v09(b"x");
        let h = hash_val(b"x", HASH_INIT);
        let mut want = Vec::new();
        pack85_v09(h as u32, &mut want);
        pack85_v09((h >> 32) as u32, &mut want);
        assert_eq!(enc.as_bytes(), &want[..]);
        assert_eq!(format!("{MUTEX_PREFIX}r{enc}").len() + 1, 22);
        // 1.0: the prefix, the letter, then the whole hash.
        let name = |id, ch| mutex_name_v10(id, ch).unwrap().into_bytes();
        assert_eq!(name(85 * 2 + 1, b'r'), b"Global\\MDBr12");
        assert_eq!(name(0, b'w'), b"Global\\MDBw");
        assert_eq!(name(u64::MAX, b'w').len(), 11 + 10);
    }

    #[test]
    fn data_map_bounds() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[7u8; 8192]).unwrap();
        // A writable handle: the file grows to the map size.
        let m = DataMap::map_readonly(f.try_clone().unwrap(), 1 << 20, true)
            .unwrap()
            .with_meta_pages(2048);
        assert_eq!(f.metadata().unwrap().len(), 1 << 20);
        assert_eq!(m.slice(4096, 4096).unwrap()[0], 7);
        assert!(m.slice(0, 16).is_none() && m.slice(2047, 1).is_none());
        assert!(m.slice(2048, 1).is_some());
        assert_eq!(m.slice(8192, 4096).unwrap()[0], 0);
        assert!(m.slice(1 << 20, 1).is_none());
        assert!(m.slice(usize::MAX, 2).is_none());
        let mut buf = [0u8; 4];
        assert!(m.read_volatile(8190, &mut buf));
        assert_eq!(buf, [7, 7, 0, 0]);
        // Written pages show through the map.
        write_all_at(&f, &[9u8; 4096], 8192).unwrap();
        assert_eq!(m.slice(8192, 4096).unwrap()[4095], 9);
    }

    /// LMDB 1.0's map: the file keeps its size, and pages written past its
    /// end show through the reserved view (through this handle or another
    /// one, as another process's writes would).
    #[test]
    fn reserved_map_shows_the_file_growing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mdb");
        let rw = || {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let f = rw();
        write_all_at(&f, &[7u8; 8192], 0).unwrap();
        let m = map_data(f.try_clone().unwrap(), 1 << 20, false, Format::V10)
            .unwrap()
            .with_meta_pages(2048);
        assert_eq!(
            f.metadata().unwrap().len(),
            8192,
            "the file is not extended"
        );
        assert_eq!(m.len(), 1 << 20);
        assert_eq!(m.slice(4096, 4096).unwrap()[0], 7);
        assert!(m.slice(8192, 4096).is_none(), "past the end of the file");
        write_all_at(&f, &[9u8; 4096], 8192).unwrap();
        assert_eq!(m.slice(8192, 4096).unwrap()[4095], 9);
        let other = rw();
        write_all_at(&other, &[5u8; 4096], 64 << 10).unwrap();
        assert_eq!(m.slice(64 << 10, 4096).unwrap()[0], 5);
        assert_eq!(
            m.slice(12288, 4096).unwrap()[0],
            0,
            "the hole reads as zeros"
        );
        let mut buf = [0u8; 4];
        assert!(m.read_volatile(8190, &mut buf));
        assert_eq!(buf, [7, 7, 9, 9]);
        drop((m, f));
        // A read-only handle reserves the map size too, and sees pages
        // another handle adds.
        let ro = File::open(&path).unwrap();
        let m = map_data(ro, 1 << 20, false, Format::V10).unwrap();
        eprintln!("read-only reserved view: {} bytes", m.len());
        assert_eq!(m.slice(64 << 10, 1).unwrap()[0], 5);
        write_all_at(&other, &[3u8; 4096], 256 << 10).unwrap();
        if m.len() == 1 << 20 {
            assert_eq!(m.slice(256 << 10, 4096).unwrap()[4095], 3);
        } else {
            assert!(m.slice(256 << 10, 1).is_none());
        }
    }

    /// A 128 KiB file grown 64 KiB at a time under a reserved view: every
    /// page shows, also past the first MiB, where the view faulted without
    /// the explicit extension of the section.
    #[test]
    fn reserved_map_follows_the_file_past_the_first_mib() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mdb");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        write_all_at(&f, &vec![1u8; 128 << 10], 0).unwrap();
        let m = map_data(f.try_clone().unwrap(), 256 << 20, false, Format::V10).unwrap();
        for i in 2..40u64 {
            let page = vec![i as u8; 64 << 10];
            write_all_at(&f, &page, i << 16).unwrap();
            let got = m.slice((i << 16) as usize, 64 << 10).unwrap();
            assert_eq!(
                (got[0], got[(64 << 10) - 1]),
                (i as u8, i as u8),
                "page {i}"
            );
        }
        // The file was not extended beyond what was written.
        assert_eq!(f.metadata().unwrap().len(), 40 << 16);
        // Another handle's writes too, far out.
        let other = OpenOptions::new().write(true).open(&path).unwrap();
        write_all_at(&other, &[7u8; 4096], 8 << 20).unwrap();
        assert_eq!(m.slice(8 << 20, 4096).unwrap()[0], 7);
        assert_eq!(m.slice(5 << 20, 1).unwrap()[0], 0, "the hole");
    }

    /// Pages committed in one reserved view (as a writer allocates them)
    /// and then written show in another view of the file, with a section
    /// of its own, as in another process; without the explicit extension
    /// of its section, which would hide a missing commit.
    #[test]
    fn committed_pages_show_in_other_views() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mdb");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        write_all_at(&f, &[1u8; 8192], 0).unwrap();
        let writer = map_data(f.try_clone().unwrap(), 64 << 20, false, Format::V10).unwrap();
        let other = map_data(f.try_clone().unwrap(), 64 << 20, false, Format::V10).unwrap();
        for (i, off) in [(2u8, 3usize << 20), (3, 5 << 20), (4, (5 << 20) + 4096)] {
            writer.commit_pages(off, 4096).unwrap();
            assert!(f.metadata().unwrap().len() >= off as u64 + 4096);
            write_all_at(&f, &[i; 4096], off as u64).unwrap();
            // SAFETY: inside `other`'s view; the page was committed above,
            // which is what this test checks (a fault fails it).
            let got = unsafe { ptr::read_volatile(other.ptr.add(off + 4095)) };
            assert_eq!(got, i, "page at {off}");
        }
        assert!(
            writer.commit_pages(64 << 20, 4096).is_err(),
            "past the view"
        );
        // Not a reserved view: nothing to commit.
        let v09 = map_data(f.try_clone().unwrap(), 8 << 20, false, Format::V09).unwrap();
        v09.commit_pages(4096, 4096).unwrap();
    }

    #[test]
    fn data_map_read_only_handle_maps_what_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.mdb");
        std::fs::write(&path, [5u8; 8192]).unwrap();
        let f = File::open(&path).unwrap();
        let m = DataMap::map_readonly(f, 1 << 20, false).unwrap();
        assert_eq!(m.len(), 8192);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
        assert_eq!(m.slice(4096, 4096).unwrap()[0], 5);
        assert!(m.slice(8192, 1).is_none());
        drop(m);
        // An empty file cannot be mapped.
        std::fs::write(&path, []).unwrap();
        assert!(DataMap::map_readonly(File::open(&path).unwrap(), 4096, false).is_err());
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
        m.write_bytes(8, b"Global");
        assert_eq!(m.read_bytes(8, 6), b"Global");
        let mut back = [0u8; 8];
        read_at(&f, &mut back, 64).unwrap();
        assert_eq!(u64::from_ne_bytes(back), 0x1122_3344_5566_7788);
    }

    /// Locks belong to handles: a second handle of the file conflicts, the
    /// same handle may add a shared lock to its exclusive one, and the
    /// downgrade leaves only the shared one.
    #[test]
    fn byte_locks_are_per_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock.mdb");
        let open = || {
            env_file_options(
                OpenOptions::new().read(true).write(true).create(true),
                0o644,
            )
            .open(&path)
            .unwrap()
        };
        let (a, b) = (open(), open());
        lock_byte(&a, 0, LockKind::Exclusive, false).unwrap();
        assert!(lock_byte(&b, 0, LockKind::Shared, false).is_err());
        downgrade_lock(&a).unwrap();
        lock_byte(&b, 0, LockKind::Shared, false).unwrap();
        assert!(lock_byte(&b, 0, LockKind::Exclusive, false).is_err());
        unlock_lock_file(&a);
        unlock_lock_file(&b);
        lock_byte(&b, 0, LockKind::Exclusive, false).unwrap();
        // No sharing for deletion while open.
        assert!(std::fs::remove_file(&path).is_err());
    }

    #[test]
    fn process_liveness() {
        assert!(pid_alive(&tempfile::tempfile().unwrap(), getpid()));
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        // Exited, while we still hold a handle to it (C would say alive).
        assert!(!pid_alive(&tempfile::tempfile().unwrap(), pid));
        drop(child);
    }

    #[test]
    fn named_mutexes() {
        let name = CString::new(format!("Global\\rlmdb-test-{}", getpid())).unwrap();
        let a = SharedMutex::create(&name).unwrap();
        let b = SharedMutex::open(&name).unwrap();
        assert_eq!(a.lock().unwrap(), LockStatus::Acquired);
        a.unlock().unwrap();
        // A thread that exits holding it abandons it.
        std::thread::scope(|s| {
            s.spawn(|| assert_eq!(b.lock().unwrap(), LockStatus::Acquired));
        });
        assert_eq!(a.lock().unwrap(), LockStatus::OwnerDied);
        a.mark_consistent().unwrap();
        a.unlock().unwrap();
    }
}
