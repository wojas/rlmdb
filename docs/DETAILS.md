# rlmdb in detail

This file holds what the [README](../README.md) leaves out: the goals
and their status, what each crate implements, the compatibility in
short, the benchmark per workload, notes on using the libraries, the
tests, the repository layout and what is out of scope. The exact promises and the
tests behind them are in [`COMPATIBILITY.md`](../COMPATIBILITY.md).

rlmdb is a rewrite, not a binding: no C code is linked into it. It
consists of the Rust library (`rlmdb`), a C library that replaces
`liblmdb` (`rlmdb-capi`), and the command line tools `mdb_stat`,
`mdb_dump`, `mdb_load`, `mdb_copy` and `mdb_drop` (`rlmdb-tools`).

## Goals and status

**C interoperability**: same data and lock files as C; C and Rust processes on one environment at once.

C and Rust processes share one environment, readers and writer lock
included, and the [interop suite](../interop/README.md) runs every
writer/reader pair in both formats on macOS, Linux (x86-64, arm64 and
big-endian s390x) and Windows (MSVC and MinGW); FreeBSD runs the library
and C API suites in a VM. The other BSDs and Android are not tested, and
32-bit targets are not supported ([COMPATIBILITY.md](../COMPATIBILITY.md)).

**No footguns**: safe Rust outside the OS layer; the types reject the classic LMDB misuse.

All `unsafe` code sits in `rlmdb/src/os/`, and the compiler rejects the
usual LMDB mistakes: a value kept past its transaction or past the next
write, a parent used while its child transaction lives, a write
transaction sent to another thread. The C library cannot check C
pointers, so misuse there stays as dangerous as in C.

**Untrusted files**: every read bounds-checked; damaged files give errors, not crashes.

Every offset read from the file is checked, so a damaged or crafted file
gives an error instead of a crash, a panic or a hang; the
[robustness tests](../rlmdb/tests/robust_mutation.rs) open, read and
write hundreds of randomly damaged files. One kind of damage gets
through: a file or lock file that tricks a writer into reusing a page a
reader still holds lets that reader see changing data, just as in C.
Reads still stay inside the file.

**Performance on par with C**: C's time per operation for reads and writes, without giving up safety.

Reads take about 0.94 times C's time on Linux, and the Lightning Stream
scan is at or ahead of C. Writes are still 13 % slower on Linux, with
one-put transactions and random deletes furthest behind; appends and
`DUPSORT` are not tuned. Numbers and method: [`bench/RESULTS.md`](../bench/RESULTS.md).

**0.9 and 1.0 formats**: both in one library, chosen per environment; new environments are 0.9 unless 1.0 is requested.

rlmdb writes 1.0 files page for page as C 1.0 does and shares them with
C 1.0 processes; one process can have both formats open. The 1.0 support
is recent. Converting copies, `MDB_PREVSNAPSHOT`, checksums and
encryption are not implemented ([`DUAL_FORMAT.md`](../DUAL_FORMAT.md)).

**Compatible improvements**: features C lacks that leave the files usable by C processes.

Read transactions are not tied to a thread, programs can ask for rlmdb's
version and capabilities, and C bugs that do not touch the files are
fixed ([`KNOWN_C_BUGS.md`](../KNOWN_C_BUGS.md)). All of this is always
on. The first opt-in feature, a copy that converts between the 0.9 and
1.0 formats, is planned.

**Drop-in C library**: `liblmdb.a`/`.so`/`.dylib` with the unmodified `lmdb.h`; upstream programs and tools run unchanged.

