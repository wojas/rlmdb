# Task L: the per-record cost of a cursor scan (`ls_scan`)

Cuts what a cursor step costs per record when it stays on its leaf page:
one format dispatch, one read of `NUMKEYS`, one node lookup and one
bounds check for key and data together, with the item returned straight
to the caller. All library changes are in `rlmdb` (`src/core/`,
`src/format/node.rs`), based on `90cc145`, behaviour-preserving (same
results, same errors on corrupt input, nothing on the write path or on
disk changes), without new `unsafe`; the public API is unchanged. The
bench's Lightning Stream scan loop is compiled out of line, as C's is
(`bench/src/workloads.rs`).

**Summary.** On macOS a step of `ls_scan` now takes 78 instructions
per record (base: 147; C: 113 counted the same way). Interleaved
against the base build, `ls_scan` takes 0.78-0.81x the time per record
(17.9 to 14.5 ns), the plain scans of the `read` group 0.80 (forward)
and 0.88 (backward), and `NEXT_DUP` 0.86 by the median (0.97 by the
minimum). Rust's `ls_scan` went from 1.11x C to **0.83x C** on macOS
(14.0 against 16.8 ns per record) and from 1.25x to **0.89x C** on
Linux. `ls_update` does not run the changed code per put; its
interleaved ratios ranged from 0.84 to 1.14 depending on the pair of
binaries (see below).

## What changed

1. **One pass over the page for a step within a leaf**
   (`core/cursor.rs`: `step_in_leaf`, `item_in_leaf`, `next_in_page`,
   `prev_in_page`). It takes the cursor to the neighbouring item when
   * `C_INITIALIZED` is set and `C_EOF` and `C_DEL` are clear,
   * the tree is not `MDB_DUPSORT` (sub-cursors over duplicates qualify:
     the record they are given does not have the flag),
   * the neighbour is on the same leaf page, and
   * the neighbour is a `P_LEAF2` key, a key on a sub-cursor, or a node
     without `F_BIGDATA` and `F_DUPDATA` whose key and data lie in the
     page.

   The step does one format dispatch, one stack-index check, one read of
   the page flags and `NUMKEYS`, one node lookup and one bounds check for
   key and data together, then moves the index and returns the item. In
   every other case it returns `None` without touching the cursor:
   the end of a page, `F_BIGDATA`, `F_DUPDATA`, a `DUPSORT` main cursor,
   `C_EOF` or `C_DEL` set, an unpositioned cursor, and anything the
   general path reports as corrupt. The unchanged general path then runs
   from the same state. So either the result is the general path's own,
   or the step only answers where, as the doc comment of `step_in_leaf`
   argues, `mdb_cursor_next` / `mdb_cursor_prev` would move the index by
   one within the page whatever the operation and then read the same
   item, leaving the flags as they were. A new unit test checks exactly
   that (below).
2. **The general path out of line** (`cursor_next_any`,
   `cursor_prev_any`: the old `cursor_next` / `cursor_prev`, unchanged).
   `cursor_next` / `cursor_prev` are now the fast step, inlined, plus a
   call of these. Every caller takes the fast step first: `RoCursor::get`,
   the sub-cursor steps inside the general path (`NEXT_DUP` / `PREV_DUP`
   within a key's duplicates), and the free-list walks of the write path
   (`core/freelist.rs`).
3. **`RoCursor::next` and `RoCursor::prev` return the fast step
   directly** (`core/ro_cursor.rs`). The step leaves `C_DEL` clear, so
   `finish` would have nothing to do. The general path and `finish`
   moved to `next_any` / `prev_any`, out of line. On the common path
   the step saves only the frame record, where the base's `cursor_next`
   saved ten registers and `read_position` four more, and it writes
   `Ok(Some(..))` straight into the caller's result. Before, the item
   went through a `Result<Kv>` in memory and `C_DEL` was tested again.
4. **Key and data with one bounds check** (`format/node.rs`
   `NodeRef::key_and_data`). It returns `Some((key()?, inline_data()?))`
   with a single check: the key ends where the data starts (in 1.0 at
   most one padding byte earlier), so the result is `None` exactly when
   `inline_data` is.
5. **The bench's scan loop out of line** (`bench/src/workloads.rs`
   `ls_changed`, `#[inline(never)]`). Inlined into the large
   `workloads::run`, the loop kept its `changed` counter on the stack
   (a load, a conditional increment and a store per record) and needed
   two adds per address of its cursor and result, because the frame is
   over 4 KiB. C's `ls_changed` is a function of its own (clang does not
   inline it) with the counters in registers. Both loops do the same
   work as before: the same cursor call, the same length check, one load
   and a byte swap. Now both are 16 instructions per record. This did
   not change the time measurably (see the A/B below); it makes the
   instruction counts comparable.

