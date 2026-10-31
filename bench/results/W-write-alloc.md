# Task W: write-path allocations

Removes the allocation overhead of the write path listed in
[`RESULTS.md`](../RESULTS.md) ("No page-buffer recycling", "`DUPSORT` puts
allocate", "`RwTxn::put` re-validates the DBI"). All changes are in
`rlmdb` (`src/core/`), based on `9f17f7e`, behaviour-preserving (same
results, same file bytes; the write differential and interop tests compare
against C) and without new `unsafe`. The public API is unchanged.

**Summary** (macOS, interleaved A/B against the base build, median
ns/op): random overwrites **-30 %**, random deletes and 4 KiB puts
**-24 %**, random puts **-22 %**, `DUPSORT` puts **-12 %**, sequential
appends **-7 %**, 64 KiB puts and one-put transactions **-4 %**; reads
and the mixed reader/writer workload unchanged within noise.

## What changed

1. **Page-buffer pool** (C's `me_dpages`).
   * `core/dirty.rs`: new `PagePool` (spare single-page buffers). The
     dirty list (`DirtyPages`) now creates and drops every page buffer
     itself: `alloc_zeroed` (new pages: a recycled buffer is zero-filled,
     so the bytes are what the old `calloc`ed buffer had) and `alloc_copy`
     (a page about to be dirtied: copied over completely, no zeroing);
     `insert` (on a duplicate), `remove`, `clear`, `retain` and `merge`
     (the parent's replaced pages) recycle the buffers they drop.
     `alloc_pages` is gone. Overflow runs are not pooled (as in C's
     `mdb_dpage_free`).
   * Policy: between transactions the pool keeps as many buffers as the
     last write transaction asked for (`PagePool::trim`, a counter in
     `take`), at most 256 MiB (`POOL_MAX_BYTES`). C keeps every buffer
     until the environment closes; here memory pinned by one big
     transaction goes back to the allocator when the next, smaller one
     ends.
   * Where it lives: `core/env.rs` `WriterState::pages` (under the
     in-process writer lock) between transactions; `RwTxn::begin` moves it
     into the transaction's dirty list, `RwTxn::end` trims it and moves it
     back (`core/txn_rw.rs`). A nested transaction takes it from its
     parent and gives it back before the parent's lists are merged or
     restored (`core/nested.rs` `begin_nested_txn`, `end_nested`). The
     hand-offs swap the list (O(1)); the first version pushed buffers one
     by one and made `txn_nosync` 4 % slower.
   * Callers: `core/freelist.rs` `page_alloc_init` (new and touched
     pages), `ovpage_free` and the loose-page path of `freelist_save`
     (removals); `core/spill.rs` `page_unspill` (now a single copy instead
     of zero fill plus copy); `core/wcursor.rs` `copy_ancestor_page`
     (nested `mdb_page_touch`); `core/commit.rs` `write_pages` (written
     pages are recycled through `retain`).
2. **Scratch buffers for puts and splits** (C's `me_pbuf` and stack
   buffers).
   * `core/txn_rw.rs`: `WriteState::scratch`, a few spare `Vec<u8>`s,
     lent out by `RwTxn::take_scratch` and returned by `give_scratch`
     (the caller owns the buffer meanwhile, so it cannot alias a page that
     is being read). A nested transaction borrows them.
   * `core/cursor_put.rs`: `dup_prepare` (now a wrapper around
     `dup_prepare0` that lends the buffer for the old node data and takes
     it back on every path), the fake and the grown sub-page and the sub-DB
     record are scratch buffers; `put_item` and `put_sub0` return them
     after use.
   * `core/split.rs`: `page_split` lends `page_split0` two buffers
     (`SplitBufs`: the copy of the page being split, and the temporary
     left half, zero-filled as before since node padding bytes reach the
     page). The page copy is no longer made for `MDB_APPEND` splits, which
     never read it. The separator key and the `P_LEAF2` keys moved by
     `split_leaf2` are borrowed from that copy instead of copied into new
     `Vec`s.
3. **The DBI is resolved once per `put`/`del`** (`core/txn_rw.rs`
   `mdb_put`, `del`, `user_dbi`; `core/dbi.rs` `TxnCore::refreshed`):
   `mdb_put` validated the handle three times (`dbi_entry`, then
   `user_dbi` = `dbi_entry` + `fresh_dbi` = `dbi_entry` again) and
   borrowed the DBI table once more for the `MDB_RESERVE` check; `del`
   likewise. Now one `dbi_entry`, then the stale refresh on that entry
   (same checks, same order and errors as C's `mdb_put`/`mdb_del`).
   `check_reserve` takes the flags it checks.

## A/B: new against base

`rlmdb-bench --baseline <base build> --rounds 3 --reps 3 --sync-commits
200 --only seq_put,rand_put,overwrite,dupsort,overflow,small_txn,mixed,delete`
(the base build is the bench at `9f17f7e`; both sides alternate per group;
n = 1 000 000). ns/op: median (min) over 9 samples per side.

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, 16 KiB pages, APFS on
  the internal SSD; rustc 1.93.1 (Homebrew), Apple clang 17.0.0.
* The machine was shared with other jobs (load average 6-9 throughout).

| workload | ops/rep | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|---:|
| seq_put_append | 1000000 | 221 (212) | 205 (198) | **0.93** | 0.93 |
| rand_put | 1000000 | 3306 (3083) | 2589 (2474) | **0.78** | 0.80 |
| rand_overwrite | 1000000 | 5257 (4906) | 3681 (3571) | **0.70** | 0.73 |
| dup_put | 1000000 | 1263 (1256) | 1109 (1072) | **0.88** | 0.85 |
| dup_get_both | 1000000 | 889 (830) | 873 (807) | **0.98** | 0.97 |
| dup_next_dup | 1000000 | 100 (87.8) | 99.2 (93.8) | **0.99** | 1.07 |
| put_4k | 50000 | 9473 (9126) | 7220 (6723) | **0.76** | 0.74 |
| get_4k | 1000000 | 640 (610) | 654 (515) | **1.02** | 0.84 |
| put_64k | 5000 | 27151 (24505) | 25964 (23261) | **0.96** | 0.95 |
| get_64k | 1000000 | 290 (262) | 279 (263) | **0.96** | 1.00 |
| txn_nosync | 50000 | 8431 (8077) | 8392 (8041) | **1.00** | 1.00 |
| txn_sync | 200 | 10711885 (9925669) | 9837193 (9699422) | **0.92** | 0.98 |
| mixed_writer | 50000 | 5913 (5457) | 6096 (5402) | **1.03** | 0.99 |
| mixed_readers | 58613760 | 5.1 (4.8) | 5.1 (4.8) | **1.00** | 1.00 |
| del_rand | 1000000 | 4654 (4417) | 3549 (3453) | **0.76** | 0.78 |

The read workloads of the groups (`dup_get_both`, `dup_next_dup`,
`get_4k`, `get_64k`, `mixed_readers`) do not run any of the changed
paths; their ratios show the noise level. `txn_sync` is
`F_FULLFSYNC`-bound. `txn_nosync` and `mixed_writer` came out at 0.97 /
0.97 in an earlier full run (a build that also had the reverted change
below, which neither touches) and at 1.00 / 1.03 here, so they were run
again with 5 rounds (`--rounds 5 --reps 3 --sync-commits 50 --only
small_txn,mixed`):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| txn_nosync | 8483 (7954) | 8158 (7745) | **0.96** | 0.97 |
| txn_sync (50 commits) | 9820017 (9614479) | 9990104 (9420555) | **1.02** | 0.98 |
| mixed_writer | 5757 (5484) | 5646 (5503) | **0.98** | 1.00 |
| mixed_readers | 4.9 (4.7) | 4.9 (4.8) | **1.00** | 1.02 |

### Per change

Interleaved A/B between intermediate builds (3 rounds x 3 reps each;
median, min in parentheses):

| change | workload | new/base |
|---|---|---:|
| 1, pool capped at 32 MiB (first hand-off), against base | rand_put / put_4k / put_64k | 0.83 (0.86) / 0.80 (0.78) / 0.96 (0.92) |
| 1, 512 MiB cap against 32 MiB cap | rand_put / del_rand | 0.92 (0.92) / 0.88 (0.87) |
| 2, scratch buffers | dup_put / seq_put_append | 0.95 (0.92) / 0.97 (0.98) |
| 3 + O(1) pool hand-off | seq_put_append / txn_nosync | 0.94 (0.94) / 0.96 (1.01) |

The first two rows were 2 rounds x 3 reps. Counted with a small program
on the same shapes (1M random keys, 100-byte values, 10 000 operations
per transaction, 16 KiB pages), a `rand_put` transaction dirties 4 756
pages (median; up to 6 664) and a `del_rand` transaction 6 568 (up to
6 825, 105 MiB), so a fixed 32 MiB pool (2 048 pages) recycled only a
third to a half of them; hence the adaptive policy with a larger cap.

### Tried and not kept

* `put_position` (`cursor_put.rs`) builds an `MdbError::NotFound` for
  every `MDB_APPEND` put and matches it away again (1 % of
  `seq_put_append` in `drop_in_place<Error>`). Returning the position
  directly measured 0.99 (min 0.98) on `seq_put_append`: within noise,
  reverted.
* A fixed 32 MiB pool cap (see above): replaced by the adaptive policy.
* Handing the pool between environment and transaction by pushing each
  buffer (`txn_nosync` 1.04): replaced by a swap.

## Profiles

`sample` (macOS) on the bench's worker process (`rlmdb-bench --worker
<group> <dir> <n> <reps> <sync-commits>`), self time aggregated per
function within the subtrees of `RwTxn::put`/`del`/`commit` of the timed
loop. Share of those samples in allocator frames (`malloc`, `free`, and
the `madvise`/`mach_vm_deallocate` that `free` of 16 KiB blocks does):

| workload | allocator before | after | `memmove` before | after |
|---|---:|---:|---:|---:|
| rand_put | 9.1 % | 0.5 % | 19.2 % | 12.5 % |
| del_rand (the delete loop and its commits) | 8.6 % | 0.8 % | 24.6 % | 17.6 % |
| dup_put | 15.3 % | 0.3 % | 9.6 % | 11.4 % |

* `rand_put` before: `free` 6 % (almost all of it the written pages at
  the end of `write_pages`: `madvise` and `mach_vm_deallocate` inside
  `free_small`), `malloc` 3 % (mostly the buffers of touched pages);
  `memmove` 19 %. After: `memmove` 12.5 %: copying a clean page into a
  recycled buffer that is already mapped is cheaper than into fresh
  memory (the page faults showed up as copy time).
* `dup_put` before: `malloc`/`free` of the `Vec`s of `dup_prepare` (the
  copy of the node data, the new sub-page) and of the node data dropped
  at the end of `put_at`, plus the two 16 KiB `Vec`s of `page_split0`.
  After: none left on the put path.
* `del_rand` before: `free` 6 % (commit), `malloc` 2.5 % (touched
  pages). After: `memmove` 17.6 % (from 24.6 %), for the same reason as
  above.
* What remains in these workloads is the B-tree search (`node_search`,
  15-35 % self), the whole-page copy when a clean page is touched
  (`memmove`, 11-18 %; kept whole so unused bytes stay as before) and the
  commit I/O; in `seq_put_append` the page lookups by number and the
  read-cursor rebuilds of `read_op` (see below).
* `txn_nosync` (after changes 1 and 3): `pwrite` 59 %, the page copy on
  touch 11 %; about 5 % is `malloc`/`free` of small per-transaction
  `Vec`s (dirty list, free list, cursor arena, the lists of
  `page_flush`/`write_pages`, `Idl::to_bytes`), spread over a dozen call
  sites, so not changed here.

## For the stable dirty-page handles task

Observed while profiling `seq_put_append` (after these changes; shares of
the put/commit samples), all due to write cursors holding page numbers:

* `wcursor.rs:760` (`page_touch`): `self.core.page(mp)?.flags()` resolves
  every level's page (`TxnCore::page` -> `locate`: spill set, dirty list,
  ancestors) on each put, only to see it is already dirty; `TxnCore::page`
  is 13 % inclusive.
* `wcursor.rs:689` (`read_op`) with `load_read` (`:416`) and `write_back`
  (`:468`): two `Cursor`s (32-slot stacks) are built and every page
  re-resolved per positioning; most of `put_position`'s 10.6 % self time
  plus `write_back` 5 %, `read_position` 3.6 %, `load_read` 2 %.
* `wcursor.rs:478` (`db_and_cmp`): `db_of`/`cmp_of` borrow the DBI
  table's `RefCell` and copy the record about five times per put (4.9 %
  self); a cursor could carry its record (C's `mc_db`).
* `split.rs:152`: the split copies the whole source page (C reads `mp`
  in place and only copies the left half back); with stable buffers the
  source could stay where it is. `split.rs:177` allocates `copy_ptrs` per
  split.
* `cursor_put.rs:725` (`dup_prepare0`): the node data is copied (now into
  a scratch buffer) because the leaf cannot stay borrowed across the
  `&mut self` calls that follow; C reads it in place.
* `DirtyPages` keeps its surface small for that work: buffers are only
  created through `alloc_zeroed`/`alloc_copy` + `insert` and only dropped
  through `remove`/`clear`/`retain`/`merge`, so a slab with a free list
  can replace `Id2List<Box<[u8]>>` + `PagePool` behind it.

## Tests

From the repository root, on the final tree:

* `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace
  --no-deps`: clean.
* `cargo test --workspace --no-fail-fast` (macOS): 56 test binaries, 615
  passed, 0 failed, 7 ignored.
* Ignored long tests, macOS, release: `cargo test --release -p rlmdb
  --test write_differential diff_spill_full_size -- --ignored` (the
  full-size spill lock-step against C) passed; `write_soak` with
  `SOAK_SECS=30`, with and without `SOAK_SPILL=1`, passed.
* Linux (`scripts/linux-test.sh test -p rlmdb -p rlmdb-interop`, Docker
  via Lima, aarch64, 4 KiB pages): 35 test binaries, 534
  passed, 0 failed, 7 ignored.

One earlier run of `cargo test -p rlmdb` (during development) failed once
in `c_databases::mtest3_dupsort`: C's own cursor helper aborted
(`mdb_page_search: Assertion 'root > 1'`) on the database that C's
`mtest3` wrote, and the read-cursor comparison then differed. That test
writes with C only (`mtest3` seeds `rand()` with the time) and compares C
and Rust read cursors, so no code changed here runs in it; it did not
recur in about 30 later runs. Worth a look separately (a seed-dependent C
crash the harness does not classify).

New unit tests in `core/dirty.rs`: buffers are recycled clean (zeroed or
fully overwritten; overflow runs not kept; a pool without a page size keeps
nothing), `trim` keeps what the last transaction used, `merge` recycles
the replaced pages.

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: test result: ok. 65 passed; 0 failed; 1 ignored; finished in 2.64s
    tests/c_concurrency.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 3.52s
    tests/c_databases.rs: test result: ok. 12 passed; 0 failed; 0 ignored; finished in 5.76s
    tests/core_api_c2.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.01s
    tests/env_copy.rs: test result: ok. 6 passed; 0 failed; 0 ignored; finished in 5.79s
    tests/handbuilt.rs: test result: ok. 2 passed; 0 failed; 0 ignored; finished in 0.08s
    tests/model.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 30.13s
    tests/nested_txn.rs: test result: ok. 9 passed; 0 failed; 0 ignored; finished in 0.21s
    tests/robust_io_errors.rs: test result: ok. 4 passed; 0 failed; 0 ignored; finished in 0.05s
    tests/robust_mutation.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 15.45s
    tests/robust_send_sync.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/rust_env.rs: test result: ok. 13 passed; 0 failed; 0 ignored; finished in 0.03s
    tests/rust_writes.rs: test result: ok. 23 passed; 0 failed; 0 ignored; finished in 2.69s
    tests/write_api.rs: test result: ok. 14 passed; 0 failed; 0 ignored; finished in 0.20s
    tests/write_differential.rs: test result: ok. 18 passed; 0 failed; 2 ignored; finished in 19.57s
    tests/write_interop.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 3.53s
    tests/write_review.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.03s
    tests/write_smoke.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.07s
    tests/write_soak.rs: test result: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    rlmdb_bench src/main.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    lmdb src/lib.rs: test result: ok. 4 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/capi_read.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 2.93s
    tests/capi_write.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 4.03s
    tests/concurrency.rs: test result: ok. 7 passed; 0 failed; 0 ignored; finished in 9.92s
    tests/cross_process.rs: test result: ok. 2 passed; 0 failed; 0 ignored; finished in 5.24s
    tests/mtest.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 11.22s
    tests/robust_capi.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.02s
    tests/tools.rs: test result: ok. 4 passed; 0 failed; 0 ignored; finished in 5.97s
    interop src/lib.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/c_vs_c.rs: test result: ok. 39 passed; 0 failed; 0 ignored; finished in 6.95s
    tests/c_writes_conrust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.51s
    tests/c_writes_rust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.30s
    tests/conrust_vs_conrust.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 8.92s
    tests/conrust_writes_c_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.88s
    tests/conrust_writes_rust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 9.44s
    tests/layout.rs: test result: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    tests/rpeer.rs: test result: ok. 10 passed; 0 failed; 0 ignored; finished in 15.75s
    tests/rust_vs_rust.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 8.69s
    tests/rust_writes_c_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.97s
    tests/rust_writes_conrust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 9.58s
    tests/three_peers.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 2.39s
    rlmdb_tools src/lib.rs: test result: ok. 10 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/copy_drop.rs: test result: ok. 6 passed; 0 failed; 0 ignored; finished in 8.62s
    tests/dump.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 1.81s
    tests/load.rs: test result: ok. 9 passed; 0 failed; 0 ignored; finished in 5.26s
    tests/readers.rs: test result: ok. 2 passed; 0 failed; 0 ignored; finished in 2.18s
    tests/stat.rs: test result: ok. 11 passed; 0 failed; 0 ignored; finished in 1.11s
    doc-tests rlmdb: test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.57s
    doc-tests interop: test result: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
    doc-tests rlmdb_tools: test result: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s

Linux, scripts/linux-test.sh test -p rlmdb -p rlmdb-interop:
    rlmdb src/lib.rs: test result: ok. 63 passed; 0 failed; 1 ignored; finished in 2.33s
    tests/c_concurrency.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.65s
    tests/c_databases.rs: test result: ok. 12 passed; 0 failed; 0 ignored; finished in 6.23s
    tests/core_api_c2.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/env_copy.rs: test result: ok. 6 passed; 0 failed; 0 ignored; finished in 4.23s
    tests/handbuilt.rs: test result: ok. 2 passed; 0 failed; 0 ignored; finished in 0.07s
    tests/model.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 30.06s
    tests/nested_txn.rs: test result: ok. 9 passed; 0 failed; 0 ignored; finished in 0.07s
    tests/robust_io_errors.rs: test result: ok. 4 passed; 0 failed; 0 ignored; finished in 0.01s
    tests/robust_mutation.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 13.27s
    tests/robust_send_sync.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/rust_env.rs: test result: ok. 13 passed; 0 failed; 0 ignored; finished in 0.43s
    tests/rust_writes.rs: test result: ok. 23 passed; 0 failed; 0 ignored; finished in 2.14s
    tests/write_api.rs: test result: ok. 14 passed; 0 failed; 0 ignored; finished in 0.27s
    tests/write_differential.rs: test result: ok. 18 passed; 0 failed; 2 ignored; finished in 17.46s
    tests/write_interop.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    tests/write_review.rs: test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.01s
    tests/write_smoke.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.04s
    tests/write_soak.rs: test result: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    interop src/lib.rs: test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    tests/c_vs_c.rs: test result: ok. 39 passed; 0 failed; 0 ignored; finished in 3.98s
    tests/c_writes_conrust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 4.73s
    tests/c_writes_rust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 4.63s
    tests/conrust_vs_conrust.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.35s
    tests/conrust_writes_c_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.28s
    tests/conrust_writes_rust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.46s
    tests/layout.rs: test result: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    tests/rpeer.rs: test result: ok. 10 passed; 0 failed; 0 ignored; finished in 14.27s
    tests/rust_vs_rust.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.72s
    tests/rust_writes_c_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.05s
    tests/rust_writes_conrust_reads.rs: test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.25s
    tests/three_peers.rs: test result: ok. 1 passed; 0 failed; 0 ignored; finished in 2.33s
    doc-tests rlmdb: test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.31s
    doc-tests interop: test result: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
```

</details>
