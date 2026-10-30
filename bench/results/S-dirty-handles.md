# Task S: stable handles for dirty pages

Gives the dirty pages of a write transaction stable places (slots) and
lets write cursors remember them, so the write path stops searching the
dirty list for pages it already knows. It also takes the cheap wins that
came with this: the arena slot of every `mdb_put` and the copies of the
database record. All changes are in `rlmdb` (`src/core/`, plus one doc
comment in `src/idl.rs`), based on `30d5566`.

The changes preserve the layout: same results, same file bytes, same free
list, same spill decisions. The write differential against C, the interop
matrix, the model test and `mtest` pass without changes to their
expectations. No new `unsafe`, and the public API is unchanged.

**Summary** (macOS, interleaved A/B against the base build, median
ns/op):

| workload | change |
|---|---:|
| sequential appends | **-19 %** |
| `DUPSORT` puts | **-9 %** |
| random puts and random deletes | **-5 %** |
| one-put transactions, overwrites | unchanged |
| reads | unchanged within noise (they run none of the changed code) |

Timed by thread CPU time of the put and delete calls alone (commits
excluded):

| workload | change |
|---|---:|
| sequential appends | -21 % |
| `DUPSORT` puts | -11 % |
| random deletes | -9 % |
| random puts | -5 % |
| overwrites | -2 % |

Against C, on the same machine and day, sequential appends are at
2.0-2.2x C (2.5-3x before this change). Random puts, deletes and
`DUPSORT` puts are at 1.0-1.2x C.

## Design

### Slots in the dirty list (`core/dirty.rs`)

`DirtyPages` was an `Id2List<Box<[u8]>>`: page buffers sorted by page
number, so a page's position moved with every insert or removal below it.
Now:

* the buffers live in a slab, `slots: Vec<Entry>` (an `Entry` is a page
  number and an `Option<Box<[u8]>>`), with a free list of empty slots
  (`vacant`). A page keeps its slot, and so its buffer, for as long as it
  is in the list;
* the sorted list that C searches (`mt_u.dirty_list`,
  `mdb_mid2l_search`) is `index: Id2List<PageSlot>`, mapping page numbers
  to slots. Inserts and removals now shift 16-byte `(pgno, slot)` pairs
  instead of the 24-byte `(pgno, Box)` pairs of before;
* a `PageSlot` (a `u32`; `PageSlot::NONE` means "unknown") is the handle.
  Every access by slot (`DirtyPages::at`, `at_mut`, `lookup`, `resolve`)
  names the page number it expects and checks it against what the slot
  holds now. On a mismatch the caller searches by number. The 16 hints of
  the list (indexed by `pgno % 16`) now hold slots too, so they no longer
  go stale when the index shifts.

### Handles are checked hints, so nothing needs invalidating

A slot is never trusted, only checked, and the check is one compare of a
page number stored next to the buffer pointer. Every case below is
therefore safe without bookkeeping, and a stale hint costs at most one
search:

* **Spills** (`write_pages` called from `page_spill`): written pages
  leave the list and their slots are emptied. `page_unspill` puts a page
  back into whichever slot is free. A hint to the old slot either misses
  (the slot is empty or holds another page), or finds the page itself if
  it got the same slot back, in which case that is its current buffer.
* **Commit** (`page_flush`, `write_pages`): written pages leave the list,
  and the list is cleared when the transaction ends. Slots are only ever
  checked against the list of the running transaction.
* **Frees** (`ovpage_free` of a run dirtied in this transaction, loose
  pages dropped by `freelist_save`): handled like a spill.
* **Nested begin**: the parent's list moves, unchanged, into the child's
  first `Layer`, and the child starts an empty list of its own. The
  parent's cursors (the child takes over the arena) still carry slots of
  the parent's list. Checked against the child's list, those slots miss,
  or they find the child's own copy of that very page
  (`copy_ancestor_page`), which is what the child must use.
