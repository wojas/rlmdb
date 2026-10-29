# LMDB in Rust: design brief

This directory holds a clean-room-style *rewrite* of LMDB 0.9 in Rust. It is
not a binding. The goals, in priority order:

1. **Binary compatibility with C LMDB 0.9** (the source in
   `../libraries/liblmdb`, version 0.9.36 / `mdb.RE/0.9`). A C LMDB process and
   a Rust LMDB process must be able to open the same environment *at the same
   time*, share the lock file, and read and write the same data file. The data
   file format (`MDB_DATA_VERSION 1`) and lock file format (`MDB_LOCK_VERSION 1`)
   are fixed by the C code and are the specification.
2. **Clear, readable, documented Rust.** Every module starts with a doc comment
   explaining what it models and which part of `mdb.c` it corresponds to.
   Every pub item has a doc comment. Prefer small functions with names that say
   what they do over C-style monoliths. Where behaviour must match C exactly,
   say so in a comment and cite the C function (`mdb_page_split`, …).
3. **A Rust API without the usual footguns.** Lifetimes tie values to the
   transaction, mutation needs `&mut`, read transactions are not tied to a
   thread, handles cannot outlive the environment, there is no `void*` and no
   "undefined behaviour if you call this twice".
4. **A C API compatible with C LMDB** (`lmdb-capi`): the same `mdb_*` symbols,
   same `lmdb.h`, same error codes and flag values, so existing C programs link
   and run unchanged against the Rust build (`liblmdb.a` / `liblmdb.so`).
5. **Performance comparable to C.** No allocations in the hot read path, binary
   search on pages, cursor stacks in fixed arrays, zero-copy values.
6. **Minimal `unsafe`.** `unsafe` is confined to `lmdb/src/os/` (mmap, file
   locks, fsync, shared mutexes/semaphores, pread/pwrite) and to `lmdb-capi`
   (inherently FFI). Everything else, including all page parsing and B-tree
   logic, is safe Rust over byte slices. `lmdb/src/lib.rs` carries
   `#![forbid(unsafe_code)]` with a per-module `#![allow(unsafe_code)]` only
   under `os/`.

Out of scope for now (leave the door open, do not design it away):

* Windows. Keep OS specifics behind the `os` module so a `windows.rs` can be
  added later. Only `unix` is implemented.
* 32-bit targets. The on-disk format depends on `sizeof(size_t)`; we hard-code
  the 64-bit layout and emit `compile_error!` on 32-bit targets.
* `MDB_VL32`, `MDB_FIXEDMAP`, encryption / checksums (those are 1.0 features
  and not in 0.9 anyway), `MDB_NOMEMINIT`, `MDB_MAPASYNC`.
* `MDB_WRITEMAP` is deferred (not needed for compatibility: it is a per-process
  choice). Design `RwTxn` so dirty pages are owned buffers; WRITEMAP can be
  added later as an alternative page backing.

## Reference material

* `../libraries/liblmdb/mdb.c` (10.5k lines) is the spec. Read it. Line
  references in this document refer to that file at commit `a69ce88`.
* `../libraries/liblmdb/lmdb.h` is the public C API and documents the
  semantics of every flag and function. The C API crate ships a copy of it.
* `../libraries/liblmdb/midl.c` / `midl.h`: the ID lists (`MDB_IDL`,
  `MDB_ID2L`) used for free lists and dirty lists.
* `../libraries/liblmdb/mtest*.c`: small C test programs; they must run
  unchanged against `lmdb-capi`.