The upstream `mtest` programs and C tools, built against the Rust
library, give the same output and the same free list as against C. A
few calls behave differently where C would crash or misbehave, for
example a second open of one environment returns `EBUSY`
([list](../rlmdb-capi/README.md#differences-from-c-lmdb-0936)).

## What is implemented

All of it is tested against the C library on macOS (arm64) and Linux
(glibc, aarch64, in Docker). The full test suite has also passed on an
x86-64 Linux server.

**`rlmdb`, the Rust library.** All of LMDB 0.9:

* Environments: creating and opening them, also read-only, as a single
  file (`MDB_NOSUBDIR`) and without a lock file (`MDB_NOLOCK`); map
  resizing.
* The table of readers, with detection of readers whose process died.
* Read transactions, including reset and renew.
* Every cursor operation, on every kind of database: `DUPSORT`,
  `DUPFIXED`, `INTEGERKEY`, `INTEGERDUP`, `REVERSEKEY`, `REVERSEDUP`, and
  values on overflow pages. Custom comparison functions.
* The write path: page allocation and reuse of free pages exactly as C
  does it, page splits, deletes with rebalancing and merging, nested
  transactions, spilling of huge transactions to disk, and dropping
  databases.
* Plain and compacting copies, byte for byte what `mdb_copy` and
  `mdb_copy -c` write.
* The LMDB 1.0 data file, chosen per environment (below).

The typed API modelled on the `heed` crate, described in
[`DESIGN.md`](../DESIGN.md), is not written yet. The byte-level API is
validated through the C API first.

**`rlmdb-capi`, the C library.** All 56 functions of `lmdb.h`. The
unmodified upstream programs `mtest.c` to `mtest6.c`, `mdb_stat.c`,
`mdb_dump.c`, `mdb_load.c` and `mdb_copy.c` run against it and give the
same results as against C, down to the page numbers on the free list.
`mdb_version()` reports 0.9.85, so programs can tell it from C LMDB.
`rlmdb.h` adds:

* `rlmdb_version()`, `rlmdb_version_string()` and
  `rlmdb_has_capability()`: rlmdb's version, the C release each format
  matches, and capability names such as `format-1.0`. The Rust side is
  `rlmdb::version_info()`.
* The format functions `rlmdb_env_set_format`, `rlmdb_env_get_format`,
  `rlmdb_sniff_format`, `rlmdb_set_default_format` and
  `rlmdb_env_set_pagesize`, and LMDB 1.0's constants and error codes.

**`rlmdb-tools`, the command line tools.** `mdb_stat`, `mdb_dump`,
`mdb_load` and `mdb_copy`, with the same options, output, messages and
exit codes as the C tools. Also `mdb_drop`, which 0.9.36 does not have
(modelled on 1.0's), and `mdb_load -F 0.9|1.0` to choose the format of a
new environment. For a 1.0 file they print what C 1.0's tools print.

```sh
cargo build -p rlmdb-tools --release
target/release/mdb_stat -efa /path/to/env
target/release/mdb_dump -a /path/to/env | target/release/mdb_load -a /path/to/copy
```

The C man pages (`tests/lmdb-c/0.9/*.1`) apply. The few
differences are in [`rlmdb-tools/README.md`](../rlmdb-tools/README.md).

**`interop`, the cross-process test bed.** Separate processes write and
read one environment at the same time: C, Rust, and a C program on the
Rust library. All nine writer and reader pairs run for each format; the C
peer is C 0.9 on 0.9 environments and C 1.0 on 1.0 ones. One rlmdb
process also has a 0.9 and a 1.0 environment open next to C peers of both
releases. See [`interop/README.md`](../interop/README.md).

**`bench`, the benchmark.** The same workloads against C and Rust, and
interleaved comparisons of two Rust builds. See
[`bench/README.md`](../bench/README.md) and
[`bench/RESULTS.md`](../bench/RESULTS.md).

## The LMDB 1.0 data file

rlmdb also reads and writes the data file of LMDB 1.0
(`MDB_DATA_VERSION 3`), chosen per environment:

* An existing file is opened in its own format (`rlmdb::sniff_format`
  tells which one it has).
* A new one is 0.9 unless 1.0 is asked for:
  `EnvironmentBuilder::format(Format::V10)` in Rust,
  `rlmdb_env_set_format` in C, `mdb_load -F 1.0` for the tools. A
  process-wide default can be set (`rlmdb::set_default_format`,
  `rlmdb_set_default_format`), and so can the **experimental**
  environment variable `RLMDB_DEFAULT_FORMAT=1.0`, which may be removed
  or restricted later.
* 1.0 environments can have pages from 256 B to 64 KiB
  (`EnvironmentBuilder::page_size`, `rlmdb_env_set_pagesize`).
* What rlmdb writes is page for page what C LMDB 1.0 writes for the same
  operations, including the transaction id stamped on every page.
* A 1.0 environment has 1.0's lock file, so C 1.0 and rlmdb processes can
  have it open at the same time.
* Encrypted or checksummed 1.0 environments are refused.

The design, and what each stage needed, is in
[`DUAL_FORMAT.md`](../DUAL_FORMAT.md).

## Compatibility, in short

* The data file and the lock file are C's, on macOS and on Linux with
  glibc. Environments move freely between C and Rust, and C and Rust
  processes can use one environment at the same time.
* A Rust writer leaves exactly the pages and the free list a C writer
  would. A compacting copy is byte-identical to C's.
* Everything a program sees through the C API matches C, except a short
  list of differences. None of them affects the files:
  * read transactions are never bound to a thread (as with `MDB_NOTLS`;
    this allows more, never less);
  * opening one environment twice in a process fails with `EBUSY`;
  * misuse that crashes C returns `EINVAL`;
  * plain C bugs are not reproduced: cursor reads past the end of a page,
    stale duplicate counts, unchecked `MDB_DUPFIXED` sizes, and others.

Details: [`COMPATIBILITY.md`](../COMPATIBILITY.md), the
[differences of the C library](../rlmdb-capi/README.md#differences-from-c-lmdb-0936)
and [`KNOWN_C_BUGS.md`](../KNOWN_C_BUGS.md).

Two more limits of the safety goal, besides the one in the README:

* Another program that truncates or rewrites the data file outside the
  LMDB protocol while it is open is not covered. Pages of a live snapshot
  are read in place, so a shrunken file raises `SIGBUS`, as in C.
* The C library keeps C's rules for handles: using a handle after it was
  freed, or one transaction from two threads at once, is undefined
  behaviour there, as in C
  ([list](../rlmdb-capi/README.md#undefined-behaviour-that-remains)).

## Performance per workload

Setup:

* Machine: a Linux x86-64 virtual machine with 4 vCPUs (AMD EPYC Milan),
  4 KiB pages, environments on a tmpfs in RAM.
* C and Rust ran the same workloads in one session, taking turns.
* Each value is Rust's median time per operation divided by C's; above 1
  means Rust is slower. "6 % faster" in the README means 0.94.
* The README's means are geometric means over the read rows and the write
  rows of the table below.
* The `-O2` column (C's upstream flags) comes from two sessions: the
  write rows, `get_4k`, `get_64k`, `mixed_readers` and the `ls` rows from
  the session after the latest write-path change; the other read rows
  from the compiler-flags session (the write-path change did not touch
  them).
* The `-O3` column (C with `-O3 -flto -DNDEBUG`) is the compiler-flags
  session. Its write rows are derived: the `-O2` value, with C's time
  scaled by C's own `-O3`/`-O2` ratio in that session. "–" means the
  workload was not run with these flags.

| workload | what it does | Rust/C, C `-O2` | Rust/C, C `-O3` |
|---|---|---:|---:|
| `rand_get` | random gets, 1 M keys | 0.93 | 0.98 |
| `scan_fwd` | cursor scan, forwards | 0.93 | 0.96 |
| `scan_bwd` | cursor scan, backwards | 0.95 | 0.99 |
| `readers8_get` | 8 threads of random gets | 0.97 | 0.93 |
| `get_4k` | gets of 4 KiB values | 0.97 | – |
| `get_64k` | gets of 64 KiB values | 1.05 | – |
| `mixed_readers` | the 4 scanning readers next to `mixed_writer` | 0.81 | – |
| **reads, geometric mean** | | **0.94** | **0.97** (4 rows) |
| `rand_put` | random puts | 1.12 | 1.14 |
| `rand_overwrite` | random overwrites | 1.13 | 1.12 |
| `del_rand` | random deletes | 1.23 | 1.27 |
| `put_4k` | puts of 4 KiB values | 1.01 | – |
| `put_64k` | puts of 64 KiB values | 1.01 | – |
| `txn_nosync` | one put per transaction, no sync | 1.32 | 1.35 |
| `mixed_writer` | commits of 100 random overwrites, next to 4 scanning readers | 1.09 | – |
| **writes, geometric mean** | | **1.13** | **1.22** (4 rows) |
| `ls_scan` | Lightning Stream sync scan, 5 M records | 0.81 | 1.00 |
| `ls_update` | Lightning Stream update | 1.09 | 1.13 |

Unless noted, keys are 16 bytes and values 100 bytes. Over the same rows
as the `-O3` means, the `-O2` means are 0.94 (reads) and 1.20 (writes).

The write gaps that matter are one-put transactions (`txn_nosync`, 1.32)
and random deletes (`del_rand`, 1.23); random puts and overwrites are at
about 1.1 (1.12 and 1.13). Large values (`put_4k`, `put_64k`) are at
parity.

Left out of the means:

* Sequential appends (`seq_put_append`, puts with `MDB_APPEND`: 1.49
  against `-O2` C, 1.45 against `-O3` C, derived) and the `DUPSORT`
  workloads (`dup_put` 1.67, `dup_get_both` 1.17, `dup_next_dup` 1.24).
  They are not representative of the write-heavy workloads this project
  targets.
* `txn_sync`: one put per transaction with sync. On the tmpfs the sync
  costs almost nothing, so it measures the commit path (1.28); on a real
  disk the sync dominates and both sides are equal (1.02 on ext4).

Notes:

* `ls_scan` varies most between sessions, because C's own time does: it
  was 0.92 against `-O2` C in the compiler-flags session (the README's
  0.81-0.92). Read it as 0.8 to 1.0.
* The `-O3` write values are derived because the compiler-flags session
  ran before the latest write-path change. The `ls_update` value in the
  `-O3` column is measured, in that older session.

The macOS column of the README: Apple M1 Pro, 16 KiB pages, environments
on the internal SSD; the values come from the macOS table of
[`bench/RESULTS.md`](../bench/RESULTS.md). The same means there are 0.85
for reads and 0.92 for writes; `ls_scan` is 0.81, `ls_update` 0.83.

The other machines, the history of each change and the profiles are in
[`bench/RESULTS.md`](../bench/RESULTS.md).

## Using the libraries: notes

Rust:

* Requires Rust 1.85 or later. The crate is not on crates.io yet; depend
  on it from git (`rlmdb = { git = "https://github.com/wojas/rlmdb" }`)
  or by path.
* The directory of an environment must exist before `open`.
* `EnvironmentBuilder::format(Format::V10)` applies to new environments
  only. An existing environment keeps its format; asking for the other
  one fails with `MDB_VERSION_MISMATCH`. `rlmdb::sniff_format` tells
  which format a file has.
* `map_size` sets how large the data file may grow (1 MiB by default for
  a new environment, as in C); `max_dbs` the number of named databases.
* The crate docs (`cargo doc -p rlmdb --no-deps --open`) cover nested
  transactions, write cursors and the rules the types enforce.

C:

* Compile against `rlmdb-capi/include`: the unmodified 0.9.36 `lmdb.h`,
  and `rlmdb.h`, which includes `lmdb.h` and adds rlmdb's functions.
* With the shared library, the loader must find it at run time
  (`LD_LIBRARY_PATH` on Linux; on macOS the dylib's install name is its
  absolute build path, or use `DYLD_LIBRARY_PATH`).
* `rlmdb_env_set_format` must come before `mdb_env_open` and applies to a
  new environment only. If the environment exists in the other format,
  `mdb_env_open` fails with `MDB_VERSION_MISMATCH`.
  `rlmdb_env_get_format` reports the format after the open.
* `rlmdb_set_default_format(RLMDB_FORMAT_10)` sets the format of every new
  environment of the process.
* `RLMDB_DEFAULT_FORMAT=1.0` does the same for unmodified programs. It is
  experimental and may be removed or restricted in a later release,
  because a stray variable silently creates files that C LMDB 0.9 cannot
  open. It never affects existing environments.
* Every function, the differences from C and the thread rules:
  [`rlmdb-capi/README.md`](../rlmdb-capi/README.md).

## Testing in detail

Requires Rust 1.85 or later and a C compiler: the tests build the C
library and C test programs with the `cc` crate.

```sh
cargo test --workspace                    # on this machine (macOS or Linux)
scripts/linux-test.sh test --workspace    # Linux (Debian, glibc) in Docker
scripts/check-all.sh                      # all of the below, with a summary
```

`scripts/check-all.sh` runs from any directory. It does `cargo fmt
--check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo
doc --workspace --no-deps` with `-D warnings`, and `cargo test
--workspace` on the host, then the same tests in the Linux container
(both with `--no-fail-fast`). It prints PASS or FAIL per step.
`--no-linux` skips the container.

`scripts/linux-test.sh [cargo args]` runs cargo in `rust:1-bookworm`,
with the repository mounted and a build directory of its own; without
arguments it runs `test --workspace`. Linux matters because its lock file
differs (process-shared robust pthread mutexes instead of named
semaphores, and a different header size) and its pages are 4 KiB instead
of macOS's 16 KiB.

The interop matrix runs C programs, Rust programs and C programs on the
Rust library as separate processes on one environment, in every pairing
and in both formats.

Most tests run C and Rust side by side. The C library and tools are built
at test time from `tests/lmdb-c/0.9` (LMDB 0.9.36) and `tests/lmdb-c/1.0`
(a pinned LMDB 1.0 commit; see [`interop/README.md`](../interop/README.md))
and serve as the oracle. Useful subsets:

```sh
cargo test -p rlmdb           # C-written files, differential write tests, damaged files
cargo test -p rlmdb-capi      # C programs against both libraries
cargo test -p rlmdb-tools     # the tools against the C tools
cargo test -p rlmdb-interop   # concurrent C and Rust processes, both formats
cargo test -p rlmdb-interop --test c10_writes_rust_reads   # one pair
INTEROP_LOOP_SECS=20 cargo test -p rlmdb-interop --no-fail-fast   # longer stress run
```

A few long tests are ignored by default: a full-size spill comparison
with C (in both formats) and soak tests (random writes, spilling and
nested transactions checked against a model). Run them with `cargo test -p rlmdb --release -- --ignored` (and the same for
`-p rlmdb-interop`) after changes to the write path or to spilling.

Do not set `RLMDB_DEFAULT_FORMAT` while testing: the suites assume it is
unset, and `scripts/check-all.sh` unsets it.

## Repository layout

```
Cargo.toml            workspace (edition 2021, Rust 1.85 or later)
README.md             overview
AGENTS.md             rules for every change
COMPATIBILITY.md      the compatibility promises and their tests
DESIGN.md             design, file format, findings, status
DUAL_FORMAT.md        the 0.9 and 1.0 formats in one library
KNOWN_C_BUGS.md       C bugs and what rlmdb does instead
docs/DETAILS.md       this file
rlmdb/                the Rust library
  src/core/           environments, transactions, cursors, write path
  src/format/         on-disk structures over byte slices, both formats
  src/os/             all unsafe code: memory maps, file locks, shared mutexes
  tests/              C-written files, differential and robustness tests
rlmdb-capi/           liblmdb.{a,so,dylib}: the C API
  include/            lmdb.h (unmodified 0.9.36) and rlmdb.h
  tests/c/            C test programs run against both libraries
rlmdb-tools/          mdb_stat, mdb_dump, mdb_load, mdb_copy, mdb_drop
interop/              cross-process test bed: C and Rust peers, scenarios
bench/                benchmarks against C; RESULTS.md, results/
tests/lmdb-c/0.9/     C LMDB 0.9.36 sources, the test oracle
tests/lmdb-c/1.0/     C LMDB 1.0 sources, the oracle for the 1.0 format
scripts/
  linux-test.sh       cargo in a Linux (Debian, glibc) container
  check-all.sh        format, lints, docs, tests on host and Linux
.github/workflows/    ci.yml and nightly.yml
```

## Out of scope, or not yet

* **32-bit targets.** The file format depends on the size of `size_t`; a
  `compile_error!` says so. Not planned unless needed. The same goes for
  `MDB_VL32`.
* **`MDB_WRITEMAP`, `MDB_FIXEDMAP`, `MDB_MAPASYNC`, `MDB_NOMEMINIT`.**
  Accepted and ignored. They are choices of one process, so C peers that
  use them are not affected. `MDB_WRITEMAP` may come later.
* **LMDB 1.0 features beyond its data file:** encryption, checksums,
  `MDB_PREVSNAPSHOT` (rejected with `EINVAL`, as 0.9 does, also for 1.0
  environments) and two-phase commit. Possible later.
* **A copy that converts between the formats** (capability
  `convert-copy`, `DUAL_FORMAT.md` §4.3). Planned.
* **The typed API modelled on `heed`.** Deferred until the byte-level API
  has been validated through the C API.
