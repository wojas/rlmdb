# Compatibility contract

What the Rust rewrite (`rlmdb`, and `rlmdb-capi`/`rlmdb-tools` built on
it) promises with respect to C LMDB 0.9.36 (`../libraries/liblmdb`), and
which tests check each promise. The last section covers the LMDB 1.0 data
file, the second format `rlmdb` handles. "C" below means the C library built with
its platform defaults (the upstream `Makefile`: `MDB_USE_POSIX_MUTEX` with
robust mutexes on Linux, `MDB_USE_POSIX_SEM` on macOS), 64-bit.

## Platforms

| Platform | Lock primitive (C's and ours) | Status |
|---|---|---|
| macOS (arm64) | POSIX named semaphores `/MDBr…`, `/MDBw…`; reader slots at offset 128 | tested |
| Linux, glibc, aarch64 | process-shared robust `pthread_mutex_t` (48 bytes) in the lock file; slots at 192 | tested (Docker, `scripts/linux-test.sh`) |
| Linux, glibc, x86-64 | as above, 40-byte mutex; slots at 128 | layout follows `size_of::<pthread_mutex_t>()`; not run in CI here |
| FreeBSD, Android | robust pthread mutex layout | compiles in principle; untested |
| Windows, 32-bit targets | | not supported (`compile_error!`) |

C and Rust processes must share the platform ABI: a lock file is only
meaningful between processes that agree on `pthread_mutex_t` (Linux) or
the semaphore naming (macOS). This is the same restriction C has between
two C builds; it does not hold across a musl and a glibc build, or a C
build with non-default lock options (`MDB_USE_SYSV_SEM`, a Linux build
with `MDB_USE_POSIX_SEM`).

## The files

* **Data file: format 1** (`MDB_DATA_VERSION`, `mm_magic 0xBEEFC0DE`),
  the 64-bit layout, host byte order. Every structure (page header,
  nodes, `P_LEAF2` and `P_SUBP` pages, overflow runs, `MDB_db`, `MDB_meta`)
  is read and written with C's exact layout. Either implementation opens
  what the other created, and continues writing it.
* **Lock file: format 1** (`MDB_LOCK_VERSION`, stored as
  `0x10001` with `MDB_PIDLOCK`), with C's layout for the platform (table
  above): header with reader and writer locks, then 64-byte reader slots.
  A Rust process can be the first opener (it creates and initialises the
  lock file, the mutexes or semaphores) or a later one; C on the other
  side either way. Reader liveness uses the same `fcntl` byte locks at the
  process id.
