# rlmdb-capi: the LMDB C API over the Rust implementation

`rlmdb-capi` builds `liblmdb.a`, `liblmdb.so` (Linux) and `liblmdb.dylib`
(macOS), on Windows `lmdb.dll` with its import library and a static
library (see "Windows" below), from the Rust rewrite in `../rlmdb`. They export the `mdb_*`
symbols of LMDB 0.9.36 with the same ABI, error codes and flag values, so
existing C programs compile against `include/lmdb.h` (the unmodified 0.9.36
header, with one comment added on top) and link against the Rust library
unchanged. `include/rlmdb.h` declares the few functions rlmdb has beyond
`lmdb.h` and the LMDB 1.0 constants (see "`rlmdb.h`" below).

The library opens environments of both data-file formats rlmdb handles,
LMDB 0.9 and LMDB 1.0 (`../DUAL_FORMAT.md`), through the same `mdb_*`
functions: an existing file in its format, a new one as LMDB 0.9 unless
1.0 is asked for (`rlmdb_env_set_format`, `rlmdb_set_default_format`, or
the experimental `RLMDB_DEFAULT_FORMAT=1.0`; see "`rlmdb.h`: data-file
formats").

Status (stage 4): every function of `lmdb.h` is implemented, read and
write side. The unmodified upstream programs `mtest.c` .. `mtest6.c`,
`mdb_load.c`, `mdb_dump.c`, `mdb_copy.c` and `mdb_stat.c` run against it and
produce what they produce against the C library, down to the page numbers on
the free list (see "Acceptance"). There are no known gaps (see "Known
gaps").

## Building and linking

```sh
cargo build -p rlmdb-capi --release
# target/release/liblmdb.a, target/release/liblmdb.{so,dylib}
```

Compile C programs with `-I rlmdb-capi/include`. Link the static
library together with the system libraries the Rust standard library needs
(from `cargo rustc -p rlmdb-capi --lib --crate-type staticlib -- --print
native-static-libs`):

| Platform | Link line |
|---|---|
| Linux (glibc) | `liblmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc` |
| macOS | `liblmdb.a` (needs `-lSystem -lc -lm`, which the compiler driver adds) |
| Windows, MinGW-w64 (`x86_64-pc-windows-gnu`) | `liblmdb.a -lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp` |
| Windows, MSVC (`x86_64-pc-windows-msvc`) | `lmdb.lib legacy_stdio_definitions.lib kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib` (compile with `cl /MD`: the library uses the dynamic C runtime, `msvcrt`) |

For example `cc -I rlmdb-capi/include -o mdb_stat
tests/lmdb-c/0.9/mdb_stat.c target/release/liblmdb.a -lpthread
-lgcc_s -lutil -lrt -lm -ldl` on Linux. The shared library needs no extra
flags (`-L target/release -llmdb`).

On the BSDs (untested, see `COMPATIBILITY.md`; from `--print
native-static-libs` of the Rust targets): FreeBSD `-lrt -lutil -lexecinfo
-lkvm -lmemstat -lprocstat -ldevstat -lpthread -lgcc_s -lc -lm`, NetBSD
`-lutil -lrt -lexecinfo -lpthread -lgcc_s -lc -lm`, OpenBSD `-lutil
-lexecinfo -lpthread -lc++abi -lc -lm -lcompiler_rt`, DragonFly `-lrt
-lutil -lexecinfo -lkvm -lpthread -lgcc_pic -lc -lm`.

Note: the library target is named `lmdb` so that the artifacts are called
`liblmdb.*`; the Rust library it is built on is the `rlmdb` package
(lib `rlmdb`).

### Windows

64-bit Windows with either Rust toolchain; `cargo build -p rlmdb-capi
--release` (add `--target x86_64-pc-windows-gnu` or use that toolchain for
MinGW) writes to `target/release/`:

| | MSVC | MinGW-w64 |
|---|---|---|
| DLL | `lmdb.dll` | `lmdb.dll` |
| its import library | `lmdb.dll.lib` | `liblmdb.dll.a` |
| static library | `lmdb.lib` | `liblmdb.a` |

Link a C program against the DLL through its import library, which needs
nothing else (`cl /I rlmdb-capi\include prog.c target\release\lmdb.dll.lib`,
`gcc -I rlmdb-capi/include prog.c target/release/liblmdb.dll.a`), and put
`lmdb.dll` next to the program or on `PATH`; or link the static library
with the system libraries of the table above. `lmdb.h` has no
`__declspec(dllimport)`: the functions are reached through the import
library's stubs, as with a C `lmdb.dll`. A program that calls `LoadLibrary`
and `FreeLibrary` gets a fresh copy of the DLL after the last
`FreeLibrary`, as with C's DLL (no `-z nodelete` counterpart is needed).

On Windows the types and texts follow C LMDB's `_WIN32` build:
`mdb_filehandle_t` is a `HANDLE` (`mdb_env_get_fd` gives the data file's,
`INVALID_HANDLE_VALUE` before the open; `mdb_env_copyfd` takes one),
`mdb_mode_t` is `int` with MSVC and MinGW's 16-bit `mode_t` (ignored, as in
C), paths are UTF-8, and errors that come from the system are `GetLastError`
codes whose `mdb_strerror` text is the system's message (the C runtime's
`strerror` for the `errno` values LMDB uses itself); each such text is kept
for the life of the process, where C returns a buffer on its stack. The
upstream `mtest*.c` and tools build unchanged with MinGW's `gcc`; MSVC has
no `getopt`, which the tools need, so the tests give `cl` the
`tests/compat/msvc/unistd.h` of this repository.

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