* `../libraries/liblmdb/mdb_stat.c`, `mdb_dump.c`, `mdb_load.c`,
  `mdb_copy.c`: the tools, reimplemented in `lmdb-tools` (`mdb_drop` does not
  exist in 0.9.36; the Rust one is an extra modelled on 1.0's).

## On-disk format (what "compatible" means)

All integers are host-endian (LMDB does not byte-swap). We implement the
64-bit layout: `pgno_t`, `txnid_t`, `size_t`, `MDB_ID` are all 8 bytes.

### Data file

* Page size `psize` is stored in meta (`mm_dbs[0].md_pad`); default is the OS
  page size (4096 on Linux x86-64, 16384 on Apple Silicon macOS!). Max 32768
  in 0.9 (`MAX_PAGESIZE`, because `PAGEBASE == 0`).
* Pages 0 and 1 are meta pages. A snapshot is the meta with the larger
  `mm_txnid` (`mdb_env_pick_meta`, line 4078). Txn N writes meta page `N % 2`
  (`mdb_env_write_meta`, line 3935; note it writes from `mm_mapsize` onward
  via `pwrite`, and uses an `O_DSYNC` fd unless `MDB_NOMETASYNC`).
* Page header (`MDB_page`, line 858; `PAGEHDRSZ == 16`):

  | offset | size | field |
  |---|---|---|
  | 0 | 8 | `mp_pgno` |
  | 8 | 2 | `mp_pad` (key size for `P_LEAF2` pages) |
  | 10 | 2 | `mp_flags` (`P_BRANCH 1, P_LEAF 2, P_OVERFLOW 4, P_META 8, P_DIRTY 0x10, P_LEAF2 0x20, P_SUBP 0x40, P_LOOSE 0x4000, P_KEEP 0x8000`) |
  | 12 | 2 | `mp_lower` (or low half of `mp_pages` for overflow pages) |
  | 14 | 2 | `mp_upper` (or high half of `mp_pages`) |
  | 16 | 2*n | `mp_ptrs[]`: sorted offsets of nodes within the page |

  `NUMKEYS(p) = (mp_lower - PAGEHDRSZ) / 2`. Nodes live at `page + mp_ptrs[i]`
  (`PAGEBASE` is 0 in 0.9). Overflow pages store `mp_pages` (u32 at offset 12)
  and only the first page has a header.
* Node (`MDB_node`, line 918; `NODESIZE == 8`): `mn_lo:u16, mn_hi:u16`
  (little-endian hosts; swapped on big-endian), `mn_flags:u16`, `mn_ksize:u16`,
  then key bytes, then data bytes (no padding, `NODEDATA = mn_data + mn_ksize`).
  Leaf nodes: data size = `mn_lo | mn_hi << 16`. Branch nodes: child pgno =
  `mn_lo | mn_hi << 16 | mn_flags << 32`. Node flags `F_BIGDATA 1`
  (data is an 8-byte overflow pgno), `F_SUBDATA 2` (data is a 48-byte `MDB_db`),
  `F_DUPDATA 4` (data is a `P_SUBP` sub-page, or with `F_SUBDATA` a sub-DB).
* `P_LEAF2` pages (for `MDB_DUPFIXED` sub-DBs): no `mp_ptrs`, no node
  headers; fixed-size keys packed from offset 16, key size in `mp_pad`.
* `MDB_db` (line 1058, 48 bytes): `md_pad:u32, md_flags:u16, md_depth:u16,
  md_branch_pages:u64, md_leaf_pages:u64, md_overflow_pages:u64,
  md_entries:u64, md_root:u64` (`P_INVALID = !0` means empty tree).
* `MDB_meta` (line 1096, 136 bytes, at offset 16 inside meta pages):
  `mm_magic:u32 (0xBEEFC0DE), mm_version:u32 (1), mm_address:u64 (0),
  mm_mapsize:u64, mm_dbs[2]: MDB_db (0 = free DB, 1 = main DB),
  mm_last_pg:u64, mm_txnid:u64`. `mm_dbs[0].md_pad` doubles as page size and
  `mm_dbs[0].md_flags` as persistent env flags.
* Named databases are entries in the main DB whose node has `F_SUBDATA` and
  whose data is an `MDB_db` (`mdb_dbi_open`, line 9955). A new environment
  stores `(env_flags & 0xffff) | MDB_INTEGERKEY` in the free DB's
  `md_flags` (line 3870), so `MDB_FIXEDMAP`/`MDB_NOSUBDIR` bits land in the
  file (mtest produces 0x09). Stale-record refresh (line 5757): empty main
  DB gives `NOTFOUND`, inexact match `BAD_DBI`, changed flags `INCOMPATIBLE`.
* The free DB (DBI 0) maps `txnid_t` keys (8 bytes host-endian) to `MDB_IDL`
  values: `[count:u64, pgno..]` with pgnos sorted **descending**
  (`mdb_freelist_save`, line 3169; `midl.h`). Pages listed under txn T may be
  reused once no reader is using a snapshot older than T (`mdb_find_oldest`).
  A Rust writer must leave the free DB in exactly the state a C writer would
  accept: every freed page recorded once, no page both free and in use, loose
  pages handled, and overflow pages counted correctly.
* Max key size is `511` (`MDB_MAXKEYSIZE`); `mdb_env_get_maxkeysize` returns
  511. `MDB_DUPSORT` data items are keys of a sub-DB and share the limit.
  `me_nodemax = ((psize - PAGEHDRSZ) / MDB_MINKEYS) & -2 - sizeof(indx_t)`;
  a leaf item whose `LEAFSIZE` exceeds nodemax goes to overflow pages.
* Comparators (`mdb_default_cmp`, line 9940) decide sort order and must match
  bit for bit. The free DB's key comparator is set explicitly to
  `mdb_cmp_long` at open (line 5136). `mdb_cmp_clong` is `mdb_cmp_long` on
  x86 and `mdb_cmp_cint` elsewhere (line 1494); C swaps `mdb_cmp_int` for it
  on 8-byte items in `mdb_xcursor_init1` (line 7771), `mdb_cursor_set` for
  `GET_BOTH` (line 6342) and `mdb_dcmp`. The implemented `crate::cmp`
  module is the reference now; see `KNOWN_C_BUGS.md` #7 for the
  mismatched-size behaviour that is ported on purpose. Otherwise: keys use `mdb_cmp_memnr` for `MDB_REVERSEKEY`, `mdb_cmp_cint`
  for `MDB_INTEGERKEY`, else `mdb_cmp_memn`. Dup data: `MDB_INTEGERDUP` →
  `mdb_cmp_int` if `MDB_DUPFIXED` else `mdb_cmp_cint`; `MDB_REVERSEDUP` →
  `mdb_cmp_memnr`; else `mdb_cmp_memn`. Read those five functions (lines
  5321-5405) carefully, including their `size` handling.

### Lock file (`<path>/lock.mdb`, or `<path>-lock` with `MDB_NOSUBDIR`)

`MDB_txninfo` (line 779). Layout is `CACHELINE` (64) padded and depends on the
platform's shared-lock primitive, which **must match the C build on the same
platform**:

* **Linux** (`MDB_USE_POSIX_MUTEX`): header = `magic:u32, format:u32,
  rmutex: pthread_mutex_t, txnid:u64, numreaders:u32`, padded to a multiple
  of 64; then `wmutex: pthread_mutex_t` padded to a multiple of 64; then the
  reader slots. `pthread_mutex_t` is 40 bytes on x86-64 glibc and 48 on
  aarch64 glibc, so compute offsets from `size_of::<libc::pthread_mutex_t>()`
  and `align_of`. Mutexes are process-shared and robust
  (`PTHREAD_MUTEX_ROBUST`); on `EOWNERDEAD` see `mdb_mutex_failed` (line
  10470): reader mutex → mark consistent and continue; writer mutex → run
  `mdb_reader_check0`, mark consistent, continue if no dead readers.
* **macOS** (`MDB_USE_POSIX_SEM`): header = `magic:u32, format:u32,
  rmname:[u8;32], txnid:u64, numreaders:u32` → 64 bytes; `wmname:[u8;32]`
  padded to 64; readers at offset 128. The names are POSIX named semaphores
  `"/MDBr<enc>"`, `"/MDBw<enc>"` where `<enc>` is the FNV-1a hash of
  `(st_dev, st_ino)` of the lock file encoded with `mdb_hash_enc` (lines
  4823-4880; read the exact byte encoding). Opened with `sem_open`; the creator
  (exclusive opener) `sem_unlink`s then creates with `O_CREAT|O_EXCL`, value 1.
  Semaphores are not robust; C has the same limitation. C hashes a struct
  with 4 bytes of uninitialised padding (line 4996), so the names cannot be
  recomputed: later openers read them from the lock file.
* `format == MDB_LOCK_VERSION + (MDB_PIDLOCK << 16) == 0x10001` on Unix.
* Reader slot (`MDB_reader`, 64 bytes): `txnid:u64` at 0, `pid:i32` at 8,
  `tid:u64 (pthread_t)` at 16 (after padding); rest padding. Free slot ⇔ `pid == 0`. `txnid == !0` marks a
  slot owned but not in a txn.
* File locking (`mdb_env_excl_lock`, line 4741; `mdb_reader_pid`, line 2718):
  try `fcntl F_WRLCK` on byte 0 of the lock file; on success we are exclusive
  and (re)initialise the table; else take `F_RDLCK` on byte 0 with a
  *blocking* `F_SETLKW` (later openers wait until the creator downgrades,
  line 4776). The lock file is opened `O_RDWR|O_CREAT` even for read-only
  environments and only skipped on `EROFS` (line 4893). An exclusive opener
  only grows the lock file; a larger existing file raises `max_readers`.
  Reader liveness: each process holds `F_WRLCK` on byte `pid` of the lock file
  (`MDB_PIDLOCK`); `mdb_reader_check` probes with `F_GETLK` (line 10403).
* Txn begin/renew for readers: lock `rmutex`, find a free slot (`pid == 0`),
  set `pid`, `tid`, bump `numreaders` if needed, unlock; then write the
  snapshot `txnid` into the slot (line 2758). Readers start from
  `metas[mti_txnid & 1]`, not "the newer meta"; only with `mti_txnid == 0`
  in a read-only env does C fall back to the newer meta (line 2833).
  `mdb_env_read_header` (line 3804) reads 152 bytes per meta page at offset 0
  and at the first meta's page size. Writers lock `wmutex` for the
  whole write transaction and commit sets `mti_txnid`.

Compatibility means **both** files: a Rust process must be able to be the
first opener (initialise the table) or a later opener, with C on the other
side either way.

## Crate layout

```
rust/
  Cargo.toml          workspace
  DESIGN.md           this file
  lmdb/               the library (package `lmdb`, lib `lmdb`)
  lmdb-capi/          cdylib+staticlib `lmdb` exporting the C ABI, ships include/lmdb.h
  lmdb-tools/         mdb_stat, mdb_dump, mdb_load, mdb_copy, mdb_drop in Rust
  interop/            cross-process compatibility tests (builds the C library)
```

### `lmdb` module map

```
src/lib.rs          crate docs, lint settings, heed-compatible re-exports
src/consts.rs       MDB_* flag bits, error codes and cursor op values (the C numbers)
src/flags.rs        bitflags types: EnvironmentFlags, DatabaseFlags, WriteFlags
src/error.rs        MdbError (C code mapping) and the heed-style Error, Result<T>
src/format/         pure parsing/serialisation of on-disk structures over &[u8]
  mod.rs            PAGE_HDR_SIZE, NODE_SIZE, …; `layout` constants
  page.rs           PageRef/PageMut: header accessors, node index, leaf2 keys
  node.rs           NodeRef: key(), data(), flags(), child_pgno(), ovf_pgno()
  meta.rs           Meta (+ Db) read/write, pick newest
  db.rs             Db (MDB_db) read/write
  lockfile.rs       Txninfo/Reader layout constants and accessors
src/idl.rs          sorted id lists (MDB_IDL semantics) and dirty lists (ID2L)
src/cmp.rs          the five comparators + Comparator enum
src/os/             ALL unsafe lives here. mod.rs selects unix; unix.rs: Mmap,
                    file open/lock/pread/pwrite/writev/fsync (F_FULLFSYNC on
                    macOS), pid/tid, page size; shared_lock.rs: SharedMutex
                    (pthread on linux, named sem on macos) + reader table access
src/core/mod.rs     the byte-level API (see "Core layer")
src/core/env.rs     Environment, EnvironmentBuilder, open sequence, meta selection, mapsize
src/core/reader.rs  reader table: slot acquire/release, oldest reader, reader_check, reader_list
src/core/txn.rs     RoTxn, RwTxn, nested txns, dirty page store, spill, commit
src/core/freelist.rs page allocation and free DB maintenance (mdb_page_alloc, mdb_freelist_save)
src/core/cursor.rs  cursor core: search, next/prev, set, dup sub-cursor
src/core/cursor_put.rs  put, node add/del, split (write side)
src/core/cursor_del.rs  del, rebalance, merge, drop
src/core/dbi.rs     Dbi handles, open/create/close, flags, stat, comparators
src/core/copy.rs    env copy (plain and compacting)
src/env.rs, src/env_open_options.rs, src/txn.rs, src/database.rs,
src/iterator/*.rs, src/iteration_method.rs, src/reserved_space.rs,
src/traits.rs, src/types/*.rs      the heed-compatible layer over core
```

Keep files under ~1500 lines; split further rather than growing a file.

### Public Rust API: mirror `heed` 0.23.1

The public API **mirrors the `heed` crate, version 0.23.1** (Meilisearch's
LMDB wrapper, the actively maintained Rust binding). Code written against
`heed` should compile against this crate with at most an import change. The
sources are in `~/.cargo/registry/src/*/heed-0.23.1/src/`,
`heed-traits-0.20.0/src/lib.rs` and `heed-types-0.21.0/src/`; read them and
copy names, signatures, generics, lifetimes and doc comments.

The crate has two layers:

* **`lmdb::core`** (byte-level, documented, public): `Environment`,
  `EnvironmentBuilder`, `RoTxn`, `RwTxn`, `Dbi` (a `Copy` handle),
  `RoCursor`, `RwCursor`, `CursorOp`, `Stat`, `Info`, `ReaderInfo`. This is
  what the C API crate, the tools, the interop peer and the heed layer are
  built on. Its shape is described in the next subsection.
* **top level** (heed-compatible, typed): `Env<T = WithTls>`,
  `EnvOpenOptions<T>`, `RoTxn<'e, T = AnyTls>`, `RwTxn<'p>`,
  `Database<'t, KC, DC, C = DefaultComparator, CDUP = DefaultComparator>`,
  `DatabaseOpenOptions`, `DatabaseStat`, `EnvInfo`, `ReservedSpace`,
  `CompactionOption`, `FlagSetMode`, `EnvClosingEvent`, `env_closing_event`,
  `lmdb_version`/`LmdbVersion`, `Unspecified`, the iterator families
  (`RoIter/RwIter/RoRevIter/RwRevIter`, `RoRange/RwRange/RoRevRange/RwRevRange`,
  `RoPrefix/RwPrefix/RoRevPrefix/RwRevPrefix`) with the `IterationMethod`
  markers (`MoveThroughDuplicateValues`, `MoveBetweenKeys`,
  `MoveOnCurrentKeyDuplicates`), the TLS markers (`WithTls`, `WithoutTls`,
  `AnyTls`, `TlsUsage`), the traits (`BytesEncode`, `BytesDecode`,
  `Comparator`, `LexicographicComparator`, `BoxedError`), the comparators
  (`DefaultComparator`, `IntegerComparator`), flags (`EnvFlags`,
  `DatabaseFlags`, `PutFlags` with the heed names), `Error { Io, Mdb(MdbError),
  Encoding, Decoding, EnvAlreadyOpened }`, `MdbError` (the C codes), and the
  `types` module (`Bytes`, `Str`, `Unit`, `DecodeIgnore`, `LazyDecode`/`Lazy`,
  and the integer codecs `U8..U64, I8..I64` with `BE`/`LE` order markers;
  re-export `byteorder` like heed does). Serde codecs (`SerdeBincode`,
  `SerdeJson`, `SerdeRmp`) are feature-gated exactly as in heed and may be
  left for later.

Deliberate differences from heed:

* No `EncryptedEnv` / `EncryptedDatabase` (those need LMDB 1.0 features).
* No `Env::env_mut_ptr()`-style raw pointer accessors.
* `WithTls` vs `WithoutTls`: our read transactions are never thread-bound,
  so both behave like `WithoutTls`; keep the markers so signatures match and
  so `RoTxn<'_, WithoutTls>: Send` holds.
* `EnvAlreadyOpened`: heed keeps a process-global registry of opened paths
  and refuses a second open of the same path (LMDB forbids opening one env
  twice in one process because of the pid-based reader locks). Implement the
  same registry; it is needed for correctness, not just API parity.
* `Env::open_databases_and_commit` / `create_databases_and_commit` helpers
  and the `OnCommit` machinery may be implemented as thin wrappers; they are
  low priority.
* Custom comparators (`C`, `CDUP` type params) are per-process callbacks in
  C too (`mdb_set_compare`); a C peer must install the same comparator. The
  core layer accepts `fn(&[u8], &[u8]) -> Ordering` comparators.

### Core layer (`lmdb::core`)

This is the C API in Rust clothing, byte slices in and out, with lifetimes
doing the work that the C docs' "do not do X" paragraphs do.

```rust
pub struct Environment;             // Send + Sync; Drop closes; share via Arc
pub struct EnvironmentBuilder;      // map_size, max_readers, max_dbs, flags(EnvironmentFlags), open(path), open_with_permissions(path, mode)
impl Environment {
    pub fn open_db(&self, name: Option<&str>) -> Result<Dbi>;                     // existing only
    pub fn create_db(&self, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi>;
    pub fn close_db(&self, db: Dbi);                                             // later use -> Err(BadDbi)
    pub fn begin_ro_txn(&self) -> Result<RoTxn<'_>>;
    pub fn begin_rw_txn(&self) -> Result<RwTxn<'_>>;
    pub fn sync(&self, force: bool) -> Result<()>;
    pub fn stat(&self) -> Result<Stat>;  pub fn info(&self) -> Result<Info>;  pub fn freelist(&self) -> Result<usize>;
    pub fn flags(&self) -> EnvironmentFlags;  pub fn set_flags(&self, flags: EnvironmentFlags, on: bool) -> Result<()>;
    pub fn path(&self) -> &Path;  pub fn max_readers(&self) -> u32;  pub fn max_key_size(&self) -> usize;  // 511
    pub fn set_map_size(&self, size: usize) -> Result<()>;
    pub fn reader_list(&self) -> Result<Vec<ReaderInfo>>;  pub fn reader_check(&self) -> Result<usize>;
    pub fn copy_to_path(&self, path: &Path, compact: bool) -> Result<()>;  pub fn copy_to_fd(&self, fd: BorrowedFd<'_>, compact: bool) -> Result<()>;
    pub fn set_compare(&self, db: Dbi, cmp: Option<fn(&[u8], &[u8]) -> Ordering>) -> Result<()>;  pub fn set_dupsort(..) likewise
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)] pub struct Dbi(u32);
pub trait Transaction {                      // implemented by RoTxn and RwTxn
    fn env(&self) -> &Environment;  fn id(&self) -> u64;
    fn open_db(&self, name: Option<&str>) -> Result<Dbi>;
    fn get<'txn>(&'txn self, db: Dbi, key: &[u8]) -> Result<Option<&'txn [u8]>>;
    fn cursor<'txn>(&'txn self, db: Dbi) -> Result<RoCursor<'txn>>;
    fn db_flags(&self, db: Dbi) -> Result<DatabaseFlags>;  fn stat(&self, db: Dbi) -> Result<Stat>;
}
pub struct RoTxn<'env>;              // reset(self) -> InactiveTxn<'env>; InactiveTxn::renew(self) -> Result<RoTxn<'env>>; abort(self); Drop aborts; Send
pub struct RwTxn<'env> {
    pub fn create_db(&mut self, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi>;
    pub fn put(&mut self, db: Dbi, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()>;
    pub fn reserve<'txn>(&'txn mut self, db: Dbi, key: &[u8], len: usize, flags: WriteFlags) -> Result<&'txn mut [u8]>;
    pub fn del(&mut self, db: Dbi, key: &[u8], data: Option<&[u8]>) -> Result<bool>;   // false = not found
    pub fn cursor_mut<'txn>(&'txn mut self, db: Dbi) -> Result<RwCursor<'txn>>;
    pub fn clear_db(&mut self, db: Dbi) -> Result<()>;  pub fn drop_db(&mut self, db: Dbi) -> Result<()>;
    pub fn nested<'txn>(&'txn mut self) -> Result<RwTxn<'txn>>;
    pub fn commit(self) -> Result<()>;  pub fn abort(self);   // Drop aborts
}
pub enum CursorOp { First, FirstDup, GetBoth, GetBothRange, GetCurrent, GetMultiple, Last, LastDup, Next, NextDup, NextMultiple, NextNoDup, Prev, PrevDup, PrevNoDup, Set, SetKey, SetRange, PrevMultiple }   // <-> MDB_cursor_op values
pub struct RoCursor<'txn> {          // get returns &'txn [u8]: pages of a read snapshot never move
    pub fn get(&mut self, key: Option<&[u8]>, data: Option<&[u8]>, op: CursorOp) -> Result<Option<(&'txn [u8], &'txn [u8])>>;
    pub fn first(&mut self) -> ..; next(); prev(); last(); set_range(key); get_both(key, data); count() -> Result<u64>; ...
}
pub struct RwCursor<'txn> {          // get borrows &self: dirty pages may move on the next put/del
    pub fn get<'c>(&'c mut self, key: Option<&[u8]>, data: Option<&[u8]>, op: CursorOp) -> Result<Option<(&'c [u8], &'c [u8])>>;
    pub fn put(&mut self, key: &[u8], data: &[u8], flags: WriteFlags) -> Result<()>;
    pub fn reserve<'c>(&'c mut self, key: &[u8], len: usize, flags: WriteFlags) -> Result<&'c mut [u8]>;
    pub fn del(&mut self, flags: WriteFlags) -> Result<()>;  pub fn count(&self) -> Result<u64>;
}
pub struct Stat { page_size, depth, branch_pages, leaf_pages, overflow_pages, entries }
pub struct Info { map_size, last_pgno, last_txnid, max_readers, num_readers }
```

Core flags: `EnvironmentFlags` (`NO_SUB_DIR, NO_SYNC, READ_ONLY, NO_META_SYNC,
WRITE_MAP, MAP_ASYNC, NO_TLS, NO_LOCK, NO_READ_AHEAD, NO_MEM_INIT, FIXED_MAP,
PREV_SNAPSHOT`), `DatabaseFlags` (`REVERSE_KEY, DUP_SORT, INTEGER_KEY,
DUP_FIXED, INTEGER_DUP, REVERSE_DUP, CREATE`), `WriteFlags` (`NO_OVERWRITE,
NO_DUP_DATA, CURRENT, RESERVE, APPEND, APPEND_DUP, MULTIPLE`), all with the C
bit values; the heed layer re-exports them under heed's names (`EnvFlags`,
`DatabaseFlags`, `PutFlags`). Unsupported env flags (`FIXED_MAP`,
`WRITE_MAP`, `MAP_ASYNC`, `NO_MEM_INIT`) are accepted and ignored, or return
`Incompatible`; document which per flag.

