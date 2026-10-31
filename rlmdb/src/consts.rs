//! The numeric constants of C LMDB 0.9: flag bits, error codes, cursor
//! operation numbers and format limits.
//!
//! Everything here has exactly the value used by `lmdb.h` and `mdb.c`
//! (version 0.9.36). The C API crate exports these numbers unchanged, and the
//! on-disk and lock-file formats depend on several of them, so they must never
//! be "tidied up".

use std::os::raw::c_int;

// ---------------------------------------------------------------------------
// Environment flags (`mdb_env_open`), lmdb.h "mdb_env Environment Flags".
// ---------------------------------------------------------------------------

/// `MDB_FIXEDMAP`: map the data file at a fixed address (accepted, ignored).
pub const MDB_FIXEDMAP: u32 = 0x01;
/// `MDB_NOSUBDIR`: the path names the data file; the lock file is `path-lock`.
pub const MDB_NOSUBDIR: u32 = 0x4000;
/// `MDB_NOSYNC`: do not fsync after commit.
pub const MDB_NOSYNC: u32 = 0x10000;
/// `MDB_RDONLY`: open the environment read-only.
pub const MDB_RDONLY: u32 = 0x20000;
/// `MDB_NOMETASYNC`: do not fsync the meta page after commit.
pub const MDB_NOMETASYNC: u32 = 0x40000;
/// `MDB_WRITEMAP`: use a writable memory map (accepted, ignored).
pub const MDB_WRITEMAP: u32 = 0x80000;
/// `MDB_MAPASYNC`: asynchronous msync with `MDB_WRITEMAP` (accepted, ignored).
pub const MDB_MAPASYNC: u32 = 0x100000;
/// `MDB_NOTLS`: reader slots are tied to transactions, not threads.
pub const MDB_NOTLS: u32 = 0x200000;
/// `MDB_NOLOCK`: do not use the lock file at all.
pub const MDB_NOLOCK: u32 = 0x400000;
/// `MDB_NORDAHEAD`: `madvise(MADV_RANDOM)` on the data map.
pub const MDB_NORDAHEAD: u32 = 0x800000;
/// `MDB_NOMEMINIT`: do not zero malloc'd pages (accepted, ignored).
pub const MDB_NOMEMINIT: u32 = 0x1000000;
/// `MDB_PREVSNAPSHOT` from LMDB 1.0 / mdb.master3. Not part of 0.9; rejected.
pub const MDB_PREVSNAPSHOT: u32 = 0x2000000;

/// Env flags that may be changed on an open environment (`CHANGEABLE`).
pub const CHANGEABLE: u32 = MDB_NOSYNC | MDB_NOMETASYNC | MDB_MAPASYNC | MDB_NOMEMINIT;
/// Env flags that can only be given at open time (`CHANGELESS`).
pub const CHANGELESS: u32 = MDB_FIXEDMAP
    | MDB_NOSUBDIR
    | MDB_RDONLY
    | MDB_WRITEMAP
    | MDB_NOTLS
    | MDB_NOLOCK
    | MDB_NORDAHEAD;

// ---------------------------------------------------------------------------
// Database flags (`mdb_dbi_open`), lmdb.h "mdb_dbi_open Database Flags".
// ---------------------------------------------------------------------------

/// `MDB_REVERSEKEY`: keys compare from the last byte backwards.
pub const MDB_REVERSEKEY: u32 = 0x02;
/// `MDB_DUPSORT`: sorted duplicate data items per key.
pub const MDB_DUPSORT: u32 = 0x04;
/// `MDB_INTEGERKEY`: keys are native `unsigned int` or `size_t`.
pub const MDB_INTEGERKEY: u32 = 0x08;
/// `MDB_DUPFIXED`: all duplicates have the same size (enables LEAF2 pages).
pub const MDB_DUPFIXED: u32 = 0x10;
/// `MDB_INTEGERDUP`: duplicates are native integers.
pub const MDB_INTEGERDUP: u32 = 0x20;
/// `MDB_REVERSEDUP`: duplicates compare from the last byte backwards.
pub const MDB_REVERSEDUP: u32 = 0x40;
/// `MDB_CREATE`: create the named database if it does not exist.
pub const MDB_CREATE: u32 = 0x40000;

