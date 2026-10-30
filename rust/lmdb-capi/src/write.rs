//! The write side of `lmdb.h`: `mdb_put`, `mdb_del`, `mdb_cursor_put`,
//! `mdb_cursor_del`, `mdb_drop` and `mdb_env_copy*`.
//!
//! Each function performs C's argument checks in C's order and then calls
//! the write transaction of the core ([`lmdb_core::RwTxn`]), which repeats
//! the transaction-level checks (`MDB_BAD_TXN`, `MDB_BAD_VALSIZE`, ...) in
//! C's order. Every write through a read-only transaction fails with
//! `EACCES`, as in C.
//!
//! Output values follow C: `MDB_RESERVE` returns the reserved space in
//! `data` (valid until the next change in the transaction), a failed
//! `MDB_NOOVERWRITE` put returns the existing data in `data` together with
//! `MDB_KEYEXIST`, and `MDB_MULTIPLE` stores the number of items written in
//! `data[1].mv_size` (and advances `data[0].mv_data` as C does).
//!
//! Keys and data given to these functions may point into the
//! transaction's own pages (for example a key just returned by
//! `mdb_cursor_get` in the same write transaction, as `mtest.c` does). The
//! core changes those pages while it works, so such inputs are first copied
//! (keys always, data up to the largest size a leaf page can hold inline;
//! larger data lives on overflow pages, which C also requires not to be
//! passed back).

use std::os::raw::{c_char, c_int, c_uint};
use std::os::unix::ffi::OsStrExt;

use lmdb_core::consts::{
    MDB_APPEND, MDB_APPENDDUP, MDB_CURRENT, MDB_DUPFIXED, MDB_MAXKEYSIZE, MDB_MULTIPLE,
    MDB_NODUPDATA, MDB_NOOVERWRITE, MDB_RESERVE,
};
use lmdb_core::{Dbi, MdbError, RwTxn, Transaction, WriteFlags};

use crate::handles::{CursorAccess, MDB_cursor, MDB_env, MDB_txn};
use crate::types::*;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// Flags `mdb_put` accepts (others give `EINVAL`, as in C).
const PUT_FLAGS: c_uint =
    MDB_NOOVERWRITE | MDB_NODUPDATA | MDB_RESERVE | MDB_APPEND | MDB_APPENDDUP;

/// Flags `mdb_cursor_put` uses; C ignores all other bits.
const CURSOR_PUT_FLAGS: c_uint = PUT_FLAGS | MDB_CURRENT | MDB_MULTIPLE;

/// Data up to this size is copied before a put (see the module
/// documentation): half the largest page size, an upper bound for the
/// data of a node stored inline in a leaf page.
const COPY_DATA_MAX: usize = lmdb_core::consts::MAX_PAGESIZE / 2;

/// `MDB_MULTIPLE` items up to this size are copied before the put: a page,
/// the most that `MDB_GET_MULTIPLE` / `MDB_NEXT_MULTIPLE` return.
const MULTIPLE_COPY_MAX: usize = lmdb_core::consts::MAX_PAGESIZE;

/// `TXN_DBI_EXIST(txn, dbi, DB_USRVALID)` of a read-only transaction:
/// `false` unless `dbi` is a user database handle of the active
/// transaction.
fn user_dbi_exists(txn: &MDB_txn, dbi: MDB_dbi) -> bool {
    txn.with_txn(EINVAL, |rt| {
        match rt.db_flags(Dbi::from_raw(dbi)) {
            // C does not check for closed/reused handles here.
            Ok(_) | Err(lmdb_core::Error::Mdb(MdbError::BadDbi)) => Ok(()),
            Err(_) => Err(EINVAL),
        }
    })
    .is_ok()
}

/// A key copied out of the caller's memory (see the module documentation).
/// Keys longer than `MDB_MAXKEYSIZE` are not copied: the core refuses them
/// before it changes anything.
struct KeyCopy {
    buf: [u8; MDB_MAXKEYSIZE],
    len: usize,
}

