//! The plain C types of `lmdb.h`: value, statistics and info structs, and
//! the callback function types, with the exact C layout; also the types
//! LMDB 1.0's `lmdb.h` adds for its crypto functions (`MDB_enc_func`,
//! `MDB_sum_func`, `MDB_str2key_func`, `MDB_crypto_funcs`; declared in
//! `include/lmdb.h`, LMDB 1.0's).
//!
//! Callback parameters that C allows to be NULL are `Option<fn>` in the
//! exported signatures (Rust guarantees the same representation as a
//! nullable C function pointer).
//!
//! The callback types use the `"C-unwind"` ABI: the calling convention is
//! C's, but a callback may unwind (a C++ exception, a Rust panic) into the
//! library (with a plain `"C"` callback type that would be undefined
//! behaviour). The library calls every callback through `callback::call`,
//! which aborts the process where the callback returned by unwinding,
//! before any frame of the library or the program is unwound. A callback
//! that `longjmp`s out of the library skips Rust frames and is undefined
//! behaviour. A callback that
//! frees the transaction or a cursor it was called for aborts the process
//! (see "Re-entrancy" in `crate::handles`).

use std::os::raw::{c_char, c_int, c_uint, c_void};

use crate::handles::MDB_env;

/// `mdb_mode_t`: Unix permissions for creating files (`mode_t`).
#[cfg(unix)]
pub type mdb_mode_t = libc::mode_t;

/// `mdb_mode_t` with MSVC: `int` (`lmdb.h`, `#ifdef _MSC_VER`). Ignored on
/// Windows, as in C.
#[cfg(all(windows, target_env = "msvc"))]
pub type mdb_mode_t = c_int;

/// `mdb_mode_t` with MinGW: its `mode_t`, an `unsigned short`. Ignored on
/// Windows, as in C.
#[cfg(all(windows, not(target_env = "msvc")))]
pub type mdb_mode_t = u16;

/// `mdb_filehandle_t`: a file descriptor.
#[cfg(unix)]
pub type mdb_filehandle_t = c_int;

/// `mdb_filehandle_t` on Windows: a `HANDLE`.
#[cfg(windows)]
pub type mdb_filehandle_t = *mut c_void;

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

/// `MDB_enc_func` (LMDB 1.0): a callback function used to encrypt or
/// decrypt pages. Never called: encryption is not implemented
/// (`mdb_env_set_encrypt` returns `ENOTSUP`).
pub type MDB_enc_func = unsafe extern "C-unwind" fn(
    src: *const MDB_val,
    dst: *mut MDB_val,
    key: *const MDB_val,
    encdec: c_int,
) -> c_int;

/// `MDB_sum_func` (LMDB 1.0): a callback function used to checksum pages.
/// Never called: checksums are not implemented (`mdb_env_set_checksum`
/// returns `ENOTSUP`).
pub type MDB_sum_func =
    unsafe extern "C-unwind" fn(src: *const MDB_val, dst: *mut MDB_val, key: *const MDB_val);

/// `MDB_str2key_func` (LMDB 1.0): a function that turns a password into
/// an encryption key. Never called.
pub type MDB_str2key_func =
    unsafe extern "C-unwind" fn(passwd: *const c_char, key: *mut MDB_val) -> c_int;

/// `MDB_crypto_funcs` (LMDB 1.0): what a crypto module returns, read by
/// `mdb_modsetup`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MDB_crypto_funcs {
    /// `mcf_str2key`: turns a password into a key.
    pub mcf_str2key: Option<MDB_str2key_func>,
    /// `mcf_encfunc`: the encryption function.
    pub mcf_encfunc: Option<MDB_enc_func>,
    /// `mcf_sumfunc`: the checksum function.
    pub mcf_sumfunc: Option<MDB_sum_func>,
    /// `mcf_keysize`: the size of an encryption key, in bytes.
    pub mcf_keysize: c_int,
    /// `mcf_esumsize`: the size of the MAC, for authenticated encryption.
    pub mcf_esumsize: c_int,
    /// `mcf_sumsize`: the size of the checksum, for plain checksums.
    pub mcf_sumsize: c_int,
}

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
