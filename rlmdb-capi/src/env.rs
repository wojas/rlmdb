//! `lmdb.h` section "Environment": `mdb_env_*`, plus `mdb_reader_list`
//! and `mdb_reader_check`.
//!
//! An `MDB_env` collects settings until `mdb_env_open`, which builds an
//! [`Environment`] with them. Functions that need an open environment and
//! would crash or misbehave in C before the open return `EINVAL`.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;
use std::sync::atomic::Ordering;

use rlmdb::consts::{CHANGEABLE, CHANGELESS};
use rlmdb::{Environment, EnvironmentFlags};

use crate::handles::{lock, MDB_env};
use crate::types::*;
use crate::{rc, SUCCESS};

const EINVAL: c_int = libc::EINVAL;

/// The open environment behind `env`, or `EINVAL`.
///
/// # Safety
///
/// `env` is NULL or a live handle.
unsafe fn open_env<'a>(env: *mut MDB_env) -> Result<&'a Environment, c_int> {
    // SAFETY: per this function's contract.
    unsafe { MDB_env::from_ptr(env) }
        .and_then(MDB_env::open_env)
        .ok_or(EINVAL)
}

/// Turns `Result<(), code>` into a return code.
fn code(r: Result<(), c_int>) -> c_int {
    r.err().unwrap_or(SUCCESS)
}

/// `mdb_env_create`: create an LMDB environment handle.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_create(env: *mut *mut MDB_env) -> c_int {
    if env.is_null() {
        return EINVAL;
    }
    let handle = Box::into_raw(Box::new(MDB_env::new()));
    // SAFETY: env is non-NULL and writable (lmdb.h).
    unsafe { *env = handle };
    SUCCESS
}

/// `mdb_env_open`: open an environment handle.
///
/// Flags set earlier with `mdb_env_set_flags` are added to `flags`, as in
/// C. Unknown flags give `EINVAL`, among them LMDB 1.0's
/// `MDB_PREVSNAPSHOT` for environments of either format (it is not
/// implemented) and `MDB_ENCRYPT`. `MDB_FIXEDMAP`, `MDB_WRITEMAP`,
/// `MDB_MAPASYNC` and `MDB_NOMEMINIT` are accepted and ignored;
/// `MDB_NOTLS` is accepted and is how this implementation always behaves.
/// Opening an environment that this process already has open fails with
/// `EBUSY`, except that any number of `MDB_NOLOCK` handles may open one
/// environment: they share one in-process reader table and writer lock
/// (a handle with a lock file and a `MDB_NOLOCK` one still refuse each
/// other).
///
/// An existing data file is opened in its format (LMDB 0.9 or 1.0); a
/// format requested with `rlmdb_env_set_format` that differs from it gives
/// `MDB_VERSION_MISMATCH`. A new one is created in the requested format,
/// else in the process default (`rlmdb_set_default_format`), else in the
/// one the experimental `RLMDB_DEFAULT_FORMAT` variable names, else as
/// LMDB 0.9; a page size set with `rlmdb_env_set_pagesize` applies to a
/// new 1.0 environment ([`rlmdb::EnvironmentBuilder::format`],
/// [`rlmdb::EnvironmentBuilder::page_size`]).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_open(
    env: *mut MDB_env,
    path: *const c_char,
    flags: c_uint,
    mode: mdb_mode_t,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if e.open_env().is_some() || flags & !(CHANGEABLE | CHANGELESS) != 0 || path.is_null() {
        return EINVAL;
    }
    // SAFETY: path is a NUL-terminated string (lmdb.h).
    let path = unsafe { CStr::from_ptr(path) };
    // mdb_mode_t is u16 on macOS and with MinGW, u32 on Linux, i32 with MSVC.
    #[allow(clippy::unnecessary_cast)]
    let mode = mode as u32;
    let config = *e.config();
    let mut builder = Environment::builder();
    builder
        .map_size(config.map_size)
        .max_readers(config.max_readers)
        .max_dbs(config.max_dbs)
        .flags(EnvironmentFlags::from_bits_retain(flags | config.flags));
    if let Some(format) = config.format {
        builder.format(format);
    }
    if let Some(page_size) = config.page_size {
        builder.page_size(page_size);
    }
    match builder.open_with_permissions(crate::platform::path_from_c(path), mode) {
        Ok(opened) => {
            e.set_opened(opened, path.to_owned());
            SUCCESS
        }
        Err(err) => err.to_err_code(),
    }
}

/// `mdb_env_close`: close the environment and release the memory map.
/// All transactions and cursors must have been closed before (as in C).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_close(env: *mut MDB_env) {
    if !env.is_null() {
        // SAFETY: env came from Box::into_raw in mdb_env_create and, per
        // lmdb.h, is not used after this call.
        drop(unsafe { Box::from_raw(env) });
    }
}