* **Lock file: format 2** (LMDB 1.0's `MDB_LOCK_VERSION 2`), the lock file
  of LMDB 1.0 data files: the version follows the data format. Header
  `magic, format, txnid, numreaders`, then the reader lock; `mti_format` is
  `2 + (MDB_lock_desc << 12)`, a per-platform constant computed with C's
  formula: `0x28570002` on macOS, `0x2cd82002` on Linux aarch64 glibc,
  `0x2c2f6002` on Linux x86-64 glibc (measured once with `layout_dump10`
  in an emulated x86-64 container; the test runs do not cover x86-64).
  On macOS the file holds a 64-bit hash (`mtb_mutexid`) and
  the semaphore names are derived from it, as 1.0 does; there is no writer
  lock cache line, so the reader slots start at 64. On Linux the mutexes
  are at 24 and 128 (64 on x86-64), the slots at 192 (128). rlmdb creates
  and attaches to it as C 1.0 does, but only through the lock-file code so
  far (`rlmdb::core::reader::LockFile`, not part of the API): an
  `Environment` refuses `Format::V10` until the 1.0 data layout exists.
* **Lock files of the other version** are refused with
  `MDB_VERSION_MISMATCH` while another process holds them, and
  re-initialised in the opener's version by an exclusive opener, by C and
  Rust alike.
* **Page size** is chosen when an environment is created and recorded in
  the file, as in C: the OS page size, capped at 32 KiB (`MAX_PAGESIZE`):
  4 KiB on Linux, 16 KiB on Apple Silicon macOS. An existing environment
  is always opened with the page size recorded in it.
* **Maximum key size** is 511 bytes (`mdb_env_get_maxkeysize`), also for
  `MDB_DUPSORT` data items; the item size that moves a value to overflow
  pages is computed as C does.
* **What a Rust writer leaves behind** is what a C writer would leave for
  the same operations: same tree shape and page counts (including
  `md_branch_pages`/`md_leaf_pages` counting sub-DB pages), same free list
  page by page, same spill decisions. Compacting copies are byte-identical
  to `mdb_copy -c`, plain copies to `mdb_copy`. Where C 0.9.36 would leave
  a corrupt tree or leak pages (`KNOWN_C_BUGS.md` #11, #14, #15, #19), Rust
  writes the correct result instead, which C reads normally. Where a C bug
  decides shared state (sort order of misused integer keys #7, pages
  leaked by `mdb_drop` on the main DB #21, a child's dirty room #22), the
  bug is ported, so both sides agree.
* A new Rust environment records map address 0 (`mm_address`), because
  `MDB_FIXEDMAP` is ignored; to C, 0 means "map anywhere" (#9). The
  `MDB_FIXEDMAP` bit itself is stored in the meta flags as C stores it.

## Mixed C and Rust processes

* Any number of C and Rust processes may use one environment at the same
  time: readers of both see consistent snapshots, and writers of both are
  serialised by the shared writer lock (a write transaction of either
  waits for the other's). MVCC holds across implementations: a snapshot a
  reader of either pins is never overwritten by a writer of either. A C
  reader that races a commit in `mdb_txn_renew0` can see a snapshot newer
  than the txnid it pins, and its next snapshot is then older
  (`KNOWN_C_BUGS.md` #31); readers on `rlmdb` re-check the meta page and
  never go back.
* A process that dies holding the writer lock: on Linux the robust mutex
  lets the next writer (C or Rust) recover, as between C processes. On
  macOS semaphores are not robust; C has the same limitation.
* Reader slots of dead processes stay until `mdb_reader_check` (C or Rust,
  `mdb_stat -rr` of either) or an exclusive re-open clears them, as in C.
* When a writer of either side grows the data file beyond another
  process's map, that process gets `MDB_MAP_RESIZED` at the next
  transaction begin and adopts the new size with `mdb_env_set_mapsize(env,
  0)` (`Environment::set_map_size`), as in C.
* Custom comparators are per-process callbacks in both implementations:
  every process must install the same order (`mdb_set_compare`).
* **Not supported:** opening the same environment in one process through
  both the C library and the Rust library. POSIX record locks belong to the
  process, so closing either handle would drop the other's locks. (Within
  the Rust library, a second open of the same path fails with `EBUSY`.)
* `MDB_NOLOCK` environments have no lock file in either implementation; the
  application must serialise access, as in C.

## Flags

Environment flags (`mdb_env_open`, `EnvironmentFlags`):

| Flag | Rust |
|---|---|
| `MDB_NOSUBDIR`, `MDB_RDONLY`, `MDB_NOSYNC`, `MDB_NOMETASYNC`, `MDB_NOLOCK`, `MDB_NORDAHEAD` | honoured, as in C |
| `MDB_NOTLS` | accepted; always in effect (see below) |
| `MDB_WRITEMAP`, `MDB_MAPASYNC` | accepted and ignored: pages are always written with `pwrite`, which is compatible with C peers that use a writable map |
| `MDB_FIXEDMAP` | accepted and ignored (the bit is recorded in a new file, the address is 0) |
| `MDB_NOMEMINIT` | accepted and ignored (buffers are always zeroed) |
| `MDB_PREVSNAPSHOT` (1.0) | `EINVAL`, as 0.9 does for flags it does not know |

`mdb_env_set_flags` changes the same flags as C (`MDB_NOSYNC`,
`MDB_NOMETASYNC`, `MDB_MAPASYNC`, `MDB_NOMEMINIT`). All of these are
per-process choices that the files do not depend on (except
`MDB_NOSUBDIR`, which names the files), so a C peer cannot tell which a
Rust process uses.

Database flags (`MDB_REVERSEKEY`, `MDB_DUPSORT`, `MDB_INTEGERKEY`,
`MDB_DUPFIXED`, `MDB_INTEGERDUP`, `MDB_REVERSEDUP`, `MDB_CREATE`) and write
flags (`MDB_NOOVERWRITE`, `MDB_NODUPDATA`, `MDB_CURRENT`, `MDB_RESERVE`,
`MDB_APPEND`, `MDB_APPENDDUP`, `MDB_MULTIPLE`) are all honoured with C's
semantics and stored as C stores them, with three refusals where C would
corrupt data: `MDB_RESERVE` on `MDB_DUPSORT` is `EINVAL` (#16),
`MDB_DUPFIXED` items of another size are `MDB_BAD_VALSIZE` (#12), and
writes to a database with `MDB_DUPFIXED` but not `MDB_DUPSORT` are
`MDB_INCOMPATIBLE` (#17).

## `MDB_NOTLS`: a superset

C without `MDB_NOTLS` ties a read transaction to its thread (one per
thread, reader slot in thread-local storage). The Rust library always
behaves like `MDB_NOTLS`: a reader slot belongs to a transaction, a
thread may hold any number of read transactions, and a read transaction
may move between threads (`RoTxn: Send`). The lock file does not record
the choice, and slots are used the same way, so C peers in either mode
are unaffected. Every program correct against C is correct against the
Rust library; the reverse is not true for programs that rely on it.

## API-level deviations

Everything below is visible only to the program calling the library; none
of it changes the files or what another process concludes about them.

* Plain C bugs not ported (`KNOWN_C_BUGS.md`): cursor operations on a
  cursor left past the end of a page (#4–#6, #8, #13, #18) return
  `MDB_NOTFOUND` or move sensibly instead of reading free page space;
  other cursors' duplicate counts are correct (#14); malformed input
  gives errors, never a crash or a read outside a slice (#3). Tool fixes:
  `mdb_load` keeps the first database's flags (#1) and handles `-a`,
  `format=print` and odd lines correctly (#23–#26), `mdb_stat -r` exits 0
  (#2).
* Version: `mdb_version()` reports 0.9.85 and `LMDB 0.9.85: (rlmdb
  <version>)`, and the tools' `-V` prints that string; the shipped `lmdb.h`
  stays the 0.9.36 header. The patch level, above every C 0.9 release,
  tells rlmdb apart. `rlmdb_version()` (`rlmdb-capi/include/rlmdb.h`;
  `rlmdb::version_info()` in Rust) reports the rest: rlmdb's version, the
  C release the file format matches (0.9.36) and capability tokens.
* Through the C API (`rlmdb-capi/README.md`, "Differences from C LMDB
  0.9.36"): opening one environment twice in a process is `EBUSY`; NULL
  handles and use after free return `EINVAL` instead of crashing;
  handles of existing databases opened in a transaction are published at
  once; every function on a parent with an active child returns
  `MDB_BAD_TXN`; `MDB_MULTIPLE` that fails part-way reports 0 items;
  `mdb_cmp`/`mdb_dcmp` with built-in orders return -1/0/1;
  `mdb_set_compare(NULL)` restores the default order; at most 64 distinct
  C comparison functions per process; `mdb_env_set_assert` callbacks are
  never called (the library returns errors instead of asserting);
  `mdb_env_set_mapsize` is allowed while read transactions are active.

## What verifies it

All of these run on macOS and, through `scripts/linux-test.sh`, on Linux;
`scripts/check-all.sh` runs both. In each, a C library or tool built from
`../libraries/liblmdb` at test time is the oracle.

| Promise | Verified by |
|---|---|
| Struct layout of every on-disk and lock-file structure equals C's | `interop/tests/layout.rs` (C `layout_dump` vs the Rust constants), `rlmdb` unit tests in `format/` |
| Rust reads C-written files exactly | `rlmdb/tests/c_databases.rs` (cursor walks, `get`, all `SET*`/dup ops vs `mdb_dump`/`mdb_stat` on `mtest*`/`mdb_load` output), `rlmdb/tests/handbuilt.rs`, `interop` C→Rust scenarios |
| C reads Rust-written files; no page lost or shared | `rlmdb/tests/rust_writes.rs`, `write_interop.rs`, `interop` Rust→C scenarios, the C peer's page audit (`check-freelist`) after every scenario and C API test |
| Rust writes produce C's trees, cursor positions and free list | `rlmdb/tests/write_differential.rs` (same scripts in C and Rust, every cursor compared after every step; full-size spill comparison, ignored by default), `rlmdb-capi/tests/mtest.rs` (upstream `mtest*` against both libraries: same `mdb_stat -ff` free list, same dumps) |
| Concurrent C and Rust processes on one environment | `interop` (three peers: C, Rust, C-on-Rust; all nine writer/reader pairs; concurrent writer loops of both with reader loops of all three; `three_peers`), `rlmdb/tests/c_concurrency.rs`, `rlmdb-capi/tests/concurrency.rs` and `cross_process.rs` |
| Lock file created by either side | `interop` `reader_creates_lock_file`, `rlmdb/tests/c_concurrency.rs` (`rust_creates_environment_c_uses_it`, `c_creates_environment_rust_attaches`, `exclusive_opener_resizes_small_lock_file`) |
| Lock file format 2 (LMDB 1.0): every offset, size and `MDB_LOCK_FORMAT` equals C 1.0's; `mti_format` per platform; macOS semaphore names | `interop/tests/lockfile10.rs` `lock_file_lines_match_c` (`layout_dump` and `layout_dump10` vs `LOCK_V09`/`LOCK_V10`), `rlmdb` unit tests in `format/lockfile.rs` (the measured values, the `MDB_lock_desc` formula for all three platforms) and `os/shared_lock.rs` (`mdb_pack85` names) |
| Lock file format 2 shared with C 1.0 processes: rlmdb attaches to C 1.0's table (header, `mtb_mutexid` equal to C's hash, C's reader in `reader_list` as in `mdb_stat10 -r`, writer lock exclusive both ways, a killed C reader cleared by `reader_check`) and C 1.0 attaches to rlmdb's (lists the Rust reader, `mdb_stat10 -e` sees rlmdb's table size, a C commit moves rlmdb's `mti_txnid`) | `interop/tests/lockfile10.rs` `rust_attaches_to_c10_lock_file`, `c10_attaches_to_rust_lock_file` |
| Lock files of the two versions refuse each other while held (`MDB_VERSION_MISMATCH` from rlmdb, C 0.9 and C 1.0), and an exclusive opener re-initialises the other version | `interop/tests/lockfile10.rs` `lock_versions_refuse_each_other` and the two tests above; `rlmdb` `core::reader` unit test `exclusive_opener_reinitialises_other_version` |
| Reader table, stale readers, `mdb_reader_check` | `interop` `readers_list`, `stale_reader_cleared_by_reader`, `stale_reader_cleared_by_mdb_stat`; `rlmdb-tools/tests/readers.rs` |
| MVCC across implementations, map growth | `interop` `mvcc_hold_reader`, `rlmdb-capi/tests/concurrency.rs` (snapshots byte-identical while the other side commits; `MDB_MAP_RESIZED`) |
| `MDB_NOTLS` superset | `interop` `notls_readers` and every scenario (C peers in TLS mode next to Rust readers), `rlmdb/tests/rust_env.rs` (`transactions_move_between_threads`) |
| `MDB_NOSUBDIR`, read-only, `MDB_NOLOCK` | `interop` `nosubdir`, `rlmdb/tests/core_api_c2.rs` |
| Nested transactions | `rlmdb/tests/nested_txn.rs` (written by one side, checked by the other), `interop` `nested_txns`, `reader_loop_during_nested`, `rlmdb-capi/tests/c/capi_write.c` |
| Copies byte-identical to `mdb_copy [-c]` | `rlmdb/tests/env_copy.rs`, `rlmdb-tools/tests/copy_drop.rs`, `interop` `copy_dump_equal` |
| The C API behaves as C (return codes, outputs, traces) | `rlmdb-capi/tests/c/capi_read.c`, `capi_write.c` (run against both libraries; traces must be identical; `DIFFERS()`/`DEVIATION()` mark the documented deviations), `rlmdb-capi/tests/tools.rs` (upstream `mdb_stat.c`/`mdb_dump.c` linked against both print the same) |
| The tools behave as C's | `rlmdb-tools/tests/*.rs` (every option on every environment: same stdout, stderr, exit code; loaded environments identical as C sees them; `-V` prints rlmdb's version line where C prints its own) |
| `mdb_version` reports 0, 9, 85 and `LMDB 0.9.85: (rlmdb <version>)`; major and minor equal `lmdb.h`'s | `rlmdb-capi/tests/c/capi_read.c` (`MDB_VERSION_STRING` and 0.9.36 for C, 0.9.85 for Rust), `capi_version.c`, unit tests in `rlmdb/src/version.rs`, `rlmdb-capi/src/version.rs`, `rlmdb-tools/src/common.rs` |
| `rlmdb.h`: `rlmdb_version()` (struct `size`, numbers, sorted NULL-terminated capabilities), `rlmdb_version_string()` (the `Display` line of `rlmdb::VersionInfo`), `rlmdb_has_capability()`; usable from C++ | `rlmdb-capi/tests/capi_version.rs` (`tests/c/capi_version.c` built as C and as C++, against the Rust library) |
| Corrupt input never crashes | `rlmdb` `format::page` unit tests (`corrupt_pointers_do_not_panic`), `rlmdb/tests/handbuilt.rs` (`corrupt_pages_give_errors_not_panics`) |

## The LMDB 1.0 data file

rlmdb also reads and writes the data file of C LMDB 1.0 (`mdb.RE/1.0` at
4865523: LMDB 1.0.2 plus ITS#10642/10643; `MDB_DATA_VERSION 3`), with the
differences of `DESIGN.md` ("The LMDB 1.0 data file"). The promises:

* **Format selection.** An existing data file is opened in its format
  (magic and version of the first meta page, `rlmdb::sniff_format`); asking
  for the other format is `MDB_VERSION_MISMATCH`, as is a version 2 file
  (lmdb-js prerelease builds); a header of neither format is `MDB_INVALID`.
  All of it is decided before the lock file is touched. A new environment is
  LMDB 0.9 unless 1.0 is requested (`EnvironmentBuilder::format`).
* **Page sizes** 256 B to 64 KiB for new 1.0 environments
  (`EnvironmentBuilder::page_size`, C 1.0's `mdb_env_set_pagesize`; the OS
  page size by default); the maximum key size and the overflow threshold
  follow the page size as in C 1.0 (442 bytes on 1 KiB pages, 1978 on 4 KiB,
  8122 on 16 KiB, 32698 on 64 KiB).
* **What a Rust writer leaves behind** in a 1.0 environment is what C 1.0
  leaves for the same operations, page by page: tree shape, split points,
  free list, spill decisions, sub-pages, `MDB_ovpage` references, and the
  `mp_txnid` of every page (`mdb_page_flush`'s stamp, which C 1.0 writers
  rely on). Copies are byte-identical to `mdb_copy10` and `mdb_copy10 -c`;
  a load is page for page `mdb_load10`'s.
* **Refused 1.0 features:** encrypted environments (`MDB_ENV_ENCRYPTION`)
  and page checksums (`MDB_BAD_CHECKSUM`), with C 1.0's codes.
* **Lock file:** a 1.0 environment uses the version 2 lock file ("Lock
  file: format 2" above), so C 1.0 and rlmdb processes share an open
  environment: readers pin pages against the other side's writer, and
  both see each other's readers and the writer lock. The C API and the
  tools (`rlmdb-capi`, `rlmdb-tools`) are 0.9's (stage 4).

| Promise (1.0 format) | Verified by |
|---|---|
| Format detection and selection, page sizes, key size limit, refused features | `rlmdb/tests/format_v10.rs`, `rlmdb/tests/rust_env.rs` (`data_file_format`), `rlmdb::format::meta` unit tests (`sniff`) |
| The 1.0 page, node, overflow and meta layout | `rlmdb::format::{layout, page, node, meta}` unit tests (hand-built 1.0 pages), `interop/tests/oracle10.rs` (`layout_dump10`) |
| Rust reads C 1.0's files exactly (512 B to 64 KiB pages, `mtest*10`) | `interop/tests/format10.rs` (`rlmdb_reads_c10_files_at_every_page_size`, `rlmdb_reads_mtest10_environments`: cursor walks both ways, `get`, statistics vs `mdb_dump10`/`mdb_stat10`) |
| Rust writes C 1.0's pages, cursor positions and free list, at 1, 4, 16 and 64 KiB pages | `interop/tests/write_differential10.rs` (the lock-step harness of `rlmdb/tests/common/differential.rs` with `writeops` built against 1.0: plain, integer, reverse, DUPSORT, DUPFIXED, overflow overwrites, deep trees, nested transactions, drops, spills; every step compared, then `mdb_dump10`, `cpeer10 check-freelist`, and `interop::pagecmp` page by page including `mp_txnid` and `MDB_ovpage`) |
| Spills write what C 1.0 would | `write_differential10.rs` (`diff10_spilling_rust_side`: Rust spills where C does not, same files; `diff10_spill_full_size`, `#[ignore]`d: both spill), `rlmdb/tests/format_v10.rs` (`spilling_leaves_the_same_file`) |
| Copies byte-identical to `mdb_copy10 [-c]`, loads to `mdb_load10` | `interop/tests/format10.rs` (`copies_match_mdb_copy10`, `rlmdb_loads_what_mdb_load10_loads`) |
| C 1.0 continues writing what Rust wrote and the reverse | `interop/tests/format10.rs` (`c10_and_rlmdb_take_turns`, with the C 1.0 page audit after every turn) |
| The page audit and the structural comparison understand 1.0 | `interop/tests/audit10.rs` |
| Corrupt 1.0 input never crashes | `rlmdb/tests/robust_mutation.rs` (`*_v10`, `robust_huge_overflow_refs_v10`), `rlmdb/tests/format_v10.rs` (`unknown_headers_are_refused_before_the_lock_file`) |
| Concurrent C 1.0 and Rust processes | `interop/tests/format10.rs` (`c10_reader_pins_pages_against_rlmdb_writer`, `rlmdb_reader_pins_pages_against_c10_writer`, `c10_and_rlmdb_share_an_open_environment`), `interop/tests/lockfile10.rs` |
