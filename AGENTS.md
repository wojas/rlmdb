# Working on rlmdb

This repository holds `rlmdb`, a rewrite of LMDB 0.9 in Rust that is
binary compatible with the C library in `tests/lmdb-c/0.9` (the reference
is whatever that tree holds; 0.9.37 at the time of writing), and that also
reads and writes the data file of LMDB 1.0 (the reference is the pinned
upstream `mdb.RE/1.0` commit vendored in `tests/lmdb-c/1.0`; see
`tests/lmdb-c/README.md`). This file lists the rules that every change
must respect. The details live in:

* `DESIGN.md`: goals, the on-disk and lock-file format, architecture.
* `COMPATIBILITY.md`: the compatibility contract and which test checks it.
* `KNOWN_C_BUGS.md`: C bugs not ported (and the few ported on purpose).
* `rlmdb-capi/README.md`: using the C API; `rlmdb-capi/DETAILS.md`: its
  documented deviations.
* `bench/README.md`, `bench/RESULTS.md`: benchmarks against C.

## 1. Binary compatibility with C LMDB 0.9 comes first

* The data file (`MDB_DATA_VERSION 1`) and the lock file
  (`MDB_LOCK_VERSION 1`) are specified by the C source. A C process and a
  Rust process must be able to use one environment at the same time.