impl KeyCopy {
    fn new(key: &[u8]) -> KeyCopy {
        let mut k = KeyCopy {
            buf: [0; MDB_MAXKEYSIZE],
            len: usize::MAX,
        };
        if let Some(dst) = k.buf.get_mut(..key.len()) {
            dst.copy_from_slice(key);
            k.len = key.len();
        }
        k
    }

    /// The key to pass on: the copy, or `original` if it was too long.
    fn get<'a>(&'a self, original: &'a [u8]) -> &'a [u8] {
        self.buf.get(..self.len).unwrap_or(original)
    }
}

/// Data copied out of the caller's memory if it could live inline in a
/// leaf page.
fn copy_data(data: &[u8]) -> Option<Vec<u8>> {
    (data.len() <= COPY_DATA_MAX).then(|| data.to_vec())
}

/// The key of a write operation (`EINVAL` if its pointer is NULL with a
/// non-zero size; C would crash).
///
/// # Safety
///
/// `key` points to a valid value.
unsafe fn key_input<'a>(key: *const MDB_val) -> Result<&'a [u8], c_int> {
    // SAFETY: per this function's contract.
    unsafe { input(key) }.map(Option::unwrap_or_default)
}

/// Turns `Result<(), code>` into a return code.
fn code(r: Result<(), c_int>) -> c_int {
    r.err().unwrap_or(SUCCESS)
}

/// After `MDB_KEYEXIST` from an `MDB_NOOVERWRITE` put, C returns the
/// existing data item of the key (the first duplicate) in `data`. Not with
/// `MDB_APPEND` (the key was compared with the last key only) or
/// `MDB_CURRENT` (nothing was looked up).
///
/// # Safety
///
/// `data` is writable.
unsafe fn report_existing(
    w: &RwTxn<'static>,
    db: Dbi,
    key: &[u8],
    flags: c_uint,
    data: *mut MDB_val,
) {
    if flags & MDB_NOOVERWRITE == 0 || flags & (MDB_APPEND | MDB_CURRENT) != 0 {
        return;
    }
    if let Ok(Some(existing)) = w.get(db, key) {
        // SAFETY: per this function's contract; the value lives in the
        // transaction until its next change, as in C.
        unsafe { output(data, existing) };
    }
}

/// The result of a put, with `MDB_KEYEXIST` reporting the existing data.
///
/// # Safety
///
/// `data` is writable.
unsafe fn put_result(
    w: &RwTxn<'static>,
    db: Dbi,
    key: &[u8],
    flags: c_uint,
    data: *mut MDB_val,
    result: lmdb_core::Result<()>,
) -> Result<(), c_int> {
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            if matches!(e, lmdb_core::Error::Mdb(MdbError::KeyExist)) {
                // SAFETY: per this function's contract.
                unsafe { report_existing(w, db, key, flags, data) };
            }
            Err(e.to_err_code())
        }
    }
}

/// `mdb_put`: store items into a database.
///
/// Flags: `MDB_NODUPDATA`, `MDB_NOOVERWRITE` (on `MDB_KEYEXIST`, `data`
/// returns the existing data), `MDB_RESERVE` (`data->mv_size` bytes are
/// reserved and `data->mv_data` returns their address, valid until the
/// next change in the transaction; not for `MDB_DUPSORT` databases, which
/// get `EINVAL`, see `KNOWN_C_BUGS.md` #16), `MDB_APPEND`,
/// `MDB_APPENDDUP`. Other flags give `EINVAL`. `EACCES` in a read-only
/// transaction.
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
        Some(t) if !key.is_null() && !data.is_null() => t,
        _ => return EINVAL,
    };
    if t.is_read_only() {
        if !user_dbi_exists(t, dbi) || flags & !PUT_FLAGS != 0 {
            return EINVAL;
        }
        return libc::EACCES;
    }
    // SAFETY: key and data point to valid values (lmdb.h).
    let (key_in, value) = match unsafe { (key_input(key), &mut *data) } {
        (Ok(k), v) => (k, v),
        (Err(code), _) => return code,
    };
    let key_copy = KeyCopy::new(key_in);
    let k = key_copy.get(key_in);
    let db = Dbi::from_raw(dbi);
    let wflags = WriteFlags::from_bits_retain(flags);
    code(t.with_write(|w| {
        if flags & MDB_RESERVE != 0 {
            let result = w.reserve(db, k, value.mv_size, wflags).map(|space| {
                value.mv_data = space.as_mut_ptr().cast();
            });
            // SAFETY: data is writable (lmdb.h).
            return unsafe { put_result(w, db, k, flags, data, result) };
        }
        // SAFETY: data points to a valid value (lmdb.h).
        let d = unsafe { input(data) }?.unwrap_or_default();
        let d_copy = copy_data(d);
        let result = w.put(db, k, d_copy.as_deref().unwrap_or(d), wflags);
        // SAFETY: data is writable (lmdb.h).
        unsafe { put_result(w, db, k, flags, data, result) }
    }))
}

