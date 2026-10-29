//! `lmdb.h` sections "mdb_get" and "Cursors" (read side): `mdb_get`,
//! `mdb_cursor_open`, `mdb_cursor_close`, `mdb_cursor_renew`,
//! `mdb_cursor_txn`, `mdb_cursor_dbi`, `mdb_cursor_get`,
//! `mdb_cursor_count`.
//!
//! Returned keys and data point into the memory map (zero copy) and stay
//! valid until the transaction ends, as in C. In a write transaction they
//! may point into the transaction's dirty pages and are valid until the
//! next change in that transaction, as in C.
//!
//! Output parameters: `mdb_cursor_get` writes `key` and `data` (when not
//! NULL) on success exactly where C does. In particular `key` is left
//! untouched by the exact-match lookups, `MDB_GET_MULTIPLE`,
//! `MDB_FIRST_DUP`/`MDB_LAST_DUP` on a key with duplicates and
//! `MDB_PREV_MULTIPLE` on a positioned cursor.

use std::os::raw::{c_int, c_uint};
use std::ptr::{self, NonNull};

use lmdb_core::consts::{FREE_DBI, MDB_DUPSORT};
use lmdb_core::{CursorOp, Dbi, Transaction};

use crate::handles::{CursorAccess, CursorInner, MDB_cursor, MDB_txn};
use crate::types::*;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

fn not_found() -> c_int {
    lmdb_core::MdbError::NotFound.to_err_code()
}

/// `mdb_get`: get items from a database. For `MDB_DUPSORT` databases the
/// first data item of the key.
#[no_mangle]
pub unsafe extern "C" fn mdb_get(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    key: *mut MDB_val,
    data: *mut MDB_val,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if !key.is_null() && !data.is_null() => t,
        _ => return EINVAL,
    };
    // SAFETY: key points to a valid value (lmdb.h).
    let key = match unsafe { input(key) } {
        Ok(Some(k)) => k,
        Ok(None) => return EINVAL,
        Err(code) => return code,
    };
    let result = t.with_txn(EINVAL, |rt| {
        match rt.get(Dbi::from_raw(dbi), key) {
            Ok(Some(value)) => {
                // SAFETY: data is non-NULL and writable (lmdb.h); the
                // value lives in the map until the transaction ends.
                unsafe { output(data, value) };
                Ok(())
            }
            Ok(None) => Err(not_found()),
            Err(e) => Err(e.to_err_code()),
        }
    });
    result.err().unwrap_or(SUCCESS)
}

/// The persistent flags of `dbi` for a new cursor (the free DB has none
/// that matter here).
fn cursor_db_flags(txn: &dyn Transaction, dbi: MDB_dbi) -> c_uint {
    if dbi == FREE_DBI {
        return 0;
    }
    txn.db_flags(Dbi::from_raw(dbi)).map_or(0, |f| f.bits())
}

