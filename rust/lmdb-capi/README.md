# lmdb-capi: the LMDB C API over the Rust implementation

`lmdb-capi` builds `liblmdb.a`, `liblmdb.so` (Linux) and `liblmdb.dylib`
(macOS) from the Rust rewrite in `../lmdb`. They export the `mdb_*` symbols
of LMDB 0.9.36 with the same ABI, error codes and flag values, so existing C
programs compile against `include/lmdb.h` (the unmodified 0.9.36 header,
with one comment added on top) and link against the Rust library unchanged.

Status (stage 2, D1): the read side is complete. Write transactions and
environment copies arrive with the write path of the core (stage 3); until
then their entry points exist so programs link (see "Placeholders").

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

Note: the library target is named `lmdb`, like the core crate, so that the
artifacts are called `liblmdb.*`. `cargo build --workspace` therefore warns
about an output filename collision for `liblmdb.rlib`; nothing uses that
rlib (it only makes cargo build the library before this crate's tests).

## Differences from C LMDB 0.9.36

Everything not listed behaves as in C: the same checks in the same order
with the same return codes (`EINVAL`, `EACCES`, `MDB_BAD_TXN`,
`MDB_BAD_DBI`, `MDB_INCOMPATIBLE`, …), the same `mdb_strerror` texts, the
same `mdb_reader_list` lines, and values returned in `MDB_val`s point into
the memory map (zero copy).

| Area | C LMDB | This library |
|---|---|---|
| Threads | Without `MDB_NOTLS` a thread may hold one read transaction at a time (`MDB_BAD_RSLOT` otherwise) and a read transaction is tied to its thread. | Always behaves like `MDB_NOTLS`: any number of read transactions per thread, usable from any thread (one at a time). `MDB_NOTLS` is accepted. A superset of C. |
| Opening one environment twice in a process | Allowed but unsafe (closing one handle drops the other's locks). | `mdb_env_open` fails with `EBUSY`. |
| `MDB_FIXEDMAP`, `MDB_WRITEMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT` | Honoured. | Accepted and ignored (`mdb_env_get_flags` reports them). A C-created `MDB_FIXEDMAP` environment's recorded address is still reported by `mdb_env_info`. |
| Handles opened in a read transaction | Visible to other transactions only after that transaction commits; discarded if it aborts. | Published at once and kept after commit or abort. |
| `mdb_set_compare` / `mdb_set_dupsort` | Takes effect immediately, also in the calling transaction. | Takes effect in transactions that first use the handle after the call; a transaction that already used the handle (for example the one that just opened it) keeps the order it started with. Install comparators right after `mdb_dbi_open` and commit, as `lmdb.h` recommends. Known gap. A NULL function restores the default order (C would crash). At most 64 distinct comparison functions per process (`ENOMEM` beyond). |
| `mdb_dbi_open(txn, NULL, flags)` in a read transaction | ORs `flags` into the transaction's copy of the main DB record and resets a custom comparator of the main DB. | Ignores `flags`; keeps the comparator. |
| `mdb_cmp` / `mdb_dcmp` | Return the comparator's raw value. | Built-in orders return exactly -1, 0 or 1 (C functions installed by the program are called directly). Need an active transaction for the built-in orders; return 0 for invalid arguments. |
| `mdb_cursor_get` output `key` | `MDB_FIRST_DUP`/`MDB_LAST_DUP` on a key with duplicates and `MDB_PREV_MULTIPLE` on a positioned cursor leave `key` untouched. | Store the current key there (the same key the cursor is on). All other operations write exactly what C writes. |
| Cursor of a reset transaction after `mdb_txn_renew` without `mdb_cursor_renew` | Keeps stale page pointers (undefined). | Starts over unpositioned. |
| Using a cursor after its read transaction was committed or aborted (other than `mdb_cursor_renew`/`mdb_cursor_close`) | Use after free. | `EINVAL`; `mdb_cursor_txn` returns NULL. |
| NULL handles where C would crash (`mdb_txn_begin(NULL, …)`, `mdb_cursor_dbi(NULL)`, `mdb_env_stat` before open, …) | Crash. | `EINVAL` (or 0/NULL for functions without an error result). |
| `mdb_env_open` after a failed open on the same handle | Not allowed (`mdb_env_close` only). | Allowed. |
| `mdb_env_set_mapsize` on an open environment | No transaction may be active in the process. | Read transactions may be active (they keep their old map); `EINVAL` while a write transaction is active, as in C. |
| `mdb_env_set_assert` | Calls the function on internal assertion failures. | Stored, never called: the Rust core returns errors instead of asserting. |
| `mdb_set_relfunc` / `mdb_set_relctx` | Stored, never called. | Validated and ignored. |
| Database names in `mdb_dbi_open` | Any bytes. | Must be UTF-8 (the core API takes `&str`); other names give `MDB_NOTFOUND`. |
| `mdb_reader_list` on a read-only environment on a read-only file system (no lock file) | `(no reader locks)`. | `(no active readers)`. |
| C bugs not ported | See `../KNOWN_C_BUGS.md` (#4–#8 are visible through `mdb_cursor_get`/`mdb_cursor_count`). | |

## Placeholders (stage 3)

`write::stage3_stub()` returns `ENOSYS` and is the only placeholder:

* `mdb_txn_begin` without `MDB_RDONLY` on a writable environment (on a
  read-only environment it returns `EACCES`, as C), and nested transactions
  under a write parent;
* `mdb_env_copy`, `mdb_env_copyfd`, `mdb_env_copy2`, `mdb_env_copyfd2`.

`mdb_put`, `mdb_del`, `mdb_cursor_put`, `mdb_cursor_del` and `mdb_drop` are
complete as far as the read side goes: they perform C's argument checks and
return `EACCES` for read-only transactions, exactly as C does. Write
transactions cannot be created yet; the `MDB_txn` state enum has an
(uninhabited) `Write` variant so that stage 3 slots in.

## Design notes

* `MDB_env`, `MDB_txn` and `MDB_cursor` are Rust structs allocated with
  `Box` and handed to C as pointers (`src/handles.rs`).
* `src/erased.rs` is the one place where lifetimes are erased:
  `RoTxn<'env>` and `RoCursor<'txn>` are stored as `'static` in heap
  handles, under the C rules (environment outlives transactions, cursors
  are dropped before their transaction ends).
* C read-only transactions do not own their cursors; here each transaction
  keeps a list of the cursors bound to it and drops their Rust state before
  it resets or ends, leaving the C handles valid for `mdb_cursor_renew` and
  `mdb_cursor_close`.
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
oracle) and against the Rust `liblmdb.a` that cargo builds for the tests:

* `tests/tools.rs`: the unchanged `mdb_stat.c` and `mdb_dump.c`, linked
  against both libraries, must print byte for byte the same (stdout, stderr,
  exit status) for `mdb_stat` with `-e`, `-a`, `-f`/`-ff`/`-fff`,
  `-r`/`-rr`, `-s`, `-n`, and `mdb_dump` with `-a`, `-l`, `-p`, `-s`, on
  environments written by C `mtest`..`mtest5` (including the recorded
  `MDB_FIXEDMAP` address), `mdb_load` (integer keys and duplicates, reverse
  orders, `MDB_DUPFIXED`, overflow pages) and `tests/c/fixture.c`.
* `tests/capi_read.rs`: `tests/c/capi_read.c` checks every implemented
  function and the error codes (bad arguments, finished transactions,
  unknown databases, cursor operations at the ends, `MDB_GET_MULTIPLE` on
  `MDB_DUPFIXED`, reader list text, custom comparators, reset/renew, cursor
  lifetimes and closing order, `MDB_DBS_FULL`, writes in read-only
  transactions). It must pass against the C library and the Rust library.
  Known gaps are marked `GAP(...)`: the Rust build prints them as `known
  gap: ...` and fails if one starts behaving like C, so this list stays
  accurate.
* `tests/concurrency.rs`: `tests/c/capi_snapshot.c`, linked against the
  Rust library, holds read transactions while C writers (`mtest`,
  `fixture.c`) commit to the same environment: snapshots stay identical
  byte for byte and at the same addresses, C's `mdb_stat -r` sees the
  reader slots, and a new transaction after the file outgrew the map fails
  with `MDB_MAP_RESIZED` until `mdb_env_set_mapsize(env, 0)` adopts the
  new size. The C-linked build runs the same scenario.

Known gaps reported by the Rust build today: `mdb_set_compare` in the
transaction that opened the handle; write transactions; `mdb_env_copy*`.
