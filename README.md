# rlmdb: LMDB in Rust

A rewrite of [LMDB](http://www.lmdb.tech/) 0.9 in Rust that is binary
compatible with the C implementation (LMDB 0.9.37, whose sources the tests
build as their oracle from `tests/lmdb-c/0.9`): the
data file and the lock file are byte for byte what C writes and expects,
so C and Rust processes can open one environment **at the same time**,
share its reader table and writer lock, and read and write it in turns.
It is a rewrite, not a binding: no C code is linked into the library.

On top of the Rust library sit a C library with LMDB's `mdb_*` ABI and
`lmdb.h` (`librlmdb`, plus two stubs named `liblmdb` that stand in for C
LMDB 0.9 or 1.0 in programs built against it) and the command line
tools.

* [`COMPATIBILITY.md`](COMPATIBILITY.md): the compatibility contract and
  what verifies it.
* [`DESIGN.md`](DESIGN.md): goals, the on-disk and lock file format
  specification, the architecture, findings, and the status per stage.
* [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md): bugs of C 0.9.37 that are not
  ported (and the few that are, on purpose).

## Status

Implemented and tested against the C library on macOS (arm64) and Linux
(glibc, aarch64):

* **`rlmdb`**: the whole of LMDB 0.9: creating and opening
  environments (also read-only, `MDB_NOSUBDIR`, `MDB_NOLOCK`), the reader
  table with stale-reader detection, read transactions (reset/renew), all
  cursor operations including `DUPSORT`/`DUPFIXED`/`INTEGERKEY`/
  `INTEGERDUP`/`REVERSEKEY`/`REVERSEDUP` databases and overflow pages; the
  write path with page allocation and free-list reuse exactly as C does
  it, splits, deletes, rebalancing and merges, nested transactions,
  spilling of huge transactions, `mdb_drop`, map resizing, and plain and
  compacting environment copies (byte-identical to `mdb_copy` and
  `mdb_copy -c`). Custom comparators.
* **`rlmdb-capi`**: all 56 functions of `lmdb.h`, and the 13 that LMDB
  1.0's `lmdb.h` adds (page size, transaction flags, `mdb_cursor_is_db`
  and two-phase commit implemented; encryption, checksums, incremental
  backups and crypto modules return `ENOTSUP`). The unmodified upstream
  `mtest.c` .. `mtest6.c`, `mdb_stat.c`, `mdb_dump.c`, `mdb_load.c` and
  `mdb_copy.c` run against it and give the same results as against C,
  down to the page numbers on the free list, and so do 1.0's `mtest.c` ..
  `mtest5.c` against C 1.0. `mdb_version()` reports rlmdb's own version
  (0.9.85 and 1.0.85 in the stubs), so programs can tell it from C LMDB;
  `rlmdb.h` adds
  `rlmdb_version()` and `rlmdb_version_string()` (rlmdb's version and the
  C release each format matches), which `rlmdb::version_info()` provides
  on the Rust side, and the data-file format functions
  (`rlmdb_env_set_format`, `rlmdb_env_get_format`, `rlmdb_sniff_format`,
  `rlmdb_set_default_format`, `rlmdb_env_set_pagesize`) with LMDB 1.0's
  constants.
