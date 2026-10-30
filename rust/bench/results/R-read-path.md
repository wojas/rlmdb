# Task R: read-path per-operation and per-level overhead

Cheaper cursor stacks and page lookups on the read path of `rlmdb`, plus
the local wins the profiles of the read workloads showed. All changes are
in `rust/rlmdb` (`src/core/`, `src/format/`, `src/cmp.rs`), read paths
only, behaviour-preserving (same results, same errors on corrupt input,
nothing on disk changes), no new `unsafe`. Measured on macOS against the
unchanged library (`71e228d`), interleaved.

**Summary** (interleaved A/B, new/base of the medians): random gets
0.91, gets of 4 KiB / 64 KiB values 0.92 / 0.78,
8 concurrent readers 0.86, `GET_BOTH` 0.87, `NEXT_DUP`
0.97, scans 0.97 (forward) and 0.88 (backward), the readers of `mixed`
0.87. Write workloads are unchanged within noise (see `put_64k` below).
macOS only; deeper trees were covered by scratch loops on environments
with 4 KiB pages.

## Setup

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD, environments in the temp directory; 16 KiB pages.
* rustc 1.93.1 (Homebrew), the workspace's release profile.
* The machine was shared with other jobs throughout, so only interleaved
  comparisons are reported.

## Changes

1. **Short cursor stacks for `mdb_get`** (`core/cursor.rs` `Cursor`,
   `core/txn.rs` `TxnCore::get`, `get_with_stack`, `get_deep`).
   `Cursor` has a const parameter `N` for the capacity of its page stack,
   defaulting to C's `CURSOR_STACK` (32), so `Cursor<'t>` and
   `Cursor::new` mean what they did. `mdb_get` builds its temporary
   cursor (and the sub-cursor of a `DUPSORT` database) with 8 levels when
   the tree's `md_depth` is at most 8: 8 slots to fill instead of 32. A
   deeper tree than its record says (a corrupt one), or a sub-DB of
   duplicates deeper than 8 levels, makes that search fail with
   `MDB_CURSOR_FULL`, and the lookup is repeated with full stacks; a tree
   whose record says it is deeper goes to full stacks directly. The search
   only reads the snapshot, so the repeated lookup gives exactly the old
   result, including `MDB_CURSOR_FULL` beyond 32 levels. The movement
   functions (`cursor_set`, `cursor_first`, `cursor_next`, …,
   `xcursor_init1`, `new_xcursor`) are generic over `N`; the write path
   only ever names `Cursor<'t>` and keeps using full stacks.
   Why the fill mattered more than its size suggests: the 32 two-word
   stores of the fill sit in the store queue behind the previous lookup's
   cache misses, and in `get_64k` 20 % of the samples landed on the first
   instruction after them.
2. **A read-only page lookup** (`core/txn.rs` `TxnCore::page`,
   `mapped_page`, `written_or_mapped_page`, `overflow_data`,
   `TxnCore::new_read_only`). `TxnCore` knows whether it belongs to a read
   transaction (`read_only`, set by `RoTxn::begin` and
   `InactiveTxn::renew`) and caches the page size. For a read transaction
   `page` goes straight to the map (`mapped_page`, inlined), skipping the
   spill set, dirty list and parent layers, which are always empty there;
   write transactions take the old path unchanged. `Cursor::push` indexes
   with the level it has just checked against the capacity, so the two
   stores need no further bounds checks.
3. **memcmp order compared inline** (`cmp.rs` `cmp_memn`). The default
   key order was `a.cmp(b)`, a call into the C library's `memcmp` through
   the dynamic-linker stub for every probe of every binary search, and the
   search loop spilled its registers around the call. Operands whose
   common length is at most 32 bytes are now compared in the function,
   8 bytes at a time as big-endian integers (which order exactly like
   `memcmp`), then byte by byte, then by length; longer ones still go to
   `memcmp`, which is faster on long common prefixes. In a
   micro-benchmark of the comparison alone (not inlined, so without the
   spill savings) it took 2.6-3.9 ns where `memcmp` took 3.4-4.4 ns for
   8 to 32-byte operands, except 32-byte operands sharing 24 bytes
   (4.41 against 4.32 ns); longer operands take the same `memcmp` call as
   before. A new unit test checks the order against slice order for every
   pair of lengths up to 67 bytes and every shared prefix.
