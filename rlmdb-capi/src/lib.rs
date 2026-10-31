//! The C API of LMDB 0.9 (`lmdb.h`), implemented over the Rust [`rlmdb`]
//! crate: `liblmdb.a` / `liblmdb.so` / `liblmdb.dylib` built from this crate
//! are drop-in replacements for the C library, so existing C programs link
//! against them unchanged. `include/lmdb.h` is the unmodified 0.9.36 header;
//! `include/rlmdb.h` declares the few functions of rlmdb's own and the LMDB
//! 1.0 constants a program needs with a 1.0 environment.
//!
//! Environments of both data-file formats the core handles, LMDB 0.9 and
//! LMDB 1.0, are opened through the same `mdb_*` functions: an existing
//! file in its format, a new one in the format `rlmdb_env_set_format` (or
//! a default, see [`env::mdb_env_open`]) asks for.
//!
//! This layer is deliberately thin: argument validation (the same checks,
//! in the same order, with the same return codes as `mdb.c`), translation
//! between C and Rust types, and ownership of the handles. All database
//! logic lives in [`rlmdb::core`].
//!
//! # Modules (grouped like the sections of `lmdb.h`)
//!
//! * `mdb_version`, `mdb_strerror`, and `rlmdb.h`'s `rlmdb_version`,
//!   `rlmdb_version_string`, `rlmdb_has_capability` in [`version`].
//! * `rlmdb.h`'s data-file format functions (`rlmdb_env_set_format`,
//!   `rlmdb_env_get_format`, `rlmdb_sniff_format`,
//!   `rlmdb_set_default_format`, `rlmdb_env_set_pagesize`) in
//!   [`format`](mod@format).
//! * `mdb_env_*`, `mdb_reader_list`, `mdb_reader_check` in [`env`](mod@env).
//! * `mdb_txn_*` in [`txn`].
//! * `mdb_dbi_*`, `mdb_stat`, `mdb_set_compare` & co., `mdb_cmp`,
//!   `mdb_dcmp` in [`dbi`].
//! * `mdb_get`, `mdb_cursor_open`/`close`/`renew`/`get`/`count` & co. in
//!   [`cursor`], for read-only and write transactions.
//! * `mdb_put`, `mdb_del`, `mdb_cursor_put`, `mdb_cursor_del`, `mdb_drop`,
//!   `mdb_env_copy*` in [`write`](mod@write).
//! * The `#[repr(C)]` structs and callback types of the header in
//!   [`types`]; the opaque handles `MDB_env`, `MDB_txn`, `MDB_cursor` in
//!   [`handles`].
//! * Private: `erased`, the one place where Rust lifetimes are erased so
//!   that transactions and cursors can live in heap handles owned by C;
//!   `cmp`, adapting C comparison functions to Rust comparators.
//!
//! # Safety contract
//!
//! Every exported function is an `unsafe extern "C" fn` with the contract of
//! `lmdb.h`: handle arguments are NULL or live handles returned by this
//! library, `MDB_val` pointers are NULL or point to valid values whose
//! `mv_data` covers `mv_size` bytes, output pointers are NULL or writable.
//! Using a handle after it was freed is undefined behaviour, as in C (for
//! example closing an environment while transactions are alive). Where C
//! would dereference a NULL handle, this implementation returns `EINVAL`
//! instead. A panic inside an exported function aborts the process (the
//! Rust core does not panic on corrupt data; a panic would be a bug).

#![deny(unsafe_op_in_unsafe_fn, missing_docs)]
#![warn(clippy::all)]
// The C names of the header are kept on purpose (`MDB_env`, `MDB_val`, …).
#![allow(non_camel_case_types)]
// The safety contract of every exported function is the one documented in
// lmdb.h; it is stated once in the crate documentation above.
#![allow(clippy::missing_safety_doc)]

mod cmp;
pub mod cursor;
pub mod dbi;
pub mod env;
mod erased;
pub mod format;
pub mod handles;
mod platform;
pub mod txn;
pub mod types;
pub mod version;
pub mod write;

pub use handles::{MDB_cursor, MDB_env, MDB_txn};
pub use types::*;

use std::os::raw::c_int;

/// `MDB_SUCCESS`.
pub(crate) const SUCCESS: c_int = 0;

/// The C return code for a Rust result.
pub(crate) fn rc(result: rlmdb::Result<()>) -> c_int {
    match result {
        Ok(()) => SUCCESS,
        Err(e) => e.to_err_code(),
    }
}
