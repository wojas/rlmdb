# rlmdb in detail

What the [README](../README.md) leaves out. The exact compatibility
promises, and the test behind each, are in
[`COMPATIBILITY.md`](../COMPATIBILITY.md).
The design is in [`DESIGN.md`](../DESIGN.md), the two formats in
[`DUAL_FORMAT.md`](../DUAL_FORMAT.md), and the rules for changes in
[`AGENTS.md`](../AGENTS.md).

## What is implemented

`rlmdb`, the Rust library, has all of LMDB 0.9:

* Environments: read-only, single file (`MDB_NOSUBDIR`), without a lock
  file (`MDB_NOLOCK`), map resizing while read transactions are open.
* The reader table, with cleanup of readers whose process died.
* Read transactions, with reset and renew.
* Every cursor operation on every kind of database (`DUPSORT`,
  `DUPFIXED`, `INTEGERKEY`, `INTEGERDUP`, `REVERSEKEY`, `REVERSEDUP`),
  overflow pages, custom comparison functions.
* The write path: free-page reuse as C does it, splits, rebalancing,
  nested transactions, spilling, dropping databases.
* Plain and compacting copies, byte for byte what `mdb_copy` and
  `mdb_copy -c` write.

From LMDB 1.0 it has the data file (below), two-phase commit and
`MDB_PREVSNAPSHOT`, for files of either format, and incremental backups,
for 1.0 files.

`rlmdb-capi`, the C library, has all 56 functions of LMDB 0.9.37's
`lmdb.h` and the 13 that 1.0 adds. Of those 13, encryption, checksums
and crypto modules check their arguments as C 1.0 does and return
`ENOTSUP`. It is built three times:

| Library | Package | `mdb_version()` | New environments |
|---|---|---|---|
| `librlmdb` (`-lrlmdb`) | `librlmdb` | rlmdb's version | 0.9 |
| 0.9 stub, installed as `liblmdb.so.0` | `lmdb09-stub` | 0.9.85 | 0.9 |
| 1.0 stub, installed as `liblmdb.so` | `lmdb10-stub` | 1.0.85 | 1.0 |

The patch level 85 lets a program tell it runs on rlmdb. Upstream's
`mtest*.c` and C tools give the same results against it as against C,
down to the page numbers on the free list. `rlmdb.h` adds the version
and format functions (`rlmdb_*`).

`rlmdb-tools` has `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy` and
`mdb_drop`, with the C tools' options, output and exit codes. Upstream's
man pages apply; the differences are in
[`rlmdb-tools/README.md`](../rlmdb-tools/README.md).

`interop` runs C and Rust processes on one environment at the same time,
every writer and reader pair, for both formats
([`interop/README.md`](../interop/README.md)). `bench` runs the same
workloads against C and Rust ([`bench/README.md`](../bench/README.md)).

## The LMDB 1.0 data file

rlmdb reads and writes LMDB 1.0's data file (`MDB_DATA_VERSION 3`) as
well as 0.9's, chosen per environment:

* An existing file opens in its own format (`rlmdb::sniff_format`).
  Asking for the other one fails with `MDB_VERSION_MISMATCH`.
* A new file is 0.9 unless 1.0 is asked for: `.format(Format::V10)`,
  `rlmdb_env_set_format`, `mdb_load -F 1.0`, or the 1.0 stub.
* 1.0 page sizes range from 256 B to 64 KiB.
* rlmdb writes the pages C 1.0 writes, and C 1.0 processes can share the
  environment. One process can have both formats open.
* Encrypted or checksummed 1.0 files are refused.

The design is in [`DUAL_FORMAT.md`](../DUAL_FORMAT.md).

## Compatibility with C

C and Rust processes use one environment at the same time. A Rust writer
leaves the same pages and free list a C writer would. Both must be built
for the same ABI to share a lock file, as two C builds must: musl and
glibc do not share one.

