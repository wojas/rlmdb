//! `lmdb.h` section "Transactions": `mdb_txn_*`.
//!
//! Read transactions are not bound to a thread: the C rule "one read
//! transaction per thread" (without `MDB_NOTLS`) is not enforced, which is
//! a superset of the C behaviour. Every transaction owns its reader slot
//! from begin to abort, as with `MDB_NOTLS` in C.
//!
//! Write transactions follow C's thread rule: a write transaction must be
//! committed or aborted on the thread that began it, because it holds the
//! environment's writer lock, which on Linux is a process-shared pthread
//! mutex owned by the locking thread (on macOS a POSIX semaphore, which
//! would tolerate another thread, but programs must not rely on that). Only
//! one write transaction exists per environment at a time, across all
//! processes: `mdb_txn_begin` waits for the lock. Beginning a second write
//! transaction on a thread that already has one deadlocks, as in C.

use std::os::raw::{c_int, c_uint};
use std::ptr::{self, NonNull};

use lmdb_core::consts::MDB_RDONLY;

use crate::erased::{ErasedRoTxn, ErasedRwTxn};
use crate::handles::{MDB_env, MDB_txn};
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// `MDB_TXN_BEGIN_FLAGS` in 0.9.36: the only flag `mdb_txn_begin` uses.
const TXN_BEGIN_FLAGS: c_uint = MDB_RDONLY;

/// `mdb_txn_begin`: create a transaction for use with the environment.
///
/// `MDB_RDONLY` begins a read transaction; without it, the write
/// transaction (waiting for the writer lock; `EACCES` in a read-only
/// environment). Other flags are ignored, as in C. A write transaction must
/// end on the thread that began it (see the module documentation).
///
/// With a `parent`, a nested write transaction: it sees the parent's
/// changes, its own become the parent's when it commits and are discarded
/// when it aborts. While it is active the parent cannot be used
/// (`MDB_BAD_TXN`), so a parent has at most one child. `EINVAL` for a
/// read-only parent; `MDB_BAD_TXN` with `MDB_RDONLY` or a blocked parent,
/// as in C. Cursors of the parent cannot be used in the child (C lets the
/// child use them; known gap).
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
    if let Some(p) = unsafe { MDB_txn::from_ptr(parent) } {
        // Nested transactions: write parents only, one child at a time.
        return match p.begin_child(flags & MDB_RDONLY != 0) {
            Ok(child) => {
                let handle = Box::into_raw(Box::new(MDB_txn::new_write(env_ptr, child, parent)));
                p.set_child(handle);
                // SAFETY: txn is non-NULL and writable (lmdb.h).
                unsafe { *txn = handle };
                SUCCESS
            }
            Err(code) => code,
        };
    }
    let begun = if flags & MDB_RDONLY == 0 {
        // SAFETY: the environment lives in `e` until mdb_env_close, which
        // lmdb.h forbids while transactions are alive.
        unsafe { ErasedRwTxn::begin(open) }
            .map(|rw| MDB_txn::new_write(env_ptr, rw, ptr::null_mut()))
    } else {
        // SAFETY: as above.
        unsafe { ErasedRoTxn::begin(open) }.map(|ro| MDB_txn::new_read(env_ptr, ro))
    };
    match begun {
        Ok(t) => {
            let handle = Box::into_raw(Box::new(t));
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

/// `mdb_txn_commit`: commit all the operations of a transaction and free
/// the handle (also on failure, as in C: a failed commit aborts).
///
/// For a write transaction, its cursors are closed (freed) and databases
/// it created become visible to other transactions. A transaction that
/// hit an error that left it unusable (`MDB_MAP_FULL`, `MDB_TXN_FULL`,
/// `MDB_PAGE_FULL`, I/O errors, ...) returns `MDB_BAD_TXN`. For a read-only
/// transaction (active or reset) this ends it; database handles opened in
/// it stay valid.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_commit(txn: *mut MDB_txn) -> c_int {
    if txn.is_null() {
        return EINVAL;
    }
    // SAFETY: txn is a live handle; lmdb.h forbids using it afterwards.
    unsafe { MDB_txn::commit_and_free(txn) }
}

/// `mdb_txn_abort`: abandon all the operations of the transaction and free
/// the handle. NULL is ignored. Cursors of a write transaction are closed
/// (freed) with it; cursors of a read-only transaction stay allocated until
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
/// failed-renew) read transaction (so also for write transactions). `MDB_MAP_RESIZED` if another process
/// grew the database beyond this environment's map.
#[no_mangle]
pub unsafe extern "C" fn mdb_txn_renew(txn: *mut MDB_txn) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) => t.renew(),
        None => EINVAL,
    }
}