/// `mdb_del`: delete items from a database. Without `data` (or for a
/// database without `MDB_DUPSORT`) all items of `key`; else only the
/// matching data item. `MDB_NOTFOUND` if there is nothing to delete,
/// `EACCES` in a read-only transaction.
#[no_mangle]
pub unsafe extern "C" fn mdb_del(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    key: *mut MDB_val,
    data: *mut MDB_val,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if !key.is_null() => t,
        _ => return EINVAL,
    };
    if t.is_read_only() {
        return if user_dbi_exists(t, dbi) {
            libc::EACCES
        } else {
            EINVAL
        };
    }
    // SAFETY: key points to a valid value, data is NULL or one (lmdb.h).
    let (key_in, data_in) = match unsafe { (key_input(key), input(data)) } {
        (Ok(k), Ok(d)) => (k, d),
        (Err(code), _) | (_, Err(code)) => return code,
    };
    let key_copy = KeyCopy::new(key_in);
    let data_copy = data_in.and_then(copy_data);
    let data_in = data_copy.as_deref().or(data_in);
    code(t.with_write(
        |w| match w.del(Dbi::from_raw(dbi), key_copy.get(key_in), data_in) {
            Ok(true) => Ok(()),
            Ok(false) => Err(MdbError::NotFound.to_err_code()),
            Err(e) => Err(e.to_err_code()),
        },
    ))
}

