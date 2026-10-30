# Working on rlmdb

`rust/` holds `rlmdb`, a rewrite of LMDB 0.9 in Rust that is binary
compatible with the C library in `../libraries/liblmdb` (the reference is
whatever that tree holds; 0.9.36 at the time of writing), and that also
reads and writes the data file of LMDB 1.0 (the reference is the pinned
`mdb.RE/1.0` commit of `interop/build.rs`). This file lists the rules that
every change must respect. The details live in:

* `DESIGN.md`: goals, the on-disk and lock-file format, architecture.
* `COMPATIBILITY.md`: the compatibility contract and which test checks it.
* `KNOWN_C_BUGS.md`: C bugs not ported (and the few ported on purpose).
* `rlmdb-capi/README.md`: the C API and its documented deviations.
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
  (`KNOWN_C_BUGS.md` #7). Shared state decides what to port: free-list
  format, lock-file fields and reader slots match C even where C is wrong
  (#21, #22).
* Layout exactness is a test oracle, not a product requirement. Today a
  Rust writer leaves exactly the file a C writer would (same pages in the
  same places, same free list, same spill decisions, same compacting copy)
  and the tests check it down to page numbers
  (`rlmdb/tests/write_differential.rs`, `rlmdb-capi/tests/mtest.rs`,
  `interop/`). Keep it that way by default: it catches write-path bugs
  cheaply. An optimisation may diverge deliberately (which free page is
  reused, where a split lands, when to spill) if it keeps the invariants
  above, is validated by the logical-equivalence tests (dumps, stat,
  cursor positions, C peers reading and writing the result), moves only
  the affected checks from "same layout" to "equivalent", and is recorded
  in a short ledger. Never diverge as a side effect of another change.
* Never recompute what C reads from the file. Example: macOS semaphore
  names are stored in the lock file with uninitialised padding, so they are
  read, not derived; in lock-file version 2 they are derived from the hash
  stored in the file (`mtb_mutexid`), which is read, not recomputed.
* Platform ABI is part of the contract: `sizeof(pthread_mutex_t)` and the
  reader-table offset differ between Linux and macOS, and page sizes differ
  (16 KiB on macOS, 4 KiB on Linux). Test on both before claiming done.
* LMDB 1.0's data file (`MDB_DATA_VERSION 3`, `DUAL_FORMAT.md`) is a
  supported format with the same rules: C 1.0 is its specification and its
  oracle (`interop`: `mdb_*10`, `cpeer10`, `writeops10`), and a Rust writer
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
  `rlmdb/tests/robust_*.rs` and the mutation tests check this.
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

* `rlmdb-capi` builds `liblmdb.a` / `liblmdb.so` / `liblmdb.dylib` with
  the same `mdb_*` symbols, the unmodified upstream `include/lmdb.h`, the
  same flag values and error codes. Unmodified `mtest*.c` and the C tools
  must behave identically against it.
* Argument validation follows `mdb.c`: same checks, same order, same
  return codes. Misuse that crashes C (NULL handles, use after free)
  returns `EINVAL` instead; keep that list in `rlmdb-capi/README.md`
  current.
* The layer is thin: no database logic in `rlmdb-capi`. Lifetimes are
  erased in one place (`erased.rs`); handle ownership is explicit.
* The tools in `rlmdb-tools` match the C tools' options, output, messages
  and exit codes; the C tools are their test oracle.

## 6. Performance

* Target: comparable to C. No allocations in the hot read path, binary
  search on pages, fixed-size cursor stacks, zero-copy values into the map.
* Measure before and after with the interleaved A/B of `bench`
  (`rlmdb-bench --baseline <old binary> --rounds 3 --reps 3`); the machine
  may be noisy, so never compare two separate runs. Keep a change only if
  it measurably helps; speculative complexity is reverted.
* Performance changes must not add `unsafe` and must keep the invariants
  of section 1; unless the task says otherwise they are also
  layout-preserving (same file bytes). Record findings in
  `bench/RESULTS.md` or a file under `bench/results/`.

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

* The C library and tools are built from `../libraries/liblmdb` at test
  time; most tests run C and Rust side by side or in lock step.
* The LMDB 1.0 oracle (`mdb_stat10`, `cpeer10`, … in `interop`) is built
  from sources that `interop/build.rs` extracts with `git show` at a pinned
  `mdb.RE/1.0` commit, which must be fetched (`interop/README.md`).
* Run `scripts/check-all.sh` (fmt, clippy, doc, host tests, then the same
  tests in a Linux container via `scripts/linux-test.sh`) before declaring
  work done. `--no-linux` skips the container only for quick iteration.
* Long tests are `#[ignore]`d; run `cargo test -p rlmdb --release --
  --ignored` after changes to the write path or spilling.
* New behaviour needs a test on the appropriate level: unit tests next to
  format parsing (hand-built pages), `rlmdb/tests/` for API behaviour,
  differential tests for anything that affects what is written, `interop/`
  scenarios for anything cross-process.
* Test scratch directories must be per process (tests run in parallel and
  in shared containers).

## 9. Not implemented yet, and what that means

* Planned: Windows. Keep every OS specific behind `rlmdb/src/os/` so a
  `windows.rs` can be added without touching the engine.
* Possible later: `MDB_WRITEMAP` (dirty pages are owned buffers today;
  keep the page backing replaceable), the typed Rust API, LMDB 1.0
  features beyond its data file (`MDB_PREVSNAPSHOT`, two-phase commit,
  checksums, encryption; `DUAL_FORMAT.md` §7). The 1.0 lock file, the
  format selection of the C API and the tools follow as stages 3 and 4 of
  `DUAL_FORMAT.md`.
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
