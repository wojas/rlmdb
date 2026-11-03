# rlmdb-capi: the LMDB C API over the Rust implementation

`rlmdb-capi` is the C API of the Rust rewrite in `../rlmdb`: the `mdb_*`
functions of LMDB 0.9.37 with the same ABI, error codes and flag values,
the 13 functions LMDB 1.0's `lmdb.h` adds, and rlmdb's own `rlmdb_*`
functions. Its public API is LMDB 1.0's plus rlmdb's: C programs compile
against `include/lmdb.h` (the unmodified LMDB 1.0.3 header, with one
comment added on top) and `include/rlmdb.h` (rlmdb's own functions, see
"`rlmdb.h`" below). A program written for LMDB 0.9 compiles against
0.9's `lmdb.h` instead (with `rlmdb.h` after it if it calls rlmdb's
functions): 1.0's API is a superset of 0.9's with the same ABI, so it
links against the same symbols. Programs link against one of three
libraries built from this crate:

| Library | Package | Files | `mdb_version()` | New environments |
|---|---|---|---|---|
| `librlmdb`, linked with `-lrlmdb` | `librlmdb` | `librlmdb.a`, `librlmdb.so` (SONAME `librlmdb.so`), `librlmdb.dylib`; `rlmdb.dll` | rlmdb's version (0.0.0 today) | LMDB 0.9 |
| the 0.9 stub | `lmdb09-stub` | built as `liblmdb09.*` (SONAME `liblmdb.so.0`), installed as `liblmdb.so.0` / `liblmdb.dylib` / `lmdb.dll` | 0.9.85 | LMDB 0.9 |
| the 1.0 stub | `lmdb10-stub` | built as `liblmdb10.*` (SONAME `liblmdb.so`), installed as `liblmdb.so` / `liblmdb.dylib` / `lmdb.dll` | 1.0.85 | LMDB 1.0 |

`librlmdb` is the library to build programs with. The two stubs are named
like C LMDB and stand in for one C release each, for programs that were
built against C LMDB 0.9 or 1.0: a user or a distribution puts one in
front of the C library (`LD_LIBRARY_PATH`, `LD_PRELOAD`, an rpath; see
"Installing"). All three export the same functions, every one of both
`lmdb.h` versions and of `rlmdb.h`; they differ only in their names, in
the version `mdb_version()` (and `rlmdb_version()`) reports, and in the
format of a new environment that asks for none. `librlmdb` reports
rlmdb's own version: programs link against it explicitly. The stubs
report their C series with the patch level 85, above every C release, so
that a program can tell rlmdb from C LMDB.

The libraries open environments of both data-file formats rlmdb
handles, LMDB 0.9 and LMDB 1.0 (`../DUAL_FORMAT.md`), through the same
`mdb_*` functions: an existing file in its format, a new one in the
format asked for (`rlmdb_env_set_format`, `rlmdb_set_default_format`, or
the experimental `RLMDB_DEFAULT_FORMAT`), else in the library's own: 0.9
in `librlmdb` and the 0.9 stub, because almost all programs are written
for LMDB 0.9 today (this may change in a later release), and 1.0 in the
1.0 stub (see "`rlmdb.h`: data-file formats").

Status (stage 4): every function of `lmdb.h` is implemented, read and
write side; of LMDB 1.0's additions, those rlmdb can implement (see "LMDB
1.0's functions"). The unmodified upstream programs `mtest.c` ..
`mtest6.c`, `mdb_load.c`, `mdb_dump.c`, `mdb_copy.c` and `mdb_stat.c` run
against it and produce what they produce against the C library, down to
the page numbers on the free list, and so do LMDB 1.0's `mtest.c` ..
`mtest5.c` on the 1.0 stub against C 1.0 (see "Acceptance"). There are no
known gaps (see "Known gaps").

## Building and linking

```sh
cargo build --release -p librlmdb -p lmdb09-stub -p lmdb10-stub
# target/release/librlmdb.a, target/release/librlmdb.{so,dylib}
# target/release/liblmdb09.*, target/release/liblmdb10.* (the stubs)
```

`rlmdb-capi` itself is the implementation (a Rust library); the three
packages in `artifacts/` link it into the C libraries. Cargo cannot name
two libraries of one workspace `liblmdb`, so the stubs get their final
names when they are installed (`scripts/install-libs.sh`, below); their
SONAMEs (and macOS install names, `@rpath/liblmdb.dylib`) are already the
final ones. `RLMDB_LMDB10_SONAME` at build time gives the 1.0 stub
another SONAME (`liblmdb.so.1`, say) for a system whose C LMDB 1.0 has
one: upstream's Makefile sets none, and no distribution ships C 1.0 yet.

Compile C programs with `-I rlmdb-capi/include` and link with `-lrlmdb`
(`-L target/release -lrlmdb`, or after installing `$(pkg-config --cflags
--libs rlmdb)`). A program written for LMDB 0.9 puts the directory of
0.9's `lmdb.h` in front (`-I tests/lmdb-c/0.9 -I rlmdb-capi/include`, or
includes its own copy), and gets `rlmdb.h` from `rlmdb-capi/include`. The
static library needs the system libraries the Rust standard library
needs (from `cargo rustc -p librlmdb --release -- --print
native-static-libs`):

| Platform | Link line |
|---|---|
| Linux (glibc) | `librlmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc` |
| macOS | `librlmdb.a` (rustc lists `-liconv -lSystem -lc -lm`; the compiler driver adds the last three) |
| Windows, MinGW-w64 (`x86_64-pc-windows-gnu`) | `librlmdb.a -lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp` |
| Windows, MSVC (`x86_64-pc-windows-msvc`) | `rlmdb.lib legacy_stdio_definitions.lib kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib` (compile with `cl /MD`: the library uses the dynamic C runtime, `msvcrt`) |

For example `cc -o mdb_stat tests/lmdb-c/0.9/mdb_stat.c
target/release/librlmdb.a -lpthread -lgcc_s -lutil -lrt -lm -ldl` on Linux
(`mdb_stat.c` includes the 0.9 `lmdb.h` next to it). The shared library
needs no extra flags (`-L target/release -lrlmdb`). The stubs' static
libraries (`liblmdb.a` once installed) need the same system libraries.