/// `mdb_cursor_put`: store by cursor, leaving the cursor on the new item.
///
/// Flags as [`mdb_put`], plus `MDB_CURRENT` (replace the item at the cursor
/// position; the key must be the current one) and `MDB_MULTIPLE` (store
/// `data[1].mv_size` items of `data[0].mv_size` bytes each from
/// `data[0].mv_data` as duplicates of `key`, `MDB_DUPFIXED` only; on return
/// `data[1].mv_size` is the number stored). Unknown flag bits are ignored,
/// as in C. `EACCES` in a read-only transaction, after C's `MDB_MULTIPLE`
/// checks (which, as in C, clear `data[1].mv_size`).
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_put(
    cursor: *mut MDB_cursor,
    key: *mut MDB_val,
    data: *mut MDB_val,
    flags: c_uint,
) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let c = match unsafe { MDB_cursor::from_ptr(cursor) } {
        Some(c) if !key.is_null() && !data.is_null() => c,
        _ => return EINVAL,
    };
    let Some(t) = c.txn() else {
        return EINVAL;
    };
    let flags = flags & CURSOR_PUT_FLAGS;
    let mut multiple = None;
    if flags & MDB_MULTIPLE != 0 {
        // SAFETY: with MDB_MULTIPLE, data points to an array of two values
        // (lmdb.h).
        let (first, second) = unsafe { (&*data, &mut *data.add(1)) };
        let count = second.mv_size;
        if count == 0 {
            return EINVAL;
        }
        second.mv_size = 0;
        if c.db_flags() & MDB_DUPFIXED == 0 {
            return MdbError::Incompatible.to_err_code();
        }
        if first.mv_size.checked_mul(count).is_none() {
            return MdbError::BadValSize.to_err_code();
        }
        multiple = Some((first.mv_size, count));
    }
    if t.is_read_only() {
        return libc::EACCES;
    }
    // SAFETY: key and data point to valid values (lmdb.h).
    let (key_in, value) = match unsafe { (key_input(key), &mut *data) } {
        (Ok(k), v) => (k, v),
        (Err(code), _) => return code,
    };
    let key_copy = KeyCopy::new(key_in);
    let k = key_copy.get(key_in);
    let wflags = WriteFlags::from_bits_retain(flags);
    code(c.with_cursor(|access| {
        let CursorAccess::Write(w, id) = access else {
            return Err(libc::EACCES);
        };
        let db = Dbi::from_raw(c.dbi());
        if let Some((size, count)) = multiple {
            if size == 0 {
                // C goes on to its size checks: MDB_BAD_VALSIZE for empty
                // duplicates (after MDB_BAD_TXN and the key checks).
                let flags = WriteFlags::from_bits_retain(flags & !MDB_MULTIPLE);
                return w.cursor_put(id, k, &[], flags).map_err(|e| e.to_err_code());
            }
            if value.mv_data.is_null() {
                return Err(EINVAL);
            }
            // SAFETY: data[0] covers count items of size bytes (lmdb.h);
            // the product does not overflow (checked above).
            let items =
                unsafe { std::slice::from_raw_parts(value.mv_data as *const u8, size * count) };
            // The items may be what MDB_GET_MULTIPLE returned from a dirty
            // page of this transaction, which the put changes: copy a
            // page's worth or less (see the module documentation).
            let items_copy = (items.len() <= MULTIPLE_COPY_MAX).then(|| items.to_vec());
            let items = items_copy.as_deref().unwrap_or(items);
            let stored = w
                .cursor_put_multiple(id, k, items, size, wflags)
                .map_err(|e| e.to_err_code())?;
            // C: the count, and data[0] advanced to the last item stored
            // (or to the first one not stored).
            let advanced = if stored == count { stored - 1 } else { stored };
            value.mv_data = value
                .mv_data
                .cast::<u8>()
                .wrapping_add(advanced * size)
                .cast();
            // SAFETY: data points to an array of two values (lmdb.h).
            unsafe { (*data.add(1)).mv_size = stored };
            return Ok(());
        }
        if flags & MDB_RESERVE != 0 {
            let result = w
                .cursor_reserve(id, k, value.mv_size, wflags)
                .map(|space| value.mv_data = space.as_mut_ptr().cast());
            // SAFETY: data is writable (lmdb.h).
            return unsafe { put_result(w, db, k, flags, data, result) };
        }
        // SAFETY: data points to a valid value (lmdb.h).
        let d = unsafe { input(data) }?.unwrap_or_default();
        let d_copy = copy_data(d);
        let result = w.cursor_put(id, k, d_copy.as_deref().unwrap_or(d), wflags);
        // SAFETY: data is writable (lmdb.h).
        unsafe { put_result(w, db, k, flags, data, result) }
    }))
}

/// `mdb_cursor_del`: delete the current key/data pair; with
/// `MDB_NODUPDATA`, all data items of the current key (`MDB_DUPSORT`).
/// Other flag bits are ignored, as in C. `EINVAL` if the cursor is not
/// positioned, `MDB_NOTFOUND` if it is past the end, `EACCES` in a
/// read-only transaction.
#[no_mangle]
pub unsafe extern "C" fn mdb_cursor_del(cursor: *mut MDB_cursor, flags: c_uint) -> c_int {
    // SAFETY: cursor is NULL or a live handle (lmdb.h).
    let Some(c) = (unsafe { MDB_cursor::from_ptr(cursor) }) else {
        return EINVAL;
    };
    match c.txn() {
        Some(t) if t.is_read_only() => return libc::EACCES,
        Some(_) => {}
        None => return EINVAL,
    }
    let wflags = WriteFlags::from_bits_retain(flags & MDB_NODUPDATA);
    code(c.with_cursor(|access| match access {
        CursorAccess::Write(w, id) => w.cursor_del(id, wflags).map_err(|e| e.to_err_code()),
        CursorAccess::Read(_) => Err(libc::EACCES),
    }))
}