Footgun rules: `put`/`del` take `&mut self` so no `&[u8]` borrowed from a
write txn can be alive across a mutation; a `Dbi` is `Copy` and borrows
nothing (mirrors C; using a dropped/closed handle returns `BadDbi`); a nested
txn borrows the parent mutably; read transactions are not bound to a thread
(we always behave like `MDB_NOTLS` internally, which is compatible with any C
peer because `MDB_NOTLS` is a per-process behaviour the lock file does not
record); `Drop` aborts. No panics on corrupt input: a bad page/offset yields
`Corrupted` or `PageNotFound`, never an index-out-of-bounds panic.

### Error codes

`Error` must map 1:1 to the C codes: `KeyExist (-30799)`, `NotFound (-30798)`,
`PageNotFound (-30797)`, `Corrupted (-30796)`, `Panic (-30795)`,
`VersionMismatch (-30794)`, `Invalid (-30793)`, `MapFull (-30792)`,
`DbsFull (-30791)`, `ReadersFull (-30790)`, `TlsFull (-30789)`,
`TxnFull (-30788)`, `CursorFull (-30787)`, `PageFull (-30786)`,
`MapResized (-30785)`, `Incompatible (-30784)`, `BadRslot (-30783)`,
`BadTxn (-30782)`, `BadValSize (-30781)`, `BadDbi (-30780)`, `Problem (-30779)`,
plus `Other(c_int)` for errno. This is `MdbError` in the heed layer and
`core::Error` in the core layer (one type, re-exported). `to_err_code()`
returns the C int; `from_err_code`; `Display` strings match `mdb_strerror`.

