//! `lmdb.h` section "Database handles": `mdb_dbi_open`, `mdb_stat`,
//! `mdb_dbi_flags`, `mdb_dbi_close`, `mdb_set_compare`, `mdb_set_dupsort`,
//! `mdb_set_relfunc`, `mdb_set_relctx`, `mdb_cmp`, `mdb_dcmp`.
//!
//! C's `TXN_DBI_EXIST` check fails for a reset read transaction (C sets
//! its `mt_numdbs` to 0), so these functions return `EINVAL` there, except
//! `mdb_dbi_open`, which checks `MDB_TXN_BLOCKED` and returns `MDB_BAD_TXN`.
//!
//! Handles of databases created by a write transaction (`MDB_CREATE`) are
//! usable in that transaction at once and by other transactions after it
//! commits; if it aborts, the handle is closed (as C's `mdb_dbis_update`).

use std::cmp::Ordering;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint, c_void};

use rlmdb::consts::{FREE_DBI, MDB_CREATE, MDB_DUPSORT, MDB_INTEGERKEY, VALID_FLAGS};
use rlmdb::{Comparator, DatabaseFlags, Dbi, Transaction};

use crate::cmp::trampoline_for;
use crate::handles::{bad_txn, MDB_env, MDB_txn};
use crate::types::*;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// Turns `Result<(), code>` into a return code.
fn code(r: Result<(), c_int>) -> c_int {
    r.err().unwrap_or(SUCCESS)
}

/// The transaction behind `txn`, or `EINVAL` for NULL.
///
/// # Safety
///
/// `txn` is NULL or a live handle.
unsafe fn txn_ref<'a>(txn: *mut MDB_txn) -> Result<&'a MDB_txn, c_int> {
    // SAFETY: per this function's contract.
    unsafe { MDB_txn::from_ptr(txn) }.ok_or(EINVAL)
}

