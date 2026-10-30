# LMDB in Rust: design brief

This directory holds a clean-room-style *rewrite* of LMDB 0.9 in Rust. It is
not a binding. The goals, in priority order:

1. **Binary compatibility with C LMDB 0.9** (the source in
   `../libraries/liblmdb`, version 0.9.36 / `mdb.RE/0.9`). A C LMDB process and
   a Rust LMDB process must be able to open the same environment *at the same
   time*, share the lock file, and read and write the same data file. The data
   file format (`MDB_DATA_VERSION 1`) and lock file format (`MDB_LOCK_VERSION 1`)
   are fixed by the C code and are the specification. As a second format,
   rlmdb reads and writes LMDB 1.0's data file (`MDB_DATA_VERSION 3`, see
   "The LMDB 1.0 data file" below and `DUAL_FORMAT.md`).
2. **Clear, readable, documented Rust.** Every module starts with a doc comment
   explaining what it models and which part of `mdb.c` it corresponds to.
   Every pub item has a doc comment. Prefer small functions with names that say
   what they do over C-style monoliths. Where behaviour must match C exactly,
   say so in a comment and cite the C function (`mdb_page_split`, …).
3. **A Rust API without the usual footguns.** Lifetimes tie values to the
   transaction, mutation needs `&mut`, read transactions are not tied to a
   thread, handles cannot outlive the environment, there is no `void*` and no
   "undefined behaviour if you call this twice".
4. **A C API compatible with C LMDB** (`rlmdb-capi`): the same `mdb_*` symbols,
   same `lmdb.h`, same error codes and flag values, so existing C programs link
   and run unchanged against the Rust build (`liblmdb.a` / `liblmdb.so`).
5. **Performance comparable to C.** No allocations in the hot read path, binary
   search on pages, cursor stacks in fixed arrays, zero-copy values.
6. **Minimal `unsafe`.** `unsafe` is confined to `rlmdb/src/os/` (mmap,
   file locks, fsync, shared mutexes/semaphores, pread/pwrite) and to
   `rlmdb-capi` (inherently FFI). Everything else, including all page parsing
   and B-tree logic, is safe Rust over byte slices. `rlmdb/src/lib.rs`
   carries `#![deny(unsafe_code)]` and `#[forbid(unsafe_code)]` on every
   module except `os`, which opts back in with `#![allow(unsafe_code)]`
   (a crate-level `forbid` could not be relaxed for one module).

Out of scope for now (leave the door open, do not design it away):

* Windows. Keep OS specifics behind the `os` module so a `windows.rs` can be
  added later. Only `unix` is implemented.
* 32-bit targets. The on-disk format depends on `sizeof(size_t)`; we hard-code
  the 64-bit layout and emit `compile_error!` on 32-bit targets.
* `MDB_VL32`, encryption / checksums (1.0 features, not in 0.9 anyway).
  `MDB_FIXEDMAP`, `MDB_NOMEMINIT` and `MDB_MAPASYNC` are accepted and
  ignored (per-process choices; see `COMPATIBILITY.md`).
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
  unchanged against `rlmdb-capi`.