### Concurrency model and memory safety

* The data file is mapped `PROT_READ, MAP_SHARED` at `map_size` bytes (may be
  larger than the file). `os::Mmap::page(pgno, npages) -> Result<&[u8]>` is
  bounds-checked against the map and the snapshot's `last_pg`.
* LMDB's MVCC guarantees that pages reachable from a snapshot are not modified
  while a reader slot pins that snapshot. This is the same invariant the C
  code relies on; document it in `os::Mmap`.
* Meta pages *are* written concurrently by other processes. Read them with
  volatile reads (`ptr::read_volatile` in `os`) into an owned `Meta` value and
  validate (`magic`, `version`), re-reading as C does at txn begin
  (`mdb_txn_renew0`).
* Dirty pages in a write txn are owned `Box<[u8]>` buffers (one per page or
  overflow run), kept in a sorted list by pgno (`ID2L` semantics). Readers in
  the same txn resolve a pgno first in the dirty list, then parent txns'
  lists, then the map. At commit they are written with `pwritev` batching
  (`mdb_page_flush`, line 3394), data is `fdatasync`ed (or `fsync`/
  `F_FULLFSYNC`), then the meta page is written.
* Spilling (`mdb_page_spill`, line 2080) exists so huge write txns do not
  exhaust memory. Implement it; it is part of the commit correctness path for
  nested txns too. If deferred in a first pass, return `TxnFull` rather than
  silently misbehaving.