## `rlmdb.h`

`include/rlmdb.h` includes `lmdb.h` and declares rlmdb's own functions:
the version and capabilities (module `version`) and the data-file formats
(module `format`), plus the LMDB 1.0 constants. C LMDB does not have the
functions, so a program that calls them needs this library. The header's
comments are their reference documentation.

### Version and capabilities

```c
#include "rlmdb.h"

typedef struct rlmdb_version_info {
	unsigned	size;		/* sizeof(rlmdb_version_info) of the library */
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;	/* the rlmdb version */
	int		lmdb_major, lmdb_minor, lmdb_patch;	/* what mdb_version() reports: 0, 9, 85 */
	int		v09_major, v09_minor, v09_patch;	/* C release the 0.9 format and API match: 0, 9, 36 */
	int		v10_major, v10_minor, v10_patch;	/* C release the 1.0 format matches: 1, 0, 2 */
	const char *const *capabilities;		/* sorted, NULL-terminated */
} rlmdb_version_info;

const rlmdb_version_info *rlmdb_version(void);	/* static, never freed */
const char *rlmdb_version_string(void);		/* static, one line */
int rlmdb_has_capability(const char *name);	/* 1 or 0; 0 for NULL */
```

* `rlmdb_version()` returns a static structure. Fields are only ever
  appended; `size` tells a program compiled against a newer header which
  fields the library has.
* `rlmdb_version_string()` returns everything in one line, for logs and
  bug reports: `rlmdb 0.1.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36;
  format 1.0 = LMDB 1.0.2; caps: default-format-env format-0.9
  format-1.0)`.
* `rlmdb_has_capability(name)` tests for one capability token. Tokens are
  short lower-case names, added when a feature lands and kept while it
  exists. Today: `default-format-env` (the experimental
  `RLMDB_DEFAULT_FORMAT` is honoured), `format-0.9`, `format-1.0`; planned
  are `split-freelist`, `convert-copy`, `prevsnapshot`, `txn-prepare`,
  `checksum` and `encrypt`. `v10_*` are 0, 0, 0 in a library without
  `format-1.0`.

The Rust side is `rlmdb::version_info()`, from which the C structure and
strings are filled once, at first use.