* The lock-file version follows the data format: version 1 with 0.9 data
  files, version 2 (LMDB 1.0's, `format::lockfile::LOCK_V10`) with 1.0 data
  files. Both are per-platform layouts; every access goes through the
  `LockLayout` of the environment's format, never through fixed offsets.
* Hard invariants, never negotiable: every structure C reads is laid out
  as C expects; no page is lost or shared (the peers' page audit checks
  this after every scenario); a page freed in a transaction is not reused
  while a reader can still see it; the meta page, transaction id and
  reader-slot protocol are C's; sort order is C's, even where C is wrong
  (`KNOWN_C_BUGS.md` row 7). Shared state decides what to port: free-list
  format, lock-file fields and reader slots match C even where C is wrong
  (rows 21 and 22).
* Layout exactness is a test oracle, not a product requirement. Today a
  Rust writer leaves exactly the file a C writer would (same pages in the
  same places, same free list, same spill decisions, same compacting copy)
  and the tests check it down to page numbers
  (`rlmdb/tests/write_differential.rs`, `rlmdb-capi/tests/mtest.rs`,
  `interop/`). Keep it that way: it catches write-path bugs cheaply, and
  the peer and differential tests always run in this plain compatibility
  mode. A change that writes something else (which free page is reused,
  where a split lands, when to spill, another free-list layout) is an
  opt-in feature a caller enables by name (`rlmdb::feature`, issue #2):
  off by default, the files still readable and writable by C, with its own
  tests (the C peers reading and writing what a Rust process with the
  feature wrote). The peers and the differential tests disable every
  feature that is on by default. Never diverge as a side effect of another
  change.
* Never recompute what C reads from the file. Example: macOS semaphore
  names are stored in the lock file with uninitialised padding, so they are
  read, not derived; in lock-file version 2 they are derived from the hash
  stored in the file (`mtb_mutexid`), which is read, not recomputed.
* Platform ABI is part of the contract: `sizeof(pthread_mutex_t)` and the
  reader-table offset differ between Linux and macOS, and page sizes differ
  (16 KiB on macOS, 4 KiB on Linux). Test on both before claiming done.
* LMDB 1.0's data file (`MDB_DATA_VERSION 3`, `DUAL_FORMAT.md`) is a
  supported format with the same rules: C 1.0 is its specification and its
  oracle (`interop`: `mdb_*10`, `cpeer10`, `writeops10`), C 1.0 and Rust
  processes share a 1.0 environment as C 0.9 and Rust share a 0.9 one, one
  process may have environments of both formats open, and a Rust writer
  leaves the pages C 1.0 would, `mp_txnid` included (C 1.0 writers decide
  from it which pages they may change in place). Every format-dependent
  offset, size and decision goes through `format::Layout`; never hard-code
  the 0.9 header in engine code, and keep 0.9 files byte-identical when
  touching the 1.0 side (and the reverse). Page sizes of 1.0 environments
  range from 256 B to 64 KiB, so test small pages too.

## 2. Do not port plain C bugs; track them

Where C behaviour is clearly a bug, implement the correct behaviour
and add a row to `KNOWN_C_BUGS.md` (where in C, what C does, what Rust
does, interop impact). The constraint: the deviation must not change the
files, the lock file, or what a C peer concludes about them. If it would,
port the bug and note that too. Malformed input returns an error, never a
panic, hang or out-of-bounds read.

Cite a ledger row as `KNOWN_C_BUGS.md row nn`, or `row nn` where the file
is clear from the context, never as `#nn`: GitHub links `#nn` to issue or
PR nn in commit messages, docs and comments (`ITS#nnnn` is safe).

## 3. Safety and soundness

* All `unsafe` lives in `rlmdb/src/os/` (mmap, file locks, shared
  mutexes and semaphores, pread/pwrite, volatile meta reads) and in
  `rlmdb-capi` (FFI). Everything else, including all page parsing and
  B-tree logic, is safe Rust over `&[u8]`. `rlmdb/src/lib.rs` enforces this
  with `deny(unsafe_code)` and per-module `forbid`. No `MaybeUninit`,
  `transmute` or pointer casts of mapped memory outside `os`.
* Each `unsafe` block carries a `// SAFETY:` comment stating the invariant
  it relies on. The central one is LMDB's MVCC guarantee: pages reachable
  from a pinned snapshot are not modified while the reader slot pins it.
* Integers from disk are read with `from_ne_bytes` over sub-slices; every
  offset into a page or the map is bounds-checked. Crafted files must not
  cause UB, panics (including overflow in debug builds) or endless loops;
  `rlmdb/tests/robust_*.rs` and the mutation tests check this. The one
  documented exception is damage that breaks MVCC itself (a free list
  naming a live page, a lock file with a forged `mti_txnid` or a cleared
  live reader slot): a writer may then rewrite a page under a live
  reader's slice, as in C; see `os::DataMap` and the residual-risk list
  in `rlmdb-capi/DETAILS.md`.
* Meta pages are written concurrently by other processes: read them with
  volatile reads into an owned value, validate, and re-check `mti_txnid`.
* `RwTxn` is `!Send` (the Linux writer mutex is thread-owned). Read
  transactions are never thread-bound (`MDB_NOTLS` semantics, always).
* Dirty pages are owned buffers; readers in a write transaction resolve a
  page number in the dirty list, then parent lists, then the map. No
  references into buffers the same transaction may mutate.
* `set_map_size` must keep slices of live read transactions valid (maps
  are `Arc`'d; old maps live until their last transaction drops).

## 4. The Rust API has no footguns

Lifetimes tie values to their transaction, mutation needs `&mut`, a parent
is unusable while a nested transaction lives, handles cannot outlive the
environment, and there is no "undefined behaviour if called twice". Keep
the byte-level core API small and explicit. The `heed`-style typed layer is
deferred until the core has been validated through the C API.

## 5. The C API is a drop-in `liblmdb`

* `rlmdb-capi` (the implementation, an rlib) is linked into three
  libraries by the packages in `rlmdb-capi/artifacts`: `librlmdb`
  (`librlmdb.a` / `.so` / `.dylib`, `rlmdb.dll`; `-lrlmdb`), and two stubs
  named `liblmdb` that stand in for C LMDB 0.9 (SONAME `liblmdb.so.0`)
  and 1.0 (SONAME `liblmdb.so`), built as `liblmdb09.*` / `liblmdb10.*`
  and renamed by `scripts/install-libs.sh`. They differ only in their
  names, in `mdb_version` (`librlmdb` rlmdb's own version, programs link
  against it explicitly; the stubs 0.9.85 and 1.0.85) and in the format
  of new environments that ask for none (0.9, 0.9, 1.0: almost all
  programs are written for 0.9 today); everything else is common code,
  and the profile is a build-time property (`rlmdb-capi/src/profile.rs`:
  no cargo feature, no load-time code).
* Every artifact exports the full symbol set: every function of LMDB
  0.9.37's `lmdb.h`, of LMDB 1.0's `lmdb.h` (what rlmdb does not
  implement makes C 1.0's argument checks, then returns `ENOTSUP`) and of
  `rlmdb.h`, with the same flag values and error codes
  (`rlmdb-capi/tests/capi_artifacts.rs` checks the set against the
  headers). Unmodified `mtest*.c` and the C tools of each release must
  behave identically against it.
* Argument validation follows `mdb.c`: same checks, same order, same
  return codes. Misuse that crashes C (NULL handles, use after free)
  returns `EINVAL` instead; keep that list in `rlmdb-capi/DETAILS.md`
  current.
* The layer is thin: no database logic in `rlmdb-capi`. Lifetimes are
  erased in one place (`erased.rs`); handle ownership is explicit.
* The tools in `rlmdb-tools` match the C tools' options, output, messages
  and exit codes; the C tools are their test oracle, each release for its
  own format (C 1.0's for 1.0 files, in `interop/tests/tools10.rs`).
* The public C API of `librlmdb` is LMDB 1.0's plus rlmdb's own
  functions: `include/lmdb.h` is the pristine LMDB 1.0.3 header (byte for
  byte `tests/lmdb-c/1.0/lmdb.h` after one comment block;
  `scripts/update-c-sources.sh` writes it and a test compares the two),
  and `include/rlmdb.h` holds only rlmdb's own functions (`rlmdb_*`). It
  must compile after either `lmdb.h`, 1.0's or 0.9's (it needs only
  `MDB_env` and plain C types, and includes the `lmdb.h` next to it only
  when none came before). Programs written for 0.9 (the tests' 0.9
  programs, `cpeer_capi`) compile against the vendored 0.9 header
  (`tests/lmdb-c/0.9/lmdb.h`), with `rlmdb.h` after it where they use
  rlmdb's functions, and link against the same symbols.

## 6. Performance

* Target: comparable to C. No allocations in the hot read path, binary
  search on pages, fixed-size cursor stacks, zero-copy values into the map.
* Measure before and after with the interleaved A/B of `bench`
  (`rlmdb-bench --baseline <old binary> --rounds 3 --reps 3`); the machine
  may be noisy, so never compare two separate runs. Keep a change only if
  it measurably helps; speculative complexity is reverted.
* Performance changes must not add `unsafe` and must keep the invariants
  of section 1; unless the task says otherwise they are also
  layout-preserving (same file bytes). What matters is the relative
  change: the before/after of the interleaved A/B goes into the change's
  commit or PR message (and into the session, to decide whether the
  change was worth it). Do not commit benchmark results at points during
  development: a history of absolute numbers from a shifting code base on
  shifting hardware is useless. `bench/RESULTS.md` and `bench/results/`
  are updated near a release, when the numbers are to be compared and
  published.
* Linux x86-64 is the platform that matters most. Performance decisions
  are judged on real x86-64 Linux hardware first: results lead with an
  x86-64 Linux column, and a regression there outweighs a gain on macOS
  or arm64. The arm64 Linux VM and emulation (Rosetta, QEMU) prove
  correctness, not performance.

## 7. Code style

* Clear, documented Rust: every module starts with a doc comment saying
  what it models and which part of `mdb.c` it corresponds to; every `pub`
  item has a doc comment; comments say why. Where behaviour must match C
  exactly, say so and cite the C function.
* Name things after the concept (`lower`, `upper`, `num_keys`,
  `child_pgno`), mention the C name once in the doc comment.
* Small functions over C-style monoliths. Prefer returning errors over
  panicking in library code.
* `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` must be
  clean. MSRV is in `Cargo.toml` (`rust-version`).

## 8. Testing: C is the oracle

* The C library and tools are built from `tests/lmdb-c/0.9` at test
  time; most tests run C and Rust side by side or in lock step.
* The LMDB 1.0 oracle (`mdb_stat10`, `cpeer10`, … in `interop`) is built
  from `tests/lmdb-c/1.0`, a pinned upstream `mdb.RE/1.0` commit
  (`interop/README.md`). The C sources are vendored and only change through
  `scripts/update-c-sources.sh` (upstream plus the patches in
  `tests/lmdb-c/patches/`); no build step needs git or the network.
* The interop matrix has four peers: `cpeer` (C 0.9), `cpeer10` (C 1.0),
  `rpeer` (Rust) and `cpeer_capi` (C on the Rust library); the last two
  open both formats (`--format 1.0`). Every scenario runs for every
  (writer, reader) pair that shares a format, with the C peer of that
  format verifying and auditing: `tests/c_vs_c.rs` … for 0.9,
  `tests/c10_vs_c10.rs`, `rust_writes_c10_reads.rs`, `rust_vs_rust10.rs`
  … for 1.0, `pagesize10.rs` at 256 B, 1 KiB and 64 KiB pages,
  `three_peers*.rs`, and `mixed_formats.rs` (both formats in one
  process). `cargo test -p rlmdb-interop` runs all of it; `--test
  c10_writes_rust_reads` one pair; `INTEROP_LOOP_SECS=20` a stress run.
  A new scenario goes into `interop/src/scenarios` and runs in both
  formats: take the reference peer, the tools and the page size from the
  `Pair`, never `Peer::C` or the 0.9 tools directly.
* The CI runs the whole suite on Linux x86-64 and the platform suite
  (`RLMDB_TEST_SCOPE=platform`) on macOS, Windows and Linux arm64; the
  nightly runs the whole suite everywhere (`interop/README.md`, "Scope").
  A new test runs in both scopes unless it only repeats on another
  platform what Linux checks (another page size, more of the generic
  matrix, mutation or model runs): then it returns early with a SKIPPED
  note in the platform suite (`interop::scope`,
  `common::full_scope_only`), and a new scenario that depends on the
  platform goes into `scenarios::PLATFORM_SCENARIOS`.
* Run `scripts/check-all.sh` (fmt, clippy, doc, host tests, then the same
  tests in a Linux container via `scripts/linux-test.sh`) before declaring
  work done. `--no-linux` skips the container only for quick iteration.
  CI (`.github/workflows/`) runs the host steps on Linux x86-64, Linux
  arm64 and macOS, and the ignored tests nightly; it is not a substitute
  for running the checks locally. `windows.yml` runs clippy, rustdoc and
  the tests on Windows with both toolchains (MSVC, MinGW-w64);
  `scripts/windows-test.sh` runs a command in a copy of the checkout on a
  Windows machine over ssh.
* Every test and benchmark must run on a machine with 2 GB of RAM, the
  build, the C oracles, parallel test binaries and peers and any tmpfs
  for scratch included. `scripts/linux-test.sh` holds its container to
  that by default (`LINUX_TEST_MEMORY=2g`, no swap; its 1.5 GiB tmpfs at
  `/tmp` counts against it) and prints the peak at the end; needing more
  is a regression. Prefer making a test lighter; where the size is the
  point (an interop environment whose file may grow to a map of 1 GiB),
  it takes a `LargePermit` (one per process at a time).
* Long tests are `#[ignore]`d; run `cargo test -p rlmdb --release --
  --ignored` after changes to the write path or spilling.
* New behaviour needs a test on the appropriate level: unit tests next to
  format parsing (hand-built pages), `rlmdb/tests/` for API behaviour,
  differential tests for anything that affects what is written, `interop/`
  scenarios for anything cross-process.
* Test scratch directories must be per process (tests run in parallel and
  in shared containers). Make them with the helpers of the crate's tests
  (`common::tempdir`, `Builder::scratch`, `TestEnv::new`, …), which honour
  `$RLMDB_TEST_SCRATCH` (CI points it at a file system in memory). The
  target of a copy (`copy_to_path`, `mdb_copy`, a peer's `copy`) goes to
  the disk helper instead (`common::disk_tempdir`,
  `Builder::disk_scratch`, `TestEnv::derived_on_disk`) unless any copy
  error is accepted (the robustness tests): copies are written with
  `O_DIRECT` on Linux, whose alignment only a disk file system checks.
  Nothing else the tests check differs on tmpfs.

## 9. Not implemented yet, and what that means

* Windows (64-bit, MSVC and MinGW) is implemented in
  `rlmdb/src/os/windows.rs`, whose module documentation lists every
  Windows decision. Keep every OS specific behind `rlmdb/src/os/`.
* Possible later: `MDB_WRITEMAP`'s writable map (the flag is accepted
  with C's API limits, refused for writing on OpenBSD; dirty pages are
  owned buffers today; keep the page backing replaceable), the typed Rust
  API, LMDB 1.0
  features beyond its data file (checksums, encryption;
  `DUAL_FORMAT.md` §7) and the format-converting
  copy (`convert-copy`, §4.3). The C API and the tools handle both formats
  (stage 4), the interop matrix covers both (stage 5); new environments
  are 0.9 unless 1.0 is asked for, and the
  environment variable `RLMDB_DEFAULT_FORMAT` that can ask for it is
  experimental (keep it, `RLMDB_FEATURES` and `RLMDB_IGNORE_ENV` out of
  test environments: the suites assume they are unset, and
  `scripts/check-all.sh` unsets them).
* Not unless needed and feasible: 32-bit targets (`compile_error!` today;
  the format depends on `sizeof(size_t)`) and `MDB_VL32`.
* `MDB_FIXEDMAP`, `MDB_MAPASYNC` and `MDB_NOMEMINIT` are accepted and
  ignored because they are per-process choices.

Do not design any of these away; do not implement them as a side effect
of another task either.

## 10. Workflow

* Several agents may edit one checkout. Stay inside the crate(s) your task
  names; stage explicit paths and never `git commit -a` or `git add -A`.
* Do not commit test scratch files, benchmark environments or profiles.
* Documentation that records results must not contain local paths,
  usernames or hostnames.
