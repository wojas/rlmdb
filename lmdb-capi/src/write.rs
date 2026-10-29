//! The write side of `lmdb.h`: `mdb_put`, `mdb_del`, `mdb_cursor_put`,
//! `mdb_cursor_del`, `mdb_drop` and `mdb_env_copy*`.
//!
//! The write path of the Rust core arrives with stage 3. Until then these
//! symbols exist so that C programs link. Each function performs C's
//! argument checks in C's order, so misuse gets the C error code today: in
//! particular every write through a read-only transaction fails with
//! `EACCES`, exactly as in C. What remains (a write through a write
//! transaction, which cannot be created yet, and environment copies) goes
//! to [`stage3_stub`].

use std::os::raw::{c_char, c_int, c_uint};

use lmdb::consts::{
    MDB_APPEND, MDB_APPENDDUP, MDB_DUPFIXED, MDB_MULTIPLE, MDB_NODUPDATA, MDB_NOOVERWRITE,
    MDB_RESERVE,
};
use lmdb::{Dbi, Transaction};

use crate::handles::{MDB_cursor, MDB_env, MDB_txn, TxnState};
use crate::types::*;

const EINVAL: c_int = libc::EINVAL;

/// The single placeholder for functionality that needs the write path of
/// the Rust core (stage 3): returns `ENOSYS` ("function not implemented").
///
/// Used by `mdb_txn_begin` for write and nested transactions and by
/// `mdb_env_copy`, `mdb_env_copyfd`, `mdb_env_copy2` and `mdb_env_copyfd2`.
#[cold]
pub fn stage3_stub() -> c_int {
    libc::ENOSYS
}

/// C's write-access check: `EACCES` for a read-only transaction (C checks
/// `MDB_TXN_RDONLY` before `MDB_TXN_BLOCKED`, so a reset one gets `EACCES`
/// too).
fn write_access(txn: &MDB_txn) -> c_int {
    match &*txn.state() {
        TxnState::Read(_) | TxnState::Reset(_) | TxnState::Finished => libc::EACCES,
        TxnState::Write(w) => match *w {},
    }
}

/// `TXN_DBI_EXIST(txn, dbi, DB_USRVALID)`: `EINVAL` unless `dbi` is a
/// user database handle of the (active) transaction.
fn user_dbi_exists(txn: &MDB_txn, dbi: MDB_dbi) -> bool {
    txn.with_read(EINVAL, |rt| {
        match rt.txn().db_flags(Dbi::from_raw(dbi)) {
            // C does not check for closed/reused handles here.
            Ok(_) | Err(lmdb::Error::Mdb(lmdb::MdbError::BadDbi)) => Ok(()),
            Err(_) => Err(EINVAL),
        }
    })
    .is_ok()
}

/// `mdb_put`: store items into a database. `EACCES` in a read-only
/// transaction, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_put(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    key: *mut MDB_val,
    data: *mut MDB_val,
    flags: c_uint,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if !key.is_null() && !data.is_null() && user_dbi_exists(t, dbi) => t,
        _ => return EINVAL,
    };
    if flags & !(MDB_NOOVERWRITE | MDB_NODUPDATA | MDB_RESERVE | MDB_APPEND | MDB_APPENDDUP) != 0 {
        return EINVAL;
    }
    write_access(t)
}

/// `mdb_del`: delete items from a database. `EACCES` in a read-only
/// transaction, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_del(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    key: *mut MDB_val,
    _data: *mut MDB_val,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if !key.is_null() && user_dbi_exists(t, dbi) => write_access(t),
        _ => EINVAL,
    }
}

/// `mdb_cursor_put`: store by cursor. `EACCES` in a read-only
/// transaction, after C's `MDB_MULTIPLE` checks (which, as in C, clear
/// `data[1].mv_size`).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_put(
    cursor: *mut MDB_cursor,
    key: *mut MDB_val,
    data: *mut MDB_val,
    flags: c_uint,
) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let c = match unsafe { MDB_cursor::from_ptr(cursor) } {
        Some(c) if !key.is_null() => c,
        _ => return EINVAL,
    };
    let Some(t) = c.txn() else {
        return EINVAL;
    };
    if flags & MDB_MULTIPLE != 0 {
        if data.is_null() {
            return EINVAL;
        }
        // SAFETY: with MDB_MULTIPLE, data points to an array of two values
        // (lmdb.h).
        let (first, second) = unsafe { (&*data, &mut *data.add(1)) };
        let count = second.mv_size;
        if count == 0 {
            return EINVAL;
        }
        second.mv_size = 0;
        if c.db_flags() & MDB_DUPFIXED == 0 {
            return lmdb::MdbError::Incompatible.to_err_code();
        }
        if first.mv_size.checked_mul(count).is_none() {
            return lmdb::MdbError::BadValSize.to_err_code();
        }
    }
    write_access(t)
}

/// `mdb_cursor_del`: delete current key/data pair. `EACCES` in a
/// read-only transaction, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_del(cursor: *mut MDB_cursor, _flags: c_uint) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    match unsafe { MDB_cursor::from_ptr(cursor) }.and_then(MDB_cursor::txn) {
        Some(t) => write_access(t),
        None => EINVAL,
    }
}

/// `mdb_drop`: empty or delete and close a database. `EACCES` in a
/// read-only transaction, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_drop(txn: *mut MDB_txn, dbi: MDB_dbi, del: c_int) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if (del as c_uint) <= 1 && user_dbi_exists(t, dbi) => write_access(t),
        _ => EINVAL,
    }
}

/// `mdb_env_copy`: copy an environment to the specified path.
/// Placeholder: [`stage3_stub`].
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copy(_env: *mut MDB_env, _path: *const c_char) -> c_int {
    stage3_stub()
}

/// `mdb_env_copyfd`: copy an environment to the specified file
/// descriptor. Placeholder: [`stage3_stub`].
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copyfd(_env: *mut MDB_env, _fd: mdb_filehandle_t) -> c_int {
    stage3_stub()
}

/// `mdb_env_copy2`: copy an environment to the specified path, with
/// options (`MDB_CP_COMPACT`). Placeholder: [`stage3_stub`].
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copy2(
    _env: *mut MDB_env,
    _path: *const c_char,
    _flags: c_uint,
) -> c_int {
    stage3_stub()
}

/// `mdb_env_copyfd2`: copy an environment to the specified file
/// descriptor, with options. Placeholder: [`stage3_stub`].
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copyfd2(
    _env: *mut MDB_env,
    _fd: mdb_filehandle_t,
    _flags: c_uint,
) -> c_int {
    stage3_stub()
}