* **`rlmdb-tools`**: `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy` with the
  same options, output, messages and exit codes as the C tools, plus
  `mdb_drop` (not part of 0.9.37; modelled on 1.0's), and `mdb_load -F
  0.9|1.0` for the format of a new environment.
* **`interop`**: the cross-process test bed: C, Rust and "C on the Rust
  library" peers writing and reading one environment concurrently, in all
  nine combinations, for each format (the C peer is C 0.9 on 0.9
  environments and C 1.0 on 1.0 ones); one rlmdb process with a 0.9 and a
  1.0 environment open next to C peers of both releases.

**The LMDB 1.0 data file.** `rlmdb` also reads and writes the data file of
LMDB 1.0 (`MDB_DATA_VERSION 3`), chosen per environment: an existing file
is opened in its format (`rlmdb::sniff_format`), a new one is 0.9 unless
`EnvironmentBuilder::format(Format::V10)` asks for 1.0, which also allows
page sizes from 256 B to 64 KiB (`page_size`). What it writes is page for
page what C LMDB 1.0 writes for the same operations, `mp_txnid` included
(checked against a C 1.0 build in `interop`); encrypted or checksummed 1.0
environments are refused. A 1.0 environment has 1.0's lock file, so C 1.0
and rlmdb processes can have it open at the same time, and one process can
have environments of both formats open. The C API and the
tools handle both formats: an existing file in its format, a new one as
0.9 unless 1.0 is asked for (`rlmdb_env_set_format`,
`rlmdb_set_default_format`, `mdb_load -F 1.0`, the C library's 1.0 stub,
or the **experimental**
environment variable `RLMDB_DEFAULT_FORMAT=1.0`, which may be removed or
restricted later); for a 1.0 file the tools print what C 1.0's tools
print. LMDB 1.0's `MDB_PREVSNAPSHOT` (open the previous snapshot, as the
tools' `-v` do) works for environments of both formats.

Deliberately out of scope:

* 32-bit targets (the file format depends on `sizeof(size_t)`; a
  `compile_error!` says so). Windows (64-bit, MSVC and MinGW), the BSDs
  and big-endian Linux are supported; `COMPATIBILITY.md` says how far each
  is tested.
* `MDB_FIXEDMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT`: accepted and ignored.
  They are per-process choices, so C peers using them are unaffected.
* A writable map (`MDB_WRITEMAP`): the flag is accepted, pages are written
  as without it, and its limits are C's (no nested transactions). On
  OpenBSD, whose maps do not see what is written to the file, an open for
  writing with it is refused (`ENOTSUP`); see `COMPATIBILITY.md`.
* LMDB 1.0 features beyond its data file: `MDB_VL32`, encryption,
  checksums; the format-converting copy (`DUAL_FORMAT.md` §4.3) is
  planned. (Two-phase commit, `mdb_txn_prepare` and `mdb_env_rollback`,
  is implemented, for 0.9 environments too.)
* The `heed`-compatible typed API described in `DESIGN.md` is deferred:
  the byte-level API is validated through the C API first.
  Performance work against C (benchmarks) has not started.

## Using it

Requires Rust 1.85 or later and a C compiler (the tests build the C
library and C test programs with the `cc` crate).

### `rlmdb`: the Rust library

```toml
[dependencies]
rlmdb = { git = "https://github.com/wojas/rlmdb" }
```

```rust
use rlmdb::{DatabaseFlags, Environment, Transaction, WriteFlags};

let env = Environment::builder()
    .map_size(1 << 30)
    .max_dbs(4)
    .open("/path/to/env")?;               // the directory must exist

let mut txn = env.begin_rw_txn()?;        // the writer lock, shared with C
let db = txn.create_db(Some("users"), DatabaseFlags::CREATE)?;
txn.put(db, b"alice", b"1", WriteFlags::empty())?;

let mut child = txn.begin_nested_txn()?;  // borrows the parent
child.put(db, b"bob", b"2", WriteFlags::empty())?;
child.commit()?;                          // abort() or drop discards
txn.commit()?;

let txn = env.begin_ro_txn()?;            // pins a snapshot; Send, no TLS
assert_eq!(txn.get(db, b"alice")?, Some(&b"1"[..]));
let mut cursor = txn.cursor(db)?;
while let Some((key, value)) = cursor.next()? {
    println!("{key:?} = {value:?}");      // slices into the memory map
}
```

The crate documentation (`cargo doc -p rlmdb --no-deps --open`) has a
runnable version of this example, the rules the types enforce (slices
cannot outlive a change, a parent is unusable while its child lives, ...),
and the `unsafe` policy: all `unsafe` code is in the private `os` module.

### `rlmdb-capi`: `librlmdb` for C programs

```sh
cargo build --release -p librlmdb -p lmdb09-stub -p lmdb10-stub
# target/release/librlmdb.a and target/release/librlmdb.{so,dylib}
# (the stubs: liblmdb09.*, liblmdb10.*)
scripts/install-libs.sh     # or build and install, PREFIX=/usr/local
```

Compile against the shipped headers (`lmdb.h`, LMDB 1.0's unmodified, and
`rlmdb.h`, rlmdb's own functions) and link with `-lrlmdb` instead of
`-llmdb`; a program written for LMDB 0.9 can keep 0.9's `lmdb.h` (as
`mtest.c` below, which includes the one next to it) and links the same way:

```sh
# from the repository root; shared library, macOS and Linux
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   -L target/release -lrlmdb
# Linux (glibc), static: the library needs the Rust runtime's system libs
cc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c \
   target/release/librlmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
# installed: pkg-config knows the flags
cc $(pkg-config --cflags rlmdb) -o mtest tests/lmdb-c/0.9/mtest.c $(pkg-config --libs rlmdb)
gcc -I rlmdb-capi/include -o mtest tests/lmdb-c/0.9/mtest.c target/release/librlmdb.dll.a  # Windows, MinGW (MSVC: rlmdb.dll.lib)
```

With the shared library from the build directory, the loader must find
it at run time (`LD_LIBRARY_PATH` on Linux, `DYLD_LIBRARY_PATH` on macOS,
`rlmdb.dll` next to the program on Windows). `mdb_version()` reports
rlmdb's version, and new environments are LMDB 0.9 unless the program
asks for 1.0: almost all programs are written for LMDB 0.9 today (this
default may change in a later release).

Two stubs stand in for C LMDB under its own name, for programs that were
built against it: the 0.9 stub with C 0.9's SONAME `liblmdb.so.0`, and
the 1.0 stub (`liblmdb.so`; `mdb_version()` says 1.0.85, and new
environments are LMDB 1.0). Both export everything `librlmdb` exports.
`scripts/install-libs.sh` puts them into `$libdir/rlmdb/lmdb-0.9` and `$libdir/rlmdb/lmdb-1.0`, off
the default library path, so that they never replace a C `liblmdb` by
accident: a program uses one when `LD_LIBRARY_PATH`, `LD_PRELOAD` or its
rpath names it (`DYLD_LIBRARY_PATH` on macOS; on Windows, as `lmdb.dll`
next to the program). Behaviour, the deviations from C, the thread rule
for write transactions, the install layout and `rlmdb.h` (rlmdb's version
and data-file format functions, next to `lmdb.h`) are in
[`rlmdb-capi/README.md`](rlmdb-capi/README.md).

### `rlmdb-tools`

```sh
cargo build -p rlmdb-tools --release
target/release/mdb_stat -efa /path/to/env
target/release/mdb_dump -a /path/to/env | target/release/mdb_load -a /path/to/copy
```

The documentation of the C tools applies (upstream LMDB's man pages,
`mdb_stat.1` … in `libraries/liblmdb` of
[github.com/LMDB/lmdb](https://github.com/LMDB/lmdb); they are not copied
here); see [`rlmdb-tools/README.md`](rlmdb-tools/README.md) for `mdb_drop`
and the few documented differences.

## Testing

```sh
cargo test --workspace                    # host (macOS or Linux)
scripts/linux-test.sh test --workspace    # Linux (Debian glibc) in Docker
scripts/check-all.sh                      # everything below, with a summary
```

`scripts/check-all.sh` (runs from any directory) does `cargo fmt --check`,
`cargo clippy --workspace --all-targets -D warnings`, `cargo doc
--workspace --no-deps` with `-D warnings`, `cargo test --workspace` on the
host, then the same in the Linux container (both with `--no-fail-fast`), and prints
PASS/FAIL per step (`--no-linux` skips the container).

`scripts/linux-test.sh [cargo args]` runs cargo in `rust:1-bookworm` with
the repository bind-mounted and its own target volume, limited to 2 GiB
of memory: everything here, build and benchmarks included, runs on a
machine with 2 GB of RAM. Linux matters
because its lock file differs (process-shared robust pthread mutexes
instead of named semaphores, a different header size) and its pages are
4 KiB instead of macOS's 16 KiB.

Most tests run C and Rust side by side: the C library and tools are built
from `tests/lmdb-c/0.9` at test time and serve as the oracle. Useful
subsets:

```sh
cargo test -p rlmdb                   # library: C-written files, differential write tests
cargo test -p rlmdb-capi                   # C programs against both libraries
cargo test -p rlmdb-tools                  # tools against the C tools
cargo test -p rlmdb-interop                     # concurrent C/Rust processes, 9 peer pairs per format
INTEROP_LOOP_SECS=20 cargo test -p rlmdb-interop --no-fail-fast   # longer stress run
```

A few long tests are `#[ignore]`d (a randomised differential run, a
full-size spill comparison with C, soak tests); run them with
`cargo test -p rlmdb --release -- --ignored`.

