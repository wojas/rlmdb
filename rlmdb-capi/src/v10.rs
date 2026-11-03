//! The 13 functions LMDB 1.0's `lmdb.h` declares and 0.9's does not
//! (`tests/lmdb-c/1.0/lmdb.h`, `mdb.c` and `module.c`), so that a program
//! built for C LMDB 1.0 links against every rlmdb library; rlmdb's
//! `include/lmdb.h`, LMDB 1.0's, declares them.
//!
//! Implemented, as in C 1.0:
//!
//! * `mdb_env_set_pagesize` (as `rlmdb_env_set_pagesize`);
//! * `mdb_txn_flags`;
//! * `mdb_cursor_is_db` ([`rlmdb::RoCursor::is_db`]);
//! * two-phase commit: `mdb_txn_prepare` ([`rlmdb::RwTxn::prepare`]) and
//!   `mdb_env_rollback` ([`rlmdb::Environment::rollback`]).
//!
//! Not implemented: encryption and checksums (`mdb_env_set_encrypt`,
//! `mdb_env_set_checksum`), incremental backups (`mdb_env_incr_dump`,
//! `mdb_env_incr_dumpfd`, `mdb_env_incr_loadfd`) and crypto modules
//! (`mdb_modload`, `mdb_modunload`, `mdb_modsetup`); `DUAL_FORMAT.md` §7.
//! The functions that return an error code make C 1.0's argument checks
//! first, in C's order (`EINVAL`; also where C would crash on a NULL
//! handle or an environment that is not open), then return `ENOTSUP`
//! without changing anything. The module functions return `NULL` and
//! an error message (`mdb_modload`), do nothing (`mdb_modunload`), and
//! make `mdb_env_open` refuse an environment that `mdb_modsetup` asked to
//! encrypt or checksum.

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

use rlmdb::consts::MDB_RDONLY;

use crate::handles::{CursorAccess, MDB_cursor, MDB_env, MDB_txn};
use crate::types::*;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// What the unimplemented functions return after their argument checks.
const ENOTSUP: c_int = libc::ENOTSUP;

/// The message `mdb_modload` stores in `*errmsg`.
const MODLOAD_MESSAGE: &std::ffi::CStr = c"rlmdb does not support LMDB crypto modules";

/// The open environment behind `env`, or `EINVAL` for NULL and for an
/// environment that is not open (where C would crash).
///
/// # Safety
///
/// `env` is NULL or a live handle.
unsafe fn open_env<'a>(env: *mut MDB_env) -> Result<&'a MDB_env, c_int> {
    // SAFETY: per this function's contract.
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) if e.open_env().is_some() => Ok(e),
        _ => Err(EINVAL),
    }
}

/// `mdb_env_set_pagesize`: set the size of DB pages of a new LMDB 1.0
/// environment, as [`rlmdb_env_set_pagesize`] does (C 1.0's checks:
/// `EINVAL` for NULL, after the open, outside 256 .. 65536, not a power of
/// two).
///
/// [`rlmdb_env_set_pagesize`]: crate::format::rlmdb_env_set_pagesize
/// LMDB 0.9 has no page-size setting: `mdb_env_open` fails with `EINVAL`
/// if the environment turns out to be 0.9 (a new one in `librlmdb` or the
/// 0.9 stub without a request for 1.0, or an existing 0.9 file).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_pagesize(env: *mut MDB_env, size: c_int) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    unsafe { crate::format::set_pagesize(env, i64::from(size)) }
}

/// `mdb_txn_flags` (C 1.0, `mdb.c` line 3663): store the transaction's
/// `MDB_RDONLY` flag in `*flags` (`MDB_RDONLY` for a read-only
/// transaction, also a reset one, 0 for a write transaction, also a
/// nested one). `EINVAL` for a NULL transaction, and for NULL `flags`
/// (where C would crash).
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_flags(txn: *mut MDB_txn, flags: *mut c_uint) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let Some(t) = (unsafe { MDB_txn::from_ptr(txn) }) else {
        return EINVAL;
    };
    if flags.is_null() {
        return EINVAL;
    }
    let value = if t.is_read_only() { MDB_RDONLY } else { 0 };
    // SAFETY: flags is non-NULL and writable (lmdb.h).
    unsafe { *flags = value };
    SUCCESS
}