On the BSDs (from `--print native-static-libs` of the Rust targets; the
interop tests link the C peer this way, tested on OpenBSD and FreeBSD, see
`COMPATIBILITY.md`): FreeBSD `-lrt -lutil -lexecinfo -lkvm -lmemstat
-lprocstat -ldevstat -lpthread -lgcc_s -lc -lm`, NetBSD `-lutil -lrt
-lexecinfo -lpthread -lgcc_s -lc -lm`, OpenBSD `-lpthread -lc++abi -lc -lm
-lutil -lexecinfo -lcompiler_rt`, DragonFly `-lrt -lutil -lexecinfo -lkvm
-lpthread -lgcc_pic -lc -lm`.

Load only one of the three into a process: each is a complete copy of
the library with its own process-wide state, as two different C
libraries would be (see "Several handles and `dlopen`").

### Windows

64-bit Windows with either Rust toolchain; `cargo build --release -p
librlmdb -p lmdb09-stub -p lmdb10-stub` (add `--target
x86_64-pc-windows-gnu` or use that toolchain for MinGW) writes to
`target/release/`:

| | MSVC | MinGW-w64 |
|---|---|---|
| DLL | `rlmdb.dll` | `rlmdb.dll` |
| its import library | `rlmdb.dll.lib` | `librlmdb.dll.a` |
| static library | `rlmdb.lib` | `librlmdb.a` |
| the stubs | `lmdb09.dll`, `lmdb10.dll`, `lmdb09.lib`, `lmdb10.lib` | `lmdb09.dll`, `lmdb10.dll`, `liblmdb09.a`, `liblmdb10.a` |

Link a C program against the DLL through its import library, which needs
nothing else (`cl /I rlmdb-capi\include prog.c target\release\rlmdb.dll.lib`,
`gcc -I rlmdb-capi/include prog.c target/release/librlmdb.dll.a`), and put
`rlmdb.dll` next to the program or on `PATH`; or link the static library
with the system libraries of the table above. `lmdb.h` has no
`__declspec(dllimport)`: the functions are reached through the import
library's stubs, as with a C `lmdb.dll`. A program that calls `LoadLibrary`
and `FreeLibrary` gets a fresh copy of the DLL after the last
`FreeLibrary`, as with C's DLL (no `-z nodelete` counterpart is needed).
A stub stands in for a C `lmdb.dll` as `lmdb.dll` next to the program
(Windows finds a DLL by its file name); the stubs' import libraries name
`lmdb09.dll` and `lmdb10.dll` and are not meant for linking.

On Windows the types and texts follow C LMDB's `_WIN32` build:
`mdb_filehandle_t` is a `HANDLE` (`mdb_env_get_fd` gives the data file's,
`INVALID_HANDLE_VALUE` before the open; `mdb_env_copyfd` takes one),
`mdb_mode_t` is `int` with MSVC and MinGW's 16-bit `mode_t` (ignored, as in
C), paths are UTF-8, and errors that come from the system are `GetLastError`
codes whose `mdb_strerror` text is the system's message (the C runtime's
`strerror` for the `errno` values LMDB uses itself); each such text is kept
for the life of the process, where C returns a buffer on its stack. The
upstream `mtest*.c` and tools build unchanged with MinGW's `gcc`; MSVC has
no `<unistd.h>` (`getopt`, and `ssize_t`, which `mdb_stat.c` uses), so the
tests give `cl` the `tests/compat/msvc/unistd.h` of this repository.

## Installing

`scripts/install-libs.sh` builds the three libraries (or, with `--from
<dir>`, takes those built before: all three, `cargo build --release -p
librlmdb -p lmdb09-stub -p lmdb10-stub`; it installs nothing if one is
missing) and installs them with the headers and a pkg-config file. `PREFIX` (default `/usr/local`), `LIBDIR`
(`$PREFIX/lib`), `INCLUDEDIR` (`$PREFIX/include`) and `DESTDIR` (a
staging directory in front of every path written) work as with `make
install`; `--target <triple>` installs a cross build. The layout:

```
$LIBDIR/librlmdb.a
$LIBDIR/librlmdb.so                      (macOS: librlmdb.dylib)
$LIBDIR/pkgconfig/rlmdb.pc               Cflags: -I${includedir}/rlmdb, Libs: -L${libdir} -lrlmdb
$INCLUDEDIR/rlmdb/lmdb.h                 (the LMDB 1.0.3 header)
$INCLUDEDIR/rlmdb/rlmdb.h
$LIBDIR/rlmdb/lmdb-0.9/liblmdb.so.0      the 0.9 stub (macOS: liblmdb.dylib)
$LIBDIR/rlmdb/lmdb-0.9/liblmdb.so -> liblmdb.so.0
$LIBDIR/rlmdb/lmdb-0.9/liblmdb.a
$LIBDIR/rlmdb/lmdb-1.0/liblmdb.so        the 1.0 stub, under its SONAME (macOS: liblmdb.dylib)
$LIBDIR/rlmdb/lmdb-1.0/liblmdb.a
```

`rlmdb.pc` also lists the system libraries of the table above as
`Libs.private` (for `pkg-config --static`). On macOS the script sets each
library's install name to its installed path (without `DESTDIR`), as a
`make install` there does, and signs it again (ad hoc). On Windows (run
it from MSYS2 or Git Bash) it installs `rlmdb.dll`, its import library
and the static library in `$LIBDIR`, and the stubs as `lmdb.dll` (with
`lmdb.lib` or `liblmdb.a`) in the two `lmdb-*` directories. Files are
replaced by renaming, so running programs keep the library they mapped.

The two `lmdb-*` directories are deliberately not on the default library
path: the stubs carry C LMDB's names and must not replace a C `liblmdb`
the system has, unless a user or a distribution asks for that:

* Linux and the BSDs: `LD_LIBRARY_PATH=$LIBDIR/rlmdb/lmdb-0.9 program`
  runs a program linked against C LMDB 0.9 (`liblmdb.so.0`) on the 0.9
  stub; `LD_PRELOAD=$LIBDIR/rlmdb/lmdb-0.9/liblmdb.so.0` does it too, also
  for a program with an rpath to the C library. A program built against C
  LMDB 1.0 needs the 1.0 stub's directory (SONAME `liblmdb.so`, or what
  `RLMDB_LMDB10_SONAME` named). A distribution can point a program's
  rpath (or `RUNPATH`) at one of the directories, or install a stub in
  place of its C library.