4. **`mdb_node_read` without a call** (`core/cursor.rs` `node_read`,
   `overflow_read`). The inline-data case is inlined into its callers; the
   overflow case stays out of line. It returned its result through memory
   once per scanned item.
5. **The duplicates branch of `read_position` out of line**
   (`core/cursor.rs` `enter_dups`). `read_position` (the tail of every
   first/last/next/prev) set up a frame for the sub-cursor calls even for
   items without duplicates.
6. **Page header fields from a fixed-size view** (`format/page.rs`
   `PageRef::header`, `hdr16`, `pgno`, `overflow_pages`). Every header
   field read bounds-checked its own offset against the page slice and
   branched; reading from a `&[u8; 16]` leaves one length check that
   compiles to a conditional select.
7. **The DBI check of every `get` inlined** (`core/dbi.rs` `dbi_entry`,
   `first_use`, `fresh_dbi`). The first use of a slot in a transaction
   moved to a cold function; the common path (copy the entry, check
   validity and sequence) is inlined, which saves copying the 96-byte
   entry through memory twice per call.

## The write path

Nothing in the write-path files changed. What they use of the read cursor
stays source-compatible, because `Cursor<'t>` still names the full-stack
cursor and `Cursor::new` still builds one:

* `core/wcursor.rs`: `Cursor::new` and `Cursor::load` (`build_read`,
  `load_read`, `RwTxn::read_op`, whose closure takes `&mut Cursor<'t>`),
  `store_back` (`flags`, `depth`, `top_level`, `ki_array`, `subpage`,
  `page_at(i).pgno()`), `write_back` (`db`, `cmp`), `empty_page`;
* `core/freelist.rs` `reclaim_run`: `Cursor::new` with `cursor_first`,
  `cursor_next`, `cursor_set` on it (they infer `N` = 32 from it);
* `core/cursor_del.rs` `page_search_lowest`: `top_page`, `set_ki_top`,
  `push`, `page_search_root`; `cursor_put.rs`: `cursor_set`,
  `cursor_last` inside `read_op`;
* `core/txn_rw.rs`: `RoCursor::from_parts`/`parts`, `read_cursors`,
  `current_has_dups`;
* `TxnCore::page` and `overflow_data` everywhere: write transactions
  (`read_only` false) take exactly the old lookup.

## A/B