/// `mdb_cursor_is_db` (C 1.0, `mdb.c` line 9843): 1 if the cursor is on
/// the record of a named database (a node with `F_SUBDATA` and without
/// `F_DUPDATA`, which only the main DB has), else 0; never an error (0
/// for NULL, a cursor that is not positioned, and one whose transaction
/// cannot be used: reset, ended, or blocked by a nested transaction).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_is_db(cursor: *mut MDB_cursor) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let Some(c) = (unsafe { MDB_cursor::from_ptr(cursor) }) else {
        return 0;
    };
    let is_db = c.with_cursor(|access| match access {
        CursorAccess::Read(rc) => Ok(rc.current_is_db()),
        CursorAccess::Write(w, id) => w.cursor_is_db(id).map_err(|e| e.to_err_code()),
    });
    c_int::from(is_db.unwrap_or(false))
}

/// `mdb_txn_prepare` (two-phase commit, C 1.0 `mdb.c` line 4659): write
/// everything the transaction changed, synced as a commit syncs it, except
/// the meta page, keeping the writer lock; then only `mdb_txn_commit`
/// (which writes the meta page) and `mdb_txn_abort` finish it
/// ([`rlmdb::RwTxn::prepare`]). As in C:
///
/// * the transaction's cursors are closed: here their handles stay
///   allocated but unusable (`EINVAL`) until `mdb_cursor_close` or the
///   end of the transaction (C frees them at once);
/// * every call that checks `MDB_TXN_BLOCKED` returns `MDB_BAD_TXN` after
///   its handle checks (`mdb_get`, `mdb_put`, `mdb_del`, `mdb_drop`,
///   `mdb_cursor_open`, `mdb_dbi_open`, `mdb_stat`, `mdb_txn_begin` of a
///   child); `mdb_dbi_flags`, `mdb_set_compare`, `mdb_set_dupsort`,
///   `mdb_cmp`, `mdb_dcmp`, `mdb_txn_id` and `mdb_txn_flags` still work; a
///   second `mdb_txn_prepare` returns `MDB_TXN_PENDING`;
/// * an active child is committed first (if that fails, the transaction
///   is aborted);
/// * a read-only or a nested transaction is committed (as
///   `mdb_txn_commit`, which frees its handle);
/// * a transaction with nothing to write ends as an empty commit does
///   (the writer lock is released); `mdb_txn_commit` then returns 0 where
///   C's returns `MDB_BAD_TXN` and frees memory it does not own
///   (`KNOWN_C_BUGS.md` row 61);
/// * if the prepare fails, the transaction is aborted: `mdb_txn_abort`
///   frees the handle, `mdb_txn_commit` returns `MDB_BAD_TXN` and frees it
///   (C returns the same and frees memory it does not own, row 61).
///
/// `EINVAL` for NULL (C would crash).
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_prepare(txn: *mut MDB_txn) -> c_int {
    if txn.is_null() {
        return EINVAL;
    }
    // SAFETY: txn is a live handle (lmdb.h); a read-only or nested one is
    // freed, as mdb_txn_commit frees it.
    unsafe { MDB_txn::prepare(txn) }
}

/// `mdb_env_rollback` (two-phase commit, C 1.0 `mdb.c` line 4668): undo
/// the last commit, transaction `txnid`, by overwriting its meta page with
/// one that holds no snapshot (C's bytes), so that the previous snapshot
/// is current again ([`rlmdb::Environment::rollback`]). Under the writer
/// lock. `MDB_CANT_ROLLBACK` if `txnid` is not the newest snapshot or
/// there is none before it, as in C, and also (not in C) if the previous
/// meta page was itself rolled back (`KNOWN_C_BUGS.md` row 62) or a read
/// transaction still uses `txnid` (row 63). `EACCES` for a read-only
/// environment (C: `EBADF`, and the environment becomes unusable). `EINVAL`
/// for NULL or an environment that is not open (C would crash).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_rollback(env: *mut MDB_env, txnid: usize) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { open_env(env) }.map(MDB_env::open_env) {
        Ok(Some(e)) => crate::rc(e.rollback(txnid as u64)),
        Ok(None) => EINVAL,
        Err(code) => code,
    }
}

/// `mdb_env_set_encrypt`: not implemented. C 1.0's checks first: `EINVAL`
/// if `env`, `func` or `key` is NULL or the environment is open; else
/// `ENOTSUP`, and the environment stays unencrypted (`mdb_env_open`
/// creates a plain one, and refuses an encrypted 1.0 file with
/// `MDB_ENV_ENCRYPTION`).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_encrypt(
    env: *mut MDB_env,
    func: Option<MDB_enc_func>,
    key: *const MDB_val,
    _size: c_uint,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if func.is_none() || key.is_null() || e.open_env().is_some() {
        return EINVAL;
    }
    ENOTSUP
}