* **Nested commit** (`DirtyPages::merge`): a page that the parent had and
  the child replaced keeps the parent's slot. The child's buffer moves
  into it and the parent's goes to the pool, so the parent's hints for that
  page stay valid. The child's other pages get new slots in the parent's
  list. Slots recorded during the child miss in the parent's list and are
  looked up again.
* **Nested abort**: the child's list is dropped and the parent's comes
  back unchanged. The parent's cursors are restored from their backup
  (`mc_backup`) with the slots they had, which are valid again.
* **Spill set**: a page is never both in a transaction's list and in its
  spill set. Spilling removes it from the list and unspilling from the set;
  a child's commit removes the child's dirty pages from the parent's spill
  set and the child's spilled pages from the parent's list. Since
  `mdb_page_get` checks the spill set first, a page found through a slot
  of the transaction's own list is exactly what `TxnCore::page` returns
  for it.

### Slots in write cursors (`core/wcursor.rs`)

Write cursors still hold no references into buffers: `WCursor` has
`pg_slot: [PageSlot; 32]` next to `pg`. The slots get there in three ways:

* `mdb_page_touch` (`RwTxn::page_touch`, and `cursor_touch` over every
  level before each put or delete) stores the slot of the page it found
  dirty, copied, or copied from an ancestor. For a copy,
  `page_alloc_copy` now returns the new page's slot, which also goes to
  the other cursors moved to the copy;
* a write through a cursor (`page_mut_of`, `top_page_mut`) stores the slot
  it found;
* `copy_state`, `xcursor_init2` and cursor clones (`copy_cursor`, the
  nested backup) carry them along.

Everything else that changes a cursor's page numbers leaves the old slot
in place: splits, merges, the fixups that copy `pg` entries between
cursors, and the read operations whose results `store_back` writes back.
Such a slot misses once, and the next `page_touch` stores the right one.
The cursor slot of `mdb_put` is now reset in place and keeps its slots
from the previous put; for sequential and local writes they name the same
pages.

These places use the slots:

* `page_touch` first checks the cursor's slot (`touch_needed`): a sub-page,
  or a page already dirty in the transaction's own list, needs nothing.
  Only then does it take the old path (`page_touch_slow`);
* `cursor_touch` sets `mc_top` only for the levels that need that path;
* `page_of`/`top_page` (through `TxnCore::page_by_slot`),
  `page_mut_of`/`top_page_mut`, `subpage_of` and `load_read` (which
  rebuilds a read cursor from a write cursor) go to the buffer directly
  when the slot holds the page.

Lookups by number are unchanged: the descent of every read operation
through `TxnCore::page` checks the transaction's spill set, then its list
(hint, binary search, slot), then its ancestors' lists, then the map.

### Cheap wins on the way

* **The arena slot of `mdb_put`** (`Arena::open`, `WCursor::reset`,
  `XState::reset`): a free slot is reset in place. Before, a `WCursor`
  (and for `DUPSORT` an `XState`) was built on the stack and moved in.
  `open_slot` now reads the DBI's flags and duplicate order instead of
  copying its 96-byte entry. The resulting state is the same as before
  (the stacks are reset completely) apart from the slot hints.
* **Record copies** (`db_and_cmp`, `db_of`, `cmp_of`): these copied the
  whole 96-byte DBI entry to return the 48-byte record or the comparator;
  now they copy only what is asked for. `put_at0` reads the record once
  and passes `md_depth` to `spill_check`, and the flags and `md_pad` to
  `put_item`; a put changes none of these before they are used. A plain
  put now makes two record copies instead of about five.
* **Clean pages are searched once when touched**: the slot check of
  `page_touch` tells the slow path whether the page is in the
  transaction's own list, and `TxnCore::page_not_dirty_here` skips that
  list. Without this, the new fast path would have made every
  first touch of a clean page search the list twice.

## What changed, per file