Rust after against Rust before, interleaved: `cargo run --release -p
rlmdb-bench -- --baseline <build of 71e228d> --rounds 3 --reps 3 --only
read,dupsort,overflow,mixed` (n = 1 000 000, 3 rounds of 3 timed reps
after a warm-up, the side going first alternating; ns/op median (min);
the median ratio is the bench's, the min ratio from the rounded mins):

| workload | ops/rep | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|---:|
| rand_get | 1000000 | 934 (850) | 846 (742) | **0.91** | 0.87 |
| scan_fwd | 1000000 | 17.4 (16.8) | 16.9 (16.3) | **0.97** | 0.97 |
| scan_bwd | 1000000 | 16.5 (15.6) | 14.5 (14.4) | **0.88** | 0.92 |
| readers8_get | 8000000 | 125 (112) | 108 (100) | **0.86** | 0.89 |
| dup_put | 1000000 | 1296 (1254) | 1258 (1212) | **0.97** | 0.97 |
| dup_get_both | 1000000 | 902 (829) | 788 (737) | **0.87** | 0.89 |
| dup_next_dup | 1000000 | 92.0 (89.0) | 89.1 (85.4) | **0.97** | 0.96 |
| put_4k | 50000 | 9963 (9089) | 9481 (9118) | **0.95** | 1.00 |
| get_4k | 1000000 | 565 (538) | 519 (436) | **0.92** | 0.81 |
| put_64k | 5000 | 27117 (23945) | 28461 (25174) | **1.05** | 1.05 |
| get_64k | 1000000 | 275 (264) | 213 (201) | **0.78** | 0.76 |
| mixed_writer | 50000 | 6271 (5434) | 5716 (5609) | **0.91** | 1.03 |
| mixed_readers | 62144512 | 5.0 (4.8) | 4.4 (4.3) | **0.87** | 0.90 |

The library code of the measured build is instruction for instruction
the committed one, apart from constants (compared after the last comment
edits).

`put_64k` (5000 puts of 64 KiB values) and the writer of `mixed` are
write workloads; the writes share only the comparator, the page header
reads and the dispatch in `TxnCore::page` with this work. `put_64k`
swings by a third between samples here (24-35 µs per put on both sides).
A second interleaved run of the `overflow` group gave 1.14 (median) and
0.92 (min) for it, and a scratch loop of just that workload, 10 rounds
interleaved, 0.97 (26.8 against 26.0 µs per put): no regression. The
same second run had `get_4k` at 0.77 and `get_64k` at 0.75.

## Step by step

Scratch loops outside the repository (the bench's data generator; one
read transaction per pass, 3-second runs, 4-8 interleaved rounds, medians;
16 KiB pages unless marked 4 KiB, where the environment was created with
4 KiB pages), each change against the state before it:

| change | workload | new/base (median) |
|---|---|---:|
| 1 short `get` stacks | get_64k / get_64k 4 KiB / rand_get 4 KiB | 0.93 / 0.93 / 0.96 |
| 2 read-only page lookup, `push` | rand_get 4 KiB / get_64k 4 KiB | 0.98 / 0.96 |
| 3 inline `cmp_memn` | get_64k 4 KiB / rand_get 4 KiB | 0.84 / 0.90 |
| 3 inline `cmp_memn` | dup_get_both / dup_next_dup | 0.90 / 0.92 |
| 4 `node_read` inlined | scan_fwd / scan_bwd | 0.98 / 0.93 |
| 5 `enter_dups` out of line | scan_fwd / scan_bwd / dup_next_dup | 1.00 / 0.95 / 1.01 |
| 6 header view | scan_fwd / scan_bwd | 0.93 / 0.97 |
| 6 header view | get_64k / get_64k 4 KiB / dup_get_both / rand_get 4 KiB | 0.95 / 1.00 / 0.97 / 1.01 |
| 7 `dbi_entry` inlined | get_64k / get_64k 4 KiB / get_4k / rand_get 4 KiB | 0.96 / 0.95 / 0.95 / 0.95 |

Single steps of 2-5 % are at the edge of what this machine resolves
(repeated runs of the same pair moved by about 3 %); they are kept where
the direction held across workloads and the code got no harder to read.

## Tried and reverted

Each measured like the steps above and dropped because it did not
measurably help:

* Zero-initialised `Option<PageRef>` stack slots instead of a short stack
  (get_64k 0.97: half the bytes, but still one store per slot).
* `#[inline]` on `node_search` and `page_search` (0.99-1.00 on gets and
  `GET_BOTH`).
* A branch-free update of the binary search bounds (same probe order, so
  same results on unsorted pages): LLVM turned it back into branches;
  0.98-1.05, noise.
* Comparing the first 8 bytes inline also for operands longer than 32
  bytes (the 100-byte duplicates of `dup_get_both` differ there):
  dup_get_both 1.01, dup_next_dup 0.97, and a micro-benchmark showed it
  1-2 ns slower than `memcmp` on long common prefixes.
* Moving the `DUPSORT` branch of `cursor_next` out of line: scans 1.00 /
  0.98, dup_next_dup 1.06 (worse).
* `#[inline]` on `read_position`: scans 1.01 / 1.03, dup_next_dup 0.97.
* A direct memcmp-order path in `Cursor::cmp_at` (the first/last-key
  checks of `cursor_set` on a positioned cursor): 1.01 / 1.00.

## Profiles

`samply record --save-only --unstable-presymbolicate` on the scratch
loops, aggregated per (inlined) function and per instruction with
`atos -i`. Before the changes:

* `get_64k` (5000 keys, a small cached tree; 16 KiB and 4 KiB pages
  alike): 20 % of the samples on the first instruction after the 32-slot
  stack fill of `TxnCore::get` (about 40 stores per call: they wait in the
  store queue behind the previous lookup's cache misses, so the next
  lookup cannot start), 33-38 % in the binary search on the branch after
  the comparison (`memcmp` called through the dyld stub, with the loop's
  registers spilled around the call), 10 % reading the header of the
  first overflow page (the cache miss on the value, which C takes in the
  caller instead), 3-4 % in `fresh_dbi`/`dbi_entry`. Shrinking the stack
  to 8 slots as an experiment (no other change) gave 0.96.
* `rand_get`, `dup_get_both`, `dup_next_dup`: 50-65 % in
  `node_search`/`bsearch`, a quarter to half of that in `memcmp`.
* scans: memory-bound (a leaf's nodes are read in key order but stored
  in insertion order, two cache lines per 100-byte item); per item,
  `cursor_next` saved 12 registers, `read_position` 8 more, `node_read`
  returned through memory, and each page header field had its own
  bounds check and branch.

After the changes the profile of `get_64k` is the binary search
(unpredictable probe direction: branch mispredictions) and the cache
misses on the values; the stack fill and `memcmp` are gone from the top.
The per-level page lookup of a read transaction is now a few compares and
`DataMap::slice` inlined into the descent.

## Behaviour

* New test `tests/handbuilt.rs` `deep_trees_and_short_get_stacks`:
  hand-built trees of 1, 8, 9, 12, 32 and 33 levels with truthful and
  lying `md_depth`, and a `DUPSORT` key whose duplicates are a sub-DB of
  9, 32 and 33 levels: `get` returns what a cursor's `MDB_SET` returns,
  and `MDB_CURSOR_FULL` beyond 32 levels. It passes unchanged on the base
  commit.
* Differential mutation run (scratch harness, not committed): valid
  environments (plain keys with overflow values, `DUPSORT` with sub-pages
  and sub-DBs, `DUPFIXED`, `INTEGERKEY`) with 16 KiB and with 4 KiB
  pages, 150 mutated copies each (header bytes, pointers, `mp_lower`,
  page flags, page numbers, random bytes and bit flips), read by the base
  and the new library with the same sequence of operations (gets, full
  walks with every move operation, `SET`/`SET_RANGE`/`GET_BOTH`/
  `GET_BOTH_RANGE`, dup moves, `count`, `GET_MULTIPLE`, random cursor
  operations, `stat`), every result and error code printed: 1200 cases,
  1037 of them with at least one error result, **no difference** in the
  output.

## Tests

All from `rust/`, on the final tree:

| platform | command | outcome |
|---|---|---|
| macOS | `cargo fmt --all -- --check` | clean |
| macOS | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| macOS | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| macOS | `cargo test --workspace --no-fail-fast` | 56 test binaries and doc-test runs: 614 passed, 0 failed, 7 ignored |
| Linux | `scripts/linux-test.sh test -p rlmdb -p rlmdb-interop` (Docker through Lima, 4 KiB pages) | 35: 533 passed, 0 failed, 7 ignored |

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 63 passed; 0 failed; 1 ignored; finished in 1.95s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 4.20s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; finished in 5.65s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 5.54s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.08s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 30.17s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 0.22s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 0.06s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 12.94s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 13 passed; 0 failed; 0 ignored; finished in 0.01s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; finished in 2.69s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.16s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; finished in 18.83s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 0.03s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.09s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    lmdb src/lib.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 0.00s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.86s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 3.78s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; finished in 11.89s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 4.96s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 10.69s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.02s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 5.91s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; finished in 6.88s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 7.60s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 7.46s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.83s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.50s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 9.00s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 15.41s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.52s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 7.59s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 9.00s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.39s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 8.78s
    dump tests/dump.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 2.25s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 4.17s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 1.34s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 1.90s
    rlmdb doc-tests: ok. 3 passed; 0 failed; 0 ignored; finished in 0.67s
    interop doc-tests: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
    rlmdb_tools doc-tests: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s

Linux, scripts/linux-test.sh test -p rlmdb -p rlmdb-interop:
    rlmdb src/lib.rs: ok. 61 passed; 0 failed; 1 ignored; finished in 2.05s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 0.63s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; finished in 6.23s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 4.47s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.14s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 30.06s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 0.09s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 0.02s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 12.02s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 13 passed; 0 failed; 0 ignored; finished in 0.36s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; finished in 2.33s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.22s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; finished in 17.24s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 0.01s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.05s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; finished in 3.71s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 4.58s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 4.81s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 6.29s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 5.77s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 6.80s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 14.85s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 6.22s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 5.03s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 6.97s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.31s
    rlmdb doc-tests: ok. 3 passed; 0 failed; 0 ignored; finished in 0.37s
    interop doc-tests: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
```

</details>
