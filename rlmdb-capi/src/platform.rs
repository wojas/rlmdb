//! What differs between Unix and Windows at the C boundary: how a path
//! argument is read, what an `mdb_filehandle_t` is, the OS page size, and
//! the text of system error codes.
//!
//! On Windows the library follows C LMDB's `_WIN32` paths: a path is UTF-8
//! (`mdb_fname_init` converts it to UTF-16 with `MultiByteToWideChar`, and
//! invalid sequences become U+FFFD), `mdb_filehandle_t` is a `HANDLE`, and
//! `mdb_strerror` gives the C runtime's text for the `errno` values LMDB
//! uses and the system's message for every other code (a `GetLastError`
//! value).

use std::borrow::Cow;
use std::ffi::CStr;
use std::fs::File;
use std::path::Path;

use crate::types::mdb_filehandle_t;

/// The path a C string names: its bytes as they are (Unix).
#[cfg(unix)]
pub(crate) fn path_from_c(s: &CStr) -> Cow<'_, Path> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(Path::new(std::ffi::OsStr::from_bytes(s.to_bytes())))
}

/// The path a C string names: UTF-8 (Windows; see the module
/// documentation).
#[cfg(windows)]
pub(crate) fn path_from_c(s: &CStr) -> Cow<'_, Path> {
    match s.to_string_lossy() {
        Cow::Borrowed(p) => Cow::Borrowed(Path::new(p)),
        Cow::Owned(p) => Cow::Owned(p.into()),
    }
}

/// The OS page size (`sysconf(_SC_PAGESIZE)`), C's `me_os_psize`.
#[cfg(unix)]
pub(crate) fn os_page_size() -> usize {
    // SAFETY: sysconf has no preconditions; _SC_PAGESIZE is always valid.
    let os = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(os).unwrap_or(4096)
}

/// The OS page size (`GetSystemInfo().dwPageSize`), C's `me_os_psize`.
#[cfg(windows)]
pub(crate) fn os_page_size() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    let mut si = SYSTEM_INFO::default();
    // SAFETY: GetSystemInfo only fills the struct we pass; it cannot fail.
    unsafe { GetSystemInfo(&mut si) };
    match si.dwPageSize {
        0 => 4096,
        n => n as usize,
    }
}

/// `env->me_fd` of an environment that is not open: `-1` on Unix,
/// `INVALID_HANDLE_VALUE` on Windows.
#[cfg(unix)]
pub(crate) const NO_HANDLE: mdb_filehandle_t = -1;

/// `env->me_fd` of an environment that is not open: `-1` on Unix,
/// `INVALID_HANDLE_VALUE` on Windows.
#[cfg(windows)]
pub(crate) const NO_HANDLE: mdb_filehandle_t = -1isize as mdb_filehandle_t;

/// What C's `mdb_env_copyfd` returns for a descriptor that cannot be one
/// (negative): the error of its first `write`, `EBADF`.
#[cfg(unix)]
pub(crate) const BAD_HANDLE: std::os::raw::c_int = libc::EBADF;

/// What C's `mdb_env_copyfd` returns for a handle that cannot name a file
/// (NULL, `INVALID_HANDLE_VALUE`): the error of its first `WriteFile`,
/// `ERROR_INVALID_HANDLE`.
#[cfg(windows)]
pub(crate) const BAD_HANDLE: std::os::raw::c_int = 6;

/// The `mdb_filehandle_t` of an open file (borrowed: the file keeps it).
#[cfg(unix)]
pub(crate) fn raw_handle(file: &File) -> mdb_filehandle_t {
    use std::os::fd::AsRawFd;
    file.as_raw_fd()
}

/// The `mdb_filehandle_t` of an open file (borrowed: the file keeps it).
#[cfg(windows)]
pub(crate) fn raw_handle(file: &File) -> mdb_filehandle_t {
    use std::os::windows::io::AsRawHandle;
    file.as_raw_handle()
}

/// The descriptor a C caller passed, borrowed for the call, or `None` for
/// a value that cannot be one (negative).
///
/// # Safety
///
/// A non-negative `fd` must be an open descriptor that stays open for the
/// lifetime `'a` (the duration of the call).
#[cfg(unix)]
pub(crate) unsafe fn borrow_handle<'a>(
    fd: mdb_filehandle_t,
) -> Option<std::os::fd::BorrowedFd<'a>> {
    // SAFETY: the caller guarantees that a non-negative fd is open for 'a.
    (fd >= 0).then(|| unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) })
}

/// The handle a C caller passed, borrowed for the call, or `None` for a
/// value that cannot name a file (NULL, `INVALID_HANDLE_VALUE`).
///
/// # Safety
///
/// Any other `fd` must be an open handle that stays open for the lifetime
/// `'a` (the duration of the call).
#[cfg(windows)]
pub(crate) unsafe fn borrow_handle<'a>(
    fd: mdb_filehandle_t,
) -> Option<std::os::windows::io::BorrowedHandle<'a>> {
    if fd.is_null() || fd == NO_HANDLE {
        return None;
    }
    // SAFETY: the caller guarantees that fd is an open handle for 'a.
    Some(unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(fd) })
}

/// `mdb_strerror` of a positive code that is not LMDB's: the C library's
/// `strerror` (Unix), a string owned by the C library.
#[cfg(unix)]
pub(crate) fn system_strerror(err: i32) -> *mut std::os::raw::c_char {
    // SAFETY: strerror accepts any int and returns a NUL-terminated string
    // owned by the C library.
    unsafe { libc::strerror(err) }
}

/// `mdb_strerror` of a positive code that is not LMDB's, as C on Windows:
/// the C runtime's text for `ENOENT`, `EIO`, `ENOMEM`, `EACCES`, `EBUSY`,
/// `EINVAL` and `ENOSPC`, else the system's message ([`rlmdb::strerror`]).
/// C formats the message into a buffer on its stack; here each code's text
/// is made once and kept for the life of the process.
#[cfg(windows)]
pub(crate) fn system_strerror(err: i32) -> *mut std::os::raw::c_char {
    use std::collections::BTreeMap;
    use std::ffi::CString;
    use std::sync::{Mutex, OnceLock};
    static TEXTS: OnceLock<Mutex<BTreeMap<i32, &'static CStr>>> = OnceLock::new();
    let mut texts = TEXTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let text = texts.entry(err).or_insert_with(|| {
        let s = CString::new(rlmdb::strerror(err).replace('\0', "")).unwrap_or_default();
        Box::leak(s.into_boxed_c_str())
    });
    text.as_ptr() as *mut std::os::raw::c_char
}