* macOS: `DYLD_LIBRARY_PATH=$LIBDIR/rlmdb/lmdb-0.9 program`: dyld looks
  for the leaf name of the program's `liblmdb.dylib` there first,
  whatever install name the program recorded. `DYLD_INSERT_LIBRARIES`
  replaces functions by name only in a flat namespace
  (`DYLD_FORCE_FLAT_NAMESPACE`); programs use two-level namespaces, so it
  does not do what `LD_PRELOAD` does. System Integrity Protection strips
  all `DYLD_*` variables for system binaries (and for programs started
  through them, such as `/bin/sh` scripts).
* Windows: copy the stub as `lmdb.dll` next to the program (the
  directory of the executable is searched first).

`tests/install.rs` runs the script into a `DESTDIR` and does each of these
on Linux and macOS: a program built with `-lrlmdb` against the installed
library, and programs linked against a shared C LMDB 0.9 and 1.0 run on
the stubs.

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

### LMDB 1.0's functions

LMDB 1.0's `lmdb.h` (`include/lmdb.h`, a copy of
`../tests/lmdb-c/1.0/lmdb.h`, LMDB 1.0.3) declares 13 functions 0.9's
lacks. Every library exports them (module `v10`).
Where C 1.0 checks its arguments, the same checks come first, in C's
order; where C 1.0 would crash (a NULL handle, an environment that is not
open), the result is `EINVAL`. The functions rlmdb does not implement
(`../DUAL_FORMAT.md` §7) then return `ENOTSUP` and change nothing:

| Function | Status | Result |
|---|---|---|
| `mdb_env_set_pagesize` | Implemented (as `rlmdb_env_set_pagesize`) | `EINVAL` for NULL, after the open, outside 256 .. 65536 or not a power of two (C 1.0's checks); applies to a new 1.0 environment; an LMDB 0.9 one cannot take a page size (`EINVAL` from `mdb_env_open`). |
| `mdb_txn_flags` | Implemented | `MDB_RDONLY` or 0 in `*flags`; `EINVAL` for NULL (also NULL `flags`, where C crashes). |
| `mdb_cursor_is_db` | Implemented | 1 if the cursor is on the record of a named database (`F_SUBDATA` without `F_DUPDATA`), else 0, also for NULL and for a cursor whose transaction cannot be used. Where C reads a node the cursor is still on after `MDB_NOTFOUND` (past either end, a failed `MDB_SET`), so does rlmdb. |
| `mdb_txn_prepare` | Implemented (two-phase commit, `rlmdb::RwTxn::prepare`), both formats | As C 1.0: writes and syncs everything but the meta page and keeps the writer lock; the transaction's cursors are closed; then every call that checks `MDB_TXN_BLOCKED` returns `MDB_BAD_TXN` after its handle checks (`mdb_get`, `mdb_put`, `mdb_del`, `mdb_drop`, `mdb_cursor_open`, `mdb_dbi_open`, `mdb_stat`, a child's `mdb_txn_begin`), `mdb_dbi_flags`, `mdb_set_compare`, `mdb_set_dupsort`, `mdb_cmp`, `mdb_dcmp`, `mdb_txn_id`, `mdb_txn_flags` work, a second prepare returns `MDB_TXN_PENDING`, and `mdb_txn_commit` (the meta page) or `mdb_txn_abort` end it. An active child is committed first; a read-only or nested transaction is committed (and freed); a transaction with nothing to write ends at once as an empty commit (the writer lock is released). `EINVAL` for NULL (C crashes). Differences below. |
| `mdb_env_rollback` | Implemented (two-phase commit, `rlmdb::Environment::rollback`), both formats | As C 1.0: under the writer lock, `MDB_CANT_ROLLBACK` unless `txnid` is the newest snapshot and a previous one exists; else the newest meta page is overwritten with C 1.0's bytes (no snapshot: zero records but the page size and flags, `mm_last_pg` 2^64-1, `mm_txnid` `txnid & 1`), synced per `MDB_NOSYNC`/`MDB_NOMETASYNC`, and the previous snapshot is current (`mti_txnid`). Also `MDB_CANT_ROLLBACK` if the previous meta page is not a snapshot, a reader uses `txnid` or the environment still has `MDB_PREVSNAPSHOT`, and `EACCES` for a read-only environment (differences below). `EINVAL` for NULL or an environment that is not open (C crashes). |
| `mdb_env_set_encrypt` | Not implemented | `EINVAL` if `env`, `func` or `key` is NULL or the environment is open (C 1.0's checks), else `ENOTSUP`; the environment stays plain. |
| `mdb_env_set_checksum` | Not implemented | `EINVAL` if `env` or `func` is NULL, `size` is 0 or the environment is open (C 1.0's checks), else `ENOTSUP`. |
| `mdb_env_incr_dump`, `mdb_env_incr_dumpfd`, `mdb_env_incr_loadfd` | Not implemented (incremental backups) | `EINVAL` for NULL arguments or an environment that is not open, else `ENOTSUP`; nothing is written, created or read (C's `mdb_env_incr_dump` also sets `MDB_NOSUBDIR` on the handle). |
| `mdb_modload` | Not supported (crypto modules) | NULL, with a static message ("rlmdb does not support LMDB crypto modules") in `*errmsg` if `errmsg` is not NULL (C crashes on NULL); `*mcf_ptr` untouched, as when C's `dlopen` fails. Nothing is loaded. |
| `mdb_modunload` | Not supported | Does nothing (`mdb_modload` returns no handles). |
| `mdb_modsetup` | Not supported | `void`, so it cannot report that encryption and checksums are not implemented. Where C would set either up (a checksum function with a non-zero size; an encryption function and a passphrase) on an environment that is not open, the handle is marked and `mdb_env_open` fails with `ENOTSUP`: a program that asked for encryption must not get a plain file. Otherwise, and for NULL arguments, nothing happens. The module's functions are never called. |

`ENOTSUP` is the platform's value (95 on Linux, 45 on macOS, 129 on
Windows); `mdb_strerror` says "Operation not supported", on Windows too,
where C would give the system's message for 129, an unrelated one (C
never returns `ENOTSUP`).

## `rlmdb.h`

`include/rlmdb.h` declares rlmdb's own functions and nothing else: the
version (module `version`) and the data-file formats (module `format`).
C LMDB does not have them, so a program that calls them needs an rlmdb
library. The header's comments are their reference documentation.

It works after either `lmdb.h`: LMDB 1.0's (`include/lmdb.h`, rlmdb's
API, with 1.0's types, constants and functions) or LMDB 0.9's, for a
program written for 0.9 that also calls rlmdb's functions. It needs only
`MDB_env` and plain C types, which both have, and includes the `lmdb.h`
next to it (1.0's) only if no `lmdb.h` came before (both define
`_LMDB_H_`). With 0.9's `lmdb.h`, LMDB 1.0's error codes are numbers
without names (`mdb_strerror` knows their texts), and 1.0's page-size
function is there as `rlmdb_env_set_pagesize`. `tests/capi_headers.rs`
compiles each way, as C and as C++.

### Version

```c
#include "rlmdb.h"

typedef struct rlmdb_version_info {
	unsigned	size;		/* sizeof(rlmdb_version_info) of the library */
	int		rlmdb_major, rlmdb_minor, rlmdb_patch;	/* the rlmdb version */
	int		lmdb_major, lmdb_minor, lmdb_patch;	/* what mdb_version() reports: librlmdb rlmdb's, the stubs 0, 9, 85 and 1, 0, 85 */
	int		v09_major, v09_minor, v09_patch;	/* C release the 0.9 format and API match: 0, 9, 37 */
	int		v10_major, v10_minor, v10_patch;	/* C release the 1.0 format matches: 1, 0, 3 */
} rlmdb_version_info;

const rlmdb_version_info *rlmdb_version(void);	/* static, never freed */
const char *rlmdb_version_string(void);		/* static, one line */
```

* `rlmdb_version()` returns a static structure. Fields are only ever
  appended; `size` tells a program compiled against a newer header which
  fields the library has.
* `rlmdb_version_string()` returns everything in one line, for logs and
  bug reports: `rlmdb 0.0.0 (mdb_version 0.0.0; format 0.9 = LMDB 0.9.37;
  format 1.0 = LMDB 1.0.3)` in `librlmdb` (`mdb_version 0.9.85` and
  `mdb_version 1.0.85` in the stubs). Programs read the version numbers
  from `rlmdb_version()` instead of parsing it; opt-in features, once
  there are any, will be listed by name (issue #2).

The Rust side is `rlmdb::version_info()`, from which the C structure and
strings are filled once, at first use.

**`mdb_version()` of `librlmdb` reports rlmdb's own version** (the
workspace version, 0.0.0 today) and returns the line of
`rlmdb_version_string()`: a program links against `librlmdb` explicitly,
so it is told which rlmdb it runs on, not a C release. The shipped
`lmdb.h` still says 1.0.3 (`MDB_VERSION_*` describe the API the program
was compiled against, 0.9.37 with 0.9's header).

**The stubs report their C series**: the 0.9 stub 0.9.85 and the string
`LMDB 0.9.85: (rlmdb 0.0.0)` (the rlmdb version where C has its release
date), the 1.0 stub 1.0.85 and `LMDB 1.0.85: (rlmdb 0.0.0)`;
`rlmdb_version()` and `rlmdb_version_string()` say the same
(`mdb_version 0.9.85`, `mdb_version 1.0.85`). A program built for C LMDB
may compare the major and minor numbers with its header's, so they stay
those of the series the stub stands in for; the patch level is above
every real release, so a program can tell rlmdb from C LMDB, and a
run-time patch level above the header's is the normal case for LMDB (a
program built against an older header runs with a newer library). The
library is a build-time choice, not a setting: the three differ in these
functions and in the format of new environments only (`src/profile.rs`).

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
  else the library's: LMDB 0.9 in `librlmdb` and the 0.9 stub, because
  almost all programs are written for LMDB 0.9 today (and every C release
  reads its files; this may change in a later release), LMDB 1.0 in the
  1.0 stub (so that a program built for C 1.0 gets 1.0 files without
  asking). An explicit choice of the program wins over the variable and
  the library. **The variable is experimental**: it may be removed or
  restricted in a later release. It is read at every open; programs should
  use the functions.
* `rlmdb_env_set_pagesize` and LMDB 1.0's `mdb_env_set_pagesize` (a power
  of two from 256 to 65536, as in C 1.0) apply to a new 1.0 environment;
  `mdb_env_open` fails with `EINVAL` if the environment turns out to be
  LMDB 0.9, which has no such setting.