/// `MDB_VALID` (mdb.c): marks a valid entry in the environment's DBI table.
pub const MDB_VALID: u32 = 0x8000;
/// `PERSISTENT_FLAGS` (mdb.c): DB flags that are stored in `MDB_db.md_flags`.
pub const PERSISTENT_FLAGS: u32 = 0xffff & !MDB_VALID;
/// `VALID_FLAGS` (mdb.c): flags accepted by `mdb_dbi_open`.
pub const VALID_FLAGS: u32 = MDB_REVERSEKEY
    | MDB_DUPSORT
    | MDB_INTEGERKEY
    | MDB_DUPFIXED
    | MDB_INTEGERDUP
    | MDB_REVERSEDUP
    | MDB_CREATE;

// ---------------------------------------------------------------------------
// Write flags (`mdb_put`, `mdb_cursor_put`), lmdb.h "mdb_put Write Flags".
// ---------------------------------------------------------------------------

/// `MDB_NOOVERWRITE`: do not overwrite an existing key.
pub const MDB_NOOVERWRITE: u32 = 0x10;
/// `MDB_NODUPDATA`: do not add a duplicate key/data pair.
pub const MDB_NODUPDATA: u32 = 0x20;
/// `MDB_CURRENT`: overwrite the item at the cursor position.
pub const MDB_CURRENT: u32 = 0x40;
/// `MDB_RESERVE`: reserve space and return a pointer to it.
pub const MDB_RESERVE: u32 = 0x10000;
/// `MDB_APPEND`: the key is known to sort after all existing keys.
pub const MDB_APPEND: u32 = 0x20000;
/// `MDB_APPENDDUP`: as `MDB_APPEND`, for duplicate data.
pub const MDB_APPENDDUP: u32 = 0x40000;
/// `MDB_MULTIPLE`: store multiple contiguous `MDB_DUPFIXED` items.
pub const MDB_MULTIPLE: u32 = 0x80000;

/// `MDB_CP_COMPACT`: compacting copy (`mdb_env_copy2`).
pub const MDB_CP_COMPACT: u32 = 0x01;

// ---------------------------------------------------------------------------
// Cursor operations (`MDB_cursor_op`), in enum order.
// ---------------------------------------------------------------------------

/// `MDB_FIRST`
pub const MDB_FIRST: u32 = 0;
/// `MDB_FIRST_DUP`
pub const MDB_FIRST_DUP: u32 = 1;
/// `MDB_GET_BOTH`
pub const MDB_GET_BOTH: u32 = 2;
/// `MDB_GET_BOTH_RANGE`
pub const MDB_GET_BOTH_RANGE: u32 = 3;
/// `MDB_GET_CURRENT`
pub const MDB_GET_CURRENT: u32 = 4;
/// `MDB_GET_MULTIPLE`
pub const MDB_GET_MULTIPLE: u32 = 5;
/// `MDB_LAST`
pub const MDB_LAST: u32 = 6;
/// `MDB_LAST_DUP`
pub const MDB_LAST_DUP: u32 = 7;
/// `MDB_NEXT`
pub const MDB_NEXT: u32 = 8;
/// `MDB_NEXT_DUP`
pub const MDB_NEXT_DUP: u32 = 9;
/// `MDB_NEXT_MULTIPLE`
pub const MDB_NEXT_MULTIPLE: u32 = 10;
/// `MDB_NEXT_NODUP`
pub const MDB_NEXT_NODUP: u32 = 11;
/// `MDB_PREV`
pub const MDB_PREV: u32 = 12;
/// `MDB_PREV_DUP`
pub const MDB_PREV_DUP: u32 = 13;
/// `MDB_PREV_NODUP`
pub const MDB_PREV_NODUP: u32 = 14;
/// `MDB_SET`
pub const MDB_SET: u32 = 15;
/// `MDB_SET_KEY`
pub const MDB_SET_KEY: u32 = 16;
/// `MDB_SET_RANGE`
pub const MDB_SET_RANGE: u32 = 17;
/// `MDB_PREV_MULTIPLE`
pub const MDB_PREV_MULTIPLE: u32 = 18;