* `../libraries/liblmdb/mdb_stat.c`, `mdb_dump.c`, `mdb_load.c`,
  `mdb_copy.c`: the tools, reimplemented in `rlmdb-tools` (`mdb_drop` does not
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

### The LMDB 1.0 data file (`MDB_DATA_VERSION 3`)

rlmdb also reads and writes the data file of LMDB 1.0 (`mdb.RE/1.0`, pinned
to 4865523 in `interop/build.rs`; `DUAL_FORMAT.md` is the design). The format
of an environment is decided at open, before the lock file is touched: an
existing file keeps its format (`rlmdb::sniff_format` looks at the magic and
version of the first meta page; a request for the other one is
`MDB_VERSION_MISMATCH`), a new one gets the requested format, LMDB 0.9 by
default. The engine is the same for both; every page and node access goes
through the environment's `format::Layout`. What differs on disk:

| item | 0.9 | 1.0 |
|---|---|---|
| page header | 16 bytes: `pgno, pad, flags, lower/upper` | 24 bytes: `pgno, txnid, pad, flags, lower/upper` |
| `mp_lower`, `mp_upper`, `mp_ptrs[]` | from the page start | from the end of the header (`PAGEBASE` 24, ITS#7713): an empty page has lower 0 and upper `psize - 24` |
| node data | `mn_data + ksize` | `mn_data + EVEN(ksize)`; `LEAFSIZE` pads key and data to even lengths, which moves split points and the overflow threshold |
| `F_BIGDATA` node data | the 8-byte pgno | `MDB_ovpage {pgno, txnid, pages}`, 24 bytes |
| sub-page (`P_SUBP`) | flags `0x52`, 16-byte header | flags `0x42` (no `P_DIRTY`), 24-byte header whose `mp_txnid` is the txnid of the transaction that built it |
| meta | at 16 | at 24; `mm_flags` may carry `MDB_ENCRYPT`, page 0's last two bytes a checksum size (both refused: `MDB_ENV_ENCRYPTION`, `MDB_BAD_CHECKSUM`) |
| page size | the OS page size, at most 32 KiB | any power of two from 256 to 64 KiB (`EnvironmentBuilder::page_size`, `mdb_env_set_pagesize`; 1.0's `mtest` uses 1 KiB) |
| maximum key size | 511 | `me_nodemax - 56`: 442 on 1 KiB pages, 1978 on 4 KiB, 8122 on 16 KiB, 32698 on 64 KiB |
| named database keys | the name | the name with its terminating NUL |

Every page a 1.0 writer writes carries the id of the writing transaction in
`mp_txnid` (`mdb_page_flush`, also for pages spilled during the transaction
and in nested transactions, which share their parent's id); C 1.0 writers
tell their own pages from shared ones by that field (`IS_WRITABLE`,
`IS_MUTABLE`), so a wrong value corrupts the next C writer's transaction.
rlmdb keeps its own dirty and spill lists and stamps the field when it writes
a page. New overflow references record the writer's txnid and the run's
length; an in-place overwrite of an overflow value restamps the run, not the
reference. The compacting copy writes every page, overflow run and
reference as transaction 1, as `mdb_copy -c` does. 1.0 also hands an emptied
or collapsed root to `mdb_page_loose` in `mdb_rebalance` (0.9 frees it), which
rlmdb follows (`KNOWN_C_BUGS.md`, section "LMDB 1.0"). Free-list records,
`MDB_db`, `MDB_meta`, node headers and sort order are the same in both formats.

The lock file is still the version 1 one in both formats (stage 3 of
`DUAL_FORMAT.md` adds 1.0's): a C 1.0 process refuses it while rlmdb holds
the environment open, so for now the two take turns on a 1.0 environment.

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
  README.md           overview, build and test instructions
  COMPATIBILITY.md    the compatibility contract and what verifies it
  KNOWN_C_BUGS.md     C 0.9.36 bugs not ported (or ported on purpose)
  scripts/            linux-test.sh (cargo in Docker), check-all.sh
  rlmdb/          the library (package `rlmdb`, lib `rlmdb`)
  rlmdb-capi/          cdylib+staticlib `lmdb` exporting the C ABI, ships include/lmdb.h
  rlmdb-tools/         mdb_stat, mdb_dump, mdb_load, mdb_copy, mdb_drop in Rust
  interop/            cross-process compatibility tests (builds the C library)
```

### `rlmdb` module map

As implemented (the heed-layer files at the end are planned only):

```
src/lib.rs          crate docs, lint settings, re-exports `core::*` at the root
src/consts.rs       MDB_* flag bits, error codes and cursor op values (the C numbers)
src/flags.rs        bitflags types: EnvironmentFlags, DatabaseFlags, WriteFlags
src/error.rs        MdbError (C code mapping) and the heed-style Error, Result<T>
src/format/         pure parsing/serialisation of on-disk structures over &[u8]
  mod.rs            get/put helpers, 0.9 size wrappers (node_max, OVPAGES)
  layout.rs         the 0.9 offset constants (PAGE_HDR_SIZE, NODE_SIZE, …), and
                    Format/Layout: one static descriptor per data-file format
                    (V09, V10) with the C macros derived from it (NODEDATA,
                    LEAFSIZE, OVPAGES, me_nodemax, maxkey, MDB_ovpage I/O)
  page.rs           PageRef/PageMut over a Layout: header accessors, node index,
                    leaf2 keys
  node.rs           NodeRef over a Layout: key(), data(), flags(), child_pgno(),
                    overflow_ref()
  meta.rs           Meta (+ Db) read/write per layout, pick newest
  db.rs             Db (MDB_db) read/write
  lockfile.rs       Txninfo/Reader layout constants and accessors
src/idl.rs          sorted id lists (MDB_IDL semantics) and dirty lists (ID2L)
src/cmp.rs          the five comparators + Comparator enum
src/os/             ALL unsafe lives here. mod.rs selects unix; unix.rs: DataMap
                    (read-only data map, volatile meta reads), LockMap, fcntl
                    locks, pread/pwrite/pwritev, fdatasync (F_FULLFSYNC on
                    macOS), pid/tid, page size; shared_lock.rs: SharedMutex
                    (robust pthread mutex on Linux, named semaphore on macOS)
src/core/mod.rs     the byte-level API (see "Core layer"), Stat, Info
src/core/env.rs     Environment, EnvironmentBuilder, open sequence, meta selection,
                    map size, the per-process registry of open paths, the
                    the environment's Format/Layout (sniffed from the file,
                    requested, or V09), sniff_format
src/core/reader.rs  lock file and reader table: slots, oldest reader, reader_check/list
src/core/txn.rs     TxnCore (snapshot, DBI view, page lookup), RoTxn, InactiveTxn,
                    the Transaction trait
src/core/dbi.rs     Dbi handles, open/close, names, stale-record refresh
src/core/cursor.rs  cursor core: search, next/prev, set, dup sub-cursor
src/core/ro_cursor.rs  RoCursor, CursorOp, mdb_cursor_get dispatch
src/core/txn_rw.rs  RwTxn, RwCursor, CursorId (handle cursors), put/del/create/drop API
src/core/wcursor.rs write cursors in an arena, fixups, mdb_page_touch
src/core/dirty.rs   dirty page list (ID2L), ancestor layers, spill set
src/core/page_ops.rs byte-level node insert/delete/resize on owned pages
src/core/freelist.rs page allocation and free DB maintenance (mdb_page_alloc, mdb_freelist_save)
src/core/cursor_put.rs  put, node add (write side)
src/core/split.rs   mdb_page_split
src/core/cursor_del.rs  del, rebalance, merge, node move, drop
src/core/commit.rs  mdb_txn_commit, mdb_page_flush, mdb_env_write_meta
src/core/nested.rs  nested transactions, cursor shadowing
src/core/spill.rs   mdb_page_spill / mdb_page_unspill
src/core/copy.rs    env copy (plain and compacting)
src/core/audit.rs   (tests only) page-usage audit like the C peer's check-freelist

planned, not implemented (heed layer):
src/env.rs, src/env_open_options.rs, src/txn.rs, src/database.rs,
src/iterator/*.rs, src/iteration_method.rs, src/reserved_space.rs,
src/traits.rs, src/types/*.rs
```

Keep files under ~1500 lines; split further rather than growing a file.

### Public Rust API: mirror `heed` 0.23.1

**Deferred (see "Status"): this subsection is the intended design; the
typed layer does not exist yet and the crate root re-exports `core::*`.**

The public API **mirrors the `heed` crate, version 0.23.1** (Meilisearch's
LMDB wrapper, the actively maintained Rust binding). Code written against
`heed` should compile against this crate with at most an import change. The
sources are in `~/.cargo/registry/src/*/heed-0.23.1/src/`,
`heed-traits-0.20.0/src/lib.rs` and `heed-types-0.21.0/src/`; read them and
copy names, signatures, generics, lifetimes and doc comments.

The crate has two layers:

* **`rlmdb::core`** (byte-level, documented, public): `Environment`,
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
  twice in one process because of the pid-based reader locks). The core
  already has this registry (`Error::EnvAlreadyOpened`, `EBUSY` in C); it
  is needed for correctness, not just API parity.
* `Env::open_databases_and_commit` / `create_databases_and_commit` helpers
  and the `OnCommit` machinery may be implemented as thin wrappers; they are
  low priority.
* Custom comparators (`C`, `CDUP` type params) are per-process callbacks in
  C too (`mdb_set_compare`); a C peer must install the same comparator. The
  core layer accepts `fn(&[u8], &[u8]) -> Ordering` comparators.

### Core layer (`rlmdb::core`)

This is the C API in Rust clothing, byte slices in and out, with lifetimes
doing the work that the C docs' "do not do X" paragraphs do. The sketch
below was the stage-1 plan; the rustdoc of `rlmdb::core` is the
reference. Additions since: `Transaction::open_db_bytes`, `set_compare`,
`set_dupsort`, `comparators`; `RwTxn::create_db_bytes`; handle-based
cursors (`RwTxn::open_cursor` -> `CursorId`, `cursor_get`/`cursor_put`/
`cursor_del`/`cursor_count`/..., what the C API uses, also through nested
transactions); `RoCursor::is_initialized`/`current_has_dups`;
`Info::map_address`; `Environment::has_lock_file`, `page_size`.

```rust
pub struct Environment;             // Send + Sync; Drop closes; share via Arc
pub struct EnvironmentBuilder;      // map_size, max_readers, max_dbs, flags(EnvironmentFlags), format(Format), open(path), open_with_permissions(path, mode)
impl Environment {
    pub fn open_db(&self, name: Option<&str>) -> Result<Dbi>;                     // existing only
    pub fn create_db(&self, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi>;
    pub fn close_db(&self, db: Dbi);                                             // later use -> Err(BadDbi)
    pub fn begin_ro_txn(&self) -> Result<RoTxn<'_>>;
    pub fn begin_rw_txn(&self) -> Result<RwTxn<'_>>;
    pub fn sync(&self, force: bool) -> Result<()>;
    pub fn stat(&self) -> Result<Stat>;  pub fn info(&self) -> Result<Info>;  pub fn freelist(&self) -> Result<usize>;
    pub fn flags(&self) -> EnvironmentFlags;  pub fn set_flags(&self, flags: EnvironmentFlags, on: bool) -> Result<()>;
    pub fn path(&self) -> &Path;  pub fn max_readers(&self) -> u32;  pub fn max_key_size(&self) -> usize;  // 511; 1.0: from the page size
    pub fn format(&self) -> Format;                                              // V09 or V10 (EnvironmentBuilder::format, ::page_size)
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
    pub fn begin_nested_txn(&mut self) -> Result<RwTxn<'_>>;
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
    pub fn del(&mut self, flags: WriteFlags) -> Result<()>;  pub fn count(&mut self) -> Result<u64>;
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
`WRITE_MAP`, `MAP_ASYNC`, `NO_MEM_INIT`) are all accepted and ignored
(`flags.rs` documents each); `PREV_SNAPSHOT` (1.0 only) is `EINVAL`.

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
plus `Other(c_int)` for other codes. As implemented these are two types,
shaped like heed's: `MdbError` (the variants above) and the crate-wide
`Error { Io(io::Error), Mdb(MdbError), Encoding, Decoding,
EnvAlreadyOpened }`, which the core API returns; `errno` values become
`Error::Io`. `Error::to_err_code()` returns exactly the C int
(`EnvAlreadyOpened` is `EBUSY`); `MdbError::from_err_code`; `Display`
strings match `mdb_strerror`.

### Concurrency model and memory safety

* The data file is mapped `PROT_READ, MAP_SHARED` at `map_size` bytes (may be
  larger than the file). `os::DataMap::slice(offset, len) -> Option<&[u8]>`
  is bounds-checked against the map; `TxnCore::page` checks page numbers
  against the snapshot's `last_pg`.
* LMDB's MVCC guarantees that pages reachable from a snapshot are not modified
  while a reader slot pins that snapshot. This is the same invariant the C
  code relies on; document it in `os::Mmap`.
* Meta pages *are* written concurrently by other processes. Read them with
  volatile reads (`ptr::read_volatile` in `os`) into an owned `Meta` value and
  validate (`magic`, `version`), re-reading as C does at txn begin
  (`mdb_txn_renew0`; the Rust reader additionally re-checks `mti_txnid`
  after the copy and retries, where C accepts a torn read). The re-check
  also keeps a process's snapshots monotonic: a delayed C reader can copy
  the meta page of a commit two later than the txnid it pins, and its next
  snapshot is then older (`KNOWN_C_BUGS.md` #31).
* Dirty pages in a write txn are owned `Box<[u8]>` buffers (one per page or
  overflow run), kept in a sorted list by pgno (`ID2L` semantics). Readers in
  the same txn resolve a pgno first in the dirty list, then parent txns'
  lists, then the map. At commit they are written with `pwritev` batching
  (`mdb_page_flush`, line 3394), data is `fdatasync`ed (or `fsync`/
  `F_FULLFSYNC`), then the meta page is written.
* Spilling (`mdb_page_spill`, line 2080) exists so huge write txns do not
  exhaust memory. Implemented (`core/spill.rs`) with C's exact choice of
  pages, so the free list comes out as C's; spilled pages are read back
  through the map.
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

* `#![deny(unsafe_code)]` at the crate root, `#[forbid(unsafe_code)]` on
  every module but `os` (`#![allow(unsafe_code)]` there, each `unsafe`
  block with a `// SAFETY:` comment).
* `#![warn(missing_docs, clippy::all)]`. `cargo clippy --all-targets -D
  warnings`, `cargo doc -D warnings` and `cargo fmt --check` clean; `cargo
  test` green on both platforms (`scripts/check-all.sh` runs all of it).
* Integers from disk are read with `u16::from_ne_bytes` / `u64::from_ne_bytes`
  over sub-slices; no `transmute`, no `#[repr(C)]` pointer casts of mapped
  memory outside `os`.
* Name things after the concept, not the C field: `lower`, `upper`, `num_keys`,
  `child_pgno`. Mention the C name in the doc comment once.
* Tests: unit tests next to the code for format parsing (hand-built pages),
  integration tests in `rlmdb/tests/` for API behaviour, and `interop/`
  for cross-process checks against the C build.
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
  `rlmdb/tests` and `rlmdb-capi/tests` build it against a small shim,
  the interop harness skips it.

## Status

The work was planned in stages, each ending with a commit by the
coordinator. What landed (commits on this branch):

**Stage 1** (in parallel):

* A. `rlmdb` foundation and the complete read path (`39256da`):
  consts, flags, errors, `format`, `idl`, `cmp`, `os` (maps, locks, shared
  mutex/semaphores, reader table), environment open including creating a
  new environment (`mdb_env_init_meta`), read transactions, `open_db`,
  read cursors with every `CursorOp` (DUPSORT, DUPFIXED/LEAF2, overflow
  pages), stat/info, `reader_list`/`reader_check`. Verified against files
  written by the C tools and `mtest*`. Findings folded in above
  (`b70ea62`).
* B. `interop` (`c13c67d`): the C library built with `cc`, the C peer and
  its command protocol, `layout_dump`, C-vs-C scenarios validating the
  harness. Linux testing in Docker: `scripts/linux-test.sh` (`d189fd1`).

**Stage 2:**

* C1. The write path, part 1 (`6046d62`): page allocation and free-list
  reuse, dirty pages, `mdb_page_touch`, node add, splits, put with every
  flag, commit (flush, sync, meta write), `set_map_size`. Deletes,
  rebalancing and merges landed here too (planned for stage 3).
* D1. `rlmdb-capi` read side (`6678f83`).
* B2. The Rust peer's read side; every scenario as "C writes, Rust reads";
  the layout test against the C dump (`d85dd7c`).
* The core API gaps the C API and interop agents reported (`f2db7cd`) all
  landed in stage 3: `Transaction::set_compare`/`set_dupsort` updating the
  calling transaction, `Transaction::open_db_bytes` (database names are
  bytes), `Info::map_address`, `Environment::has_lock_file`,
  `Transaction::comparators`, `RoCursor::is_initialized`/
  `current_has_dups`, `Environment::copy_to_path`/`copy_to_fd`, and the
  rename of the library to package `rlmdb`, lib `rlmdb`
  (`028c63f`).

**Stage 3:**

* C2. Nested transactions, spilling, `drop_db`/`clear_db`, plain and
  compacting copy, the API gaps above (`b275112`; findings below).
* D2. The rest of the C API: all 56 functions of `lmdb.h`; the unmodified
  `mtest.c` .. `mtest6.c` and C tools run against it with C's results
  (`36285e9`).
* B3. The Rust peer's write side, a third peer (the C peer linked against
  the Rust `liblmdb.a`), all nine writer/reader pairs, concurrent and
  alternating writers, stale readers (`25ae621`).
* T. `rlmdb-tools`: `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`,
  `mdb_drop`, byte-identical to the C tools (`4b5c420`).

**Stage 4** (review, follow-ups, docs):

* `rlmdb-capi`: cursors of a parent transaction work inside a nested
  transaction, as in C (the last known gap of the C API; the core
  supported it since stage 3). `mdb_cursor_txn` on such a cursor returns
  the active child.
* `rlmdb-tools`: the database-name errors (empty: `MDB_BAD_VALSIZE`; over
  511 bytes: `MDB_NOTFOUND` when opening, `MDB_BAD_VALSIZE` when creating)
  are checked against the C tools.
* `scripts/check-all.sh`; `README.md`, `COMPATIBILITY.md`, the crate
  documentation of `rlmdb`, and this section.
* Other stage-4 work (the review of `rlmdb` and of the C API's
  handle code, benchmarks against C) runs separately; the coordinator
  records it here when it lands.

**Deferred (not scheduled):** the heed-compatible layer over `core`. The
user wants the core validated through the C API first. The crate root
keeps re-exporting `core::*`; the heed section above stays as the intended
design only.

## Findings of stage 3 C2

Nested transactions, spilling, `mdb_drop`, env copy; see
`KNOWN_C_BUGS.md` #18-#22.

* Nested transactions move the parent's dirty/spilled pages and cursor
  arena into the child (`core/nested.rs`); the parent is borrowed. As in C,
  the parent's cursor handles (`CursorId`) work through the child (shadowed:
  kept on commit, restored on abort, `close_cursor` on them is a no-op);
  `RwCursor`s cannot cross (borrowck).
* Spilling is byte-for-byte C's: a full-size lock-step test
  (`write_differential::diff_spill_full_size`, ignored) ends with the same
  `mdb_stat -a -f` page usage as C. A child inherits its parent's dirty room
  and cannot re-spill pages an ancestor spilled, so it can hit `TXN_FULL`
  (ported, #22).
* `mdb_drop(del=1)` closes the handle at once (C); an aborting child
  reopens what it dropped (#20), and an abort closes handles of databases
  that no longer exist (#19).
* `copy_to_path`/`copy_to_fd` output is byte-identical to `mdb_copy` and
  `mdb_copy -c` (the compacting writer reproduces C's double-buffer reuse,
  so even unused page bytes match).
* Empty DB names fail with `MDB_BAD_VALSIZE` (C's `mdb_cursor_set`); names
  over 511 bytes are `NOTFOUND` on open and `BAD_VALSIZE` on create, as C;
  names with NUL bytes are `EINVAL`.