* LMDB 1.0's `MDB_PREVSNAPSHOT`, `MDB_ENCRYPT`, `MDB_REMAP_CHUNKS` and
  error codes `MDB_PROBLEM` .. `MDB_ADDR_BUSY` (-30779 .. -30769) come
  from its `lmdb.h` (`include/lmdb.h`; `MDB_LAST_ERRCODE` is
  `MDB_ADDR_BUSY` there, 0.9's `MDB_BAD_DBI` in 0.9's header).
  `mdb_strerror` returns 1.0's messages for those codes, also to a
  program built with 0.9's header.

What changes with an LMDB 1.0 environment:

* `mdb_env_get_maxkeysize` returns the environment's limit after the open
  (1978 on 4 KiB pages, 32698 on 64 KiB); before it, the limit of a new
  environment as it would be created now: 511 for 0.9, and if it would be
  1.0 (requested, the process default, the variable, or the 1.0 stub) the
  1.0 limit for the page size it would get. (C 1.0 returns 0 before the
  open.)
* Keys and `MDB_DUPSORT` items up to that limit, inline data up to half a
  page (64 KiB pages: 32 KiB); named databases are stored under their name
  and its NUL, as C 1.0 does (`mdb_dbi_open` takes the name as before).
* `mdb_drop` of the main DB follows C 1.0: if it records named databases,
  they are deleted first (their pages are freed), and it fails with
  `MDB_DBIS_BUSY` while a handle of a named database is open (0.9 leaves
  their pages behind, `../KNOWN_C_BUGS.md` row 21).