/// `mdb_env_stat`: return statistics about the environment (its main DB
/// in the latest snapshot).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_stat(env: *mut MDB_env, stat: *mut MDB_stat) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let e = match unsafe { open_env(env) } {
        Ok(e) if !stat.is_null() => e,
        _ => return EINVAL,
    };
    match e.stat() {
        Ok(s) => {
            // SAFETY: stat is non-NULL and writable (lmdb.h).
            unsafe { *stat = s.into() };
            SUCCESS
        }
        Err(err) => err.to_err_code(),
    }
}

/// `mdb_env_info`: return information about the environment.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_info(env: *mut MDB_env, info: *mut MDB_envinfo) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let e = match unsafe { open_env(env) } {
        Ok(e) if !info.is_null() => e,
        _ => return EINVAL,
    };
    match e.info() {
        Ok(i) => {
            let out = MDB_envinfo {
                me_mapaddr: i.map_address as *mut c_void,
                me_mapsize: i.map_size,
                me_last_pgno: i.last_pgno,
                me_last_txnid: i.last_txnid,
                me_maxreaders: i.max_readers,
                // 0 without a lock file, as in C (the read transactions
                // are then tracked in memory, which C does not do;
                // KNOWN_C_BUGS.md #35).
                me_numreaders: i.num_readers,
            };
            // SAFETY: info is non-NULL and writable (lmdb.h).
            unsafe { *info = out };
            SUCCESS
        }
        Err(err) => err.to_err_code(),
    }
}

/// `mdb_env_sync`: flush the data buffers to disk. `EACCES` for a
/// read-only environment.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_sync(env: *mut MDB_env, force: c_int) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { open_env(env) } {
        Ok(e) => rc(e.sync(force != 0)),
        Err(code) => code,
    }
}

/// `mdb_env_set_flags`: set environment flags (only `MDB_NOSYNC`,
/// `MDB_NOMETASYNC`, `MDB_MAPASYNC` and `MDB_NOMEMINIT` can be changed;
/// others give `EINVAL`, LMDB 1.0's `MDB_ENCRYPT` among them: encryption
/// is not implemented). Before `mdb_env_open` the flags are remembered for
/// the open.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_flags(
    env: *mut MDB_env,
    flags: c_uint,
    onoff: c_int,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if flags & !CHANGEABLE != 0 {
        return EINVAL;
    }
    if let Some(open) = e.open_env() {
        return rc(open.set_flags(EnvironmentFlags::from_bits_retain(flags), onoff != 0));
    }
    let mut config = e.config();
    if onoff != 0 {
        config.flags |= flags;
    } else {
        config.flags &= !flags;
    }
    SUCCESS
}

/// `mdb_env_get_flags`: get environment flags.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_flags(env: *mut MDB_env, flags: *mut c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if flags.is_null() {
        return EINVAL;
    }
    let value = match e.open_env() {
        Some(open) => open.flags().bits(),
        None => e.config().flags,
    };
    // SAFETY: flags is non-NULL and writable (lmdb.h).
    unsafe { *flags = value & (CHANGEABLE | CHANGELESS) };
    SUCCESS
}

/// `mdb_env_get_path`: return the path that was used in `mdb_env_open`
/// (NULL before the open).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_path(env: *mut MDB_env, path: *mut *const c_char) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if path.is_null() {
        return EINVAL;
    }
    let value = e.path().map_or(ptr::null(), |p| p.as_ptr());
    // SAFETY: path is non-NULL and writable (lmdb.h).
    unsafe { *path = value };
    SUCCESS
}

/// `mdb_env_get_fd`: return the file descriptor of the data file (-1
/// before the open).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_fd(env: *mut MDB_env, fd: *mut mdb_filehandle_t) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if fd.is_null() {
        return EINVAL;
    }
    let value = e.open_env().map_or(crate::platform::NO_HANDLE, |open| {
        crate::platform::raw_handle(open.data_file())
    });
    // SAFETY: fd is non-NULL and writable (lmdb.h).
    unsafe { *fd = value };
    SUCCESS
}

/// `mdb_env_set_mapsize`: set the size of the memory map.
///
/// Before `mdb_env_open` the size is remembered for the open. On an open
/// environment the data file is mapped again with the new size (0 = the
/// size recorded in the file: the `MDB_MAP_RESIZED` recovery path), raised
/// to cover the pages in use. Unlike C, read transactions may still be
/// active (they keep the map they started with); `EINVAL` while a write
/// transaction is active, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_mapsize(env: *mut MDB_env, size: usize) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if let Some(open) = e.open_env() {
        return rc(open.set_map_size(size));
    }
    e.config().map_size = size;
    SUCCESS
}

/// `mdb_env_set_maxreaders`: set the maximum number of threads/reader
/// slots. Only before `mdb_env_open`; at least 1.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_maxreaders(env: *mut MDB_env, readers: c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) if e.open_env().is_none() && readers >= 1 => {
            e.config().max_readers = readers;
            SUCCESS
        }
        _ => EINVAL,
    }
}

