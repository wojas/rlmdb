# lmdb-capi: the LMDB C API over the Rust implementation

`lmdb-capi` builds `liblmdb.a`, `liblmdb.so` (Linux) and `liblmdb.dylib`
(macOS) from the Rust rewrite in `../lmdb-core`. They export the `mdb_*`
symbols of LMDB 0.9.36 with the same ABI, error codes and flag values, so
existing C programs compile against `include/lmdb.h` (the unmodified 0.9.36
header, with one comment added on top) and link against the Rust library
unchanged.

Status (stage 4): every function of `lmdb.h` is implemented, read and
write side. The unmodified upstream programs `mtest.c` .. `mtest6.c`,
`mdb_load.c`, `mdb_dump.c`, `mdb_copy.c` and `mdb_stat.c` run against it and
produce what they produce against the C library, down to the page numbers on
the free list (see "Acceptance"). There are no known gaps (see "Known
gaps").

## Building and linking

```sh
cd rust
cargo build -p lmdb-capi --release
# target/release/liblmdb.a, target/release/liblmdb.{so,dylib}
```

Compile C programs with `-I rust/lmdb-capi/include`. Link the static
library together with the system libraries the Rust standard library needs
(from `cargo rustc -p lmdb-capi --lib --crate-type staticlib -- --print
native-static-libs`):

| Platform | Link line |
|---|---|
| Linux (glibc) | `liblmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc` |
| macOS | `liblmdb.a` (needs `-lSystem -lc -lm`, which the compiler driver adds) |

For example `cc -I rust/lmdb-capi/include -o mdb_stat
libraries/liblmdb/mdb_stat.c rust/target/release/liblmdb.a -lpthread
-lgcc_s -lutil -lrt -lm -ldl` on Linux. The shared library needs no extra
flags (`-L rust/target/release -llmdb`).

Note: the library target is named `lmdb` so that the artifacts are called
`liblmdb.*`; the Rust library it is built on is the `lmdb-core` package
(lib `lmdb_core`).

## Functions

All 56 functions of `lmdb.h` are implemented (module in parentheses):

| Group | Functions |
|---|---|
| Version, errors (`version`) | `mdb_version`, `mdb_strerror` |
| Environment (`env`) | `mdb_env_create`, `mdb_env_open`, `mdb_env_close`, `mdb_env_stat`, `mdb_env_info`, `mdb_env_sync`, `mdb_env_set_flags`, `mdb_env_get_flags`, `mdb_env_get_path`, `mdb_env_get_fd`, `mdb_env_set_mapsize`, `mdb_env_set_maxreaders`, `mdb_env_get_maxreaders`, `mdb_env_set_maxdbs`, `mdb_env_get_maxkeysize`, `mdb_env_set_userctx`, `mdb_env_get_userctx`, `mdb_env_set_assert`, `mdb_reader_list`, `mdb_reader_check` |
| Copies (`write`) | `mdb_env_copy`, `mdb_env_copyfd`, `mdb_env_copy2`, `mdb_env_copyfd2` (plain and `MDB_CP_COMPACT`) |
| Transactions (`txn`) | `mdb_txn_begin` (read-only, write, nested), `mdb_txn_env`, `mdb_txn_id`, `mdb_txn_commit`, `mdb_txn_abort`, `mdb_txn_reset`, `mdb_txn_renew` |
| Databases (`dbi`) | `mdb_dbi_open` (with `MDB_CREATE`), `mdb_stat`, `mdb_dbi_flags`, `mdb_dbi_close`, `mdb_set_compare`, `mdb_set_dupsort`, `mdb_set_relfunc`, `mdb_set_relctx`, `mdb_cmp`, `mdb_dcmp` |
| Data (`cursor`, `write`) | `mdb_get`, `mdb_put`, `mdb_del`, `mdb_drop` |
| Cursors (`cursor`, `write`) | `mdb_cursor_open`, `mdb_cursor_close`, `mdb_cursor_renew`, `mdb_cursor_txn`, `mdb_cursor_dbi`, `mdb_cursor_get`, `mdb_cursor_put`, `mdb_cursor_del`, `mdb_cursor_count` |

`mdb_set_relfunc`/`mdb_set_relctx` are validated and ignored, as LMDB 0.9
never calls the function; `mdb_env_set_assert` stores the callback, which
is never called (the core reports errors instead of asserting).

## Write side

Everything behaves as in C, with the same checks in the same order and the
same return codes:

* `mdb_txn_begin` without `MDB_RDONLY` waits for the environment's writer
  lock (shared with C processes); `EACCES` in a read-only environment. With
  a `parent`: a nested transaction (`EINVAL` for a read-only parent,
  `MDB_BAD_TXN` with `MDB_RDONLY` or when the parent already has a child or
  failed). While a child is active, the parent is blocked: its operations
  return `MDB_BAD_TXN`. Committing or aborting a parent ends its child
  first. The parent's cursors (and those of further ancestors) work
  through the active child, as in C (`mdb_cursor_shadow`):
  `mdb_cursor_txn` returns the child, they see and change the child's
  data and are moved by its changes; when the child commits they keep the
  position they have then, when it aborts they are put back where they
  were when it began. `mdb_cursor_close` ignores them while the child is
  active.
* `mdb_put`/`mdb_cursor_put`: all flags. `MDB_RESERVE` returns the reserved
  space in `data->mv_data` (valid until the next change in the
  transaction); `MDB_NOOVERWRITE` returns the existing data in `data`
  together with `MDB_KEYEXIST` (the first duplicate for `MDB_DUPSORT`);
  `MDB_MULTIPLE` takes the two-element array, stores `data[1].mv_size`
  items and returns their number in `data[1].mv_size`, advancing
  `data[0].mv_data` to the last item as C does; `MDB_CURRENT` replaces the
  item under the cursor; `MDB_APPEND`/`MDB_APPENDDUP` return
  `MDB_KEYEXIST` for out-of-order input. `mdb_cursor_put` ignores unknown
  flag bits, as C. Size limits: keys 1..511 bytes, `MDB_DUPSORT` data
  1..511 bytes (`MDB_BAD_VALSIZE`).
* `mdb_del` with or without `data`, `mdb_cursor_del` with `MDB_NODUPDATA`,
  `mdb_cursor_count`: as C; other cursors on the same database are moved
  as C moves them (a cursor on a deleted item goes to the next one).
* `mdb_drop(del = 0)` empties a database, `mdb_drop(del = 1)` deletes it and
  closes its handle at once (also if the transaction aborts later, as in
  C); cursors on it become unpositioned.
* `mdb_dbi_open` with `MDB_CREATE` in a write transaction creates the
  database; its handle is usable in that transaction and its children,
  becomes visible to other transactions when it commits, and is closed if
  it aborts. Names are arbitrary bytes (not necessarily UTF-8).
* `mdb_set_compare`/`mdb_set_dupsort` take effect at once, also in the
  calling transaction (C's usual `mdb_dbi_open` -> `mdb_set_compare` ->
  `mdb_put` in one transaction).
* An operation that fails half-way (`MDB_MAP_FULL`, `MDB_TXN_FULL`,
  `MDB_PAGE_FULL`, I/O errors) leaves the transaction unusable: later
  operations return `MDB_BAD_TXN`, and `mdb_txn_commit` returns
  `MDB_BAD_TXN`, aborts and frees it. A failed child makes its parent fail.
  `mdb_txn_abort(NULL)` is a no-op; `mdb_txn_commit(NULL)` is `EINVAL`.
* Cursors of a write transaction are closed (freed) when it ends, unless
  `mdb_cursor_close` closed them before (as in C: do not use or close them
  afterwards); they cannot be renewed (`EINVAL`), nor can a read cursor be
  renewed onto a write transaction. Cursors opened in a nested transaction
  are closed when it ends.

**Thread rule.** A write transaction must be committed or aborted on the
thread that began it, as in C: it holds the writer lock, which on Linux is
a process-shared robust pthread mutex owned by the locking thread (on macOS
a POSIX semaphore). Other threads beginning a write transaction wait for
it; a second `mdb_txn_begin` for writing on the same thread deadlocks, as
in C. Read transactions are not bound to a thread (below).

## Differences from C LMDB 0.9.36

Everything not listed behaves as in C, and values returned in `MDB_val`s
point into the memory map or the transaction's dirty pages (zero copy).

| Area | C LMDB | This library |
|---|---|---|
| Threads | Without `MDB_NOTLS` a thread may hold one read transaction at a time (`MDB_BAD_RSLOT` otherwise) and a read transaction is tied to its thread. | Always behaves like `MDB_NOTLS`: any number of read transactions per thread, usable from any thread (one at a time). `MDB_NOTLS` is accepted. A superset of C. |
| Opening one environment twice in a process | Allowed but unsafe (closing one handle drops the other's locks). | `mdb_env_open` fails with `EBUSY`. |
| `MDB_FIXEDMAP`, `MDB_WRITEMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT` | Honoured. | Accepted and ignored (`mdb_env_get_flags` reports them). A new environment records map address 0 (`KNOWN_C_BUGS.md` #9); a C-created `MDB_FIXEDMAP` environment's recorded address is reported by `mdb_env_info`. |
| Handles of existing databases opened in a transaction | Visible to other transactions only after that transaction commits; closed if it aborts (read or write transaction). | Published at once and kept after commit or abort. Handles of databases *created* by a write transaction follow C (published at commit, closed on abort). |
| A parent transaction while its nested child is active | Most functions return `MDB_BAD_TXN`, but some check the DBI first (`EINVAL` for a bad one) and `mdb_dbi_flags`, `mdb_cmp`, `mdb_dcmp` still work. | Every function on the parent returns `MDB_BAD_TXN` (`mdb_txn_id`, `mdb_txn_env` work). |
| `MDB_MULTIPLE` that fails after storing some items | `data[1].mv_size` = items stored, `data[0].mv_data` advanced to the failing one. | `data[1].mv_size` = 0, `data[0].mv_data` unchanged. Failures there leave the transaction unusable anyway. |
| Keys and data that point into the transaction's own dirty pages | Read while the page is being changed (works in common cases, garbage or use-after-free in others). | Keys and data up to 16 KiB are copied before the write, so they are always read intact. |
| A C comparison function that calls back into the library with the transaction being written | Reads inconsistent state. | `EINVAL`. |
| `mdb_set_compare` / `mdb_set_dupsort` with NULL | Stores NULL, crashes on the next comparison. | Restores the default order. At most 64 distinct comparison functions per process (`ENOMEM` beyond). |
| `mdb_dbi_open(txn, NULL, flags)` in a read transaction | ORs `flags` into the transaction's copy of the main DB record and resets a custom comparator of the main DB. | Ignores `flags`; keeps the comparator. |
| `mdb_cmp` / `mdb_dcmp` | Return the comparator's raw value. | Built-in orders return exactly -1, 0 or 1 (C functions installed by the program are called directly). Return 0 for invalid arguments. |
| Cursor of a reset transaction after `mdb_txn_renew` without `mdb_cursor_renew` | Keeps stale page pointers (undefined). | Starts over unpositioned. |
| Using a cursor after its read transaction was committed or aborted (other than `mdb_cursor_renew`/`mdb_cursor_close`) | Use after free. | `EINVAL`; `mdb_cursor_txn` returns NULL. |
| NULL handles and outputs where C would crash (`mdb_txn_begin(NULL, …)`, `mdb_dbi_open(…, NULL)`, `mdb_cursor_put(…, NULL data, …)`, `mdb_env_copy2(NULL, …)`, `mdb_cursor_dbi(NULL)`, `mdb_env_stat` before open, …) | Crash. | `EINVAL` (or 0/NULL for functions without an error result). |
| `mdb_env_open` after a failed open on the same handle | Not allowed (`mdb_env_close` only). | Allowed. |
| `mdb_env_set_mapsize` on an open environment | No transaction may be active in the process. | Read transactions may be active (they keep their old map); `EINVAL` while a write transaction is active, as in C. |
| `mdb_env_set_assert` | Calls the function on internal assertion failures. | Stored, never called: the Rust core returns errors instead of asserting. |
| `mdb_set_relfunc` / `mdb_set_relctx` | Stored, never called. | Validated and ignored. |
| C bugs not ported | See `../KNOWN_C_BUGS.md`. Visible through the C API: #4–#6 and #8 (`mdb_cursor_get`/`mdb_cursor_count` on cursors left past the end), #12 (`MDB_DUPFIXED` items of another size: `MDB_BAD_VALSIZE`), #13 (`MDB_CURRENT` on a cursor not on an item: `MDB_NOTFOUND`), #14 (stale duplicate counts of other cursors), #15 (`MDB_CURRENT` growing a duplicate on a full sub-page), #16 (`MDB_RESERVE` on `MDB_DUPSORT`: `EINVAL`), #17 (`MDB_DUPFIXED` without `MDB_DUPSORT`: `MDB_INCOMPATIBLE` on writes). | |

## Known gaps

None. `tests/c/capi_write.c` and `tests/c/capi_read.c` keep the `GAP()`
macro for C behaviour the Rust library does not have yet: the Rust build
prints such a gap and fails once it behaves like C, so this section stays
accurate. (The last one, cursors of a parent transaction used inside a
nested one, was closed in stage 4.)

## Acceptance: upstream programs against the Rust library

`tests/mtest.rs` builds the unmodified upstream programs twice, against the
C library and against the Rust `liblmdb.a`:

* `mtest.c` .. `mtest6.c` seed `rand()` with `time(NULL)`; the tests build
  them with `-Dtime=mtest_time -include tests/c/mtest_shim.h`, which makes
  the seed `$MTEST_SEED` (and supplies `mdb_dkey()`, which `mtest6.c` needs
  and `liblmdb` only has with `MDB_DEBUG`). With seeds 20261030 and 7 each
  program, against either library, exits 0 with nothing on stderr and the
  **same stdout** (pointer values aside), and the environments they leave
  are **identical as far as the C tools can tell**: C `mdb_stat -e -f -ff
  -a` (depths, page counts, entries, the free list page by page) and C
  `mdb_dump -a` print the same, apart from the recorded `MDB_FIXEDMAP`
  address (C records it, Rust writes 0). The C peer's page audit
  (`check-freelist`) passes on both, and the Rust-linked `mdb_stat` and
  `mdb_dump` read the Rust-written files exactly as the C ones do.
* `mdb_load.c`/`mdb_dump.c`: a dump with integer keys (4 and 8 bytes),
  reverse keys, sorted and reverse-sorted duplicates, `MDB_DUPFIXED`
  integer duplicates in sub-databases and overflow values, loaded by the C
  and by the Rust-linked `mdb_load`, gives page-for-page the same
  environment; the Rust-linked `mdb_dump` of the C-loaded one and the C
  `mdb_dump` of the Rust-loaded one equal the C reference, also per
  database and with `-p`, and a dump/load round trip through the Rust tools
  reproduces the data.
* `mdb_copy.c`, plain and `-c`, linked against Rust, of an `mtest3`
  environment (with free pages) and of the loaded one: the C tools see the
  same copy as of the C-linked `mdb_copy`, and the source's data.

## Design notes

* `MDB_env`, `MDB_txn` and `MDB_cursor` are Rust structs allocated with
  `Box` and handed to C as pointers (`src/handles.rs`).
* `src/erased.rs` is the one place where lifetimes are erased:
  `RoTxn<'env>`, `RoCursor<'txn>` and `RwTxn<'env>` (also a nested
  `RwTxn<'parent>`) are stored as `'static` in heap handles, under the C
  rules (environment outlives transactions, cursors are dropped before
  their read transaction ends, a parent is not used while its child lives).
* C read-only transactions do not own their cursors; here each transaction
  keeps a list of the cursors bound to it and drops their Rust state before
  it resets or ends, leaving the C handles valid for `mdb_cursor_renew` and
  `mdb_cursor_close`. Cursors of write transactions are the core's tracked
  cursor handles (`CursorId`), adjusted by every change in the
  transaction as C adjusts `mt_cursors`; the transaction frees their C
  handles when it ends.
* While a nested transaction is active, its parent's state is lent to it
  (the core's child borrows the parent): the parent's `MDB_txn` refuses
  every access until the child has ended.
* C comparison functions are adapted through a table of 64 trampolines keyed
  by the C function pointer (`src/cmp.rs`): the core takes plain
  `fn(&[u8], &[u8]) -> Ordering` comparators that cannot carry a pointer.

## Tests

```sh
cd rust
cargo test -p lmdb-capi                         # macOS / host
scripts/linux-test.sh test -p lmdb-capi         # Linux in Docker
```

The tests compile C programs at test time with the `cc` crate (no build
script) against the C library (built from `../libraries/liblmdb`, the
oracle) and against the Rust `liblmdb.a` that cargo builds for the tests.
Each test process works in its own directory under `target/tmp/capi/`
(see `tests/common/mod.rs`): several test runs may share a target directory
at once (`scripts/linux-test.sh` mounts one Docker volume as `target/` for
every worktree), and fixed directories let them destroy each other's
environments.

* `tests/tools.rs`: the unchanged `mdb_stat.c` and `mdb_dump.c`, linked
  against both libraries, print byte for byte the same (stdout, stderr,
  exit status) for every option, on environments written by C
  `mtest`..`mtest5`, `mdb_load` and `tests/c/fixture.c`.
* `tests/capi_read.rs`: `tests/c/capi_read.c` checks every read-side
  function and its error codes; it must pass against the C library and the
  Rust library.
* `tests/capi_write.rs`: `tests/c/capi_write.c` checks every write-side
  function and its error codes against both libraries: transactions
  (visibility, empty commits, read-only environments), database handles
  (create, abort, `MDB_DBS_FULL`, `MDB_INCOMPATIBLE`), puts with every flag
  (`MDB_RESERVE` inline and on overflow pages, `MDB_NOOVERWRITE` returning
  the existing data, `MDB_APPEND`/`MDB_APPENDDUP`, `MDB_MULTIPLE`), size
  limits, overflow values grown and shrunk, deletes with and without data,
  keys that point into the transaction's own pages, cursor puts and
  deletes while other cursors are positioned, `MDB_CURRENT`, `DUPSORT`
  sub-page to sub-DB promotion and back, custom comparators used in the
  creating transaction, `MDB_MAP_FULL` and the `MDB_BAD_TXN` state,
  threads waiting for the writer lock, `mdb_drop`, nested transactions
  (commit, abort, two levels, failing children, parents committed or
  aborted with an active child, parent cursors used, moved, written and
  deleted through a child and a grandchild, kept on commit and restored on
  abort)
  and all four copy functions. Besides the checks, both builds print a
  trace of what they observe (statistics, counts, keys, values, copy
  layouts), which must be identical; every environment left behind passes
  the C peer's page audit. `GAP()` marks known gaps; `DIFFERS()` checks a
  difference documented above against both libraries (the C result in the
  C build, the Rust result in the Rust build); `DEVIATION()` marks
  deliberate differences where C would corrupt data (`KNOWN_C_BUGS.md`),
  checked in the Rust build only.
* `tests/mtest.rs`: the acceptance tests above.
* `tests/concurrency.rs`: `tests/c/capi_snapshot.c` holds read transactions
  while writers (the fixed-seed `mtest`, `tests/c/fixture.c`) commit to the
  same environment from other processes: snapshots stay identical byte for
  byte and at the same addresses, C's `mdb_stat -r` sees the reader slots,
  and a new transaction after the file outgrew the map fails with
  `MDB_MAP_RESIZED` until `mdb_env_set_mapsize(env, 0)` adopts the new
  size. Readers and writers run linked against either library, in every
  combination.
* `tests/cross_process.rs`: the interop test bed's C peer
  (`../interop/c`), built against both libraries, runs writer loops of both
  at once (nested transactions, aborts, all put and delete flavours; the
  shared writer lock must serialise them without lost updates) next to
  reader loops of both, which check every snapshot; then the C page audit.
  And the peer's nested-transaction test, written through one library and
  checked through the other.