* Not implemented: `MDB_ENCRYPT` and `MDB_REMAP_CHUNKS` (`EINVAL` from
  `mdb_env_open`; `MDB_ENCRYPT` also from `mdb_env_set_flags`), encrypted
  or checksummed 1.0 files (`MDB_ENV_ENCRYPTION`, `MDB_BAD_CHECKSUM`), and
  1.0's encryption, checksums, incremental backups and crypto modules
  (their functions return `ENOTSUP`, see "LMDB 1.0's functions"). Its
  two-phase commit is implemented, for 0.9 environments too. Nor are 1.0's
  read-only child transactions of a write transaction: `mdb_txn_begin`
  with a parent and `MDB_RDONLY` returns `MDB_BAD_TXN`, as in 0.9.
* Return codes stay 0.9's: a write in a read-only transaction is `EACCES`
  where C 1.0 says `MDB_IS_READONLY`, and a nested transaction in an
  `MDB_WRITEMAP` environment is `MDB_BAD_TXN` where C 1.0 says `EINVAL`.

LMDB 1.0's `MDB_PREVSNAPSHOT` (`mdb_env_open`: the previous snapshot, the
older meta page, until the first commit that writes) works for
environments of both formats, also through a library built with 0.9's
`lmdb.h`, whose programs pass `0x2000000`. As in C 1.0: a writer must be
the only user (`MDB_ENV_BUSY` while another process has the environment
open) and keeps the exclusive lock on the lock file until that commit,
which shares it and ends the flag (an empty commit, an abort or a failed
commit keep both); a read-only handle may share the environment, and keeps
the lock until it is closed if it got it exclusively; `mdb_env_get_flags`
reports the flag while it lasts, `mdb_env_set_flags` refuses it,
`mdb_env_info` and `mdb_env_stat` report the older meta page. Not as in
C 1.0 (see "Differences" below): its first commit publishes its own txnid
(row 67), a read-only handle next to other processes leaves the lock file
alone (row 68), a plain copy is one of the previous snapshot (row 69), a
handle that closes holding the exclusive lock publishes the latest
snapshot for the processes that waited (row 70), `mdb_env_rollback`
fails with `MDB_CANT_ROLLBACK` while the flag lasts (row 71), and an
`MDB_NOLOCK` handle with the flag has the file to itself in the process
until its first commit. With two-phase commit, `mdb_txn_commit` of a
prepared transaction is the commit that ends the flag, as in C 1.0
(`mdb_txn_prepare` alone does not).

## Write side

Everything behaves as in C, with the same checks in the same order and the
same return codes:

* `mdb_txn_begin` without `MDB_RDONLY` waits for the environment's writer
  lock (shared with C processes); `EACCES` in a read-only environment. With
  a `parent`: a nested transaction (`EINVAL` for a read-only parent,
  `MDB_BAD_TXN` with `MDB_RDONLY`, when the parent already has a child or
  failed, or in an environment opened with `MDB_WRITEMAP`). While a child
  is active, the parent is blocked: its operations return `MDB_BAD_TXN`.
  Committing or aborting a parent ends its child first. The parent's cursors (and those of further ancestors) work
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
  `mdb_put` in one transaction) and for its cursors opened before, except
  that such a cursor keeps the order of a key's duplicates it was opened
  with, as in C.
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

## Differences from C LMDB 0.9.37

Everything not listed behaves as in C, and values returned in `MDB_val`s
point into the memory map or the transaction's dirty pages (zero copy).