/// `mdb_env_set_checksum`: not implemented. C 1.0's checks first:
/// `EINVAL` if `env` or `func` is NULL, `size` is 0 or the environment is
/// open; else `ENOTSUP`, and the environment stays without checksums
/// (`mdb_env_open` refuses a checksummed 1.0 file with
/// `MDB_BAD_CHECKSUM`).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_set_checksum(
    env: *mut MDB_env,
    func: Option<MDB_sum_func>,
    size: c_uint,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if func.is_none() || size == 0 || e.open_env().is_some() {
        return EINVAL;
    }
    ENOTSUP
}

/// `mdb_env_incr_dumpfd` (incremental backup): not implemented. `EINVAL`
/// for NULL or an environment that is not open (C would crash), else
/// `ENOTSUP`; nothing is written to `fd`.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_incr_dumpfd(
    env: *mut MDB_env,
    _fd: mdb_filehandle_t,
    _txnid: usize,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { open_env(env) } {
        Ok(_) => ENOTSUP,
        Err(code) => code,
    }
}

/// `mdb_env_incr_dump` (incremental backup to a file): not implemented.
/// `EINVAL` for NULL `env` or `path` or an environment that is not open (C
/// would crash), else `ENOTSUP`; no file is created. (C 1.0 also sets
/// `MDB_NOSUBDIR` in the environment's flags.)
#[no_mangle]
pub unsafe extern "C" fn mdb_env_incr_dump(
    env: *mut MDB_env,
    path: *const c_char,
    _txnid: usize,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { open_env(env) } {
        Ok(_) if path.is_null() => EINVAL,
        Ok(_) => ENOTSUP,
        Err(code) => code,
    }
}

/// `mdb_env_incr_loadfd` (restore an incremental backup): not
/// implemented. `EINVAL` for NULL or an environment that is not open (C
/// would crash), else `ENOTSUP`; nothing is read from `fd`.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_incr_loadfd(env: *mut MDB_env, _fd: mdb_filehandle_t) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    match unsafe { open_env(env) } {
        Ok(_) => ENOTSUP,
        Err(code) => code,
    }
}

/// `mdb_modload` (`module.c`): load a crypto module. Not supported: loads
/// nothing, returns NULL and, if `errmsg` is not NULL, stores a static
/// message there ("rlmdb does not support LMDB crypto modules"), as C does
/// when its `dlopen` fails; `*mcf_ptr` is not touched, as in C. (C 1.0
/// would crash on a NULL `errmsg`.)
#[no_mangle]
pub unsafe extern "C" fn mdb_modload(
    _file: *const c_char,
    _symname: *const c_char,
    _mcf_ptr: *mut *mut MDB_crypto_funcs,
    errmsg: *mut *mut c_char,
) -> *mut c_void {
    // SAFETY: errmsg is NULL or writable (lmdb.h).
    if let Some(out) = unsafe { errmsg.as_mut() } {
        // The caller must not modify or free the message (C's comes from
        // dlerror() or is a literal).
        *out = MODLOAD_MESSAGE.as_ptr().cast_mut();
    }
    ptr::null_mut()
}

/// `mdb_modunload` (`module.c`): unload a crypto module. Does nothing:
/// [`mdb_modload`] never returns a handle.
#[no_mangle]
pub unsafe extern "C" fn mdb_modunload(_handle: *mut c_void) {}

/// `mdb_modsetup` (`module.c`): set an environment up with a crypto
/// module's functions, which C does by calling `mdb_env_set_checksum` (if
/// the module has a checksum function) and `mdb_env_set_encrypt` (if it
/// has an encryption function and `passphrase` is not NULL) and ignoring
/// their results. Neither is implemented and this function cannot report
/// that, so where C would set either up on an environment that is not
/// open, it records the request and `mdb_env_open` then fails with
/// `ENOTSUP`, instead of creating or opening an environment without the
/// encryption or checksums the program asked for. Nothing happens for
/// NULL arguments (C would crash), an open environment (C's calls fail
/// there) or a module without such functions. The module's
/// `mcf_str2key` is not called.
#[no_mangle]
pub unsafe extern "C" fn mdb_modsetup(
    env: *mut MDB_env,
    cf: *mut MDB_crypto_funcs,
    passphrase: *const c_char,
) {
    // SAFETY: env is NULL or a live handle; cf is NULL or points to a
    // valid MDB_crypto_funcs (lmdb.h).
    let (Some(e), Some(cf)) = (unsafe { MDB_env::from_ptr(env) }, unsafe { cf.as_ref() }) else {
        return;
    };
    if e.open_env().is_some() {
        return;
    }
    // mdb_env_set_checksum refuses a size of 0; mdb_env_set_encrypt is
    // only called with a passphrase.
    let checksums = cf.mcf_sumfunc.is_some() && cf.mcf_sumsize != 0;
    let encryption = cf.mcf_encfunc.is_some() && !passphrase.is_null();
    if checksums || encryption {
        e.config().crypto_requested = true;
    }
}