* `core/dirty.rs`
  * New: `PageSlot`, `Entry`; the slab, the free list and the index in
    `DirtyPages`.
  * Changed:
    * lookups: `at`, `at_mut`, `find`, `lookup`, `resolve` (new),
      `search` (private), `get`, `get_mut`, `contains`;
    * insert and removal: `insert` (returns the slot), `occupy`,
      `remove`, `clear`, `retain`, `vacate`;
    * `merge` keeps the parent's slots.
  * Module documentation: slots, and the invalidation rules above.
  * Two unit tests.
* `core/wcursor.rs`
  * Module documentation (dirty-list slots).
  * New fields and constructors: `WCursor::pg_slot`, `WCursor::reset`,
    `XState::new`, `XState::reset`, `Arena::open`, `Arena::sub_record`.
  * `open_slot` uses `Arena::open`.
  * Page access through slots: `subpage_of`, `load_read`, `page_of`,
    `page_mut_of` (which stores the slot).
  * Record copies: `db_and_cmp`, `db_of`, `cmp_of`.
  * Touching:
    * `page_touch` is split into the inlined check `touch_needed` and
      `page_touch_slow`, and `cursor_touch` checks each level directly;
    * `copy_ancestor_page` returns the slot;
    * `own_copied_page` writes through the slot.
  * Slots carried along by `copy_state` and `xcursor_init2`.
* `core/txn.rs`: `TxnCore::page_by_slot`, `TxnCore::page_not_dirty_here`,
  and `TxnCore::locate_in` (`locate` without the own list).
* `core/freelist.rs`: `page_alloc_copy` returns `(pgno, slot)`.
* `core/cursor_put.rs`: `put_at0` hands the record to `spill_check`
  (`md_depth`) and `put_item` (flags, `md_pad`).
* `core/cursor_del.rs`: the `spill_check` call.
* `core/spill.rs`: `insert`'s new result; two tests (see Tests).
* `idl.rs`: the doc comment of `Id2List` (it now maps to slots).

## A/B: new against base

The command, from `rust/`, was `cargo run --release -p rlmdb-bench --
--baseline <bench of 30d5566> --rounds 3 --reps 3`. Both sides alternate
per group, n = 1 000 000, and each cell is ns/op as median (min) over 9
samples per side.

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, 16 KiB pages, APFS on
  the internal SSD; rustc 1.93.1 (Homebrew), Apple clang 17.0.0.
* The machine was shared with other jobs throughout, at load averages of
  10-22 on 10 cores.

| workload | ops/rep | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|---:|
| seq_put_append | 1000000 | 203 (196) | 163 (160) | **0.81** | 0.82 |
| rand_put | 1000000 | 2938 (2727) | 2784 (2695) | **0.95** | 0.99 |
| rand_get | 1000000 | 960 (927) | 939 (839) | **0.98** | 0.91 |
| scan_fwd | 1000000 | 18.9 (18.2) | 18.1 (17.6) | **0.96** | 0.97 |
| scan_bwd | 1000000 | 16.2 (15.6) | 16.1 (15.4) | **0.99** | 0.99 |
| readers8_get | 8000000 | 119 (116) | 119 (114) | **1.00** | 0.98 |
| rand_overwrite | 1000000 | 3817 (3654) | 4161 (3497) | **1.09** | 0.96 |
| dup_put | 1000000 | 1352 (1165) | 1230 (1072) | **0.91** | 0.92 |
| dup_get_both | 1000000 | 837 (809) | 818 (730) | **0.98** | 0.90 |
| dup_next_dup | 1000000 | 93.0 (91.4) | 90.5 (86.0) | **0.97** | 0.94 |
| put_4k | 50000 | 17969 (16721) | 18011 (17481) | **1.00** | 1.05 |
| get_4k | 1000000 | 479 (451) | 535 (498) | **1.12** | 1.10 |
| put_64k | 5000 | 93249 (86504) | 93041 (90131) | **1.00** | 1.04 |
| get_64k | 1000000 | 206 (202) | 221 (209) | **1.08** | 1.03 |
| del_rand | 1000000 | 3908 (3520) | 3724 (3226) | **0.95** | 0.92 |
| txn_nosync | 50000 | 9262 (8556) | 9305 (8606) | **1.00** | 1.01 |
| txn_sync | 1000 | 10152563 (9734747) | 10256631 (9640295) | **1.01** | 0.99 |
| mixed_writer | 50000 | 6268 (5684) | 6331 (5520) | **1.01** | 0.97 |
| mixed_readers | 67928064 | 4.6 (4.3) | 4.8 (4.2) | **1.04** | 0.98 |