| Area | C LMDB | This library |
|---|---|---|
| `mdb_version()` | The header's version: 0, 9, 37 and `MDB_VERSION_STRING`, `LMDB 0.9.37: (Oct 21, 2026)`. | `librlmdb`: rlmdb's version and the line of `rlmdb_version_string()` (see "`rlmdb.h`"). The 0.9 stub: 0, 9, 85 and `LMDB 0.9.85: (rlmdb <version>)`; the 1.0 stub, standing in for C 1.0: 1, 0, 85. |
| Data-file formats | LMDB 0.9 only; a 1.0 file is `MDB_INVALID`. | LMDB 0.9 and 1.0 (see "`rlmdb.h`: data-file formats"); new environments are 0.9 by default (1.0 in the 1.0 stub). |
| Headers | 0.9.37's `lmdb.h`. | LMDB 1.0.3's `lmdb.h` and `rlmdb.h`; programs written for 0.9 may keep 0.9's `lmdb.h`. |
| Functions | `lmdb.h`'s. | Also LMDB 1.0's 13 more and `rlmdb.h`'s (see "LMDB 1.0's functions"). |
| `mdb_strerror` of LMDB 1.0's codes (-30779 .. -30769) | "Invalid error code". | 1.0's messages. |
| `mdb_env_set_maxdbs` with more than 1,048,576 | Taken; the environment breaks above about 35 million databases (`KNOWN_C_BUGS.md` row 54). | `EINVAL` (`MAX_DBS_LIMIT` in `rlmdb::consts`; every transaction allocates a slot per database). |
| `mdb_env_set_maxreaders` with more than 1,048,576 | Taken; a new lock file holds 64 bytes per slot, 256 GiB at `(unsigned)-1` (`KNOWN_C_BUGS.md` row 60). | `EINVAL` (`MAX_READERS_LIMIT` in `rlmdb::consts`; 64 MiB of lock file at the limit). An existing lock file with a larger table is attached to with its size, as in C. |
| Threads | Without `MDB_NOTLS` a thread may hold one read transaction at a time (`MDB_BAD_RSLOT` otherwise) and a read transaction is tied to its thread. | Always behaves like `MDB_NOTLS`: any number of read transactions per thread, usable from any thread (one at a time). `MDB_NOTLS` is accepted. A superset of C. |
| Opening one environment twice in a process | Allowed but unsafe (setting the lock file up again re-initialises the reader table under the first handle's readers; closing one handle drops the other's locks). | `mdb_env_open` fails with `EBUSY`, also under another name (a symlink, hard links of its files, a bind mount) and before it touches the lock file, except under `MDB_NOLOCK`, where handles share one in-process reader table and writer lock (see "Several handles and `dlopen`"). |
| Read transactions with `MDB_NOLOCK` | Not tracked: a write transaction of the same process reuses pages a live read transaction still reads (`lmdb.h` asks the caller to prevent that), and writers of different handles are not serialised. | Tracked in memory (`../KNOWN_C_BUGS.md` row 35), in one table for all `MDB_NOLOCK` handles of the environment in the process, which also share a writer lock: writers of the process never reuse pages its readers read. `mdb_env_info` (`me_numreaders` 0), `mdb_reader_list` (`(no reader locks)`) and `mdb_reader_check` (none cleared) report what C reports. Other processes remain the caller's job, as in C. |
| `MDB_PREVSNAPSHOT` (LMDB 1.0's, see "Data-file formats") | 0.9: unknown, `EINVAL`. C 1.0: the first commit after the open publishes the older meta page's txnid in the lock file, so the commit is invisible and the next one overwrites it (`../KNOWN_C_BUGS.md` row 67); a read-only handle next to other processes publishes it there too, and their next commit overwrites the latest one (row 68); a plain copy keeps the latest commit's meta page but not its pages (row 69); a handle that closes holding the exclusive lock (read-only, or a writer without a commit) leaves the older txnid to the processes that waited, whose next commit overwrites the latest one (row 70); `mdb_env_rollback` of the newest commit succeeds while the flag lasts and leaves the flag picking the page without a snapshot (row 71). Two `MDB_NOLOCK` handles of one file may use it, uncoordinated. | For both formats. The commit's txnid stays published; a read-only handle next to other processes copies the older meta page holding the writer mutex for a moment and pins it in its reader slot, leaving `mti_txnid` alone; a plain copy is a copy of the previous snapshot; a handle that closes holding the exclusive lock publishes the newest txnid first; `mdb_env_rollback` is `MDB_CANT_ROLLBACK` while the flag lasts. An `MDB_NOLOCK` handle with the flag has the file to itself in the process until its first commit (until it is closed, read-only): `EBUSY` for its open while the process has the file open, and for other opens meanwhile. |
| `MDB_WRITEMAP` | Writes through a writable map; extends the data file to the map size (`ftruncate` in `mdb_env_open` and `mdb_env_set_mapsize`). | Accepted, but pages are written with `pwrite` as without the flag: the files are those written without it, and the data file is not extended. Its limits are C's: `mdb_env_get_flags` reports it, `mdb_env_set_flags` cannot change it (`EINVAL`), `MDB_RDONLY` drops it, nested transactions are refused (`MDB_BAD_TXN`, also in the 1.0 stub, where C 1.0 returns `EINVAL`). On OpenBSD, whose maps do not see `pwrite`s, `mdb_env_open` with it and without `MDB_RDONLY` fails with `ENOTSUP` (`../COMPATIBILITY.md`, "Flags"). |
| `MDB_FIXEDMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT` | Honoured. | Accepted and ignored (`mdb_env_get_flags` reports them). A new environment records map address 0 (`KNOWN_C_BUGS.md` row 9); a C-created `MDB_FIXEDMAP` environment's recorded address is reported by `mdb_env_info`. |
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
| `mdb_txn_commit` or `mdb_txn_abort` after an `mdb_txn_prepare` that ended the transaction (it had nothing to write, or the prepare failed) | Frees the environment's preallocated write transaction: the next `mdb_txn_begin` uses freed memory, `mdb_env_close` frees it twice; `mdb_txn_commit` returns `MDB_BAD_TXN` (`../KNOWN_C_BUGS.md` row 61). | Frees the handle. `mdb_txn_commit` returns 0 after an empty prepare (nothing was to be written), `MDB_BAD_TXN` after a failed one (as C). A second `mdb_txn_prepare` returns `MDB_BAD_TXN`, as C's. |
| Cursors of a transaction after `mdb_txn_prepare` | Freed (use after free). | `EINVAL` (`mdb_cursor_renew` too); `mdb_cursor_close` frees them; the transaction's end frees those still open. |
| `mdb_env_rollback` while a read transaction uses the snapshot rolled back | Rolls back: the next writer overwrites pages the reader still reads (`lmdb.h` forbids the situation; `../KNOWN_C_BUGS.md` row 63). | `MDB_CANT_ROLLBACK`, nothing written. Also the slot of a reader that died, until `mdb_reader_check` clears it. |
| `mdb_env_rollback` twice in a row after rolling back an odd txnid | The rolled-back meta page has `mm_txnid` 1, which passes C's check: the second call zeroes the other meta page too, and no snapshot is left (`../KNOWN_C_BUGS.md` row 62). | `MDB_CANT_ROLLBACK` whenever the previous meta page holds no snapshot (as C after an even txnid). |
| `mdb_env_rollback` of a read-only environment | Fails writing (`EBADF`) and marks the environment unusable (`MDB_PANIC` for every later transaction). | `EACCES`, nothing changes. |
| `mdb_env_rollback` that fails to write its meta page | Writes back only the old `mm_txnid` and `mm_last_pg`, over zeroed records: the current snapshot's meta page loses its trees (`../KNOWN_C_BUGS.md` row 64); `MDB_PANIC` afterwards. | Writes the old meta page back whole; `MDB_PANIC` afterwards, as C. |
| NULL handles and outputs where C would crash (`mdb_txn_begin(NULL, …)`, `mdb_dbi_open(…, NULL)`, `mdb_cursor_put(…, NULL data, …)`, `mdb_env_copy2(NULL, …)`, `mdb_cursor_dbi(NULL)`, `mdb_env_stat` before open, …) | Crash. | `EINVAL` (or 0/NULL for functions without an error result). |
| `mdb_env_open` after a failed open on the same handle | Not allowed (`mdb_env_close` only). | Allowed. |
| `mdb_env_set_mapsize` on an open environment | No transaction may be active in the process. | Read transactions may be active (they keep their old map); `EINVAL` while a write transaction is active, as in C. |
| A thread that exits with its write transaction still open (a bug in the program) | Linux: the writer mutex reports its dead owner (`EOWNERDEAD`); the next `mdb_txn_begin` for writing in the same process returns `MDB_PANIC` and the environment is unusable, other processes recover and go on. macOS: every writer waits forever (semaphores are not robust). | Every later writer in the same process waits forever, on Linux too (write transactions of one process are also serialised by an in-process lock, which the abandoned transaction still holds). Other processes behave as with C. |
| `mdb_env_set_assert` | Calls the function on internal assertion failures. | Stored, never called: the Rust core returns errors instead of asserting. |
| `mdb_set_relfunc` / `mdb_set_relctx` | Stored, never called. | Validated and ignored. |
| C bugs not ported | See `../KNOWN_C_BUGS.md`. Visible through the C API: rows 4–6 and 8 (`mdb_cursor_get`/`mdb_cursor_count` on cursors left past the end), row 12 (`MDB_DUPFIXED` items of another size: `MDB_BAD_VALSIZE`), row 13 (`MDB_CURRENT` on a cursor not on an item: `MDB_NOTFOUND`), row 14 (stale duplicate counts of other cursors), row 59 (moves after a failed `MDB_SET` into the duplicates of the key the cursor was left on: read from the node, with the key's count), row 15 (`MDB_CURRENT` growing a duplicate on a full sub-page), row 38 (`MDB_CURRENT` with a key's only `MDB_DUPSORT` item too big for a node: deleted and inserted, other cursors on it move on), row 56 (`MDB_CURRENT` with a duplicate that C would leave in the wrong leaf of a sub-DB: deleted and inserted, other cursors on it move on), row 58 (`MDB_CURRENT` with a key other than the cursor's: the cursor's key is kept, for data of every size), row 16 (`MDB_RESERVE` on `MDB_DUPSORT`: `EINVAL`), row 17 (`MDB_DUPFIXED` without `MDB_DUPSORT`: `MDB_INCOMPATIBLE` on writes), row 48 (a 0.9 file of pages below 2 KiB: `MDB_INVALID` from `mdb_env_open`), row 49 (a snapshot whose last page is below the meta pages: `MDB_CORRUPTED` from `mdb_txn_begin`), row 50 (trees whose branch pages share children: `MDB_CORRUPTED` from `mdb_cursor_get`, `mdb_put` and `mdb_drop`, and from `mdb_txn_commit` for a free list naming a page twice). | |

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
`librlmdb.a` linked into several modules, `librlmdb` and a stub loaded
side by side, or `dlmopen` namespaces) has one
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
C library and against the Rust `librlmdb.a`:

* `mtest.c` .. `mtest6.c` seed `rand()` with `time(NULL)`; the tests build
  them with `tests/c/mtest_shim.h` included first (`-include`, `cl /FI`),
  which includes `<time.h>` and then makes `time` the shim's
  `mtest_time()`, so the seed is `$MTEST_SEED` (and supplies `mdb_dkey()`,
  which `mtest6.c` needs and C's `liblmdb` only has with `MDB_DEBUG`). (Not a
  `-Dtime=mtest_time` on the command line: MSVC's `<time.h>` defines
  `time()` inline and MinGW-w64's gives it an assembler name, and the
  macro would rename those.) `mtest_seed_takes_effect` checks the seed. With seeds 20261030 and 7 each
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
* LMDB 1.0's `mtest.c` .. `mtest5.c` (`../tests/lmdb-c/1.0`, which
  differ from 0.9's in `mtest.c`'s `mdb_env_set_pagesize(env, 1024)`),
  built with 1.0's `lmdb.h` against C LMDB 1.0 and against the 1.0 stub
  `liblmdb10.a`, as a program built for C 1.0 would be: with seeds
  20261030 and 7 both exit 0 with nothing on stderr and the same stdout;
  the stub creates LMDB 1.0 files without being asked; C 1.0's `mdb_stat
  -e -f -ff -a` and `mdb_dump` (with `-a` where there are named
  databases: 1.0's refuses `-a` otherwise) see the same environments
  (the map address aside), and the same tools linked against the stub
  read the stub's file as C 1.0's do (C 1.0 reports its own map address
  where the file records none: `../KNOWN_C_BUGS.md`, LMDB 1.0 section).

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
  `mdb_cmp`/`mdb_dcmp` call the C function behind the trampoline the core
  keeps for the handle, so they follow the handle as the core does (gone
  when it is closed, back when a nested transaction's abort reopens it).

## Tests

```sh
cargo test -p rlmdb-capi                         # macOS / host
scripts/linux-test.sh test -p rlmdb-capi         # Linux in Docker
WINDOWS_HOST=<ssh host> scripts/windows-test.sh cargo test -p rlmdb-capi
```

The tests compile C programs at test time with the `cc` crate against the
C library (built from `../tests/lmdb-c/0.9`, the oracle; for LMDB 1.0's
API from `../tests/lmdb-c/1.0`) and against the Rust `librlmdb.a` (or the
1.0 stub `liblmdb10.a`) that cargo builds for the tests: the three
artifact packages are dev-dependencies of this crate. The programs
written for LMDB 0.9, which run against C 0.9 too, are built for
`librlmdb` with the vendored 0.9 `lmdb.h` first and `include/rlmdb.h`
(`Lib::Rust` in `tests/common/mod.rs`); those for LMDB 1.0's API, and
the 1.0 programs on the 1.0 stub, with `include` alone (`Lib::RustApi`,
`Lib::Rust10`). `capi_dlopen` and
`capi_artifacts` load the shared builds instead.
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
* `tests/capi_artifacts.rs`: `tests/c/capi_symbols.c`, linked against no
  LMDB, loads each of the three shared libraries with `dlopen`
  (`LoadLibrary`) and resolves every function declared in LMDB 0.9's
  `lmdb.h`, in LMDB 1.0's (`include/lmdb.h`) and in `rlmdb.h` (the headers
  are parsed at test time; `rlmdb.h` must declare no `mdb_*` function);
  checks the version each reports (`mdb_version`, `rlmdb_version`,
  `rlmdb_version_string`: rlmdb's, 0.9.85, 1.0.85)
  and the format of a new environment with no request (0.9, 0.9, 1.0, as
  `rlmdb_sniff_format` tells), with `rlmdb_env_set_format`, with
  `rlmdb_set_default_format` and with `RLMDB_DEFAULT_FORMAT` set; that
  `nm` lists exactly those functions as exported (where it runs); and the
  SONAMEs and `DF_1_NODELETE` (parsed from the ELF dynamic section, and
  compared with `readelf -d` where it runs) or the macOS install names.
* `tests/capi_v10.rs`: `tests/c/capi_v10.c`, the 13 functions of LMDB
  1.0's `lmdb.h`, against C LMDB 1.0 (the oracle, with its `lmdb.h`), and
  with `include` (1.0's `lmdb.h`, `rlmdb.h`) against the 1.0 stub and
  against `librlmdb`, which is asked for 1.0 environments, on 1.0
  environments: the same checks and trace for `mdb_env_set_pagesize`
  (edge values, after the open, existing files, 256-byte and 64 KiB
  pages), `mdb_txn_flags` (read, reset, write, nested, a parent of an
  active child, a read-only environment) and `mdb_cursor_is_db` (named
  databases and plain items in the main DB, a named and a `DUPSORT`
  database, read, write and nested transactions, cursors past either end
  and after a failed `MDB_SET`); the Rust builds check `ENOTSUP` after
  C's argument checks and that nothing changed (a transaction after
  no dump file, a plain environment after `mdb_env_set_encrypt`,
  `mdb_env_open` refused after `mdb_modsetup`). C 1.0's `mdb_stat` reads
  every environment the Rust builds left.
* `tests/capi_2pc.rs`: `tests/c/capi_2pc.c`, two-phase commit
  (`mdb_txn_prepare`, `mdb_env_rollback`), against C LMDB 1.0, the 1.0
  stub and `librlmdb` (1.0 and 0.9 environments): the same trace of
  every call on a prepared transaction (return codes, what still works),
  what readers see between and after the phases, prepare then abort, an
  active child, read-only and nested transactions, a prepare that ends
  the transaction (nothing to write, a failed transaction), rollbacks
  (refused ones, after a prepared commit, with an even and an odd txnid,
  with a reader, with `MDB_NOSYNC` and `MDB_NOMETASYNC`, the handle of a
  database the rolled-back commit created); the Rust builds also what C
  1.0 cannot run (NULL handles, cursors closed by the prepare, commit and
  abort after a prepare that ended the transaction, a second rollback
  after an odd txnid, a reader of the snapshot, a read-only environment).
  The 1.0 files are C 1.0's byte for byte, the files between the phases
  included; C 1.0's `mdb_stat` reads them, and C 0.9's the 0.9 ones.
* `tests/capi_prevsnapshot.rs`: `tests/c/capi_prevsnapshot.c`,
  `MDB_PREVSNAPSHOT`, a program for LMDB 1.0's API, against C LMDB 1.0
  (the oracle), the 1.0 stub and `librlmdb` (`include`) on 1.0 and on 0.9
  environments: the same checks and trace for a new environment, the
  previous snapshot before and after an empty commit, an abort and a
  nested commit, the first commit (the flag ends), read-only handles,
  copies, `MDB_NOLOCK`, a prepared transaction's commit (it ends the flag)
  and its rollback, and a rollback while the flag is set; each build's own
  result where C 1.0 has a bug rlmdb does not port (rows 67, 69 and 71) or
  rlmdb refuses a second `MDB_NOLOCK` handle. The files a commit on the
  previous snapshot leaves (plain and prepared, and after the rollback of
  the prepared one), and a compacting copy of a previous snapshot, are
  byte for byte C 1.0's; C's peer of each format audits every
  environment. A second run, against an
  environment the test holds open: `MDB_ENV_BUSY` for a writer, the
  previous snapshot read-only, and the holder's next commit intact (lost
  with C 1.0, row 68).
* `tests/capi_headers.rs`: `tests/c/capi_headers.c` compiled with warnings
  as errors, as C and as C++, three ways, and linked against `librlmdb`:
  with `include` alone (1.0's `lmdb.h`, then `rlmdb.h`), with `rlmdb.h`
  alone (which includes the `lmdb.h` next to it), and with LMDB 0.9's
  `lmdb.h` first on the include path. It takes the address of every
  function `rlmdb.h` declares and, with 1.0's header, of the 13 functions
  1.0 adds, uses 1.0's types and constants, and checks that `mdb_version`
  reports rlmdb's version and line. `src/version.rs` checks that
  `include/lmdb.h` is `../tests/lmdb-c/1.0/lmdb.h` after its comment.
* `tests/install.rs` (Unix): `scripts/install-libs.sh` into a `DESTDIR`,
  and the uses "Installing" describes (see there); also under a prefix of
  more than 400 characters (the installed path must fit the Mach-O
  header's padding on macOS), and `--from` a directory with only
  `librlmdb`, which installs nothing.
* `tests/capi_version.rs`: `tests/c/capi_version.c` checks `rlmdb.h`
  against `librlmdb` (there is no C oracle), built with either `lmdb.h`:
  the structure's `size` and numbers, `mdb_version`'s numbers and string
  (rlmdb's version, the line of `rlmdb_version_string()`). The same
  program is compiled as C++ and linked, which checks the header's
  `extern "C"` guards.
* `tests/capi_format.rs`: `tests/c/capi_format.c` checks the format
  functions of `rlmdb.h` and the constants of 1.0's `lmdb.h` against
  `librlmdb` (no C oracle): `rlmdb_env_set_format`/`get_format` before and after the open,
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
  every 1.0 code; `MDB_ENCRYPT` refused, and `MDB_PREVSNAPSHOT` by
  `mdb_env_set_flags`. That C 1.0 reads and writes what the C API writes
  is `../interop/tests/capi10.rs`.
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
  creating transaction, installed after cursors were opened on the handle
  (in read and write transactions), and gone with the handle when its
  creation aborts (also in a nested transaction: the database that reuses
  the handle has the default order in `mdb_cmp`/`mdb_dcmp` and in what it
  stores) and kept with it when a child's `mdb_drop` is undone by its abort
  (`KNOWN_C_BUGS.md` row 20, Rust build only),
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
* `tests/capi_writemap.rs`: `tests/c/capi_writemap.c`, `MDB_WRITEMAP`
  against C LMDB 0.9 and 1.0 and the three Rust builds (both formats):
  the same trace of `mdb_env_get_flags`, `mdb_env_set_flags` (the flag
  cannot change, before or after the open; `MDB_MAPASYNC` can),
  `mdb_env_sync`, nested transactions (refused: `MDB_BAD_TXN`; C 1.0's
  `EINVAL` and read-only child are checked per library, not traced), a
  read-only parent and a read-only reopen (the flag dropped). The same
  writes with and without the flag leave the same files in the Rust
  builds (compacting copies too), which C's `mdb_stat`, `mdb_dump` and
  page audit read as they read the oracle's. On OpenBSD the Rust builds
  refuse the flag for an open for writing (`ENOTSUP`).
* `tests/capi_alias.rs`: `tests/c/capi_alias.c`, against the Rust library
  only: `mdb_env_open` of an environment the process has open, through
  hard links of both of its files and of the lock file alone, fails with
  `EBUSY`, and the open handle keeps its reader slot, the value its reader
  holds and the txnid of its next writer.
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