// ---------------------------------------------------------------------------
// Return codes, lmdb.h "errors".
// ---------------------------------------------------------------------------

/// `MDB_SUCCESS`
pub const MDB_SUCCESS: c_int = 0;
/// `MDB_KEYEXIST`: key/data pair already exists.
pub const MDB_KEYEXIST: c_int = -30799;
/// `MDB_NOTFOUND`: key/data pair not found (EOF).
pub const MDB_NOTFOUND: c_int = -30798;
/// `MDB_PAGE_NOTFOUND`: requested page not found - this usually indicates corruption.
pub const MDB_PAGE_NOTFOUND: c_int = -30797;
/// `MDB_CORRUPTED`: located page was wrong type.
pub const MDB_CORRUPTED: c_int = -30796;
/// `MDB_PANIC`: update of meta page failed or environment had fatal error.
pub const MDB_PANIC: c_int = -30795;
/// `MDB_VERSION_MISMATCH`: environment version mismatch.
pub const MDB_VERSION_MISMATCH: c_int = -30794;
/// `MDB_INVALID`: file is not a valid LMDB file.
pub const MDB_INVALID: c_int = -30793;
/// `MDB_MAP_FULL`: environment mapsize reached.
pub const MDB_MAP_FULL: c_int = -30792;
/// `MDB_DBS_FULL`: environment maxdbs reached.
pub const MDB_DBS_FULL: c_int = -30791;
/// `MDB_READERS_FULL`: environment maxreaders reached.
pub const MDB_READERS_FULL: c_int = -30790;
/// `MDB_TLS_FULL`: too many TLS keys in use (Windows only in C).
pub const MDB_TLS_FULL: c_int = -30789;
/// `MDB_TXN_FULL`: txn has too many dirty pages.
pub const MDB_TXN_FULL: c_int = -30788;
/// `MDB_CURSOR_FULL`: cursor stack too deep - internal error.
pub const MDB_CURSOR_FULL: c_int = -30787;
/// `MDB_PAGE_FULL`: page has not enough space - internal error.
pub const MDB_PAGE_FULL: c_int = -30786;
/// `MDB_MAP_RESIZED`: database contents grew beyond environment mapsize.
pub const MDB_MAP_RESIZED: c_int = -30785;
/// `MDB_INCOMPATIBLE`: operation and DB incompatible, or DB type changed.
pub const MDB_INCOMPATIBLE: c_int = -30784;
/// `MDB_BAD_RSLOT`: invalid reuse of reader locktable slot.
pub const MDB_BAD_RSLOT: c_int = -30783;
/// `MDB_BAD_TXN`: transaction must abort, has a child, or is invalid.
pub const MDB_BAD_TXN: c_int = -30782;
/// `MDB_BAD_VALSIZE`: unsupported size of key/DB name/data, or wrong DUPFIXED size.
pub const MDB_BAD_VALSIZE: c_int = -30781;
/// `MDB_BAD_DBI`: the specified DBI was changed unexpectedly.
pub const MDB_BAD_DBI: c_int = -30780;
/// `MDB_LAST_ERRCODE` of LMDB 0.9 (`MDB_BAD_DBI`).
pub const MDB_LAST_ERRCODE: c_int = MDB_BAD_DBI;