/// `mdb_dbi_open`: open a database in the environment.
///
/// Without `MDB_CREATE` this opens an existing database (`MDB_NOTFOUND`
/// otherwise). With `MDB_CREATE` a missing database is created in a write
/// transaction (and gives `EACCES` in a read-only one, as in C). The flags
/// of an existing database are taken from the file; other flags in `flags`
/// are ignored, as in C. For the main DB (`name == NULL`) a write
/// transaction adds the persistent flags to its record, as in C; in a
/// read-only transaction they are ignored (C would OR them into the
/// transaction's in-memory copy of the main DB record).
#[no_mangle]
pub unsafe extern "C" fn mdb_dbi_open(
    txn: *mut MDB_txn,
    name: *const c_char,
    flags: c_uint,
    dbi: *mut MDB_dbi,
) -> c_int {
    if flags & !VALID_FLAGS != 0 {
        return EINVAL;
    }
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { txn_ref(txn) } {
        Ok(t) if !dbi.is_null() => t,
        _ => return EINVAL,
    };
    let name = if name.is_null() {
        None
    } else {
        // SAFETY: name is a NUL-terminated string (lmdb.h).
        Some(unsafe { CStr::from_ptr(name) }.to_bytes())
    };
    let opened = if t.is_read_only() {
        t.with_read(bad_txn(), |rt| open_in_read_txn(rt.txn(), name, flags))
    } else {
        t.with_write(|w| {
            w.create_db_bytes(name, DatabaseFlags::from_bits_retain(flags))
                .map_err(|e| e.to_err_code())
        })
    };
    match opened {
        Ok(db) => {
            // SAFETY: dbi is non-NULL and writable (lmdb.h).
            unsafe { *dbi = db.to_raw() };
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `mdb_dbi_open` in a read-only transaction: existing databases only.
fn open_in_read_txn(
    txn: &dyn Transaction,
    name: Option<&[u8]>,
    flags: c_uint,
) -> Result<Dbi, c_int> {
    let Some(name) = name else {
        return Ok(Dbi::MAIN);
    };
    match txn.open_db_bytes(Some(name)) {
        Ok(db) => Ok(db),
        Err(e) if e.not_found() && flags & MDB_CREATE != 0 => {
            // C: a main DB with these flags cannot hold named DBs.
            let main = txn.db_flags(Dbi::MAIN).map_err(|e| e.to_err_code())?;
            if main.bits() & (MDB_DUPSORT | MDB_INTEGERKEY) != 0 {
                Err(rlmdb::MdbError::Incompatible.to_err_code())
            } else {
                // Creating a database needs a write transaction.
                Err(libc::EACCES)
            }
        }
        Err(e) => Err(e.to_err_code()),
    }
}

/// `mdb_stat`: retrieve statistics for a database.
#[no_mangle]
pub unsafe extern "C" fn mdb_stat(txn: *mut MDB_txn, dbi: MDB_dbi, stat: *mut MDB_stat) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { txn_ref(txn) } {
        Ok(t) if !stat.is_null() => t,
        _ => return EINVAL,
    };
    let result = t.with_txn(EINVAL, |rt| {
        rt.stat(Dbi::from_raw(dbi)).map_err(|e| e.to_err_code())
    });
    match result {
        Ok(s) => {
            // SAFETY: stat is non-NULL and writable (lmdb.h).
            unsafe { *stat = s.into() };
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `mdb_dbi_flags`: retrieve the persistent flags of a database (not for
/// the free-page DB, as in C).
#[no_mangle]
pub unsafe extern "C" fn mdb_dbi_flags(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    flags: *mut c_uint,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    let t = match unsafe { txn_ref(txn) } {
        Ok(t) if !flags.is_null() => t,
        _ => return EINVAL,
    };
    let result = t.with_txn(EINVAL, |rt| {
        rt.db_flags(Dbi::from_raw(dbi)).map_err(|e| e.to_err_code())
    });
    match result {
        Ok(f) => {
            // SAFETY: flags is non-NULL and writable (lmdb.h).
            unsafe { *flags = f.bits() };
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `mdb_dbi_close`: close a database handle. Later use of the handle in
/// any transaction fails with `MDB_BAD_DBI`. Core handles and unknown
/// handles are ignored, as in C.
#[no_mangle]
pub unsafe extern "C" fn mdb_dbi_close(env: *mut MDB_env, dbi: MDB_dbi) {
    // SAFETY: env is NULL or a live handle (lmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return;
    };
    if let Some(open) = e.open_env() {
        open.close_db(Dbi::from_raw(dbi));
        if dbi >= rlmdb::consts::CORE_DBS {
            e.forget_c_comparators(dbi);
        }
    }
}

/// Installs a C comparator on `dbi` (shared code of `mdb_set_compare` and
/// `mdb_set_dupsort`). A NULL `cmp` restores the default order (C would
/// store NULL and crash on the next comparison).
///
/// # Safety
///
/// `txn` is NULL or a live handle.
unsafe fn set_comparator(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    cmp: Option<MDB_cmp_func>,
    dup: bool,
) -> c_int {
    // SAFETY: per this function's contract.
    let t = match unsafe { txn_ref(txn) } {
        Ok(t) => t,
        Err(code) => return code,
    };
    let result = t.with_txn(EINVAL, |rt| {
        let rust_cmp = match cmp {
            Some(f) => Some(trampoline_for(f).ok_or(libc::ENOMEM)?),
            None => None,
        };
        let db = Dbi::from_raw(dbi);
        let set = if dup {
            rt.set_dupsort(db, rust_cmp)
        } else {
            rt.set_compare(db, rust_cmp)
        };
        set.map_err(|e| e.to_err_code())
    });
    if result.is_ok() {
        t.env().update_c_comparators(dbi, |c| {
            if dup {
                c.dup = cmp;
            } else {
                c.key = cmp;
            }
        });
    }
    code(result)
}

/// `mdb_set_compare`: set a custom key comparison function for a database.
///
/// As in C, the function belongs to the environment's handle: it takes
/// effect at once in `txn` and in every transaction that uses the handle
/// later (cursors opened before keep the order they started with). Call it
/// before anything is read or written with the handle, in every process
/// that uses the database (`lmdb.h`). A NULL function restores the default
/// order (C would crash on the next comparison).
#[no_mangle]
pub unsafe extern "C" fn mdb_set_compare(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    cmp: Option<MDB_cmp_func>,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { set_comparator(txn, dbi, cmp, false) }
}

/// `mdb_set_dupsort`: set a custom data comparison function for an
/// `MDB_DUPSORT` database. Same rules as [`mdb_set_compare`].
#[no_mangle]
pub unsafe extern "C" fn mdb_set_dupsort(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    cmp: Option<MDB_cmp_func>,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { set_comparator(txn, dbi, cmp, true) }
}

/// Checks `dbi` like C's `TXN_DBI_EXIST(txn, dbi, DB_USRVALID)`.
///
/// # Safety
///
/// `txn` is NULL or a live handle.
unsafe fn check_user_dbi(txn: *mut MDB_txn, dbi: MDB_dbi) -> c_int {
    // SAFETY: per this function's contract.
    let t = match unsafe { txn_ref(txn) } {
        Ok(t) => t,
        Err(code) => return code,
    };
    code(t.with_txn(EINVAL, |rt| {
        match rt.db_flags(Dbi::from_raw(dbi)) {
            // C does not check for closed/reused handles here.
            Ok(_) | Err(rlmdb::Error::Mdb(rlmdb::MdbError::BadDbi)) => Ok(()),
            Err(_) => Err(EINVAL),
        }
    }))
}

/// `mdb_set_relfunc`: set a relocation function for a `MDB_FIXEDMAP`
/// database. Accepted and ignored: LMDB 0.9 never calls it either.
#[no_mangle]
pub unsafe extern "C" fn mdb_set_relfunc(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    _rel: Option<MDB_rel_func>,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { check_user_dbi(txn, dbi) }
}

/// `mdb_set_relctx`: set a context pointer for the relocation function.
/// Accepted and ignored, as [`mdb_set_relfunc`].
#[no_mangle]
pub unsafe extern "C" fn mdb_set_relctx(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    _ctx: *mut c_void,
) -> c_int {
    // SAFETY: txn is NULL or a live handle (lmdb.h).
    unsafe { check_user_dbi(txn, dbi) }
}

/// The C sign convention for an ordering.
fn sign(o: Ordering) -> c_int {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// The built-in comparator of `dbi` in `t`, as `mdb_default_cmp` chooses it
/// from the database's flags; `None` if there is none (`mdb_dcmp` on a
/// database without `MDB_DUPSORT`: C would call a NULL function) or the
/// transaction is not active. `a_len` is the size of the first operand.
fn default_comparator(t: &MDB_txn, dbi: MDB_dbi, dup: bool, a_len: usize) -> Option<Comparator> {
    if dbi == FREE_DBI {
        // me_dbxs[FREE_DBI].md_cmp = mdb_cmp_long; no duplicates.
        return (!dup).then_some(Comparator::Long);
    }
    let flags = t
        .with_txn(EINVAL, |rt| {
            rt.db_flags(Dbi::from_raw(dbi)).map_err(|e| e.to_err_code())
        })
        .ok()?
        .bits();
    if !dup {
        return Some(Comparator::default_key(flags));
    }
    // mdb_dcmp: mdb_cmp_int on size_t-sized items becomes mdb_cmp_clong.
    Comparator::default_dup(flags).map(|c| {
        if c.is_int() && a_len == std::mem::size_of::<usize>() {
            Comparator::Clong
        } else {
            c
        }
    })
}

/// Shared code of `mdb_cmp` and `mdb_dcmp`: compares `a` and `b` with the
/// comparator of `dbi`: the C function installed for it, else the
/// built-in one.
///
/// # Safety
///
/// `txn` is NULL or a live handle; `a` and `b` are NULL or valid values.
unsafe fn compare(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    a: *const MDB_val,
    b: *const MDB_val,
    dup: bool,
) -> c_int {
    // SAFETY: per this function's contract.
    let Ok(t) = (unsafe { txn_ref(txn) }) else {
        return 0;
    };
    // SAFETY: per this function's contract.
    let (Ok(Some(sa)), Ok(Some(sb))) = (unsafe { input(a) }, unsafe { input(b) }) else {
        return 0;
    };
    let installed = t.env().c_comparators(dbi);
    if let Some(f) = if dup { installed.dup } else { installed.key } {
        // SAFETY: f is the comparator the program installed; a and b are
        // valid values (checked above).
        return unsafe { f(a, b) };
    }
    default_comparator(t, dbi, dup, sa.len()).map_or(0, |c| sign(c.compare(sa, sb)))
}

/// `mdb_cmp`: compare two keys according to a particular database.
///
/// Returns <0, 0 or >0 like C; the built-in orders return exactly -1, 0
/// or 1 (C's may return other negative or positive values). Invalid
/// arguments give 0 (C would crash).
#[no_mangle]
pub unsafe extern "C" fn mdb_cmp(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    a: *const MDB_val,
    b: *const MDB_val,
) -> c_int {
    // SAFETY: arguments as documented in lmdb.h.
    unsafe { compare(txn, dbi, a, b, false) }
}

/// `mdb_dcmp`: compare two data items according to a particular database
/// (`MDB_DUPSORT` only). Same conventions as [`mdb_cmp`].
#[no_mangle]
pub unsafe extern "C" fn mdb_dcmp(
    txn: *mut MDB_txn,
    dbi: MDB_dbi,
    a: *const MDB_val,
    b: *const MDB_val,
) -> c_int {
    // SAFETY: arguments as documented in lmdb.h.
    unsafe { compare(txn, dbi, a, b, true) }
}