The read workloads (`rand_get`, scans, `readers8_get`, `dup_get_both`,
`dup_next_dup`, `get_4k`, `get_64k`, `mixed_readers`) run none of the
changed code: read transactions never reach the dirty list. Their ratios
therefore show the noise level. `put_4k` and `put_64k` took twice as long
as in other runs that day, on both sides.

The groups with outliers were run again with 6 rounds (`--rounds 6 --reps
3 --only overwrite,overflow`):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| rand_overwrite | 4277 (3564) | 4292 (3633) | **1.00** | 1.02 |
| put_4k | 7711 (7073) | 8130 (7141) | **1.05** | 1.01 |
| get_4k | 588 (508) | 591 (472) | **1.00** | 0.93 |
| put_64k | 28910 (26940) | 28293 (25233) | **0.98** | 0.94 |
| get_64k | 267 (219) | 236 (202) | **0.89** | 0.92 |

### By CPU time

At this load, wall-clock A/Bs of one binary against itself (A/A) came out
anywhere between 0.86 and 1.07, so steps of a few percent could not be
judged from them. For the decisions during development, a scratch
harness was used. It is not committed: it runs the same loops as the
bench (same data generator; batches of 10 000), built against the base
tree and against the working tree. It times the put or delete calls of
each batch by thread CPU time (`CLOCK_THREAD_CPUTIME_ID`, commits
excluded; `txn` times whole one-put transactions) and alternates the two
builds per round.

Its A/A spread was 1-2 %. The table below is base against final, 6 rounds
of 3 reps (ns/op, median (min)). Each rep does 1 000 000 appends for
`seq`, 300 000 operations for the others, and 20 000 transactions for
`txn`.

| workload | base | new | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| seq (appends) | 182.6 (180.6) | 143.9 (141.5) | **0.79** | 0.78 |
| rand (random puts) | 1042.8 (942.8) | 985.0 (839.7) | **0.95** | 0.89 |
| ovw (overwrites) | 1359.2 (1145.9) | 1327.9 (1202.3) | **0.98** | 1.05 |
| dup (`DUPSORT` puts) | 798.7 (743.6) | 711.5 (676.9) | **0.89** | 0.91 |
| del (random deletes) | 1460.2 (1276.8) | 1324.9 (1243.0) | **0.91** | 0.97 |
| txn (one-put transactions) | 11218.9 (9986.1) | 11192.5 (10442.8) | **1.00** | 1.05 |

Separate runs against intermediate builds (everything but the single
search for clean pages) gave del 0.97 (min 0.95) and txn 1.00 (min 0.99),
8 rounds each. The single search alone measured rand 0.97 (min 1.00) and
del 1.03 (min 1.01): within noise, as expected for one binary search per
newly touched page.

## Rust against C, after

From `rust/`: `cargo run --release -p rlmdb-bench -- --only
seq_put,rand_put,dupsort,delete,small_txn --reps 5`. This is one run per
side, ns/op as median (min):

| workload | C ns/op | Rust ns/op | Rust/C |
|---|---:|---:|---:|
| seq_put_append | 80.3 (79.7) | 162 (162) | **2.02** |
| rand_put | 3000 (2815) | 2971 (2856) | **0.99** |
| dup_put | 1093 (921) | 1078 (1074) | **0.99** |
| dup_get_both | 1227 (1223) | 941 (930) | **0.77** |
| dup_next_dup | 145 (140) | 103 (98.5) | **0.71** |
| del_rand | 3846 (3467) | 3736 (3630) | **0.97** |
| txn_nosync | 28739 (28539) | 36370 (25922) | **1.27** |
| txn_sync | 26190344 (20366263) | 12002234 (10414196) | **0.46** |