// The codes LMDB 1.0 adds after `MDB_BAD_DBI` (`lmdb.h` of `mdb.RE/1.0`),
// with their upstream names and values. 0.9's `mdb_strerror` does not know
// them; this crate's [`strerror`](crate::strerror) does.

/// `MDB_PROBLEM` (LMDB 1.0): unexpected problem - txn should abort. Also
/// heed's `MdbError::Problem`.
pub const MDB_PROBLEM: c_int = -30779;
/// `MDB_BAD_CHECKSUM` (LMDB 1.0): page checksum incorrect. rlmdb returns it
/// for a 1.0 environment whose pages carry checksums, which it does not
/// implement.
pub const MDB_BAD_CHECKSUM: c_int = -30778;
/// `MDB_CRYPTO_FAIL` (LMDB 1.0): encryption/decryption failed.
pub const MDB_CRYPTO_FAIL: c_int = -30777;
/// `MDB_ENV_ENCRYPTION` (LMDB 1.0): environment encryption mismatch. rlmdb
/// returns it for an encrypted 1.0 environment, which it cannot read.
pub const MDB_ENV_ENCRYPTION: c_int = -30776;
/// `MDB_TXN_PENDING` (LMDB 1.0): transaction was already prepared.
pub const MDB_TXN_PENDING: c_int = -30775;
/// `MDB_CANT_ROLLBACK` (LMDB 1.0): environment can't rollback the last
/// transaction.
pub const MDB_CANT_ROLLBACK: c_int = -30774;
/// `MDB_DBIS_BUSY` (LMDB 1.0): can't drop main DBI while other DBIs are open.
pub const MDB_DBIS_BUSY: c_int = -30773;
/// `MDB_SHORT_WRITE` (LMDB 1.0): write was incomplete.
pub const MDB_SHORT_WRITE: c_int = -30772;
/// `MDB_ENV_BUSY` (LMDB 1.0): env is busy, can't use previous snapshot.
pub const MDB_ENV_BUSY: c_int = -30771;
/// `MDB_IS_READONLY` (LMDB 1.0): env or txn is read-only, can't write.
pub const MDB_IS_READONLY: c_int = -30770;
/// `MDB_ADDR_BUSY` (LMDB 1.0): requested map address is unavailable.
pub const MDB_ADDR_BUSY: c_int = -30769;
/// `MDB_LAST_ERRCODE` of LMDB 1.0 (`MDB_ADDR_BUSY`).
pub const MDB_LAST_ERRCODE_V10: c_int = MDB_ADDR_BUSY;

/// `MDB_ENCRYPT` (LMDB 1.0 environment flag, also recorded in the meta
/// page's `mm_flags`): the pages are encrypted.
pub const MDB_ENCRYPT: u32 = 0x2000;

// ---------------------------------------------------------------------------
// Format constants from mdb.c.
// ---------------------------------------------------------------------------