/// `mdb_cursor_open`: create a cursor handle. `EINVAL` for a reset
/// transaction (C: `TXN_DBI_EXIST` fails) and for the free DB in a write
/// transaction (as in C).
///
/// A cursor of a write transaction is closed (freed) when the transaction
/// ends, unless `mdb_cursor_close` closed it before, as in C; it stays
/// valid across all changes made in the transaction. A cursor of a
/// read-only transaction must be closed explicitly, also after its
/// transaction ended.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_open(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    cursor: *mut *mut MDB_cursor,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if !cursor.is_null() => t,
        _ => return EINVAL,
    };
    let opened = if t.is_read_only() {
        t.with_read(EINVAL, |rt| {
            // SAFETY: the cursor is registered with `t` below, which drops
            // it before resetting or ending the read transaction.
            let inner = unsafe { rt.cursor(Dbi::from_raw(dbi)) }.map_err(|e| e.to_err_code())?;
            let flags = cursor_db_flags(rt.txn(), dbi);
            Ok(MDB_cursor::new(txn, dbi, flags, CursorInner::Read(inner)))
        })
    } else {
        t.with_write(|w| {
            let id = w
                .open_cursor(Dbi::from_raw(dbi))
                .map_err(|e| e.to_err_code())?;
            let flags = cursor_db_flags(w, dbi);
            Ok(MDB_cursor::new(txn, dbi, flags, CursorInner::Write(id)))
        })
    };
    match opened {
        Ok(c) => {
            let handle = NonNull::from(Box::leak(Box::new(c)));
            t.register_cursor(handle);
            // SAFETY: cursor is non-NULL and writable (lmdb.h).
            unsafe { *cursor = handle.as_ptr() };
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `mdb_cursor_close`: close a cursor handle. For a read-only transaction
/// valid before or after the transaction ended; a cursor of a write
/// transaction must be closed before the transaction ends (it is freed
/// with it), as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_close(cursor: *mut MDB_cursor) {
    if let Some(c) = NonNull::new(cursor) {
        // SAFETY: a live handle from mdb_cursor_open; lmdb.h forbids using
        // it afterwards.
        unsafe { MDB_cursor::close(c) };
    }
}

/// `mdb_cursor_renew`: renew a cursor handle on a (new) read-only
/// transaction, for the same database. The previous transaction may be
/// live or ended. `EINVAL` for a cursor of a write transaction or a write
/// transaction `txn`, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_renew(txn: *mut MDB_txn, cursor: *mut MDB_cursor) -> c_int {
    let Some(handle) = NonNull::new(cursor) else {
        return EINVAL;
    };
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let Some(t) = (unsafe { MDB_txn::from_ptr(txn) }) else {
        return EINVAL;
    };
    // SAFETY: cursor is a live handle (lmdb.h).
    let c = unsafe { handle.as_ref() };
    let dbi = c.dbi();
    if c.write_id().is_some() || !t.is_read_only() {
        // C: C_UNTRACK cursor or a transaction with mt_cursors.
        return EINVAL;
    }
    let result = t.with_read(EINVAL, |rt| {
        // SAFETY: rebind registers the cursor with `t`, which drops it
        // before resetting or ending the read transaction.
        let inner = unsafe { rt.cursor(Dbi::from_raw(dbi)) }.map_err(|e| e.to_err_code())?;
        // SAFETY: both handles are live (lmdb.h).
        unsafe { MDB_cursor::rebind(handle, txn, inner) };
        Ok(())
    });
    result.err().unwrap_or(SUCCESS)
}

/// `mdb_cursor_txn`: the cursor's transaction handle (NULL once that
/// read-only transaction was committed or aborted).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_txn(cursor: *mut MDB_cursor) -> *mut MDB_txn {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    unsafe { MDB_cursor::from_ptr(cursor) }.map_or(ptr::null_mut(), MDB_cursor::txn_ptr)
}

/// `mdb_cursor_dbi`: the cursor's database handle (0 for NULL; C would
/// crash).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_dbi(cursor: *mut MDB_cursor) -> MDB_dbi {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    unsafe { MDB_cursor::from_ptr(cursor) }.map_or(0, MDB_cursor::dbi)
}

/// Whether C's `mdb_cursor_get` stores the key for `op`. It leaves the
/// caller's key alone for exact-match lookups and `MDB_GET_MULTIPLE`, for
/// `MDB_FIRST_DUP`/`MDB_LAST_DUP` when the item has duplicates (`has_dups`,
/// after the operation, which does not change the item), and for
/// `MDB_PREV_MULTIPLE` when the cursor was positioned before (`was_init`).
fn writes_key(
    op: CursorOp,
    was_init: impl FnOnce() -> bool,
    has_dups: impl FnOnce() -> bool,
) -> bool {
    match op {
        CursorOp::Set | CursorOp::GetBoth | CursorOp::GetBothRange | CursorOp::GetMultiple => false,
        CursorOp::FirstDup | CursorOp::LastDup => !has_dups(),
        CursorOp::PrevMultiple => !was_init(),
        _ => true,
    }
}

/// Whether `op` needs a `data` argument (`EINVAL` if NULL).
fn requires_data(op: CursorOp) -> bool {
    matches!(
        op,
        CursorOp::GetBoth
            | CursorOp::GetBothRange
            | CursorOp::GetMultiple
            | CursorOp::NextMultiple
            | CursorOp::PrevMultiple
            | CursorOp::FirstDup
            | CursorOp::LastDup
    )
}

