//! LMDB rewritten in Rust: a library that reads and writes the data file
//! (`data.mdb`) and the lock file (`lock.mdb`) of
//! [LMDB](http://www.lmdb.tech/) exactly as the C library does, so that C
//! and Rust processes can use one environment at the same time, reading
//! and writing, with the same reader table and the same writer lock.
//!
//! It is a rewrite, not a binding: no C code is linked. It handles the
//! files of both C releases, LMDB 0.9 (0.9.37) and LMDB 1.0 (1.0.3), each
//! with its own lock file ([`Format`]).
//!
//! This crate is the engine and its Rust API. The C API (`librlmdb`, and
//! stubs named `liblmdb` that stand in for C LMDB 0.9 and 1.0) is the
//! separate `rlmdb-capi` crate, the command line tools are `rlmdb-tools`;
//! both are built on this crate.
//!
//! The guides are at <https://rlmdb.wojas.nl/>: getting started, locking
//! and the opt-in features. `COMPATIBILITY.md` in the repository has the
//! exact compatibility contract.
//!
//! # Layers
//!
//! * [`core`]: the byte-level API: [`Environment`], read transactions
//!   ([`RoTxn`]), write transactions ([`RwTxn`], nested ones included),
//!   database handles ([`Dbi`]) and cursors ([`RoCursor`], [`RwCursor`],
//!   and the handle-based [`CursorId`] cursors the C API uses). It follows
//!   the C API closely, with lifetimes instead of "do not use after" rules.
//!   Its items are re-exported at the crate root. (A typed layer mirroring
//!   the `heed` crate is designed in `DESIGN.md` but not implemented.)
//! * [`format`](mod@format): parsing and writing the on-disk structures
//!   (pages, nodes, meta pages, `MDB_db` records, the lock file layout)
//!   over byte slices.
//! * [`consts`], [`flags`], [`error`]: the C numbers, typed flag sets
//!   ([`EnvironmentFlags`], [`DatabaseFlags`], [`WriteFlags`], with the C
//!   bit values) and the error codes ([`MdbError`], [`Error`]).
//! * [`cmp`]: LMDB's comparison functions, which define the sort order on
//!   disk and therefore must match C bit for bit.
//! * [`idl`]: sorted page-number lists (free lists, dirty lists).
//! * [`feature`]: opt-in features, enabled by name
//!   ([`EnvironmentBuilder::enable_feature`], [`features`]).
//! * [`version`]: which rlmdb this is, and the C release each format
//!   matches ([`version_info`]).
//!
//! # Data formats
//!
//! C LMDB 0.9 and 1.0 store data in different formats, and each opens
//! only its own. This crate opens both. An existing environment opens in
//! its format; [`sniff_format`] tells which one that is. A new one is
//! LMDB 0.9 unless the program asks for 1.0
//! ([`EnvironmentBuilder::format`]).
//!
//! An LMDB 1.0 environment allows longer keys and page sizes from 256
//! bytes to 64 KiB ([`EnvironmentBuilder::page_size`]). Of LMDB 1.0's
//! API, two-phase commit ([`RwTxn::prepare`], [`Environment::rollback`])
//! and [`EnvironmentFlags::PREV_SNAPSHOT`] work for environments of both
//! formats, incremental backups ([`Environment::incr_dump`]) for 1.0
//! environments. Encryption and page checksums are not implemented: files
//! that use them are refused. Nor is `MDB_VL32`.
//!
//! # Environment variables
//!
//! The Rust API reads no environment variable unless the program asks for
//! it, per environment, with [`EnvironmentBuilder::read_environment`] (the
//! C API and the tools do by default). Then an open reads, in this order:
//!
//! * `RLMDB_IGNORE_ENV` ([`IGNORE_ENV`]): if set to anything but the empty
//!   string, none of the others is read, whatever the program asked for;
//! * `RLMDB_FEATURES` ([`FEATURES_ENV`]): features to enable or disable
//!   ([`feature`]);
//! * `RLMDB_DEFAULT_FORMAT` ([`DEFAULT_FORMAT_ENV`], experimental): the
//!   format of a new environment that asks for none.
//!
//! What the program sets explicitly always wins over a variable. A setuid
//! or setgid process reads none of them (`secure_getenv` semantics).
//!
//! # Example
//!
//! ```
//! use rlmdb::{DatabaseFlags, Environment, Transaction, WriteFlags};
//!
//! let dir = tempfile::tempdir()?;
//! let env = Environment::builder()
//!     .map_size(16 << 20)
//!     .max_dbs(4)
//!     .open(dir.path())?;
//!
//! // A write transaction: create a named database and store some pairs.
//! let mut txn = env.begin_rw_txn()?;
//! let db = txn.create_db(Some("users"), DatabaseFlags::CREATE)?;
//! txn.put(db, b"alice", b"1", WriteFlags::empty())?;
//! txn.put(db, b"bob", b"2", WriteFlags::empty())?;
//!
//! // A nested transaction borrows its parent; its changes become the
//! // parent's when it commits ...
//! let mut child = txn.begin_nested_txn()?;
//! child.put(db, b"carol", b"3", WriteFlags::empty())?;
//! child.commit()?;
//! // ... and vanish when it aborts (or is dropped).
//! let mut child = txn.begin_nested_txn()?;
//! child.del(db, b"alice", None)?;
//! child.abort();
//!
//! // A write cursor borrows the transaction mutably.
//! let mut cursor = txn.cursor_mut(db)?;
//! cursor.put(b"dave", b"4", WriteFlags::empty())?;
//! drop(cursor);
//! txn.commit()?;
//!
//! // A read transaction pins a snapshot; the slices it returns borrow it
//! // and point straight into the memory map.
//! let txn = env.begin_ro_txn()?;
//! assert_eq!(txn.get(db, b"alice")?, Some(&b"1"[..]));
//! let mut cursor = txn.cursor(db)?;
//! let mut keys = Vec::new();
//! while let Some((key, _value)) = cursor.next()? {
//!     keys.push(key);
//! }
//! assert_eq!(keys, [&b"alice"[..], b"bob", b"carol", b"dave"]);
//! assert_eq!(cursor.set_range(b"c")?, Some((&b"carol"[..], &b"3"[..])));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Rules the types enforce
//!
//! The paragraphs of `lmdb.h` that say "do not ..." are compile-time or
//! run-time checks here:
//!
//! * Transactions borrow the [`Environment`], so it cannot be closed
//!   (dropped) while one is alive. Opening the same environment twice in
//!   one process fails with `EBUSY`: POSIX record locks belong to the
//!   process, and a second handle would release the first one's locks.
//! * With [`EnvironmentFlags::NO_LOCK`] there is no lock file and no
//!   cross-process locking, as in C, but within the process write
//!   transactions are still serialised and read transactions are tracked
//!   in memory: a writer never reuses pages a live [`RoTxn`] reads. Any
//!   number of `NO_LOCK` handles may open one environment (as in C); they
//!   share that writer lock and reader table. (C tracks no readers then;
//!   keeping other processes away stays the caller's job, as in C.)
//! * Slices returned by a [`RoTxn`] or its [`RoCursor`]s borrow the
//!   transaction and stay valid until it ends. Slices returned by a
//!   [`RwTxn`] borrow it immutably, and every change ([`RwTxn::put`],
//!   [`RwTxn::del`], ...) needs `&mut`, so no slice into a dirty page
//!   can outlive the change that moves the page:
//!
//!   ```compile_fail,E0502
//!   # use rlmdb::{Environment, Transaction, WriteFlags};
//!   # fn f(env: &Environment, db: rlmdb::Dbi) -> rlmdb::Result<()> {
//!   let mut txn = env.begin_rw_txn()?;
//!   let value = txn.get(db, b"key")?.unwrap();
//!   txn.put(db, b"other", b"x", WriteFlags::empty())?; // `txn` is borrowed by `value`
//!   println!("{value:?}");
//!   # Ok(()) }
//!   ```
//! * A nested transaction borrows its parent mutably, so the parent cannot
//!   be used until the child has ended, and an [`RwCursor`] (which borrows
//!   its transaction) cannot cross into a child. The handle-based cursors
//!   ([`RwTxn::open_cursor`], [`CursorId`]) behave like C's cursors across
//!   a child: usable through it, kept on commit, restored on abort (see
//!   [`RwTxn::begin_nested_txn`]).
//! * Dropping a transaction aborts it.
//! * A [`Dbi`] is a plain `Copy` handle, as in C. A closed or unknown handle
//!   gives `MdbError::BadDbi` or `EINVAL`, never undefined behaviour.
//! * Read transactions are not bound to a thread: [`RoTxn`] is `Send`, and
//!   a thread may hold several. This is C's `MDB_NOTLS` behaviour, which a
//!   C peer cannot tell apart (the lock file does not record it). An
//!   [`RwTxn`] is `!Send`: on Linux the writer lock is a pthread mutex
//!   owned by the thread that took it, and so is the mutex on Windows.
//! * Corrupt or truncated files give `MdbError::Corrupted` or
//!   `MdbError::PageNotFound`, never a panic or an out-of-bounds read.
//!
//! # `unsafe` policy
//!
//! All `unsafe` code lives in the private `os` module: the memory maps of
//! the data and lock files, the file locks, positional I/O and syncing,
//! and the locks that processes share (robust pthread mutexes on Linux
//! and FreeBSD, POSIX named semaphores on macOS, those or SysV semaphores
//! on the other BSDs, named mutexes on Windows). Every `unsafe` block has
//! a `// SAFETY:` comment. Every other module is `#[forbid(unsafe_code)]`:
//! page parsing, B-tree logic, the free list and the reader table are safe
//! Rust over byte slices and atomics. The one invariant the safe code
//! relies on is LMDB's own: pages reachable from a snapshot that a reader
//! slot pins (without a lock file, an entry of the environment's in-memory
//! reader table) are not modified by any process, C or Rust.
//!
//! # Platforms
//!
//! 64-bit targets only (a `compile_error!` elsewhere: the file format
//! depends on the size of `size_t`). The lock file is the one C LMDB
//! builds on the same platform, so an environment is shared between
//! programs of one platform ABI, in C or Rust.
//!
//! * Tested in CI: Linux with glibc on x86-64 and arm64 (robust pthread
//!   mutexes, 4 KiB pages), macOS on arm64 (POSIX named semaphores,
//!   16 KiB pages), Windows on x86-64 with MSVC and MinGW-w64 (named
//!   mutexes), and big-endian Linux (s390x) under emulation.
//! * FreeBSD (robust pthread mutexes) and OpenBSD (POSIX named
//!   semaphores, and SysV semaphores for LMDB 1.0 lock files) are tested
//!   by hand; FreeBSD also runs in CI every week.
//! * NetBSD, DragonFly and Android use the locks, sync calls and layouts
//!   of C's build there, and are untested (`COMPATIBILITY.md`).
//!
//! `MDB_FIXEDMAP`, `MDB_MAPASYNC` and `MDB_NOMEMINIT` are accepted and
//! ignored; `MDB_WRITEMAP` is accepted without a writable map, with C's
//! limits (no nested transactions), and refused for writing on OpenBSD
//! (see [`EnvironmentFlags`]).

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
pub mod feature;
#[forbid(unsafe_code)]
pub mod flags;
#[forbid(unsafe_code)]
pub mod format;
#[forbid(unsafe_code)]
pub mod idl;
mod os;
#[forbid(unsafe_code)]
pub mod version;

pub use crate::core::*;
pub use crate::error::{strerror, BoxedError};
pub use crate::feature::{
    features, Feature, FeatureDefault, FeatureInfo, FeatureRequests, FEATURES_ENV,
};
pub use crate::format::Format;
#[doc(hidden)]
pub use crate::os::{flush_file_maps, invalidate_file};
pub use crate::version::{version_info, VersionInfo};

/// What `mdb_version` reports where rlmdb stands in for C LMDB 0.9 (the
/// tools' `-V`, the C API's LMDB 0.9 stub), `(major, minor, patch)`:
/// `(0, 9, 85)`, the 0.9 series with a patch level above every real 0.9
/// release, so that a program can tell rlmdb from C LMDB. (The C API's
/// `librlmdb` reports the rlmdb version instead.) The C releases the
/// formats match and the rlmdb version are in [`version_info`].
pub fn lmdb_version() -> (i32, i32, i32) {
    (
        consts::MDB_VERSION_MAJOR,
        consts::MDB_VERSION_MINOR,
        consts::MDB_VERSION_PATCH,
    )
}
