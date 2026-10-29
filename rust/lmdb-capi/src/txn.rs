//! `lmdb.h` section "Transactions": `mdb_txn_*`.
//!
//! Only read-only transactions exist until the write path lands (stage 3):
//! `mdb_txn_begin` without `MDB_RDONLY` returns `EACCES` on a read-only
//! environment (as C) and the [`stage3_stub`] result otherwise.
//!
//! Read transactions are not bound to a thread: the C rule "one read
//! transaction per thread" (without `MDB_NOTLS`) is not enforced, which is
//! a superset of the C behaviour. Every transaction owns its reader slot
//! from begin to abort, as with `MDB_NOTLS` in C.

use std::os::raw::{c_int, c_uint};
use std::ptr::{self, NonNull};

use lmdb_core::consts::MDB_RDONLY;

use crate::erased::ErasedRoTxn;
use crate::handles::{MDB_env, MDB_txn, TxnState};
use crate::write::stage3_stub;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// `MDB_TXN_BEGIN_FLAGS` in 0.9.36: the only flag `mdb_txn_begin` uses.
const TXN_BEGIN_FLAGS: c_uint = MDB_RDONLY;

/// `mdb_txn_begin`: create a transaction for use with the environment.
///
/// `MDB_RDONLY` begins a read transaction. Other flags are ignored, as in
/// C. A `parent` must be a write transaction (`EINVAL` for a read-only
/// parent, as in C); nested and write transactions come with stage 3.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_begin(
    env: *mut MDB_env,
    parent: *mut MDB_txn,
    flags: c_uint,
    txn: *mut *mut MDB_txn,
) -> c_int {
    let Some(env_ptr) = NonNull::new(env) else {
        return EINVAL;
    };
    // SAFETY: env is a live handle (lmdb.h).
    let e = unsafe { env_ptr.as_ref() };
    let Some(open) = e.open_env() else {
        return EINVAL;
    };
    if txn.is_null() {
        return EINVAL;
    }
    let flags = flags & TXN_BEGIN_FLAGS;
    if open.flags().bits() & MDB_RDONLY & !flags != 0 {
        // Write txn in a read-only environment.
        return libc::EACCES;
    }
    // SAFETY: parent is NULL or a live handle (lmdb.h).
    if let Some(parent) = unsafe { MDB_txn::from_ptr(parent) } {
        // Nested transactions: write parents only.
        if parent.is_read_only() {
            return EINVAL;
        }
        return stage3_stub();
    }
    if flags & MDB_RDONLY == 0 {
        return stage3_stub();
    }
    // SAFETY: the environment lives in `e` until mdb_env_close, which
    // lmdb.h forbids while transactions are alive.
    match unsafe { ErasedRoTxn::begin(open) } {
        Ok(ro) => {
            let handle = Box::into_raw(Box::new(MDB_txn::new_read(env_ptr, ro)));
            // SAFETY: txn is non-NULL and writable (lmdb.h).
            unsafe { *txn = handle };
            SUCCESS
        }
        Err(err) => err.to_err_code(),
    }
}

/// `mdb_txn_env`: the transaction's environment (NULL for NULL).
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_env(txn: *mut MDB_txn) -> *mut MDB_env {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { MDB_txn::from_ptr(txn) }.map_or(ptr::null_mut(), MDB_txn::env_ptr)
}

/// `mdb_txn_id`: the transaction's ID (for a read transaction, the
/// snapshot it reads; kept after `mdb_txn_reset`). 0 for NULL.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_id(txn: *mut MDB_txn) -> usize {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { MDB_txn::from_ptr(txn) }.map_or(0, |t| t.id() as usize)
}

/// `mdb_txn_commit`: commit all the operations of a transaction. For a
/// read-only transaction (active or reset) this ends it and frees the
/// handle, as in C; database handles opened in it stay valid.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_commit(txn: *mut MDB_txn) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let Some(t) = (unsafe { MDB_txn::from_ptr(txn) }) else {
        return EINVAL;
    };
    match &*t.state() {
        TxnState::Read(_) | TxnState::Reset(_) | TxnState::Finished => {}
        TxnState::Write(w) => match *w {},
    }
    // SAFETY: txn is a live handle; lmdb.h forbids using it afterwards.
    unsafe { MDB_txn::end_and_free(txn) };
    SUCCESS
}

/// `mdb_txn_abort`: abandon all the operations of the transaction and free
/// the handle. Cursors of a read-only transaction stay allocated until
/// `mdb_cursor_close` (or can be renewed), as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_abort(txn: *mut MDB_txn) {
    if !txn.is_null() {
        // SAFETY: txn is a live handle; lmdb.h forbids using it afterwards.
        unsafe { MDB_txn::end_and_free(txn) };
    }
}

/// `mdb_txn_reset`: reset a read-only transaction: release its snapshot
/// but keep the reader slot for `mdb_txn_renew`. Ignored for NULL and
/// for write transactions.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_reset(txn: *mut MDB_txn) {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    if let Some(t) = unsafe { MDB_txn::from_ptr(txn) } {
        t.reset();
    }
}

/// `mdb_txn_renew`: renew a read-only transaction that was reset, on the
/// latest snapshot. `EINVAL` unless the transaction is a reset (or
/// failed-renew) read transaction. `MDB_MAP_RESIZED` if another process
/// grew the database beyond this environment's map.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_renew(txn: *mut MDB_txn) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) => t.renew(),
        None => EINVAL,
    }
}