* The write lock: one `RwTxn` per `Env` at a time; `begin_rw_txn` blocks on
  the shared `wmutex` like C. On Linux the writer mutex is a pthread mutex
  owned by the locking thread, so the write transaction must be unlocked on
  the thread that began it: `RwTxn` is `!Send`. (The C API layer keeps C
  semantics where it must.)
* `set_map_size` must not invalidate slices held by live read transactions.
  Design: transactions hold an `Arc` to the current data map; `set_map_size`
  (and the `MapResized` path at txn begin) install a new map in the
  environment, old maps live until their last transaction drops.

### Code conventions

* `#![forbid(unsafe_code)]` at crate root except `os` (`#![allow(unsafe_code)]`
  there, each `unsafe` block with a `// SAFETY:` comment).
* `#![warn(missing_docs, clippy::all)]`. `cargo clippy --all-targets` and
  `cargo fmt --check` clean. `cargo test` green.
* Integers from disk are read with `u16::from_ne_bytes` / `u64::from_ne_bytes`
  over sub-slices; no `transmute`, no `#[repr(C)]` pointer casts of mapped
  memory outside `os`.
* Name things after the concept, not the C field: `lower`, `upper`, `num_keys`,
  `child_pgno`. Mention the C name in the doc comment once.
* Tests: unit tests next to the code for format parsing (hand-built pages),
  integration tests in `lmdb/tests/` for API behaviour, and `interop/` for
  cross-process checks against the C build.