/// `mdb_env_get_maxreaders`: get the maximum number of reader slots (after
/// the open: the size of the lock file's table, which may be larger than
/// requested).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_maxreaders(env: *mut MDB_env, readers: *mut c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if readers.is_null() {
        return EINVAL;
    }
    let value = match e.open_env() {
        Some(open) => open.max_readers(),
        None => e.config().max_readers,
    };
    // SAFETY: readers is non-NULL and writable (lmdb.h).
    unsafe { *readers = value };
    SUCCESS
}

/// `mdb_env_set_maxdbs`: set the maximum number of named databases. Only
/// before `mdb_env_open`.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_maxdbs(env: *mut MDB_env, dbs: MDB_dbi) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) if e.open_env().is_none() => {
            e.config().max_dbs = dbs;
            SUCCESS
        }
        _ => EINVAL,
    }
}

/// `mdb_env_get_maxkeysize`: the maximum size of keys and `MDB_DUPSORT`
/// data. For an open environment its own limit
/// ([`rlmdb::Environment::max_key_size`]): 511 for LMDB 0.9, for 1.0 a
/// value that follows from the page size (1978 on 4 KiB pages). Before
/// the open 511, unless LMDB 1.0 was requested with
/// `rlmdb_env_set_format`: then the 1.0 limit for the page size a new
/// environment would get (`rlmdb_env_set_pagesize`, else the OS page
/// size), which an existing file with another page size changes at the
/// open. (C 1.0 returns 0 before the open: it only computes the limit in
/// `mdb_env_open`.) 511 for a NULL handle, as C 0.9.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_maxkeysize(env: *mut MDB_env) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let size = match unsafe { MDB_env::from_ptr(env) } {
        Some(e) => match e.open_env() {
            Some(open) => open.max_key_size(),
            None => {
                let config = *e.config();
                match config.format {
                    Some(f @ rlmdb::Format::V10) => f
                        .layout()
                        .max_key_size(crate::format::new_page_size(f, config.page_size)),
                    _ => rlmdb::consts::MDB_MAXKEYSIZE,
                }
            }
        },
        None => rlmdb::consts::MDB_MAXKEYSIZE,
    };
    c_int::try_from(size).unwrap_or(c_int::MAX)
}

/// `mdb_env_set_userctx`: set application information associated with the
/// environment.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_userctx(env: *mut MDB_env, ctx: *mut c_void) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) => {
            e.userctx.store(ctx, Ordering::Relaxed);
            SUCCESS
        }
        None => EINVAL,
    }
}

/// `mdb_env_get_userctx`: get the application information associated with
/// the environment (NULL for a NULL handle).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_userctx(env: *mut MDB_env) -> *mut c_void {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    unsafe { MDB_env::from_ptr(env) }.map_or(ptr::null_mut(), |e| e.userctx.load(Ordering::Relaxed))
}

/// `mdb_env_set_assert`: set or reset the assert() callback. Accepted and
/// stored; the Rust implementation returns errors instead of asserting, so
/// the callback is never called.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_assert(
    env: *mut MDB_env,
    func: Option<MDB_assert_func>,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) => {
            *lock(&e.assert_func) = func;
            SUCCESS
        }
        None => EINVAL,
    }
}

/// `mdb_reader_list`: dump the entries in the reader lock table, one
/// callback per line, exactly as C formats them. Returns -1 for NULL
/// arguments, else the last callback result (stopping at a negative one).
#[no_mangle]
pub unsafe extern "C" fn mdb_reader_list(
    env: *mut MDB_env,
    func: Option<MDB_msg_func>,
    ctx: *mut c_void,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let (Some(e), Some(func)) = (unsafe { MDB_env::from_ptr(env) }, func) else {
        return -1;
    };
    let text = match e.open_env() {
        Some(open) if open.has_lock_file() => match open.reader_list() {
            Ok(readers) => rlmdb::format_reader_list(&readers),
            Err(err) => return err.to_err_code(),
        },
        // C: no lock table (MDB_NOLOCK, a read-only environment on a
        // read-only file system, or not open yet). The in-memory table of
        // such an environment is not listed: programs may compare with C.
        _ => "(no reader locks)\n".to_owned(),
    };
    let mut result = 0;
    for line in text.split_inclusive('\n') {
        let Ok(line) = CString::new(line) else {
            continue;
        };
        // SAFETY: func is the caller's message callback; line is a valid
        // NUL-terminated string for the duration of the call.
        result = unsafe { func(line.as_ptr(), ctx) };
        if result < 0 {
            break;
        }
    }
    result
}

/// `mdb_reader_check`: clear stale entries from the reader lock table;
/// stores the number cleared in `dead` (if not NULL).
#[no_mangle]
pub unsafe extern "C" fn mdb_reader_check(env: *mut MDB_env, dead: *mut c_int) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    // SAFETY: dead is NULL or writable (lmdb.h).
    let dead = unsafe { dead.as_mut() };
    let mut cleared = 0;
    let result = match e.open_env() {
        Some(open) => open.reader_check().map(|n| cleared = n),
        None => Ok(()),
    };
    if let Some(dead) = dead {
        *dead = cleared as c_int;
    }
    code(result.map_err(|err| err.to_err_code()))
}
