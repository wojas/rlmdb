//! A rewrite of [LMDB](http://www.lmdb.tech/) 0.9 in Rust that is binary
//! compatible with the C library: the data file (`data.mdb`) and the lock
//! file (`lock.mdb`) have exactly the C layout, so C and Rust processes can
//! use one environment at the same time.
//!
//! The crate is layered:
//!
//! * [`core`]: the byte-level API (environments, transactions, cursors,
//!   database handles), closely following the C API but with lifetimes
//!   instead of "do not use after" rules. Until the heed-compatible typed
//!   layer exists, its items are also re-exported at the crate root.
//! * [`format`](mod@format): parsing and writing of the on-disk structures over byte
//!   slices.
//! * [`consts`], [`flags`], [`error`]: the C numbers, typed flag sets and
//!   error codes.
//! * [`cmp`]: LMDB's comparison functions, which define the sort order.
//! * [`idl`]: sorted page-number lists (free lists, dirty lists).
//!
//! All `unsafe` code lives in the private `os` module (memory maps, file
//! locks, process-shared mutexes and semaphores). Everything else, including
//! all page parsing and B-tree logic, is safe Rust and never panics on
//! corrupt input: malformed pages yield `MdbError::Corrupted` or
//! `MdbError::PageNotFound`.
//!
//! # Example
//!
//! ```no_run
//! use lmdb::{Environment, Transaction};
//!
//! let env = Environment::builder().max_dbs(4).open("/path/to/env")?;
//! let txn = env.begin_ro_txn()?;
//! let db = txn.open_db(Some("users"))?;
//! if let Some(value) = txn.get(db, b"alice")? {
//!     println!("{} bytes", value.len());
//! }
//! let mut cursor = txn.cursor(db)?;
//! while let Some((key, value)) = cursor.next()? {
//!     println!("{key:?} => {value:?}");
//! }
//! # Ok::<(), lmdb::Error>(())
//! ```

// No unsafe code outside `os` (which opts back in with
// `#![allow(unsafe_code)]`); every other module additionally forbids it.
#![deny(unsafe_code)]
#![warn(missing_docs, clippy::all)]

#[forbid(unsafe_code)]
pub mod cmp;
#[forbid(unsafe_code)]
pub mod consts;
#[forbid(unsafe_code)]
pub mod core;
#[forbid(unsafe_code)]
pub mod error;
#[forbid(unsafe_code)]
pub mod flags;
#[forbid(unsafe_code)]
pub mod format;
#[forbid(unsafe_code)]
pub mod idl;
mod os;

pub use crate::core::*;
pub use crate::error::{strerror, BoxedError};

/// The LMDB version this crate is compatible with, as `mdb_version` reports
/// it: `(major, minor, patch)`.
pub fn lmdb_version() -> (i32, i32, i32) {
    (
        consts::MDB_VERSION_MAJOR,
        consts::MDB_VERSION_MINOR,
        consts::MDB_VERSION_PATCH,
    )
}