Because a single run is at the mercy of the load, the comparison was
repeated interleaved, with `--rounds 3 --reps 3 --sync-commits 200`:

| workload | C ns/op | Rust ns/op | Rust/C |
|---|---:|---:|---:|
| seq_put_append | 80.2 (75.5) | 173 (165) | **2.16** |
| rand_put | 2883 (2506) | 2890 (2478) | **1.00** |
| dup_put | 943 (788) | 1113 (1029) | **1.18** |
| dup_get_both | 1218 (879) | 946 (692) | **0.78** |
| dup_next_dup | 140 (100) | 101 (79.0) | **0.72** |
| del_rand | 3615 (3114) | 3813 (3492) | **1.05** |
| txn_nosync | 29602 (8406) | 33770 (20669) | **1.14** |
| txn_sync (200 commits) | 11061760 (9359640) | 20247010 (19318328) | **1.83** |

During both runs, small file writes were 3-4 times slower than usual for
both sides: `txn_nosync` is about 6-9 µs in `RESULTS.md` and in the A/B
above. The `txn_*` rows therefore say nothing here.

## Profiles

The tool was `sample` (macOS) on the bench's worker process
(`rlmdb-bench --worker <group> <dir> 1000000 <reps> 1`), for 15 s,
with the base and the final build. Self time is aggregated per function
within the subtrees of `RwTxn::mdb_put` (puts) or `RwTxn::del` (deletes).
The `seq_put` runs finished inside the window, so their totals compare
directly: 2989 samples before, 1954 after, about the same number of
puts. For the other workloads only the shares compare.

| share of samples | seq_put before | after | rand_put before | after | dup_put before | after | del_rand before | after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| page lookups (by number or slot) | 15.6 % | 13.2 % | 9.0 % | 6.9 % | 9.7 % | 9.5 % | 8.6 % | 7.7 % |
| `page_touch`/`cursor_touch` | 9.6 % | 7.2 % | 2.8 % | 3.4 % | 3.3 % | 3.6 % | 2.1 % | 2.8 % |
| read-cursor rebuild (`put_position` glue, `load_read`, `write_back`) | 16.0 % | 17.6 % | 1.9 % | 1.6 % | 7.8 % | 9.0 % | 0.1 % | 0.4 % |
| record and DBI copies | 7.3 % | 5.3 % | 1.4 % | 1.1 % | 4.1 % | 3.6 % | 0.6 % | 0.9 % |
| cursor slot open/close | 7.2 % | 3.9 % | 2.8 % | 0.8 % | 1.9 % | 1.4 % | 0.7 % | 0.3 % |
| B-tree search and read path | 11.1 % | 15.0 % | 48.3 % | 49.3 % | 29.1 % | 28.6 % | 41.3 % | 42.6 % |
| page copies, node moves (`memmove`, `memset`) | 3.3 % | 4.7 % | 20.5 % | 22.9 % | 15.5 % | 16.9 % | 32.8 % | 31.2 % |
| node edits (`node_add_to`, `node_del`, split) | 7.9 % | 7.7 % | 8.0 % | 7.3 % | 12.2 % | 10.7 % | 6.0 % | 6.4 % |
| other | 22.1 % | 25.4 % | 5.3 % | 6.6 % | 16.5 % | 16.7 % | 7.7 % | 7.8 % |

* **`seq_put_append`**: in absolute samples, the page lookups fell from 466
  to 258 and the touch from 288 to 141. The record copies fell from 217 to
  104, and the slot open/close from 215 to 76. The descent through the
  tree and the read-cursor rebuild are now the largest parts (see "Left
  for later").
* **`rand_put`, `del_rand`, `dup_put`** are dominated by the binary
  searches in pages (cache misses on a 1M-key tree) and by the whole-page
  copy on touch. Both are unchanged by design, and together they are
  about 70 % of the put and delete paths. The cursor bookkeeping that
  this work targets was 10-20 % of those paths; it shrank, but the gain
  is bounded.
* **`txn_nosync`**: before and after, the commit's `pwrite`s (pages and
  meta page) were 57-59 % of the begin, put and commit path. Next come the
  page copies (10 %), the writer lock's semaphores (7 %) and `read_meta`
  (1.5 %). `malloc` and `free` together were 1-2 % both times. The slab's
  two extra vectors per transaction (the slots and the free list beside
  the index) do not show in the A/B.