**`mdb_version()` reports 0.9.85**, and the string
`LMDB 0.9.85: (rlmdb 0.1.0)` (the rlmdb version where C has its release
date). The patch level is above every real 0.9 release, so a program can
tell rlmdb from C LMDB, and the version stays in the 0.9 series that rlmdb
is a drop-in for. The shipped `lmdb.h` keeps `MDB_VERSION_PATCH 36`
(`MDB_VERSION_STRING` is `LMDB 0.9.36: (Aug 6, 2026)`): it is unmodified,
and a run-time patch level above the header's is the normal case for LMDB
(a program built against an older 0.9 header runs with a newer 0.9
library); `mdb_version()` is what programs are told to check. Major and
minor agree with the header.

### Data-file formats

```c
#define RLMDB_FORMAT_UNKNOWN 0
#define RLMDB_FORMAT_09      9
#define RLMDB_FORMAT_10     10

int rlmdb_env_set_format(MDB_env *env, unsigned format);    /* before mdb_env_open; 0 removes the request */
int rlmdb_env_get_format(MDB_env *env, unsigned *format);   /* requested before the open, actual after */
int rlmdb_sniff_format(const char *path, unsigned flags, unsigned *format); /* MDB_NOSUBDIR honoured */
int rlmdb_set_default_format(unsigned format);              /* process-wide; RLMDB_FORMAT_UNKNOWN resets */
int rlmdb_env_set_pagesize(MDB_env *env, unsigned size);    /* LMDB 1.0's mdb_env_set_pagesize */
```

* An existing data file is opened in its format, whatever the defaults say;
  a request for the other format (`rlmdb_env_set_format`) makes
  `mdb_env_open` fail with `MDB_VERSION_MISMATCH`, before the lock file is
  touched. A file that is no LMDB data file is `MDB_INVALID`.
* A new environment gets the requested format, else the process default
  (`rlmdb_set_default_format`), else the format the environment variable
  `RLMDB_DEFAULT_FORMAT` names (`0.9` or `1.0`; other values are ignored),
  else LMDB 0.9. **The variable is experimental**: it may be removed or
  restricted in a later release (capability `default-format-env` says
  whether it is honoured). It is read at every open; programs should use
  the functions.
* `rlmdb_env_set_pagesize` (a power of two from 256 to 65536, as in C 1.0)
  applies to a new 1.0 environment; `mdb_env_open` fails with `EINVAL` if
  the environment turns out to be LMDB 0.9, which has no such setting.
* `rlmdb.h` also defines LMDB 1.0's `MDB_PREVSNAPSHOT`, `MDB_ENCRYPT` and
  error codes `MDB_PROBLEM` .. `MDB_ADDR_BUSY` (-30779 .. -30769), with
  1.0's values, each only if no other header defined it, so a program can
  also include upstream 1.0's `lmdb.h`. `mdb_strerror` returns 1.0's
  messages for them.

What changes with an LMDB 1.0 environment:

* `mdb_env_get_maxkeysize` returns the environment's limit after the open
  (1978 on 4 KiB pages, 32698 on 64 KiB); before it, 511 unless 1.0 was
  requested with `rlmdb_env_set_format`, then the 1.0 limit for the page
  size a new environment would get. (C 1.0 returns 0 before the open.)
* Keys and `MDB_DUPSORT` items up to that limit, inline data up to half a
  page (64 KiB pages: 32 KiB); named databases are stored under their name
  and its NUL, as C 1.0 does (`mdb_dbi_open` takes the name as before).
* `mdb_drop` of the main DB follows C 1.0: if it records named databases,
  they are deleted first (their pages are freed), and it fails with
  `MDB_DBIS_BUSY` while a handle of a named database is open (0.9 leaves
  their pages behind, `../KNOWN_C_BUGS.md` #21).
* Not implemented: `MDB_PREVSNAPSHOT` (`mdb_env_open` gives `EINVAL`, for
  both formats), `MDB_ENCRYPT` (`EINVAL` from `mdb_env_open` and
  `mdb_env_set_flags`), encrypted or checksummed 1.0 files
  (`MDB_ENV_ENCRYPTION`, `MDB_BAD_CHECKSUM`), and the rest of 1.0's API.
* Return codes stay `lmdb.h`'s (0.9's): a write in a read-only transaction
  is `EACCES` where C 1.0 says `MDB_IS_READONLY`.

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
  1..511 bytes (`MDB_BAD_VALSIZE`); in an LMDB 1.0 environment up to
  `mdb_env_get_maxkeysize`.
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
in C (with `MDB_NOLOCK` also in another handle of the same environment,
whose writers share the lock). Read transactions are not bound to a thread
(below).