What the step still checks per record, as before: that the stack index
is below 32, that the page holds a header, that it is a leaf, that the
index is below `NUMKEYS` and its pointer within the page, that the node
header is within the page, and that key and data end within it. The
general path also checks key and data separately; here one check covers
both. No check on file contents was removed.

## Instructions per record (macOS)

Counted by single-stepping the release builds under lldb (an lldb
Python script steps every instruction, including into callees, and
measures the distance between successive visits of the loop's head).
It counts about 300 records, after skipping the first 3000 instructions
of the scan (the descent to the first leaf), in an `ls` database of
10 000 records (`--worker ls <dir> 2000 1 1000`) and a `read` database
of 20 000 keys. "In page" is the step every record but the first of a
page takes; "mean" includes the page changes (about one in 65 records).

| build | `ls_scan`, in page | mean | page change |
|---|---:|---:|---:|
| base (`90cc145`, loop inlined into `run`) | 147 (`cursor_next` 51, `read_position` 68, loop 28) | 148.9 | 304 |
| base library, bench loop out of line | 141 (51, 68, loop 22) | 142.7 | 298 |
| fast step in `RoCursor::next`, general path in the same function | 92 (`next` 70, loop 22) | 94.9 | 362 |
| general path and `finish` moved out of line (no stack frame) | 84 (`next` 62, loop 22) | 86.7 | 368 |
| **final**: bench loop out of line as well | **78** (`next` 62, loop 16) | **81.4** | 362 |
| C (`cbench`, same tool) | 113 (`mdb_cursor_next` 62, `mdb_cursor_get` 35, loop 16) | 113.9 | 216 |

The read group's plain scans (100-byte values), base library with the
same bench against the final build: `scan_fwd` 139 to 77, `scan_bwd`
137 to 79 instructions per item.

The previous count of C was 109 (by a different stepping method), so
compare C with the rows above only through this table. A page change
costs about 60 instructions more than before: the fast step looks
first, then the general path dispatches again. At one page change in
65 records that adds about one instruction per record on average.

## A/B against the base build (macOS)

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD, environments in the temp directory; 16 KiB pages. rustc 1.93.1,
  the workspace's release profile.
* The base is the Rust side of `rlmdb-bench` built at `90cc145`.
  Command (from the repository root): `cargo run --release -p rlmdb-bench --
  --baseline <base build> --rounds 3 --reps 3 --only ls,read,dupsort`
  (n = 1 000 000; 3 rounds alternating which side goes first, 3 timed
  reps after a warm-up each, so 9 samples per side; ns/op median (min);
  the median ratio is the bench's, the min ratio from the rounded mins).

| workload | ops/rep | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|---:|
| ls_scan | 5000000 | 17.9 (17.5) | 14.5 (14.0) | **0.81** | 0.80 |
| ls_update | 50000 | 4823 (4581) | 5515 (4700) | 1.14 | 1.03 |
| rand_get | 1000000 | 922 (821) | 854 (817) | 0.93 | 1.00 |
| scan_fwd | 1000000 | 17.0 (16.6) | 13.6 (12.9) | **0.80** | 0.78 |
| scan_bwd | 1000000 | 15.8 (15.0) | 13.9 (12.9) | **0.88** | 0.86 |
| readers8_get | 8000000 | 186 (136) | 144 (118) | 0.78 | 0.87 |
| dup_put | 1000000 | 1163 (1060) | 1046 (1034) | 0.90 | 0.98 |
| dup_get_both | 1000000 | 875 (775) | 837 (801) | 0.96 | 1.03 |
| dup_next_dup | 1000000 | 98.7 (83.9) | 84.5 (81.8) | **0.86** | 0.97 |

`rand_get`, `readers8_get`, `dup_put` and `dup_get_both` do not run the
changed code (gets and `GET_BOTH` position with `cursor_set`, puts do
not step); their ratios are noise and code layout. `dup_next_dup`
steps the sub-cursor through the fast step on every `NEXT_DUP`; its base
median is well above its minimum, so 0.86 overstates it and 0.97 by the
minimum is the safer reading.

### `ls_update`

`ls_update` came out slower in the first run, so the `ls` group was
run three more times, 5 rounds each (`--only ls --rounds 5 --reps 3`),
with the bench code held equal where possible:

| new side | base side | ls_scan median (min) | ls_update median (min) |
|---|---|---:|---:|
| final | base (`90cc145`) | 0.78 (0.80) | 1.08 (1.03) |
| final | base library + the bench change of item 5 | 0.80 (0.80) | **0.84** (0.88) |
| base library + the bench change | base (`90cc145`) | 1.00 (1.01) | 1.09 (1.08) |

With the same bench code on both sides, the new library is faster at
`ls_update` (14 of its 15 samples below the fastest base sample). The
bench change alone, with the library unchanged, gives 1.09 for
`ls_update` and 1.00 for `ls_scan`. A put does not run any of the changed
code: it positions with `cursor_set` and never steps; only the free-list
walk of a page allocation steps, a few times per transaction. Most of
an `ls_update` put is copying pages and writing them out, and its level
moves by 10-15 % between runs of the same binary. So the 1.08-1.14 of
the first comparison is about the binaries' layout, not the library's
work per put.

## Rust and C (`ls` group)

Command (from the repository root): `target/release/rlmdb-bench --only ls --rounds
3 --reps 3` (n = 1 000 000, 5 million records), and on Linux
`scripts/linux-test.sh run --release -p rlmdb-bench -- --dir
/tmp/lmdb-bench --only ls,read --rounds 3 --reps 3`. Before: the
numbers of [`LS-workloads.md`](LS-workloads.md), measured the same way.

| platform | workload | C ns/op | Rust ns/op | Rust/C median (min) | before |
|---|---|---:|---:|---:|---:|
| macOS | ls_scan | 16.8 (16.4) | 14.0 (13.7) | **0.83** (0.84) | 1.11 (1.07) |
| macOS | ls_update | 5127 (4910) | 5351 (5177) | 1.04 (1.05) | 0.86 (0.82) |
| Linux | ls_scan | 44.1 (40.6) | 39.1 (34.4) | **0.89** (0.85) | 1.25 (1.03) |
| Linux | ls_update | 4932 (4614) | 5760 (5353) | 1.17 (1.16) | 1.54 (1.27) |
| Linux | scan_fwd | 34.5 (32.1) | 28.5 (26.0) | 0.83 (0.81) | |
| Linux | scan_bwd | 29.9 (28.2) | 29.5 (28.2) | 0.99 (1.00) | |

Linux: the Lima VM behind `scripts/linux-test.sh` (8 vCPUs, 12 GB, on
the same Mac), Debian 12 `rust:1-bookworm` container, glibc, rustc
1.99.1, gcc 12.2.0, environments on the container's tmpfs `/tmp`, 4 KiB
pages. Only idle service containers shared the VM this time; the host
was otherwise idle. Still, the scan's samples moved by about 15 %
between rounds on both sides. Rust was ahead in every round (0.90,
0.90 and 0.80 of C by the round's median). The `read` group's other
rows (`rand_get` 1.13, `readers8_get` 1.12) do not run the changed code.
`ls_update` was 1.05, 1.14 and 1.17 of C by round. Its put path is
unchanged by this task, and on macOS the same workload moved by
10-15 % between builds that share that code (see above). The same goes
for macOS's `ls_update` going from 0.86x to 1.04x C between the runs
of the two notes: interleaved with the bench code held equal, the new
library's puts are not slower than the base's.

## Tried and reverted, and not done

* `#[inline]` on `RoCursor::next` and `RoCursor::prev`, to fold the step
  into the caller's loop: LLVM kept them out of line anyway (77
  instructions per record against 78). Reverted.
* Not done: fusing the `NUMKEYS` test with the pointer's bounds check
  (`2i + PAGEHDRSZ + 2 <= mp_lower + PAGEBASE`, 2 instructions per
  step); a fast step for main cursors of `DUPSORT` databases (it would
  have to read the current node too and reset the sub-cursor); one
  dispatch in the general path. The general path now runs once per page,
  and no workload of the bench steps a `DUPSORT` main cursor between
  keys, so none of these could be shown to help.

## Behaviour

* New unit test `core::cursor::tests::step_in_leaf_matches_the_general_path`:
  hand-built leaf pages of both layouts are used as they are, with every
  header field, pointer and node-header field replaced by boundary
  values (0, 1, 2, … 511, 512, 0x7fff, 0xffff), and cut short. The
  leaves hold plain items, empty key and data, an `F_BIGDATA` item, an
  `F_DUPDATA` sub-page and an `F_SUBDATA` record; there is also a
  `P_LEAF2` page and a branch page. Every cursor state is tried (six
  flag combinations, plain, `DUPSORT` and `DUPFIXED` records, `md_pad`
  0, 3 and 1000, every index up to two past the end), in both
  directions. The fast step either returns the general path's item with
  the same cursor state afterwards, or declines and leaves the cursor
  as it was. 710 280 cases, 174 426 of them taken by the fast step.
  Removing the `F_BIGDATA` test or the `C_EOF` test from the step makes
  it fail.
* New unit test `format::node::tests::key_and_data_is_key_then_inline_data`
  compares `key_and_data` with `key` and `inline_data` on every key and
  data size up to 5 bytes, claimed and stored, cut at every length, in
  both layouts.
* Differential mutation run (scratch harness, not committed): the
  mutation corpus of `tests/robust_mutation.rs` (the same mutations,
  operations and seeds) over five base environments written by the Rust
  writer with 16 KiB pages. They are the test's `main`, `big` and
  `named` plus two more: `DUPSORT` and `DUPFIXED` keys with 900
  duplicates (sub-DBs also on 16 KiB pages), and an LS-like database.
  Each seed reads and writes the damaged environment (every walk, random
  cursor operations, gets, puts, deletes, cursor writes, nested
  transactions, commits, copies). Every result and error code was
  logged, once with the base library and once with the new one.
  The two logs were identical in all **1170 cases** (`main` 300, `big`
  300, `named` 120, `subdb` 150, LS-like 300). In 725 of them some
  operation returned `MDB_CORRUPTED` or `MDB_PAGE_NOTFOUND`. That
  includes 464 of the 900 cases of the three bases without `DUPFIXED`
  data, whose unmutated cases return neither code.
* One thing the harness showed that this task did not change (same
  results before and after): on valid data, `MDB_PREV_MULTIPLE` on a
  `DUPFIXED` cursor left past the end of its page (by a failed `SET` or
  `GET_BOTH`) returns `MDB_CORRUPTED`. `RoCursor::get` reads the current
  key first (`current_key()?`); C's `mdb_cursor_get` does not read the
  key for that operation. All 17 such results in the unmutated `named`
  and `subdb` cases are this one. It is outside this task and left as it
  is.

## Tests

All from the repository root, on the final tree:

| platform | command | outcome |
|---|---|---|
| macOS | `cargo fmt --all -- --check` | clean |
| macOS | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| macOS | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| macOS | `cargo test --workspace --no-fail-fast` | 59 test binaries and doc-test runs: 676 passed, 0 failed, 7 ignored |
| Linux | `scripts/linux-test.sh test -p rlmdb -p rlmdb-interop` (Docker through Lima, 4 KiB pages) | 37: 590 passed, 0 failed, 7 ignored |

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 94 passed; 0 failed; 1 ignored; finished in 2.93s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 9.14s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; finished in 6.47s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 5.84s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.08s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 30.19s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 0.30s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; finished in 4.85s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 0.06s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 20.35s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.01s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; finished in 3.43s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.27s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; finished in 21.22s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 3.53s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 0.34s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.10s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 0.21s
    lmdb src/lib.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 0.00s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.86s
    capi_version tests/capi_version.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 0.97s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 3.86s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; finished in 10.16s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 5.29s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 12.09s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.02s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 9.32s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; finished in 9.21s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 12.06s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 12.70s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 16.24s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 11.52s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 15.84s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 0.02s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 7.13s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 15.71s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 11.84s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 9.33s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 10.96s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.42s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 8.41s
    dump tests/dump.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 1.73s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 5.53s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; finished in 1.60s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 0.92s
    rlmdb doc-tests: ok. 3 passed; 0 failed; 0 ignored; finished in 0.56s
    interop doc-tests: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
    rlmdb_tools doc-tests: ok. 0 passed; 0 failed; 0 ignored; finished in 0.00s

Linux, scripts/linux-test.sh test -p rlmdb -p rlmdb-interop:
    rlmdb src/lib.rs: ok. 92 passed; 0 failed; 1 ignored; finished in 3.43s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 3.52s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; finished in 7.26s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; finished in 5.09s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.08s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 30.09s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 0.13s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; finished in 6.72s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; finished in 0.02s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 19.97s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.00s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; finished in 2.66s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; finished in 0.40s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; finished in 135.20s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; finished in 0.08s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 0.08s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; finished in 3.30s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 5.24s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 4.82s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.07s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 6.17s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.62s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; finished in 0.51s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; finished in 15.59s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 7.84s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 5.82s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; finished in 8.42s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; finished in 2.34s
    rlmdb doc-tests: ok. 3 passed; 0 failed; 0 ignored; finished in 0.37s
    interop doc-tests: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s
```

</details>
