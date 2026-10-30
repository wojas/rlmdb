# rlmdb: LMDB in Rust

A rewrite of [LMDB](http://www.lmdb.tech/) 0.9 in Rust that is binary
compatible with the C implementation in `../libraries/liblmdb` (0.9.36): the
data file and the lock file are byte for byte what C writes and expects,
so C and Rust processes can open one environment **at the same time**,
share its reader table and writer lock, and read and write it in turns.
It is a rewrite, not a binding: no C code is linked into the library.

On top of the Rust library sit a drop-in replacement for `liblmdb`
(the same `mdb_*` C ABI and `lmdb.h`) and the command line tools.

* [`COMPATIBILITY.md`](COMPATIBILITY.md): the compatibility contract and
  what verifies it.
* [`DESIGN.md`](DESIGN.md): goals, the on-disk and lock file format
  specification, the architecture, findings, and the status per stage.
* [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md): bugs of C 0.9.36 that are not
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
* **`rlmdb-capi`**: all 56 functions of `lmdb.h`. The unmodified upstream
  `mtest.c` .. `mtest6.c`, `mdb_stat.c`, `mdb_dump.c`, `mdb_load.c` and
  `mdb_copy.c` run against it and give the same results as against C,
  down to the page numbers on the free list. `mdb_version()` reports
  0.9.85, so programs can tell it from C LMDB; `rlmdb.h` adds
  `rlmdb_version()`, `rlmdb_version_string()` and
  `rlmdb_has_capability()` (rlmdb's version, the C release each format
  matches, capability tokens), which `rlmdb::version_info()` provides on
  the Rust side, and the data-file format functions
  (`rlmdb_env_set_format`, `rlmdb_env_get_format`, `rlmdb_sniff_format`,
  `rlmdb_set_default_format`, `rlmdb_env_set_pagesize`) with LMDB 1.0's
  constants.
* **`rlmdb-tools`**: `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy` with the
  same options, output, messages and exit codes as the C tools, plus
  `mdb_drop` (not part of 0.9.36; modelled on 1.0's), and `mdb_load -F
  0.9|1.0` for the format of a new environment.
* **`interop`**: the cross-process test bed: C, Rust and "C on the Rust
  library" peers writing and reading one environment concurrently, in all
  nine combinations.

**The LMDB 1.0 data file.** `rlmdb` also reads and writes the data file of
LMDB 1.0 (`MDB_DATA_VERSION 3`), chosen per environment: an existing file
is opened in its format (`rlmdb::sniff_format`), a new one is 0.9 unless
`EnvironmentBuilder::format(Format::V10)` asks for 1.0, which also allows
page sizes from 256 B to 64 KiB (`page_size`). What it writes is page for
page what C LMDB 1.0 writes for the same operations, `mp_txnid` included
(checked against a C 1.0 build in `interop`); encrypted or checksummed 1.0
environments are refused. A 1.0 environment has 1.0's lock file, so C 1.0
and rlmdb processes can have it open at the same time. The C API and the
tools handle both formats: an existing file in its format, a new one as
0.9 unless 1.0 is asked for (`rlmdb_env_set_format`,
`rlmdb_set_default_format`, `mdb_load -F 1.0`, or the **experimental**
environment variable `RLMDB_DEFAULT_FORMAT=1.0`, which may be removed or
restricted later); for a 1.0 file the tools print what C 1.0's tools
print.

Deliberately out of scope:

* Windows (all OS code is behind `rlmdb/src/os/`, so a `windows.rs`
  can be added) and 32-bit targets (the file format depends on
  `sizeof(size_t)`; a `compile_error!` says so).
* `MDB_WRITEMAP`, `MDB_FIXEDMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT`: accepted
  and ignored. They are per-process choices, so C peers using them are
  unaffected.
* LMDB 1.0 features beyond its data file: `MDB_VL32`, encryption,
  checksums, `MDB_PREVSNAPSHOT` (rejected with `EINVAL`, as 0.9 does, for
  1.0 environments too), two-phase commit; the format-converting copy
  (`DUAL_FORMAT.md` §4.3) is planned.
* The `heed`-compatible typed API described in `DESIGN.md` is deferred:
  the byte-level API is validated through the C API first.
  Performance work against C (benchmarks) has not started.

## Using it

Requires Rust 1.85 or later and a C compiler (the tests build the C
library and C test programs with the `cc` crate).

### `rlmdb`: the Rust library

```toml
[dependencies]
rlmdb = { path = "rust/rlmdb" }
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

### `rlmdb-capi`: `liblmdb` for C programs

```sh
cd rust
cargo build -p rlmdb-capi --release
# target/release/liblmdb.a and target/release/liblmdb.{so,dylib}
```

Compile against the shipped header (the unmodified 0.9.36 `lmdb.h`) and
link either library instead of the C one:

```sh
# from the repository root; macOS
cc -I rust/rlmdb-capi/include -o mtest libraries/liblmdb/mtest.c \
   rust/target/release/liblmdb.a
# Linux (glibc): the static library needs the Rust runtime's system libs
cc -I rust/rlmdb-capi/include -o mtest libraries/liblmdb/mtest.c \
   rust/target/release/liblmdb.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
# shared library, either platform
cc -I rust/rlmdb-capi/include -o mtest libraries/liblmdb/mtest.c \
   -L rust/target/release -llmdb
```

With the shared library, the loader must find it at run time
(`LD_LIBRARY_PATH` on Linux; on macOS the dylib's install name is its
absolute build path, or use `DYLD_LIBRARY_PATH`). Behaviour, the deviations
from C, the thread rule for write transactions and `rlmdb.h` (rlmdb's
version, capability and data-file format functions, next to `lmdb.h`) are
in
[`rlmdb-capi/README.md`](rlmdb-capi/README.md).

### `rlmdb-tools`

```sh
cd rust
cargo build -p rlmdb-tools --release
target/release/mdb_stat -efa /path/to/env
target/release/mdb_dump -a /path/to/env | target/release/mdb_load -a /path/to/copy
```

The C man pages (`libraries/liblmdb/*.1`) apply; see
[`rlmdb-tools/README.md`](rlmdb-tools/README.md) for `mdb_drop` and the few
documented differences.

## Testing

```sh
cd rust
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
the repository bind-mounted and its own target volume. Linux matters
because its lock file differs (process-shared robust pthread mutexes
instead of named semaphores, a different header size) and its pages are
4 KiB instead of macOS's 16 KiB.

Most tests run C and Rust side by side: the C library and tools are built
from `../libraries/liblmdb` at test time and serve as the oracle. Useful
subsets:

```sh
cargo test -p rlmdb                   # library: C-written files, differential write tests
cargo test -p rlmdb-capi                   # C programs against both libraries
cargo test -p rlmdb-tools                  # tools against the C tools
cargo test -p rlmdb-interop                     # concurrent C/Rust processes, 9 peer pairs
INTEROP_LOOP_SECS=20 cargo test -p rlmdb-interop --no-fail-fast   # longer stress run
```

A few long tests are `#[ignore]`d (a randomised differential run, a
full-size spill comparison with C, soak tests); run them with
`cargo test -p rlmdb --release -- --ignored`.

## Compatibility, in short

* The data file (format 1) and the lock file (format 1) are C's, on
  macOS and Linux/glibc. Environments move freely between C and Rust, and
  C and Rust processes can use one environment concurrently. A Rust
  writer leaves exactly the free list a C writer would; a compacting copy
  is byte-identical to C's.
* Everything observable through the C API matches C, except a short list
  of documented API-level differences (no on-disk effect): read
  transactions are never thread-bound (`MDB_NOTLS` always; a superset),
  opening one environment twice in a process fails with `EBUSY`,
  misuse that crashes C returns `EINVAL`, and plain C bugs (cursor reads
  past the end of a page, stale duplicate counts, unchecked
  `MDB_DUPFIXED` sizes, ...) are not reproduced. See
  [`COMPATIBILITY.md`](COMPATIBILITY.md),
  [`rlmdb-capi/README.md`](rlmdb-capi/README.md#differences-from-c-lmdb-0936)
  and [`KNOWN_C_BUGS.md`](KNOWN_C_BUGS.md).

## Layout

```
rust/
  Cargo.toml          workspace (edition 2021, MSRV 1.85)
  README.md           this file
  COMPATIBILITY.md    the compatibility contract
  DESIGN.md           design brief, format spec, findings, status
  KNOWN_C_BUGS.md     C 0.9.36 bugs and what the Rust code does instead
  rlmdb/          the library (package rlmdb, lib rlmdb)
    src/core/         environments, transactions, cursors, write path
    src/format/       on-disk structures over byte slices
    src/os/           all unsafe code: mmap, file locks, shared mutexes
    tests/            C-written files, differential and interop tests
  rlmdb-capi/          liblmdb.{a,so,dylib}: the C ABI; include/lmdb.h, rlmdb.h
    tests/c/          C test programs run against both libraries
  rlmdb-tools/         mdb_stat, mdb_dump, mdb_load, mdb_copy, mdb_drop
  interop/            cross-process test bed: C and Rust peers, scenarios
  scripts/
    linux-test.sh     cargo in a Linux (Debian glibc) container
    check-all.sh      fmt, clippy, doc, tests on host and Linux
```

## License

Copyright 2026 Konrad Wojas. This is a derivative work of the LMDB C
library, copyright 2011-2021 Howard Chu, Symas Corp. Both are distributed
under the [OpenLDAP Public License, Version 2.8](LICENSE); see
[COPYRIGHT](COPYRIGHT) for the notices. The `Cargo.toml` files declare the
same license (`OLDAP-2.8`).