## Differences from C LMDB 0.9.36

Everything not listed behaves as in C, and values returned in `MDB_val`s
point into the memory map or the transaction's dirty pages (zero copy).

| Area | C LMDB | This library |
|---|---|---|
| `mdb_version()` | The header's version: 0, 9, 36 and `MDB_VERSION_STRING`, `LMDB 0.9.36: (Aug 6, 2026)`. | 0, 9, 85 and `LMDB 0.9.85: (rlmdb <version>)` (see "`rlmdb.h`"); `lmdb.h` still says 0.9.36. |
| Data-file formats | LMDB 0.9 only; a 1.0 file is `MDB_INVALID`. | LMDB 0.9 and 1.0 (see "`rlmdb.h`: data-file formats"); new environments are 0.9 by default. |
| `mdb_strerror` of LMDB 1.0's codes (-30779 .. -30769) | "Invalid error code". | 1.0's messages. |
| Threads | Without `MDB_NOTLS` a thread may hold one read transaction at a time (`MDB_BAD_RSLOT` otherwise) and a read transaction is tied to its thread. | Always behaves like `MDB_NOTLS`: any number of read transactions per thread, usable from any thread (one at a time). `MDB_NOTLS` is accepted. A superset of C. |
| Opening one environment twice in a process | Allowed but unsafe (closing one handle drops the other's locks). | `mdb_env_open` fails with `EBUSY`, except under `MDB_NOLOCK`, where handles share one in-process reader table and writer lock (see "Several handles and `dlopen`"). |
| Read transactions with `MDB_NOLOCK` | Not tracked: a write transaction of the same process reuses pages a live read transaction still reads (`lmdb.h` asks the caller to prevent that), and writers of different handles are not serialised. | Tracked in memory (`../KNOWN_C_BUGS.md` #35), in one table for all `MDB_NOLOCK` handles of the environment in the process, which also share a writer lock: writers of the process never reuse pages its readers read. `mdb_env_info` (`me_numreaders` 0), `mdb_reader_list` (`(no reader locks)`) and `mdb_reader_check` (none cleared) report what C reports. Other processes remain the caller's job, as in C. |
| `MDB_FIXEDMAP`, `MDB_WRITEMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT` | Honoured. | Accepted and ignored (`mdb_env_get_flags` reports them). A new environment records map address 0 (`KNOWN_C_BUGS.md` #9); a C-created `MDB_FIXEDMAP` environment's recorded address is reported by `mdb_env_info`. |
| Handles of existing databases opened in a transaction | Visible to other transactions only after that transaction commits; closed if it aborts (read or write transaction). | Published at once and kept after commit or abort. Handles of databases *created* by a write transaction follow C (published at commit, closed on abort). |
| A parent transaction while its nested child is active | Most functions return `MDB_BAD_TXN`, but some check the DBI first (`EINVAL` for a bad one) and `mdb_dbi_flags`, `mdb_cmp`, `mdb_dcmp` still work. | Every function on the parent returns `MDB_BAD_TXN` (`mdb_txn_id`, `mdb_txn_env` work). |
| `MDB_MULTIPLE` that fails after storing some items | `data[1].mv_size` = items stored, `data[0].mv_data` advanced to the failing one. | `data[1].mv_size` = 0, `data[0].mv_data` unchanged. Failures there leave the transaction unusable anyway. |
| Keys and data that point into the transaction's own dirty pages | Read while the page is being changed (works in common cases, garbage or use-after-free in others). | Keys (up to the environment's maximum key size) and data up to 16 KiB, and `MDB_MULTIPLE` arrays up to 32 KiB (what `MDB_GET_MULTIPLE` returns), are copied before the write, so they are always read intact; in an LMDB 1.0 environment with 64 KiB pages the data limits are 32 and 64 KiB (half a page, a page). Names given to `mdb_dbi_open` in a write transaction are copied too (a 1.0 key of the main DB ends with the name's NUL). Larger data must not point into the transaction's pages (see "Undefined behaviour" below). |
| A C comparison function that calls back into the library with the transaction being written | Reads inconsistent state. | `EINVAL`. |
| A C comparison function that frees a handle the running operation uses (`mdb_txn_abort`/`mdb_txn_commit` of its transaction or an ancestor, `mdb_cursor_close` of the cursor) | Use after free. | The process aborts with a message (`lmdb: ... called on a handle that is in use`). `mdb_txn_reset` there does nothing, `mdb_txn_renew` returns `EINVAL`. |
| A callback that throws a C++ exception (comparison functions, the `mdb_reader_list` message function) | Unwinds through the library's C frames, leaving locks held and state half-changed. | The process aborts at the library boundary (the callback types are `extern "C-unwind"`; every exported function is `extern "C"`). |
| `mdb_set_compare` / `mdb_set_dupsort` with NULL | Stores NULL, crashes on the next comparison. | Restores the default order. At most 64 distinct comparison functions per process (`ENOMEM` beyond). |
| `mdb_dbi_open(txn, NULL, flags)` in a read transaction | ORs `flags` into the transaction's copy of the main DB record and resets a custom comparator of the main DB. | Ignores `flags`; keeps the comparator. |
| `mdb_cmp` / `mdb_dcmp` | Return the comparator's raw value. | Built-in orders return exactly -1, 0 or 1 (C functions installed by the program are called directly). Return 0 for invalid arguments. |
| Cursor of a reset transaction after `mdb_txn_renew` without `mdb_cursor_renew` | Keeps stale page pointers (undefined). | Starts over unpositioned. |
| Using a cursor after its read transaction was committed or aborted (other than `mdb_cursor_renew`/`mdb_cursor_close`) | Use after free. | `EINVAL`; `mdb_cursor_txn` returns NULL. |
| NULL handles and outputs where C would crash (`mdb_txn_begin(NULL, …)`, `mdb_dbi_open(…, NULL)`, `mdb_cursor_put(…, NULL data, …)`, `mdb_env_copy2(NULL, …)`, `mdb_cursor_dbi(NULL)`, `mdb_env_stat` before open, …) | Crash. | `EINVAL` (or 0/NULL for functions without an error result). |
| `mdb_env_open` after a failed open on the same handle | Not allowed (`mdb_env_close` only). | Allowed. |
| `mdb_env_set_mapsize` on an open environment | No transaction may be active in the process. | Read transactions may be active (they keep their old map); `EINVAL` while a write transaction is active, as in C. |
| A thread that exits with its write transaction still open (a bug in the program) | Linux: the writer mutex reports its dead owner (`EOWNERDEAD`); the next `mdb_txn_begin` for writing in the same process returns `MDB_PANIC` and the environment is unusable, other processes recover and go on. macOS: every writer waits forever (semaphores are not robust). | Every later writer in the same process waits forever, on Linux too (write transactions of one process are also serialised by an in-process lock, which the abandoned transaction still holds). Other processes behave as with C. |
| `mdb_env_set_assert` | Calls the function on internal assertion failures. | Stored, never called: the Rust core returns errors instead of asserting. |
| `mdb_set_relfunc` / `mdb_set_relctx` | Stored, never called. | Validated and ignored. |
| C bugs not ported | See `../KNOWN_C_BUGS.md`. Visible through the C API: #4–#6 and #8 (`mdb_cursor_get`/`mdb_cursor_count` on cursors left past the end), #12 (`MDB_DUPFIXED` items of another size: `MDB_BAD_VALSIZE`), #13 (`MDB_CURRENT` on a cursor not on an item: `MDB_NOTFOUND`), #14 (stale duplicate counts of other cursors), #15 (`MDB_CURRENT` growing a duplicate on a full sub-page), #38 (`MDB_CURRENT` with a key's only `MDB_DUPSORT` item too big for a node: deleted and inserted, other cursors on it move on), #16 (`MDB_RESERVE` on `MDB_DUPSORT`: `EINVAL`), #17 (`MDB_DUPFIXED` without `MDB_DUPSORT`: `MDB_INCOMPATIBLE` on writes). | |

## Undefined behaviour that remains

These are the rules of `lmdb.h` whose violation is undefined behaviour in
C; they stay undefined here (the library cannot detect them cheaply), and
the safe Rust core cannot be used to break them:

* Using a handle after it was freed: `mdb_env_close` while transactions or
  cursors of the environment are alive (or from a callback), any call on
  a closed environment, closing a handle twice, using or closing a cursor
  of a write transaction after the transaction ended (it is freed with
  it), using a transaction after `mdb_txn_commit`/`mdb_txn_abort`.
* Using one transaction (or its cursors) from two threads at the same
  time; ending a write transaction on another thread than the one that
  began it.
* A callback that leaves the library with `longjmp` (it skips Rust
  frames).
* Data larger than 16 KiB (half of 1.0's 64 KiB page in such an
  environment) passed to `mdb_put`/`mdb_cursor_put` that points into the
  transaction's own dirty pages (a value read earlier in the same write
  transaction): the put may free those pages while it copies the data, as
  C does.
* Reading an `MDB_val` returned by a write transaction after the next
  change in it, or one returned by a read transaction after it ended.
* Another program truncating or rewriting the data file in place (outside
  the LMDB protocol) while it is mapped: pages of a live snapshot are read
  without copying; a shrunken file raises `SIGBUS`, as in C.
* Damage that makes writers reuse pages a reader still sees: a free list
  in the data file that lists a page still in use, or a lock file into
  which another program wrote a too-high `mti_txnid` or cleared the slot of
  a live reader. The writer (C's or this one) then rewrites such a page,
  and values a read transaction returned change under it. This is the one
  case where a damaged file reaches the Rust core's guarantees too: every
  access stays bounds-checked, so the result is wrong data or
  `MDB_CORRUPTED`, never an access outside the map, but formally a value
  changing under a live Rust reference is undefined behaviour
  (`os::DataMap` in `rlmdb`). Free-list records that name meta pages or
  pages past the end are refused; other lock-file damage only yields
  errors (`rlmdb/tests/robust_lockfile.rs`).
* Another process writing an `MDB_NOLOCK` environment while this one reads
  or writes it: `MDB_NOLOCK` drops every cross-process lock, and `lmdb.h`
  leaves keeping processes apart to the caller. This process's read
  transactions are tracked in memory, so its own writers keep their pages,
  but a writer in another process does not know them and may rewrite them,
  with the consequences of the previous item. The same holds for another
  copy of the library in this process (see "Several handles and
  `dlopen`").

## Several handles and `dlopen`

Any number of `MDB_env` handles may have one environment open with
`MDB_NOLOCK`, read-only or read-write, each with its own map and settings,
opened, used and closed from any threads: for example one handle per
embedded interpreter in a program that runs several in threads, each
loading the library through its binding with `dlopen`. Within one copy of
the library they share one reader table and one writer lock per data file
(identified by device and inode, so symlinks, an `MDB_NOSUBDIR` path to
the data file and hard links are the same file): writers of all handles
are serialised, and none reuses a page a read transaction of another
handle still reads. The shared state exists while one of the handles is
open. With a lock file the rule is C's (`EBUSY`, above), and a handle with
a lock file and an `MDB_NOLOCK` handle of one file refuse each other.

`dlopen` of the library any number of times, with `RTLD_LOCAL` or
`RTLD_GLOBAL`, returns the one loaded copy, so all handles share that
state. A program that contains separate copies of the library (the static
`liblmdb.a` linked into several modules, or `dlmopen` namespaces) has one
registry per copy: each copy coordinates only its own handles, as separate
C libraries coordinate nothing. Read-only handles are unaffected (nothing
writes); a copy that writes must be the only user of the file, as with C
`MDB_NOLOCK`.

`dlclose` does not unload the Rust library: on Linux it is linked with `-z
nodelete`, and on macOS dyld never unloads an image with thread-local
variables, which Rust's standard library has. A later `dlopen` returns the
loaded copy, with its state (the registry, the strings `mdb_strerror` and
`rlmdb_version` return). The C library is unloaded by its last `dlclose`.
`tests/capi_dlopen.rs` checks both.

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
cargo test -p rlmdb-capi                         # macOS / host
scripts/linux-test.sh test -p rlmdb-capi         # Linux in Docker
WINDOWS_HOST=<ssh host> scripts/windows-test.sh cargo test -p rlmdb-capi
```

The tests compile C programs at test time with the `cc` crate (the crate's
build script only sets a link option of the shared library) against the C
library (built from `../tests/lmdb-c/0.9`, the oracle) and against the
Rust `liblmdb.a` that cargo builds for the tests; `capi_dlopen` loads the
shared builds of both instead.
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
* `tests/capi_version.rs`: `tests/c/capi_version.c` checks `rlmdb.h`
  against the Rust library (there is no C oracle): the structure's `size`
  and numbers, the sorted, NULL-terminated capability list,
  `rlmdb_has_capability` (also with NULL and unknown names),
  `rlmdb_version_string()` equal to the Rust `Display` line and
  `mdb_version`'s string. The same program is compiled as C++ and linked,
  which checks the header's `extern "C"` guards.
* `tests/capi_format.rs`: `tests/c/capi_format.c` checks the format
  functions and constants of `rlmdb.h` against the Rust library (no C
  oracle): `rlmdb_env_set_format`/`get_format` before and after the open,
  `rlmdb_sniff_format` (no file, empty file, both formats, `MDB_NOSUBDIR`,
  junk, version 2), `rlmdb_set_default_format` and the
  `RLMDB_DEFAULT_FORMAT` variable (set and unset in the program's own
  process) in their order, `rlmdb_env_set_pagesize`'s checks,
  `mdb_env_get_maxkeysize` before and after the open, 1.0 environments of
  1, 4 and 64 KiB pages with the longest keys and `MDB_DUPSORT` items,
  inline data above 16 KiB, `MDB_RESERVE`, `MDB_CURRENT`, `MDB_MULTIPLE`
  of a page of items, `mdb_del`, and inputs (keys, data, a database name)
  that point into the transaction's dirty pages; a 1.0 file reopened
  without a request; `MDB_VERSION_MISMATCH` both ways; `mdb_strerror` of
  every 1.0 code; `MDB_PREVSNAPSHOT` and `MDB_ENCRYPT` refused. That C 1.0
  reads and writes what the C API writes is `../interop/tests/capi10.rs`.
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
  creating transaction (and gone with the handle when its creation aborts,
  also in a nested transaction: the database that reuses the handle has
  the default order in `mdb_cmp`/`mdb_dcmp` and in what it stores),
  `MDB_MAP_FULL` and the `MDB_BAD_TXN` state,
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
* `tests/capi_nolock.rs`: `tests/c/capi_nolock.c`, read transactions in
  `MDB_NOLOCK` environments against both libraries: a reader holds values
  while commits rewrite every page (C changes them, Rust keeps them), in
  one handle and across two handles of the environment, whose writers the
  Rust library serialises; reset, renew, and the reader information
  (`mdb_env_info`, `mdb_reader_list`, `mdb_reader_check`) as C reports it
  without a lock file; then the C page audit.
* `tests/capi_dlopen.rs`: `tests/c/capi_dlopen.c`, linked against no LMDB,
  loads the shared library with `dlopen` on several threads at once
  (`RTLD_LOCAL` and `RTLD_GLOBAL`), each opening the same environment
  `MDB_RDONLY | MDB_NOLOCK` with its own handle; `dlclose`; loads it again
  three times and reruns the threads. The same binary runs with the C and
  the Rust shared library; the traces must be identical, and the Rust
  library must stay loaded after `dlclose`.
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