/// A found key/data pair as `MDB_val`s.
fn raw_entry(e: Option<(&[u8], &[u8])>) -> Option<(MDB_val, MDB_val)> {
    e.map(|(k, d)| (MDB_val::from_slice(k), MDB_val::from_slice(d)))
}

/// `mdb_cursor_get`: retrieve by cursor.
///
/// `MDB_SET`, `MDB_SET_KEY`, `MDB_SET_RANGE`, `MDB_GET_BOTH` and
/// `MDB_GET_BOTH_RANGE` read `key` (and the `GET_BOTH` ones `data`); every
/// operation stores its results in the non-NULL arguments. `MDB_BAD_TXN`
/// if the transaction was reset; `EINVAL` if it ended.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_get(
    cursor: *mut MDB_cursor,
    key: *mut MDB_val,
    data: *mut MDB_val,
    op: MDB_cursor_op,
) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let Some(c) = (unsafe { MDB_cursor::from_ptr(cursor) }) else {
        return EINVAL;
    };
    let result = c.with_cursor(|access| {
        let op = CursorOp::from_raw(op).ok_or(EINVAL)?;
        if data.is_null() && requires_data(op) {
            return Err(EINVAL);
        }
        let key_in = match op {
            // SAFETY: key is NULL or a valid value (lmdb.h).
            CursorOp::Set
            | CursorOp::SetKey
            | CursorOp::SetRange
            | CursorOp::GetBoth
            | CursorOp::GetBothRange => unsafe { input(key) }?,
            _ => None,
        };
        let data_in = match op {
            // SAFETY: data is a valid value (checked non-NULL above).
            CursorOp::GetBoth | CursorOp::GetBothRange => unsafe { input(data) }?,
            _ => None,
        };
        // The values point into the map or the transaction's pages and stay
        // valid after the borrows below end, as in C.
        let (got, store_key) = match access {
            CursorAccess::Read(rc) => {
                let was_init = rc.is_initialized();
                let got = rc.get(key_in, data_in, op).map(raw_entry);
                (got, writes_key(op, || was_init, || rc.current_has_dups()))
            }
            CursorAccess::Write(w, id) => {
                let was_init = w.cursor_is_initialized(id).unwrap_or(false);
                let got = w.cursor_get(id, key_in, data_in, op).map(raw_entry);
                let store = writes_key(
                    op,
                    || was_init,
                    || w.cursor_current_has_dups(id).unwrap_or(false),
                );
                (got, store)
            }
        };
        match got {
            Ok(Some((k, d))) => {
                if store_key && !key.is_null() {
                    // SAFETY: key is non-NULL and writable (lmdb.h).
                    unsafe { *key = k };
                }
                // MDB_GET_MULTIPLE with nothing to fetch: C returns success
                // and leaves `data` untouched (KNOWN_C_BUGS.md #8).
                if !(op == CursorOp::GetMultiple && d.mv_size == 0) && !data.is_null() {
                    // SAFETY: data is non-NULL and writable (lmdb.h).
                    unsafe { *data = d };
                }
                Ok(())
            }
            Ok(None) => Err(not_found()),
            Err(e) => Err(e.to_err_code()),
        }
    });
    result.err().unwrap_or(SUCCESS)
}

/// `mdb_cursor_count`: return the count of duplicates for the current key
/// (`MDB_DUPSORT` only, `MDB_INCOMPATIBLE` otherwise).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_count(cursor: *mut MDB_cursor, countp: *mut usize) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let c = match unsafe { MDB_cursor::from_ptr(cursor) } {
        Some(c) if !countp.is_null() => c,
        _ => return EINVAL,
    };
    if c.db_flags() & MDB_DUPSORT == 0 {
        return lmdb_core::MdbError::Incompatible.to_err_code();
    }
    let result = c.with_cursor(|access| {
        match access {
            CursorAccess::Read(rc) => rc.count(),
            CursorAccess::Write(w, id) => w.cursor_count(id),
        }
        .map_err(|e| e.to_err_code())
    });
    match result {
        Ok(n) => {
            // SAFETY: countp is non-NULL and writable (lmdb.h).
            unsafe { *countp = n as usize };
            SUCCESS
        }
        Err(code) => code,
    }
}