/// `MDB_MAGIC`: stamp at the start of the meta and the lock file.
pub const MDB_MAGIC: u32 = 0xBEEF_C0DE;
/// `MDB_DATA_VERSION` of LMDB 0.9: the data file format version (1.0's is 3,
/// [`Layout::data_version`](crate::format::Layout::data_version)).
pub const MDB_DATA_VERSION: u32 = 1;
/// `MDB_LOCK_VERSION`: lock file format version.
pub const MDB_LOCK_VERSION: u32 = 1;
/// `MDB_PIDLOCK`: readers hold an fcntl lock on byte `pid` of the lock file
/// (1 on Unix). 0 on Windows, where C checks whether a reader's process
/// exists instead.
pub const MDB_PIDLOCK: u32 = if cfg!(windows) { 0 } else { 1 };
/// `MDB_LOCK_FORMAT`: `MDB_LOCK_VERSION + (MDB_PIDLOCK << 16)` (`0x10001` on
/// Unix, `0x1` on Windows).
pub const MDB_LOCK_FORMAT: u32 = MDB_LOCK_VERSION + (MDB_PIDLOCK << 16);
/// `MDB_MAXKEYSIZE`: largest key (and `MDB_DUPSORT` data item) of an LMDB 0.9
/// environment. LMDB 1.0 computes it from the page size
/// ([`Layout::max_key_size`](crate::format::Layout::max_key_size),
/// [`Environment::max_key_size`](crate::Environment::max_key_size)).
pub const MDB_MAXKEYSIZE: usize = 511;
/// `MAXDATASIZE`: node data sizes are stored in 32 bits.
pub const MAXDATASIZE: usize = 0xffff_ffff;
/// `MAX_PAGESIZE`: largest page size in 0.9 (`PAGEBASE == 0`).
pub const MAX_PAGESIZE: usize = 0x8000;
/// `MDB_MINKEYS`: minimum number of keys a page must hold.
pub const MDB_MINKEYS: usize = 2;
/// `DEFAULT_MAPSIZE`: map size of a freshly created environment.
pub const DEFAULT_MAPSIZE: usize = 1_048_576;
/// `DEFAULT_READERS`: default size of the reader table.
pub const DEFAULT_READERS: u32 = 126;
/// `CURSOR_STACK`: maximum depth of a B-tree (and of a cursor's page stack).
pub const CURSOR_STACK: usize = 32;
/// `CACHELINE`: alignment of lock-file structures.
pub const CACHELINE: usize = 64;
/// `NUM_METAS`: number of meta pages at the start of the data file.
pub const NUM_METAS: usize = 2;
/// `CORE_DBS`: databases stored in the meta page (free DB and main DB).
pub const CORE_DBS: u32 = 2;
/// `FREE_DBI`: handle of the free-page database.
pub const FREE_DBI: u32 = 0;
/// `MAIN_DBI`: handle of the main (unnamed) database.
pub const MAIN_DBI: u32 = 1;
/// `P_INVALID`: an invalid page number; the root of an empty tree.
pub const P_INVALID: u64 = !0;
/// `MDB_IDL_LOGN` (midl.h).
pub const MDB_IDL_LOGN: u32 = 16;
/// `MDB_IDL_DB_SIZE` (midl.h).
pub const MDB_IDL_DB_SIZE: usize = 1 << MDB_IDL_LOGN;
/// `MDB_IDL_UM_SIZE` (midl.h): capacity of a dirty list.
pub const MDB_IDL_UM_SIZE: usize = 1 << (MDB_IDL_LOGN + 1);
/// `MDB_IDL_DB_MAX` (midl.h).
pub const MDB_IDL_DB_MAX: usize = MDB_IDL_DB_SIZE - 1;
/// `MDB_IDL_UM_MAX` (midl.h): maximum number of dirty pages in one txn.
pub const MDB_IDL_UM_MAX: usize = MDB_IDL_UM_SIZE - 1;

// The version `mdb_version` reports. The one exception to the rule above:
// the patch level is not lmdb.h's (`DUAL_FORMAT.md` §4.4).

/// `MDB_VERSION_MAJOR`, as `mdb_version` reports it: 0, as in `lmdb.h`.
pub const MDB_VERSION_MAJOR: i32 = 0;
/// `MDB_VERSION_MINOR`, as `mdb_version` reports it: 9, as in `lmdb.h`.
pub const MDB_VERSION_MINOR: i32 = 9;
/// `MDB_VERSION_PATCH`, as `mdb_version` reports it: 85, where the 0.9.36
/// `lmdb.h` has 36. A patch level above every real 0.9 release lets a
/// program tell rlmdb from C LMDB while it stays in the 0.9 series rlmdb is
/// a drop-in for. The C release the 0.9 format matches is
/// [`VersionInfo::v09`](crate::VersionInfo::v09).
pub const MDB_VERSION_PATCH: i32 = 85;