* Linux is tested in Docker: `rust/scripts/linux-test.sh [cargo args]` runs
  cargo inside `rust:1-bookworm` (Debian, glibc, aarch64, 4 KiB pages) with
  the workspace bind-mounted. Run it before declaring a stage done; the lock
  file differs there: `lock_mode=posix_mutex`, `sizeof(pthread_mutex_t)=48`,
  reader slots at offset 192 (macOS: `posix_sem`, slots at 128).
* Do not commit. The coordinator commits between stages.

## Bugs in C 0.9.36: do not port them, track them

Where a C 0.9.36 behaviour is a plain and clear bug, do not reimplement it.
Record it in `rust/KNOWN_C_BUGS.md` (what, where in the C source, what we
do instead) and keep the Rust behaviour the obviously correct one. The one
hard constraint: the deviation must not cause interoperability issues. A
file written by Rust must still be read and written correctly by C, and the
other way round. If avoiding a bug changes bytes on disk or in the lock file,
or changes what a C peer would conclude about the state, port the bug
instead and note it. Tool bugs (`mdb_load` dropping flags) and API-only
quirks (`mdb_stat -r` exit code) are safe to fix. Crashes on malformed input
are bugs; the Rust side returns an error.

## Findings from the interop harness (stage 1B)

Facts about C 0.9.36 behaviour that implementers must honour; the harness in
`interop/` tests them. See `interop/README.md` for details.

