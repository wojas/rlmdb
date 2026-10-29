//! `lmdb.h` section "Environment": `mdb_env_*`, plus `mdb_reader_list`
//! and `mdb_reader_check`.
//!
//! An `MDB_env` collects settings until `mdb_env_open`, which builds an
//! [`Environment`] with them. Functions that need an open environment and
//! would crash or misbehave in C before the open return `EINVAL`.

use std::ffi::{CStr, CString, OsStr};
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::ptr;
use std::sync::atomic::Ordering;

use lmdb_core::consts::{CHANGEABLE, CHANGELESS, MDB_NOLOCK};
use lmdb_core::format::layout::{META_SIZE, PAGE_HDR_SIZE};
use lmdb_core::format::meta::{pick_newest, Meta};
use lmdb_core::{Environment, EnvironmentFlags};

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
/// C. Unknown flags give `EINVAL`. `MDB_FIXEDMAP`, `MDB_WRITEMAP`,
/// `MDB_MAPASYNC` and `MDB_NOMEMINIT` are accepted and ignored; `MDB_NOTLS`
/// is accepted and is how this implementation always behaves. Opening an
/// environment that this process already has open fails with `EBUSY`.
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
    // mdb_mode_t is u16 on macOS and u32 on Linux.
    #[allow(clippy::unnecessary_cast)]
    let mode = mode as u32;
    let config = *e.config();
    let mut builder = Environment::builder();
    builder
        .map_size(config.map_size)
        .max_readers(config.max_readers)
        .max_dbs(config.max_dbs)
        .flags(EnvironmentFlags::from_bits_retain(flags | config.flags));
    match builder.open_with_permissions(OsStr::from_bytes(path.to_bytes()), mode) {
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
                me_mapaddr: recorded_map_address(e) as *mut c_void,
                me_mapsize: i.map_size,
                me_last_pgno: i.last_pgno,
                me_last_txnid: i.last_txnid,
                me_maxreaders: i.max_readers,
                me_numreaders: i.num_readers,
            };
            // SAFETY: info is non-NULL and writable (lmdb.h).
            unsafe { *info = out };
            SUCCESS
        }
        Err(err) => err.to_err_code(),
    }
}

/// `me_mapaddr` as C reports it: the `mm_address` recorded in the newest
/// meta page (non-zero only for environments a C program created with
/// `MDB_FIXEDMAP`), read from the data file. 0 if the pages cannot be read.
///
/// Stop-gap until `lmdb_core::Info` carries the recorded address itself.
fn recorded_map_address(env: &Environment) -> usize {
    let read = |i: usize| {
        let mut page = [0u8; PAGE_HDR_SIZE + META_SIZE];
        env.data_file()
            .read_exact_at(&mut page, (i * env.page_size()) as u64)
            .ok()?;
        Meta::read_page(&page).ok()
    };
    match (read(0), read(1)) {
        (Some(m0), Some(m1)) => {
            let metas = [m0, m1];
            metas[pick_newest(&metas)].address as usize
        }
        (Some(m), None) | (None, Some(m)) => m.address as usize,
        (None, None) => 0,
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
/// `MDB_NOMETASYNC`, `MDB_MAPASYNC` and `MDB_NOMEMINIT` can be changed).
/// Before `mdb_env_open` the flags are remembered for the open.
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
    let value = e.open_env().map_or(-1, |open| open.data_file().as_raw_fd());
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
/// data (511).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_get_maxkeysize(_env: *mut MDB_env) -> c_int {
    lmdb_core::consts::MDB_MAXKEYSIZE as c_int
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
        Some(open) if open.flags().bits() & MDB_NOLOCK == 0 => match open.reader_list() {
            Ok(readers) => lmdb_core::format_reader_list(&readers),
            Err(err) => return err.to_err_code(),
        },
        // C: no lock table (MDB_NOLOCK, or not open yet).
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