## Tried and reverted

* **Short read cursors for the write path.** `read_op` used 8-level read
  cursors when the write cursor, its sub-cursor and both trees fit, and
  retried with full ones on `MDB_CURSOR_FULL` (as `TxnCore::get` does
  since task R). This applied to positioning for puts and deletes.
  Read operations only read the snapshot, so a retry from the unchanged
  write cursor gives exactly the full-stack result.
  * Implementation: a `read_op_short!` macro to instantiate one closure
    for both sizes, generic `Cursor::load`, `store_back` and `write_back`.
  * Result, by CPU time against the same tree without it (6-8 rounds):
    dup 0.96 (min 0.98), seq 0.99 (0.99), rand 1.05 (1.01), del 1.02
    (1.02).
  * At most a few percent on `DUPSORT`, nothing elsewhere: reverted.
* **Keeping the dirty list's own vectors between transactions**, to make
  up for the slab's extra vectors in one-put transactions: not needed.
  `txn` measured 1.00 (min 0.99) without it.

## Left for later, and why

* **The read-cursor rebuild** of every positioning (`read_op`) is now
  17.6 % of a sequential put.
  * What it costs: `Cursor::new` fills 32 page slots (64 stores);
    `store_back` copies the 64-byte `ki` array right after the read
    operation's 2-byte stores to it (a store-forwarding stall: 4 % of
    `mdb_put` sat on that copy); `MDB_APPEND` builds and drops an
    `MDB_NOTFOUND` error per put.
  * Short stacks did not pay (see above).
  * What would: running read operations on the write cursor's own stack
    of `(pgno, slot)` (a read cursor generic over where its pages come
    from), which is a change to the read path. Copying only the levels in
    use is equivalent only if no code reads `ki` beyond `mc_snum`. That
    appears to hold (C leaves those entries uninitialised), but it would
    need a careful audit.
* **The descent of each positioning** looks pages up by number (three per
  put in these trees). C does a `mdb_mid2l_search` per level too. Slots
  cannot help a fresh cursor, and the per-list hints already catch the
  root and branch pages.
* **Copies kept because a page cannot stay borrowed.** The whole-page
  copy in `page_split` with its `copy_ptrs` vector (1-2 % of `dup_put`
  together) and `dup_prepare`'s copy of the old node data (2 % of
  `dup_put`) stay.
  * Why: stable slots do not lift the borrow. A `&[u8]` into a dirty
    buffer is a borrow of the dirty list, and it cannot live across the
    `&mut self` calls in between, which may allocate pages (and so grow
    the slab).
  * Possible but not done: taking the buffer out of its slot for the
    duration. While it is out, every lookup of that page must know where
    it is. That includes the recursive parent split, and `page_alloc`
    reading the free DB, which may be the tree being split. Getting that
    wrong silently reads a stale page.
* **The small per-transaction vectors** of `txn_nosync`: left as the task
  said.
* **The whole-page copy on touch** (16-33 % of the random workloads) is
  the only large remaining item, and changing it changes unused bytes in
  the file. That is the one layout change that would help here; it was
  not made (see `RESULTS.md`). This work needed no layout change.

## Tests

From `rust/`, on the final tree:

* `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings` and `RUSTDOCFLAGS="-D warnings" cargo doc --workspace
  --no-deps`: clean.
* `cargo test --workspace --no-fail-fast` (macOS): 57 test binaries and
  doc-test runs, 637 passed, 0 failed, 7 ignored.