CI (GitHub Actions, `.github/workflows/ci.yml`) runs the host steps of
`scripts/check-all.sh` (fmt, clippy, doc, `cargo test --workspace`) on every
push and pull request on Linux x86-64, Linux arm64 and macOS arm64;
`nightly.yml` runs the ignored tests daily on Linux x86-64. Benchmarks are
not run in CI.

## Compatibility, in short

* The data file (format 1) and the lock file (format 1) are C's, on
  macOS and Linux/glibc. Environments move freely between C and Rust, and
  C and Rust processes can use one environment concurrently. A Rust
  writer leaves exactly the free list a C writer would; a compacting copy
  is byte-identical to C's.
* Everything observable through the C API matches C, except a short list
  of documented API-level differences (no on-disk effect): read
  transactions are never thread-bound (`MDB_NOTLS` always; a superset),
  opening one environment twice in a process fails with `EBUSY` (except
  under `MDB_NOLOCK`, where handles share one in-process reader table and
  writer lock),
  misuse that crashes C returns `EINVAL`, and plain C bugs (cursor reads
  past the end of a page, stale duplicate counts, unchecked
  `MDB_DUPFIXED` sizes, ...) are not reproduced. See
  [`COMPATIBILITY.md`](COMPATIBILITY.md),
  [`rlmdb-capi/README.md`](rlmdb-capi/README.md#differences-from-c-lmdb-0936)
  and [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md).

## Repository layout

The workspace is the repository root; every command above runs there.

```
./
  .github/workflows/  CI: ci.yml (every push), nightly.yml (ignored tests)
  Cargo.toml          workspace (edition 2021, MSRV 1.85)
  README.md           this file
  COMPATIBILITY.md    the compatibility contract
  DESIGN.md           design brief, format spec, findings, status
  DUAL_FORMAT.md      the LMDB 1.0 data file next to 0.9's
  KNOWN_C_BUGS.md     C 0.9.37 bugs and what the Rust code does instead
  rlmdb/          the library (package rlmdb, lib rlmdb)
    src/core/         environments, transactions, cursors, write path
    src/format/       on-disk structures over byte slices
    src/os/           all unsafe code: mmap, file locks, shared mutexes
    tests/            C-written files, differential and interop tests
  rlmdb-capi/          the C ABI; include/lmdb.h, rlmdb.h
    artifacts/        librlmdb.{a,so,dylib} and the two liblmdb stubs
    tests/c/          C test programs run against both libraries
  rlmdb-tools/         mdb_stat, mdb_dump, mdb_load, mdb_copy, mdb_drop
  interop/            cross-process test bed: C and Rust peers, scenarios
  bench/              benchmarks against C (rlmdb-bench), results
  tests/lmdb-c/       the C reference sources: LMDB 0.9.37 (0.9/) and 1.0
                      (1.0/) from pinned upstream commits, the test oracle
  scripts/
    linux-test.sh     cargo in a Linux (Debian glibc) container
    check-all.sh      fmt, clippy, doc, tests on host and Linux
    install-libs.sh   installs librlmdb, the stubs, headers, rlmdb.pc
    update-c-sources.sh  re-creates tests/lmdb-c from upstream
```

The history before this layout lived under `rust/` of a clone of the
upstream LMDB repository; commit ids in the documentation are those of
this repository, except where a commit is called upstream (an LMDB commit,
see [`tests/lmdb-c/README.md`](tests/lmdb-c/README.md)).

## License

Copyright 2026 Konrad Wojas. This is a derivative work of the LMDB C
library, copyright 2011-2021 Howard Chu, Symas Corp. Both are distributed
under the [OpenLDAP Public License, Version 2.8](LICENSE); see
[COPYRIGHT](COPYRIGHT) for the notices. The `Cargo.toml` files declare the
same license (`OLDAP-2.8`).