/// `mdb_drop`: empty (`del == 0`) or delete and close (`del == 1`) a
/// database. All its pages go to the free list; cursors on it become
/// unpositioned. With `del == 1` the handle is closed in the environment at
/// once (also if the transaction aborts later), as in C; the main DB cannot
/// be deleted and is only emptied. `EACCES` in a read-only transaction,
/// `EINVAL` for other `del` values or the free DB, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_drop(txn: *mut MDB_txn, dbi: MDB_dbi, del: c_int) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { MDB_txn::from_ptr(txn) } {
        Some(t) if (del as c_uint) <= 1 => t,
        _ => return EINVAL,
    };
    if t.is_read_only() {
        return if user_dbi_exists(t, dbi) {
            libc::EACCES
        } else {
            EINVAL
        };
    }
    let db = Dbi::from_raw(dbi);
    let result = t.with_write(|w| {
        if del == 1 {
            w.drop_db(db)
        } else {
            w.clear_db(db)
        }
        .map_err(|e| e.to_err_code())
    });
    if result.is_ok() && del == 1 && dbi >= lmdb_core::consts::CORE_DBS {
        // C: mdb_dbi_close(env, dbi).
        t.env().forget_c_comparators(dbi);
    }
    code(result)
}

/// `mdb_env_copy2`/`mdb_env_copyfd2` flag: compact while copying.
const MDB_CP_COMPACT: c_uint = 0x01;

/// The open environment behind `env`, or `EINVAL` (C would crash on NULL
/// or misbehave before `mdb_env_open`).
///
/// # Safety
///
/// `env` is NULL or a live handle.
unsafe fn open_env<'a>(env: *mut MDB_env) -> Result<&'a lmdb_core::Environment, c_int> {
    // SAFETY: per this function's contract.
    unsafe { MDB_env::from_ptr(env) }
        .and_then(MDB_env::open_env)
        .ok_or(EINVAL)
}

/// `mdb_env_copy`: copy an environment to the specified path (a directory
/// that exists, or with `MDB_NOSUBDIR` the file name), which must not hold
/// a data file yet. The copy is a snapshot; writers may continue. No lock
/// file is created.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copy(env: *mut MDB_env, path: *const c_char) -> c_int {
    // SAFETY: arguments as documented in lmdb.h.
    unsafe { mdb_env_copy2(env, path, 0) }
}

/// `mdb_env_copyfd`: copy an environment to the specified file
/// descriptor (written from its current position; not closed, not synced).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copyfd(env: *mut MDB_env, fd: mdb_filehandle_t) -> c_int {
    // SAFETY: arguments as documented in lmdb.h.
    unsafe { mdb_env_copyfd2(env, fd, 0) }
}

/// `mdb_env_copy2`: copy an environment to the specified path, with
/// options: `MDB_CP_COMPACT` omits free pages and renumbers the others (the
/// copy's free list is empty). Other flag bits are ignored, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copy2(
    env: *mut MDB_env,
    path: *const c_char,
    flags: c_uint,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let e = match unsafe { open_env(env) } {
        Ok(e) if !path.is_null() => e,
        _ => return EINVAL,
    };
    // SAFETY: path is a NUL-terminated string (lmdb.h).
    let path = unsafe { std::ffi::CStr::from_ptr(path) };
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
    crate::rc(e.copy_to_path(path, flags & MDB_CP_COMPACT != 0))
}

/// `mdb_env_copyfd2`: copy an environment to the specified file
/// descriptor, with options (see [`mdb_env_copy2`]).
#[no_mangle]
pub unsafe extern "C" fn mdb_env_copyfd2(
    env: *mut MDB_env,
    fd: mdb_filehandle_t,
    flags: c_uint,
) -> c_int {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let e = match unsafe { open_env(env) } {
        Ok(e) if fd >= 0 => e,
        _ => return EINVAL,
    };
    // SAFETY: fd is an open file descriptor of the caller (lmdb.h), which
    // stays open for the duration of the call; it is only borrowed.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    crate::rc(e.copy_to_fd(fd, flags & MDB_CP_COMPACT != 0))
}
