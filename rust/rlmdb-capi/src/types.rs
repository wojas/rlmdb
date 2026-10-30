//! The plain C types of `lmdb.h`: value, statistics and info structs, and
//! the callback function types, with the exact C layout.
//!
//! Callback parameters that C allows to be NULL are `Option<fn>` in the
//! exported signatures (Rust guarantees the same representation as a
//! nullable C function pointer).
//!
//! The callback types use the `"C-unwind"` ABI: the calling convention is
//! C's, but an exception that a C++ callback throws may unwind through the
//! library's Rust frames. Every exported function is `extern "C"`, so such
//! an exception aborts the process at the library boundary instead of
//! unwinding into the caller's C frames (with a plain `"C"` callback type it
//! would be undefined behaviour). A callback that `longjmp`s out of the
//! library skips Rust frames and is undefined behaviour. A callback that
//! frees the transaction or a cursor it was called for aborts the process
//! (see "Re-entrancy" in `crate::handles`).

use std::os::raw::{c_char, c_int, c_uint, c_void};

use crate::handles::MDB_env;

/// `mdb_mode_t`: Unix permissions for creating files (`mode_t`).
pub type mdb_mode_t = libc::mode_t;

/// `mdb_filehandle_t`: a file descriptor.
pub type mdb_filehandle_t = c_int;

/// `MDB_dbi`: a handle for an individual database in the environment.
pub type MDB_dbi = c_uint;

/// `MDB_cursor_op`: cursor get operations (a C enum, passed as an `int`).
pub type MDB_cursor_op = c_uint;

/// `MDB_val`: a generic structure used for passing keys and data in and
/// out of the database. Values returned by the library point into the
/// memory map (zero copy) and are valid until the transaction ends.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MDB_val {
    /// Size of the data item.
    pub mv_size: usize,
    /// Address of the data item.
    pub mv_data: *mut c_void,
}

impl MDB_val {
    /// A value describing `bytes` (the library never writes through it).
    pub(crate) fn from_slice(bytes: &[u8]) -> MDB_val {
        MDB_val {
            mv_size: bytes.len(),
            mv_data: bytes.as_ptr() as *mut c_void,
        }
    }
}

/// `MDB_stat`: statistics for a database in the environment.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MDB_stat {
    /// Size of a database page.
    pub ms_psize: c_uint,
    /// Depth (height) of the B-tree.
    pub ms_depth: c_uint,
    /// Number of internal (non-leaf) pages.
    pub ms_branch_pages: usize,
    /// Number of leaf pages.
    pub ms_leaf_pages: usize,
    /// Number of overflow pages.
    pub ms_overflow_pages: usize,
    /// Number of data items.
    pub ms_entries: usize,
}

impl From<rlmdb::Stat> for MDB_stat {
    fn from(s: rlmdb::Stat) -> MDB_stat {
        MDB_stat {
            ms_psize: s.page_size,
            ms_depth: s.depth,
            ms_branch_pages: s.branch_pages,
            ms_leaf_pages: s.leaf_pages,
            ms_overflow_pages: s.overflow_pages,
            ms_entries: s.entries,
        }
    }
}

/// `MDB_envinfo`: information about the environment.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MDB_envinfo {
    /// Address of map, if fixed (`mm_address` recorded in the meta page).
    pub me_mapaddr: *mut c_void,
    /// Size of the data memory map.
    pub me_mapsize: usize,
    /// ID of the last used page.
    pub me_last_pgno: usize,
    /// ID of the last committed transaction.
    pub me_last_txnid: usize,
    /// Max reader slots in the environment.
    pub me_maxreaders: c_uint,
    /// Max reader slots used in the environment.
    pub me_numreaders: c_uint,
}

/// `MDB_cmp_func`: a callback function used to compare two keys in a
/// database.
pub type MDB_cmp_func = unsafe extern "C-unwind" fn(a: *const MDB_val, b: *const MDB_val) -> c_int;

/// `MDB_rel_func`: a callback function used to relocate a position-dependent
/// data item in a fixed-address database. Never called (as in C 0.9).
pub type MDB_rel_func = unsafe extern "C-unwind" fn(
    item: *mut MDB_val,
    oldptr: *mut c_void,
    newptr: *mut c_void,
    relctx: *mut c_void,
);

/// `MDB_assert_func`: a callback function for most LMDB assert() failures.
pub type MDB_assert_func = unsafe extern "C-unwind" fn(env: *mut MDB_env, msg: *const c_char);

/// `MDB_msg_func`: a callback function used to print a message from the
/// library (`mdb_reader_list`).
pub type MDB_msg_func = unsafe extern "C-unwind" fn(msg: *const c_char, ctx: *mut c_void) -> c_int;

/// The bytes an input `MDB_val` describes, or `None` for a NULL pointer.
///
/// `Err(EINVAL)` for a value with a NULL `mv_data` and a non-zero size (C
/// would crash reading it).
///
/// # Safety
///
/// `val` is NULL or points to a valid `MDB_val` whose `mv_data` covers
/// `mv_size` readable bytes that stay unchanged for `'a`.
pub(crate) unsafe fn input<'a>(val: *const MDB_val) -> Result<Option<&'a [u8]>, c_int> {
    // SAFETY: per this function's contract.
    let Some(v) = (unsafe { val.as_ref() }) else {
        return Ok(None);
    };
    if v.mv_size == 0 {
        return Ok(Some(&[]));
    }
    if v.mv_data.is_null() {
        return Err(libc::EINVAL);
    }
    // SAFETY: non-NULL and, per this function's contract, covers mv_size
    // readable bytes for 'a.
    Ok(Some(unsafe {
        std::slice::from_raw_parts(v.mv_data as *const u8, v.mv_size)
    }))
}

/// Stores `bytes` in the output `MDB_val` at `out`, if `out` is not NULL.
///
/// # Safety
///
/// `out` is NULL or points to a writable `MDB_val`.
pub(crate) unsafe fn output(out: *mut MDB_val, bytes: &[u8]) {
    // SAFETY: per this function's contract.
    if let Some(v) = unsafe { out.as_mut() } {
        *v = MDB_val::from_slice(bytes);
    }
}