The C API differs from C only where that does not change the files. Read
transactions are never bound to a thread (as with `MDB_NOTLS`), a second
open of one environment fails with `EBUSY`, misuse that crashes C returns
`EINVAL`, and plain C bugs are not copied. The full list is in
[`rlmdb-capi/README.md`](../rlmdb-capi/README.md#differences-from-c-lmdb-0937),
the bugs in [`KNOWN_C_BUGS.md`](../KNOWN_C_BUGS.md).

## Safety

All `unsafe` code is in `rlmdb/src/os/` (memory maps, file locks, shared
mutexes, file I/O) and in the C API's FFI layer. Page parsing and the
B-tree are safe Rust over byte slices. The Rust types reject the usual
LMDB mistakes at compile time; the C library cannot, so C misuse stays
as dangerous as in C
([list](../rlmdb-capi/README.md#undefined-behaviour-that-remains)).

A damaged or crafted file gives an error, not a crash, panic or hang
(`rlmdb/tests/robust_*.rs`). Two cases are not covered, as in C: damage
that makes a writer reuse a page a reader still holds (the reader sees
it change, but stays inside the file), and another program truncating
the data file while it is open (`SIGBUS`).

## Platforms

| Platform | Status |
|---|---|
| Linux glibc, x86-64 and arm64; macOS arm64; Windows x86-64 (MSVC, MinGW-w64) | CI |
| Linux glibc, s390x (big-endian) | nightly CI under QEMU |
| FreeBSD, OpenBSD | tested by hand; FreeBSD weekly in CI |
| NetBSD, DragonFly, Android | not tested |
| 32-bit targets | not supported: the file format depends on `sizeof(size_t)` |

Lock files and page sizes differ per platform; see
[`COMPATIBILITY.md`](../COMPATIBILITY.md#platforms).

## Performance

On x86-64 Linux (4 KiB pages, tmpfs) against C built with upstream's
flags, reads take 0.94 times C's time and writes 1.13 times (geometric
means). One-put transactions (1.32), sequential appends (1.49) and
`DUPSORT` writes are furthest behind. The tables, the machines and the
method are in [`bench/RESULTS.md`](../bench/RESULTS.md).

## Notes on use

* Rust 1.85 or later. The crate is not on crates.io yet.
* `map_size` sets how large the data file may grow (1 MiB by default, as
  in C); `max_dbs` the number of named databases.
* Load only one of `librlmdb` and the two stubs into a process: each has
  its own process-wide state.
* `scripts/install-libs.sh` installs the libraries, headers and
  `rlmdb.pc`. It puts the stubs off the default library path, so they
  never replace a C `liblmdb` by accident.
* `RLMDB_DEFAULT_FORMAT=1.0` is experimental and may be removed: a stray
  variable creates files C 0.9 cannot open. To run an unchanged program
  on 1.0 files, use the 1.0 stub.

## Testing

The tests build C LMDB 0.9.37 and 1.0.3 from `tests/lmdb-c/` and need a
C compiler. [`tests/lmdb-c/README.md`](../tests/lmdb-c/README.md) says
where the sources come from.

```sh
cargo test -p rlmdb           # C-written files, differential writes, damaged files
cargo test -p rlmdb-capi      # C programs against the libraries
cargo test -p rlmdb-tools     # the tools against the C tools
cargo test -p rlmdb-interop   # C and Rust processes on one environment
cargo test -p rlmdb --release -- --ignored   # long tests, after write-path changes
```

Leave `RLMDB_DEFAULT_FORMAT` unset while testing. `scripts/linux-test.sh`
runs cargo in a Linux container limited to 2 GiB of memory;
`scripts/windows-test.sh` on a Windows machine over ssh.

CI runs fmt, clippy, docs and the tests on Linux (x86-64, arm64), macOS
and Windows for pull requests and pushes to main. Nightly it runs the
ignored tests and the s390x tests; weekly the FreeBSD tests.

## Repository layout

```
rlmdb/            the Rust library
  src/core/       environments, transactions, cursors, write path
  src/format/     on-disk structures, both formats
  src/os/         unsafe code: maps, locks, mutexes
rlmdb-capi/       the C API
  include/        lmdb.h (LMDB 1.0.3) and rlmdb.h
  artifacts/      librlmdb and the two stubs
rlmdb-tools/      the mdb_* tools
interop/          cross-process tests
bench/            benchmarks against C
tests/lmdb-c/     C LMDB 0.9.37 and 1.0.3, the reference
scripts/          check-all.sh, linux-test.sh, install-libs.sh, ...
```

## Not implemented

* The rest of LMDB 1.0: encryption, checksums, crypto modules.
* A copy that converts between 0.9 and 1.0 (planned).
* The typed API modelled on `heed`, after the byte-level API.
* A writable map for `MDB_WRITEMAP`: the flag is accepted, pages are
  written with `pwrite`.
* `MDB_FIXEDMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT`: accepted and ignored,
  since each is a choice of one process.
* 32-bit targets and `MDB_VL32`.