* A DUPSORT DB's `md_branch_pages`/`md_leaf_pages` **include its sub-DB
  pages** (ITS#9388); `mdb_stat` prints those totals. `check-freelist`
  verifies depth/page counts/entries per DB against a full walk, so the Rust
  writer must maintain them the same way.
* Reader slots of a dead process stay in the table until `mdb_reader_check`
  or an exclusive re-open; page reuse then stalls and the map fills. Same in
  Rust (do not "fix" it unilaterally; `reader_check` is the fix).
* Keys in the map are not NUL-terminated and the map may end exactly at the
  last page: never read past a slice (SIGBUS in C, must be a bounds error in
  Rust).
* `mdb_env_copy`: plain copy holds the **write** lock while copying the meta
  pages, then copies pages as they are. Compacting copy writes meta 0 with
  txnid 0 and meta 1 with txnid 1, an empty free DB, renumbered pages, the
  main root on the last page. Both create the target with `O_EXCL`.
* A new data file is exactly 2 pages; the lock file is header +
  maxreaders x 64 bytes (8192 on macOS with 126 readers) and persists.
* `mdb_reader_list` line format: `"%10d %zx %zu\n"` (pid, tid hex, txnid) with
  `-` for a slot not in a txn; header `"    pid     thread     txnid\n"`.
* `mdb_dbi_flags` refuses DBI 0; the free DB's stored flags are
  `MDB_INTEGERKEY`.
* `mdb_load` in 0.9.36 drops the flags of the first DB in its input (the
  `flags = 0;` from ITS#8662); the Rust tool must not copy that bug.
* `mtest6` calls `mdb_dkey`, which exists only with `MDB_DEBUG`;
  `lmdb/tests` builds it against a small shim, the interop harness skips it.

## Staged plan

Stage 1 is done (commits `c13c67d` interop, and the lmdb read path). The
implemented `rust/lmdb` crate is the reference for the core API from here
on; read its `src/core/mod.rs` and `src/core/txn.rs` (`TxnCore::page` is
where dirty pages plug in) before extending it.

**Stage 1 (parallel):**

* A. `lmdb` foundation + `core` read path: workspace member, consts, flags,
  errors, format, idl, cmp, os (mmap, locks, shared mutex, reader table),
  env open (including
  creating a fresh environment: `mdb_env_init_meta`, line 3882), read txns,
  `open_db` for existing DBs, read cursors (all `CursorOp`s, DUPSORT,
  DUPFIXED/LEAF2, overflow pages), stats/info, reader_list/reader_check.
  Verified against databases produced by the C tools/mtest programs.
* B. `interop` crate: build the C library from `../libraries/liblmdb` with the
  `cc` crate, a C peer program with a small command protocol, a layout-dump C
  program that prints `sizeof`/`offsetof` for every on-disk struct, and the
  Rust test runner. Stage 1 ships C-vs-C tests to validate the harness; the
  Rust peer follows in stage 2.

**Stage 2:** C1 write path part 1 (page alloc/freelist, dirty pages, touch,
node add, split, put in all variants, commit/flush/meta write, sync,
set_map_size). D1 `lmdb-capi` for everything stage 1 exposes. B2 Rust peer
read side + "C writes, Rust reads" interop tests + layout test against the C
dump.

**Stage 3:** C2 write path part 2 (del, rebalance, merge, drop, nested txns,
spill, copy/compact). D2 rest of the C API + run `mtest*.c` against it. B3
bidirectional and concurrent interop stress. T `lmdb-tools`.

**Stage 4:** review, benchmarks vs C, docs. Until the heed layer exists,
`lib.rs` re-exports `core::*` at the crate root.

**Deferred (not scheduled):** the heed-compatible layer over `core`. The
user wants the core validated through the C API first. The crate root keeps
re-exporting `core::*`; the heed section above stays as the intended design
only.