* `cargo test -p rlmdb --release -- --ignored` (macOS): passed. It
  includes the full-size spill differential against C (10.3 s), the
  spilling soak of `core/spill.rs`, and `write_soak` (20 s). In addition,
  `SOAK_SPILL=1 SOAK_SECS=30` `write_soak` passed, with 75 commits, 13
  aborts, 11 275 nested transactions and 463 540 operations.
* Linux (`scripts/linux-test.sh test -p rlmdb -p rlmdb-interop`, Docker
  through Lima, aarch64, 4 KiB pages): 36 test binaries and doc-test runs,
  556 passed, 0 failed, 7 ignored.

New tests:

* `core/dirty.rs`:
  * `slots_are_stable_hints`: slots survive other inserts and removals;
    emptied, reused and `NONE` slots never match the wrong page, not even
    `P_INVALID`; `retain` and `clear` empty them.
  * `merge_keeps_the_parents_slots`: a child's commit keeps the parent's
    slots and the slots of new pages work.
* `core/spill.rs`:
  * `cursor_slots_survive_spills_and_children`: a parent's two
    `CursorId`s write through children that copy the parent's pages,
    spill, move the cursors over pages spilled by the parent, delete
    under a cursor, and commit or abort (alternately). The parent spills
    pages its cursors touched before and writes through the cursors after
    every child. Every write is checked against a model, the positions
    after an abort against those before it, and the result by the page
    audit.
  * `cursor_returns_to_a_spilled_page`: builds the one case where a
    cursor's slot hint is stale and the slot holds something: the cursor
    leaves a dirty leaf, the leaf is spilled, then written again
    (unspilled into a different slot), then the cursor comes back and
    reads and writes through its old hint.

To check that these tests would catch a bad hint, two mutations were run
and then removed:

* `DirtyPages::at` matching pages it does not hold: caught by both new
  spill tests, `merge_keeps_the_parents_slots` and most of the older spill
  tests;
* emptied slots keeping their old page and buffer: caught by
  `cursor_returns_to_a_spilled_page` and `slots_are_stable_hints`. The
  random nested and spill tests did not catch it, because a tracked
  cursor's own pages are never spilled.

`tests/nested_txn.rs` and `tests/write_differential.rs` already cover the
nested cases against C: `diff_nested_*`, `diff_parent_cursors_in_children`
and `diff_nested_script` run parent cursors through children that commit
and abort, in lock step with C. Spills against C are covered only by the
ignored full-size test, hence the two unit tests under a lowered dirty
limit above.

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 70 passed; 0 failed; 1 ignored; 0 filtered out; finished in 2.55s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.69s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.31s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.99s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.08s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.24s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.25s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.75s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.07s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 13.15s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 13 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.02s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.22s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.22s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 22.03s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.53s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.03s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.09s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    lmdb src/lib.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.64s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.88s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.42s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.19s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.43s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.03s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.20s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.63s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.68s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.40s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.18s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.02s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.47s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 18.82s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.25s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.19s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.54s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.40s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.46s
    dump tests/dump.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.26s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.46s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.43s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.26s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.50s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
    doc-tests rlmdb_tools: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s

macOS, cargo test -p rlmdb --release -- --ignored:
    rlmdb src/lib.rs: ok. 1 passed; 0 failed; 0 ignored; 70 filtered out; finished in 20.03s
    write_differential tests/write_differential.rs: ok. 2 passed; 0 failed; 0 ignored; 18 filtered out; finished in 10.55s
    write_soak tests/write_soak.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 20.47s

Linux, scripts/linux-test.sh test -p rlmdb -p rlmdb-interop:
    rlmdb src/lib.rs: ok. 68 passed; 0 failed; 1 ignored; 0 filtered out; finished in 2.44s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.99s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.54s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.41s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.07s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.08s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.10s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.13s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.70s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 17.23s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 13 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.62s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.15s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.29s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 24.09s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.52s
    write_review tests/write_review.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.05s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 39 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.19s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.21s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.25s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.40s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 10.26s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.77s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    rpeer tests/rpeer.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 14.18s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.90s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.55s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.45s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.46s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.42s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
```

</details>
