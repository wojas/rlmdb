# Task P2: the remaining write-path constant costs

Cuts what the write path costs per operation and per transaction outside
the B-tree work itself: positioning for `mdb_put`/`mdb_del` on the write
cursor's own stack, fewer page lookups and allocations per put and per
commit, overflow runs built with their data in place and recycled, and
cheaper pointer moves in page edits. All library changes are in
`rlmdb` (`src/core/`, `src/idl.rs`), written on `c9dc6d7` and
rebased onto `c679eb2` (LMDB 1.0 data and lock file and the process
default format, dual-format stages 2 to 4; see "After the rebase" at the
end). Same
results, same file bytes, same free list, same spill decisions: the
write differential against C (including the full-size spill test), the
interop matrix, the model test and `mtest` pass without changes to their
expectations. No new `unsafe`, no `MaybeUninit`; the public API is
unchanged. `DUPSORT` paths were left alone (they keep the read-cursor
route).

**Summary.** macOS, interleaved A/B against the base build (median
ns/op): sequential appends **-27 %**, 64 KiB puts **-46 %**, one-put
transactions **-14 %**, overwrites **-12 %**, `ls_update` -6 %, 4 KiB puts
-5 %, random puts, deletes and `mixed_writer` -3 %; reads unchanged.
Linux (the shared VM, 4 KiB pages): appends -35 %, 64 KiB puts -22 %,
4 KiB puts (overflow runs there) -6 %, one-put transactions -4 %.
Instructions per operation, counted by callgrind with 4 KiB pages as on
the x86 server: appends -20 %, random puts -15 %, overwrites -13 %,
deletes -10 %, one-put transactions -24 %, 4 KiB puts -28 %, 64 KiB puts
-33 % (plus page faults and `madvise` calls, which callgrind does not
count). Against C on macOS, after: appends 1.60x (was about 2.0x),
64 KiB puts **0.59x** (1.3x), one-put transactions 1.22x (1.3x),
overwrites 0.80x, 4 KiB puts 1.01x (1.2x), deletes 1.01x (1.1x),
`mixed_writer` 1.03x (1.15x), `ls_update` 0.83x (1.04x).

## What changed

### 1. Positioning on the write cursor's own stack (item 1)

* `core/wcursor.rs`: `RwTxn::seek_key` (`mdb_cursor_set` with
  `MDB_SET`) and `RwTxn::seek_last` (`mdb_cursor_last`) for a cursor that
  `RwTxn::seeks_in_place`: an unpositioned main cursor of a tree without
  duplicates, i.e. the cursor `mdb_put` and `mdb_del` open for every
  call (and a fresh user cursor). They run on the write cursor itself
  (`descend`, a port of `mdb_page_search` + `mdb_page_search_root` for
  an unpositioned cursor, and `read_item`, the item read of
  `read_position` / the end of `cursor_set`): the same steps in the same
  order as the read path, writing page numbers, indices and flags
  straight into the `WCursor`, so the state afterwards, the result and
  every error are the same as through `RwTxn::read_op`, also when a step
  fails (page numbers are taken from the page headers, as `store_back`
  does). What goes away per put or delete: building the two read
  cursors, loading them, storing them back, and the `MDB_NOTFOUND` error
  that `MDB_APPEND` used to build and drop for every put.
* Each level's page is first looked for through the slot the cursor
  already has for that level (the cursor of `mdb_put` keeps its slots
  from put to put, and consecutive operations mostly pass the same root
  and branch pages), and the slot found is stored: `TxnCore::page_and_slot`
  (`core/txn.rs`). The touch that follows then finds every level through
  its slot.
* `core/cursor.rs`: the binary search of `mdb_node_search` is now the
  free function `search_page` (with `key_at`/`cmp_at`), shared by
  `Cursor::node_search` (a thin wrapper) and the write path. It stays one
  out-of-line function with its probes inlined, as `node_search` was.
* Callers: `put_position` (`core/cursor_put.rs`; it now returns the
  position directly, `MDB_APPEND` included) and `del0_with`
  (`core/cursor_del.rs`). Everything else (initialized cursors,
  sub-cursors, `DUPSORT` trees, `MDB_GET_BOTH`) takes the read path as
  before.
* `cursor_touch` (`core/wcursor.rs`) checks all levels first in one tight
  pass through the cursor's slots (the common case: every page already
  dirty here) and takes the per-level path only from the first level
  that needs it.

### 2. Fewer lookups in the leaf edits (items 1 and 4)

* `node_add_to` (`core/cursor_put.rs`) looks the page up once, for
  writing, and adds the node through that reference; only data that
  needs a new overflow run looks it up again after allocating it. Before:
  one lookup for reading, one for writing.
* `put_current`: inline data replaced by data of the same size through a
  main cursor (the overwrite case) is done with one lookup of the
  touched leaf; everything else, including whatever is corrupt, takes
  the old steps from the same state.
* `mdb_put` (`core/txn_rw.rs`) opens its cursor from the DBI entry it
  already read (`TxnDb::dup_order`, `core/dbi.rs`) instead of reading
  the DBI table again.
* `page_ops::insert_node` moves the higher node pointers with one
  `copy_within` and `page_ops::delete_node` rewrites the pointer array in
  one pass over the stored values, after one bounds check, instead of a
  bounds-checked read and write per pointer (C's loops over `mp_ptrs`;
  the same stored values, including C's `indx_t` truncation). A page
  whose `mp_lower` points past its end now fails before anything is
  changed (before: part-way); the operation fails with the same error
  either way, and the transaction is failed.

### 3. Overflow runs (item 2)

* `PageInit` (`core/freelist.rs`): a new page's buffer starts
  zero-filled, as a copy of a page (`mdb_page_touch`), or, new, as
  `Data`: the value right after a zeroed header and zeros after it.
  `node_add_to` creates an overflow run that way (`page_new_with`,
  `page_alloc_with`, `DirtyPages::alloc_data`): every byte is written
  once instead of zero-filling the run (`calloc`, page faults included)
  and copying the value over it. The bytes are the same. C's
  `mdb_page_malloc` zeroes only the last page of a run (which the value
  does not reach) and `mdb_node_add` copies the value. `MDB_RESERVE`
  keeps the zero fill, and a reused loose page keeps the rest of its old
  contents, as it did.
* `PagePool` (`core/dirty.rs`) now recycles overflow-run buffers too,
  per length: between write transactions it keeps as many runs of each
  length as the last transaction asked for (the single-page policy of
  task W), at most `RUN_POOL_MAX_BYTES` (128 MiB) in all. C frees runs
  (`mdb_dpage_free`). Every run buffer is zero-filled or completely
  overwritten before use, as before.

### 4. Per transaction (item 3)

* `SpareTxn` (`core/txn_rw.rs`, kept in `WriterState::spare`,
  `core/env.rs`): the containers of a top-level write transaction (the
  DBI table, the dirty list with its index, slab and free-slot list, the
  free-page and loose lists, the cursor arena, the scratch buffers, the
  commit's page list) are kept by the environment between write
  transactions, emptied, under the writer lock, and the next
  transaction fills the same memory. C allocates its write transaction
  with its dirty list and free list once per environment (`me_txn0`,
  `me_dirty_list`, `me_free_pgs`). Bounded: lists keep room for 4096
  entries, the arena 16 slots; the page buffers follow the pool's
  policy. A one-put transaction made 13 allocations; now it makes 2.
* `write_meta` (`core/commit.rs`): the map size comes from the meta page
  the transaction began from (`WriteState::base_mapsize`; only a writer
  changes meta pages, and this one holds the writer lock), and of the
  meta page about to be overwritten only `mm_last_pg` and `mm_txnid` are
  read, as C keeps them in `metab` (`Environment::meta_ids`). Before:
  two full validated copies of meta pages per commit, made with volatile
  byte reads.
* `WriterState::last_meta`: the meta page this process wrote last. The
  next write transaction begins from it, without reading meta pages,
  when the lock file still names its txnid: no other writer has
  committed since (a commit needs the writer lock and publishes a new
  txnid; dead-owner recovery republishes the newest meta's txnid), so it
  is the newest meta page. Not with `MDB_NOLOCK`.
* The commit's page writes (`page_flush`, `write_pages`, `write_runs`,
  `prepare_page`): one pass over the dirty list prepares the pages (as
  `mdb_page_flush`'s loop does) and records their slots, the writes
  reach every buffer through its slot, and the `pwritev` batch is a
  fixed array on the stack (C's `iov[MDB_COMMIT_PAGES]`). Before: three
  passes, two searches per page and three temporary vectors. Spilling
  keeps its page-number interface.
* `mdb_freelist_save` (`core/freelist.rs`): the free list is encoded
  straight into the space reserved for it (`Idl::encode_into`, C's
  `memcpy`), and `me_pghead` records into a scratch buffer
  (`Idl::encode_append`), instead of building temporary vectors.
  `mdb_page_alloc` merges a reclaimed free-DB record into `me_pghead`
  straight from its bytes, in place and from the back as C's
  `mdb_midl_xmerge` does (`Idl::xmerge_encoded`, `Idl::xmerge`), instead
  of decoding it into a vector and merging into a third one.

## What changed, per file

* `core/wcursor.rs`: new `RwTxn::seeks_in_place`, `RwTxn::seek_key`,
  `RwTxn::seek_last`, `descend`, `read_item`; `cursor_touch` (the check
  pass over the slots); `open_slot` (through `TxnDb::dup_order`); a
  test module (below).
* `core/cursor.rs`: `search_page`, `key_at`, `cmp_at` (free functions,
  from `Cursor::node_search`, `Cursor::key_at`); `Cursor::node_search`
  and `Cursor::cmp_at` are wrappers.
* `core/txn.rs`: `TxnCore::page_and_slot`.
* `core/cursor_put.rs`: `put_position` (positions in place, returns the
  position without building an error), `put_current` (the one-lookup
  overwrite), `node_add_to` (one lookup, one insertion point),
  `page_new_with`, the overflow branch of `node_add_to` (`PageInit::Data`).
* `core/cursor_del.rs`: `del0_with` (positions in place).
* `core/page_ops.rs`: `insert_node`, `delete_node` (pointer moves); a
  test.
* `core/freelist.rs`: `PageInit`, `page_alloc_with` (was
  `page_alloc_init`); `reclaim_run` (merges records in place);
  `freelist_save0` (encodes in place).
* `core/dirty.rs`: `PagePool` (overflow runs, `RunBufs`,
  `RUN_POOL_MAX_BYTES`), `DirtyPages::alloc_data`, `for_each_mut`,
  `recycle`; module documentation; two tests changed or added.
* `core/commit.rs`: `page_flush`, `write_pages`, `write_runs`,
  `prepare_page`, `write_meta`.
* `core/txn_rw.rs`: `SpareTxn`, `SPARE_ENTRIES`, `SPARE_CURSORS`,
  `RwTxn::spare`, `RwTxn::wrote_meta`; `RwTxn::begin` and `end`;
  `WriteState::base_mapsize` and `WriteState::flush`; `mdb_put`.
* `core/env.rs`: `WriterState::spare` (replaces `pages`),
  `WriterState::last_meta`, `Environment::meta_ids`.
* `core/dbi.rs`: `TxnDb::dup_order`. `core/nested.rs`: the new
  `WriteState` fields.
* `idl.rs`: `Idl::xmerge` (in place, from the back), `xmerge_encoded`,
  `encoded_ids`, `encode_append`, `encode_into`, `shrink_to`
  (`Idl` and `Id2List`); `to_bytes` uses `encode_append`; tests.

## Instructions per operation

Most of the decisions were made on instruction counts, which the load
of the shared machine does not change (the runs varied by about 2 %).
A scratch harness (not committed) does the bench's write loops on the
same data (`bench/src/data.rs`, and a C twin built on `cbench.c`'s
generator), built three times: against the base library, against the
working tree, and against `mdb.c`.

* **macOS** (Apple M1 Pro, 16 KiB pages): instructions and cycles of the
  process (user and kernel) from `proc_pid_rusage`, around the put or
  delete calls of each batch (commits excluded, except `txn`, which
  times whole one-put transactions).
* **Linux** (the aarch64 container of `scripts/linux-test.sh`, 4 KiB pages
  as on the x86 server): user instructions from `valgrind --tool=callgrind`
  (installed in a throw-away container only), for the whole run divided
  by the operations, commits included. Overwrites and deletes are the
  run minus the same build as `rand_put`, per operation; the other rows
  include the harness's own work (generating keys, opening the
  environment), a few percent, the same for both Rust builds. Per-function
  counts from the same tool guided the work (see "Profiles").

macOS, per operation:

| workload | C | base | new | new/base | new/C |
|---|---:|---:|---:|---:|---:|
| `seq_put_append` | 1,149 | 2,574 | 2,120 | 0.82 | 1.85 |
| `rand_put` | 4,019 | 7,207 | 5,999 | 0.83 | 1.49 |
| `rand_overwrite` | 4,396 | 6,634 | 6,086 | 0.92 | 1.38 |
| `mixed_writer` puts (no readers) | 5,661 | 9,380 | 8,731 | 0.93 | 1.54 |
| `del_rand` | 6,063 | 10,387 | 9,698 | 0.93 | 1.60 |
| `put_4k` (inline values on 16 KiB pages) | 9,660 | 19,087 | 16,877 | 0.88 | 1.75 |
| `put_64k` put | 32,175 | 52,607 | 23,358 | 0.44 | 0.73 |
| `put_64k` commit, per put | 90,750 | 81,464 | 26,407 | 0.32 | 0.29 |
| `txn_nosync` (begin, put, commit; kernel included) | 53,748 | 82,518 | 69,484 | 0.84 | 1.29 |

Cycles (same runs) for the workloads that do not wait on memory:
`seq_put_append` 190 / 469 / 335 (C / base / new), `put_64k` put
22,815 / 28,677 / 10,037 and its commit 51,027 / 55,677 / 32,060,
`txn_nosync` 20,065 / 26,033 / 23,461. Page faults per `put_64k` put:
1.93 / 4.25 / 0.97. The random workloads take about as many cycles in all
three (4,100-5,700 per put or delete): on the M1 they wait on memory, which
is why their instruction savings show in time only partly here and more
on x86.

Linux (callgrind), user instructions per operation:

| operation | C | base | new | new/base | new/C |
|---|---:|---:|---:|---:|---:|
| append put (`seq_put_append`) | 1,405 | 2,576 | 2,061 | 0.80 | 1.47 |
| random put (`rand_put`) | 3,097 | 5,106 | 4,355 | 0.85 | 1.41 |
| overwrite (`rand_overwrite`) | 2,776 | 4,426 | 3,840 | 0.87 | 1.38 |
| delete (`del_rand`) | 3,359 | 6,599 | 5,971 | 0.90 | 1.78 |
| one-put transaction (`txn_nosync`) | 6,986 | 23,311 | 17,671 | 0.76 | 2.53 |
| 4 KiB put (`put_4k`), commit included | 4,319 | 7,369 | 5,282 | 0.72 | 1.22 |
| 64 KiB put (`put_64k`), commit included | 12,392 | 19,680 | 13,150 | 0.67 | 1.06 |

A one-put transaction made 13 allocations and frees (16 KiB pages; more
on Linux, where the small vectors grew through `realloc`); now it makes 2
(the reclaimed `me_pghead` list, which C allocates too). Syscalls per
transaction are the same in C and Rust, before and after (1 at begin, 5
at commit).

Step by step (macOS harness, instructions per operation, each against
the build before it; about 2 % run-to-run noise):

| change | workload: before -> after |
|---|---|
| positioning in place | appends 2,569 -> 2,211; random puts 7,228 -> 6,775; overwrites 6,634 -> 6,211; deletes 10,314 -> 9,911 |
| touch check over the slots | appends 2,211 -> 2,133; random puts 6,775 -> 6,727 |
| one lookup in `node_add_to` | appends 2,133 -> 2,069; random puts 6,727 -> 6,545 |
| `write_meta` reads | one-put transaction commit 62.5k -> 60.9k |
| spare containers | its put 15.3k -> 13.2k, its commit 60.9k -> 56.2k |
| commit writes through slots, free list encoded in place | its commit 56.2k -> 53.5k |
| overflow run built with its data | `put_64k` put 45.6k -> 37.6k |
| overflow runs recycled | `put_64k` put 37.6k -> 23.0k, its commit 89k -> 24.5k |
| pointer moves | random puts 6,568 -> 6,046; deletes 10,023 -> 9,664; `put_4k` 18,348 -> 16,957 |
| last meta reused, no placeholder containers | one-put transaction begin 4.8k -> 3.7k |
| one-lookup overwrite, one DBI read in `mdb_put` | overwrites 6,258 -> 6,090 |

## A/B: new against base (macOS)

`rlmdb-bench --baseline <bench of c9dc6d7> --rounds 3 --reps 3 --only
seq_put,rand_put,overwrite,overflow,delete,small_txn,mixed,ls`
(n = 1 000 000; both sides alternate per group, 9 samples per side;
ns/op as median (min)).

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, 16 KiB pages, APFS on
  the internal SSD; rustc 1.93.1 (Homebrew), Apple clang 17.0.0.
* The machine was shared with other jobs: load average 11.5 when the run
  started, 3.3 when it ended.

| workload | ops/rep | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|---:|
| seq_put_append | 1000000 | 165 (158) | 120 (117) | **0.73** | 0.74 |
| rand_put | 1000000 | 2450 (2339) | 2374 (2252) | **0.97** | 0.96 |
| rand_overwrite | 1000000 | 3817 (3381) | 3363 (3231) | **0.88** | 0.96 |
| put_4k | 50000 | 7076 (6560) | 6694 (6465) | **0.95** | 0.99 |
| get_4k | 1000000 | 489 (451) | 559 (553) | 1.14 | 1.23 |
| put_64k | 5000 | 26839 (25910) | 14515 (13910) | **0.54** | 0.54 |
| get_64k | 1000000 | 218 (213) | 221 (213) | 1.02 | 1.00 |
| del_rand | 1000000 | 3405 (3179) | 3295 (3176) | **0.97** | 1.00 |
| txn_nosync | 50000 | 8206 (7886) | 7045 (6877) | **0.86** | 0.87 |
| txn_sync | 1000 | 10725637 (10356318) | 10481009 (10252973) | 0.98 | 0.99 |
| mixed_writer | 50000 | 5658 (5514) | 5501 (5154) | **0.97** | 0.93 |
| mixed_readers | 83292160 | 3.4 (3.4) | 3.5 (3.4) | 1.02 | 1.00 |
| ls_scan | 5000000 | 14.6 (13.8) | 14.5 (14.3) | 0.99 | 1.04 |
| ls_update | 50000 | 5543 (5188) | 5223 (4582) | **0.94** | 0.88 |

The read workloads (`get_4k`, `get_64k`, `mixed_readers`, `ls_scan`) run
none of the changed write code; they share the search with the write
path, which was only restructured (`search_page`). `get_4k` came out
slower here; rerun right after with the `read` group too (`--rounds 4
--only overflow,read`, load 4-5) it was 1.09 by the median and 1.04 by the
minimum, with `rand_get` (the same lookup) at 0.96, `get_64k` 1.01 and the
scans 1.00. Counted by the harness, a lookup runs fewer instructions than
before (1,841 against 1,934 per `mdb_get` of a 100-byte value, 1,723
against 1,789 for 4 KiB values) in the same cycles. In the first A/B
(an intermediate build, same read code) `get_4k` was 0.98. So this is
layout or noise, not read-path work. `txn_sync` is bound by
`F_FULLFSYNC`.

An earlier A/B on an intermediate build (all but the pointer moves, the
meta cache and the one-lookup overwrite; load 11-16) gave: appends 0.73,
`put_64k` 0.55, `txn_nosync` 0.90, `mixed_writer` 0.93, `del_rand` 0.93,
`rand_put`, `rand_overwrite` and `put_4k` 0.97.

## Rust against C, after (macOS)

`rlmdb-bench --rounds 3 --reps 3 --only
seq_put,rand_put,overwrite,overflow,delete,small_txn,mixed,ls` with the
final build, right after the A/B (load average 4.5-7). "Before": the
macOS column of the task brief.

| workload | C ns/op | Rust ns/op | Rust/C (median) | Rust/C (min) | before |
|---|---:|---:|---:|---:|---:|
| seq_put_append | 75.6 (73.5) | 121 (117) | **1.60** | 1.59 | ~2.0 |
| rand_put | 2458 (2359) | 2334 (2181) | **0.95** | 0.92 | |
| rand_overwrite | 4133 (3413) | 3321 (3124) | **0.80** | 0.92 | ~1.0 |
| put_4k | 6641 (5685) | 6686 (6179) | **1.01** | 1.09 | ~1.2 |
| get_4k | 602 (495) | 514 (466) | 0.85 | 0.94 | |
| put_64k | 24651 (21946) | 14591 (13825) | **0.59** | 0.63 | ~1.3 |
| get_64k | 232 (220) | 228 (215) | 0.98 | 0.98 | |
| del_rand | 3210 (3015) | 3248 (3072) | **1.01** | 1.02 | ~1.1 |
| txn_nosync | 5988 (5905) | 7282 (6941) | **1.22** | 1.18 | ~1.3 |
| txn_sync | 10607541 (9381046) | 10559032 (10220912) | 1.00 | 1.09 | |
| mixed_writer | 5289 (5040) | 5462 (4741) | **1.03** | 0.94 | ~1.15 |
| mixed_readers | 4.3 (4.2) | 3.4 (3.3) | 0.80 | 0.79 | |
| ls_scan | 17.2 (15.7) | 13.7 (13.5) | 0.79 | 0.86 | |
| ls_update | 5788 (5450) | 4806 (4403) | **0.83** | 0.81 | 1.04 |

## Linux (container, 4 KiB pages)

The Lima VM behind `scripts/linux-test.sh` (8 vCPUs, 12 GB, on the same
Mac), Debian 12 `rust:1-bookworm` container, glibc, rustc 1.99.1, gcc
12.2.0, environments on the container's tmpfs `/tmp`, 4 KiB pages. The
VM is shared with other agents' work and noisy: `ls_scan`, which runs
none of the changed code, moved by 5-7 % between the two binaries.

Rust against C, `scripts/linux-test.sh run --release -p rlmdb-bench --
--dir /tmp/lmdb-bench --only seq_put,overflow,small_txn,ls --rounds 3
--reps 3`. "Before": one round of the base at the start of this task
(the `ls` rows: the L report).

| workload | C ns/op | Rust ns/op | Rust/C (median) | Rust/C (min) | before |
|---|---:|---:|---:|---:|---:|
| seq_put_append | 99.9 (98.2) | 193 (178) | **1.93** | 1.81 | 3.03 |
| put_4k | 4451 (4291) | 4270 (4233) | **0.96** | 0.99 | 1.30 |
| get_4k | 362 (334) | 527 (480) | 1.46 | 1.44 | 1.32 |
| put_64k | 39586 (37367) | 41997 (38531) | **1.06** | 1.03 | 1.33 |
| get_64k | 198 (194) | 339 (330) | 1.71 | 1.70 | 1.53 |
| txn_nosync | 4154 (4029) | 8099 (7569) | **1.95** | 1.88 | 1.83 |
| txn_sync (tmpfs) | 3260 (3232) | 6539 (6292) | 2.01 | 1.95 | 2.21 |
| ls_scan | 41.2 (37.7) | 33.2 (32.8) | 0.81 | 0.87 | 0.89 |
| ls_update | 4526 (4517) | 5142 (5108) | **1.14** | 1.13 | 1.17 |

The `get_*` rows are reads, untouched here; Rust's times are about what
they were (527 and 339 ns against 609 and 345 in the base run), C's
moved (362 and 198 against 462 and 226).

A/B against the base on Linux (`--baseline` with the base bench built in
the container, `--only seq_put,overflow,small_txn,ls --rounds 2 --reps
3`; the `ls` group again on its own with 3 rounds):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| seq_put_append | 295 (282) | 191 (188) | **0.65** | 0.67 |
| put_4k | 4880 (4750) | 4600 (4445) | **0.94** | 0.94 |
| get_4k | 508 (497) | 495 (485) | 0.97 | 0.98 |
| put_64k | 54291 (50059) | 42437 (40831) | **0.78** | 0.82 |
| get_64k | 359 (340) | 351 (333) | 0.98 | 0.98 |
| txn_nosync | 7976 (7603) | 7660 (7501) | **0.96** | 0.99 |
| txn_sync (tmpfs) | 6568 (6427) | 6529 (6516) | 0.99 | 1.01 |
| ls_scan (3 rounds) | 33.2 (32.9) | 34.9 (33.1) | 1.05 | 1.01 |
| ls_update (3 rounds) | 5126 (5065) | 5285 (5105) | 1.03 | 1.01 |

On Linux `txn_nosync` gains far less in time than its user instructions
(-24 %) suggest: a transaction takes 7.7 µs against C's 4.2 µs, and the
10.7k user instructions Rust still runs more than C account for perhaps
half of that difference. The rest was not found (there is no profiler in
the container; both sides make the same syscalls on macOS). `ls_update`
did not move measurably on Linux (macOS: 0.94 by the median, 0.88 by
the minimum).

## Profiles

Time profiles (`sample` and `xctrace` on macOS) of the write loops were
flat: in `seq_put_append` the read-cursor round trip was about 15 % and
the page lookups by number about 9 %, everything else a few percent per
function. The per-function instruction counts of callgrind (Linux, 4 KiB
pages; inlined code summed per function) against C's were more useful:

* **Append put** (after the in-place positioning; instructions per put,
  Rust against C): node add 294 against 132 (`mdb_node_add`), the put
  functions 376 against 149 (`_mdb_cursor_put`), the touch 251 against
  103 (`mdb_cursor_touch` + `mdb_page_touch`), `mdb_put` with the DBI
  checks and the cursor slot 311 against 85 (`mdb_put` +
  `mdb_cursor_init`), write-side page accessors 178. The descent itself
  was already cheaper than C's, 379 against 650: C binary-searches the
  dirty list at every level (`mdb_mid2l_search`), Rust finds the pages
  through their slots. This led to the touch check, the single lookup in
  `node_add_to` and the pointer moves.
* **One-put transaction** (base, per transaction): `read_meta` 2,975 (three
  validated copies of meta pages, read with volatile byte reads), the
  allocator about 3,250 against C's 250, the read-cursor round trips of
  `mdb_freelist_save` about 1,700, `memcpy` 2,297 against 917 (whole-page
  copies of the touched pages, mostly near-empty free-DB pages, which C
  copies only in part; kept). This led to the spare containers, the meta
  reads and the commit's page writes.
* **`put_64k`** (macOS, `sample`): the base's put was 80 % `__bzero`
  (zero-filling the fresh `calloc`ed run, page faults included) and 10 %
  copying the value; its commit 74 % `pwritev` and 15 % `madvise` (freeing
  the runs). C's put copies the value into fresh `malloc`ed memory (the
  page faults land there) and its commit frees the runs too (`madvise`).
  With runs built in place and recycled, a put takes 0.97 page faults
  instead of 4.25, and the `madvise` calls are gone.
* **Deletes and overwrites**: after the changes, the remaining excess over
  C is the plumbing listed under "What is left": `node_del` 689 against
  424 before the pointer moves, `db_of` 179 per delete, `mdb_rebalance`
  192 against 84, `mdb_cursor_del0` 237 against 54, page accessors about
  700.

## Tried and reverted

* **`search_page` inlined everywhere** (`#[inline(always)]`, the first
  version of the shared search). LLVM copied it into five callers and
  moved its probe closures out of line, one call per probe: random puts,
  overwrites and deletes ran 120-150 instructions more per operation
  than before the change. It is now one out-of-line function with the
  probes inlined, as `node_search` was, and `Cursor::node_search` is an
  inlined wrapper, so the read path calls the same code as before.
* **`#[inline]` on a dozen small accessors** (`cur`, `cur_mut`, `db_of`,
  `cmp_of`, `db_mut`, `page_of`, `top_page`, `refreshed`, ...): appends
  -1 %, deletes +1 % in instructions, within the counter's noise.
  Reverted; the accessors' cost is the work they do (an arena lookup, a
  `RefCell` borrow and a 48-byte copy), not the call.
* **`node_add_to` with a fast path and the general path each inserting
  the node.** With two calls `page_ops::insert_node` was no longer
  inlined: +130 instructions per random put (callgrind). The final
  version has one insertion point and looks the page up again only after
  allocating an overflow run.
* **A forward merge from the encoded free-DB record** instead of C's
  merge from the back: the same instruction count within noise; the
  in-place merge was kept, since it needs no new vector.
* **The pool of overflow runs trimmed by total bytes** (keep as many
  bytes of runs as the last transaction used): it dropped runs of the
  lengths that were used and kept others. Replaced by the per-length
  policy.
* Not a code change, but a method that did not work: single-stepping
  with lldb (as in task L) needs developer mode, which is off on this
  machine. Instruction counts came from the processor's counters per
  process (macOS) and from callgrind in the Linux container instead.

## What is left, and why

* **The put and delete plumbing.** Per random put, overwrite or delete,
  Rust still runs some 600-900 instructions more than C outside the
  B-tree search: the chain `mdb_put` -> `put_at` -> `put_item` ->
  `node_add_to`, the DBI record copies (`db_of`: a `RefCell` borrow and a
  48-byte copy, about five per delete), page access through the cursor
  arena (`page_of`, `page_mut_of`, `page_by_slot`), `spill_check`, the
  touch. C follows pointers for all of these. Each is 1-5 % of an
  operation; together they are why `rand_overwrite` and `del_rand` stay
  above C on x86, where these instructions are not hidden behind cache
  misses as they are on the M1. Removing them means restructuring the
  put path around one borrowed view of the cursor and its record, which
  is a larger change than this task.
* **Read-cursor round trips that remain**: initialized cursors (user
  cursors, the cursor of `mdb_freelist_save` after its first operation,
  the sibling moves of `mdb_rebalance`), sub-cursors and `DUPSORT` trees.
  `cursor_set` on an initialized cursor has the "already on the right
  page" branch, and `cursor_first`/`cursor_next` would be needed too, to
  cover `mdb_freelist_save` (about 1.5k instructions per commit).
* **The whole-page copy on touch** (`memcpy`: 2.4k instructions per
  one-put transaction on Linux against C's 0.9k, which copies only the
  used part): left as the task says.
* **The meta page read at begin** when another process committed last:
  a validated copy with volatile byte reads (about 1k instructions);
  making it cheaper means changing the `unsafe` reader in `os/`.
* **Deletes**: `mdb_rebalance` (192 instructions per delete against C's
  84) and `mdb_cursor_del0` (237 against 54) were not changed.

## Behaviour and memory

* Same file bytes: the positioning in place leaves the positions the read
  path leaves (checked by a test against it); the pointer moves write the
  same stored values; a new overflow run holds the same bytes as before;
  recycled buffers are zero-filled or completely overwritten; the commit
  writes the same pages in the same order and batches; the free-DB
  records are encoded to the same bytes and merged in the same order. The
  free list and the spill decisions (the dirty-room accounting) are
  unchanged. The write differential against C, which checks layouts down
  to page numbers, passes unchanged, including the full-size spill test.
* Corrupt input: `insert_node`/`delete_node` now refuse a page whose
  `mp_lower` points past its end before changing it (the error and the
  failed transaction are as before). `node_add_to` looks its page up
  for writing before it allocates an overflow run (the page is always
  dirty there; were it not, the error would come before the allocation
  instead of after it). Everything else that is corrupt takes the old
  steps from the same state.
* Memory the environment keeps between write transactions, on top of
  task W's single-page pool (at most 256 MiB, trimmed to what the last
  transaction used): overflow-run buffers (at most 128 MiB, trimmed the
  same way, per length) and the emptied containers of the last
  transaction (lists with room for up to 4096 entries, up to 16 cursor
  slots, the scratch buffers).

## Tests

All from the repository root, on the final tree:

| platform | command | outcome |
|---|---|---|
| macOS | `cargo fmt --all -- --check` | clean |
| macOS | `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| macOS | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| macOS | `cargo test --workspace --no-fail-fast` | 59 test binaries and doc-test runs: 683 passed, 0 failed, 7 ignored |
| macOS | `cargo test -p rlmdb --release -- --ignored` | passed: `diff_spill_full_size` and `replay` (write differential), `core::spill::tests::soak`, `write_soak` |
| Linux | `scripts/linux-test.sh test -p rlmdb -p rlmdb-interop` (Docker through Lima, aarch64, 4 KiB pages) | 37: 597 passed, 0 failed, 7 ignored |

New and changed tests:

* `core::wcursor::tests::positioning_in_place_matches_the_read_path`:
  two fresh cursors, one positioned in place and one through `read_op`,
  for `MDB_SET` and for `mdb_cursor_last`: same result or error, same
  page numbers, indices (all 32), flags, depth. A three-level tree of
  400-byte keys, probes before, between, on and after the keys, an empty
  key; with the pages dirty, clean, dirty in the parent of a nested
  transaction and in the child, and spilled; and an empty tree. Not
  storing the leaf index in `seek_key` makes it fail.
* `core::page_ops::tests::pointer_moves_match_the_per_pointer_loop`: the
  new `insert_node`/`delete_node` against the old per-pointer loops, both
  layouts, pages of 0-11 nodes, every index, and a page whose `mp_lower`
  points past its end.
* `idl::tests::xmerge_in_place_matches_a_forward_merge` (lists of every
  length up to 6, with equal IDs, from a list and from an encoded record,
  and a short record) and `idl::tests::encode_into_and_append_match_to_bytes`.
* `core::dirty::tests::runs_are_kept_as_used_and_within_budget`, and
  `buffers_are_recycled_clean` extended to runs and `alloc_data`.

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 99 passed; 0 failed; 1 ignored; 0 filtered out; finished in 2.75s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.98s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.55s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.53s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.09s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.10s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.21s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.25s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.08s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 19.30s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.03s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.94s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.26s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 18.56s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.52s
    write_review tests/write_review.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.28s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.05s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.22s
    lmdb src/lib.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.86s
    capi_version tests/capi_version.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.87s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.78s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 13.31s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.04s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.13s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.02s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.74s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 40 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.81s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.70s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.35s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.67s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.31s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.87s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.41s
    rpeer tests/rpeer.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 18.14s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.02s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.85s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.37s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.40s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.31s
    dump tests/dump.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.07s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.73s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.33s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.13s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.41s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
    doc-tests rlmdb_tools: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s

macOS, cargo test -p rlmdb --release -- --ignored:
    rlmdb src/lib.rs: ok. 1 passed; 0 failed; 0 ignored; 99 filtered out; finished in 20.05s
    write_differential tests/write_differential.rs: ok. 2 passed; 0 failed; 0 ignored; 18 filtered out; finished in 19.78s
    write_soak tests/write_soak.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 20.70s

Linux, scripts/linux-test.sh test -p rlmdb -p rlmdb-interop:
    rlmdb src/lib.rs: ok. 97 passed; 0 failed; 1 ignored; 0 filtered out; finished in 3.11s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.17s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.50s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.27s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.07s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.12s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.09s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.68s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.02s
    robust_mutation tests/robust_mutation.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 18.71s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.39s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.39s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 134.12s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.52s
    write_review tests/write_review.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.95s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.05s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    interop src/lib.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    c_vs_c tests/c_vs_c.rs: ok. 40 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.30s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.04s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.81s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.81s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.77s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.24s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.49s
    rpeer tests/rpeer.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 14.49s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.00s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.72s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.97s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.39s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.25s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
```

</details>

## After the rebase

Rebased from `c9dc6d7` onto `25a3892` (the LMDB 1.0 data file and lock
file, dual-format stages 2 and 3), then onto `c679eb2` (stage 4: the
process default format, 1.0's `mdb_drop` of the main DB,
`mdb_cursor_is_db`, a `DUPFIXED` sub-DB offset fix; none of it touches the
paths changed here). What the rebase changed in this work:

* `MDB_APPEND` now positions with `cursor_last_for_append` on the read
  path, where an empty leaf yields no last key (the cursor stays before
  its first slot, `mc_ki` 0xffff, and the new key goes there:
  `KNOWN_C_BUGS.md` #32). `RwTxn::seek_last` does the same on the write
  cursor's own stack: it hands its closure `Option<&[u8]>`, sets `mc_ki` to
  0xffff and the flags as there for an empty leaf, and `put_position`
  treats `None` as `Ordering::Greater` on both paths before moving `mc_ki`
  on with `wrapping_add(1)`. The positioning test now compares
  `seek_last` with `cursor_last_for_append`, and runs for LMDB 0.9 and for
  1.0 (4 KiB pages, set with `page_size`), each asked for explicitly, so
  that `RLMDB_DEFAULT_FORMAT` does not change what it tests.
* `WriterState::pbuf_pad` (C's `me_pbuf->mp_pad`) stays a field of its own
  next to `pg_oldest`, outside `SpareTxn`: a write transaction loads it at
  begin and gives it back when it ends, committed or not, whatever
  happens to the spare containers.
* The environment builds its `WriterState` explicitly again, with a spare
  whose page pool has the environment's page size (`SpareTxn::new`); a
  begin that finds none (only after a leaked transaction) makes one the
  same way.
* `WriterState::last_meta` is cleared by `Environment::set_map_size`
  (which does not write a meta page, so this is only the simple rule), is
  used only with a lock file and only while the lock file names its txnid.
  The `MDB_meta` structure is the same in both formats; `meta_ids` finds
  it through `Layout::meta_offset()`.
* The one-lookup overwrite in `put_current` reads the node through the
  environment's layout (`PageRef::with_layout`, `node_data_range`) and only
  takes inline data of the same size (no `F_BIGDATA`, so the size of the
  1.0 overflow reference, 24 bytes against 8, never matters there); 1.0's
  run lengths from `op_pages` are untouched (that branch is below it).
  New overflow runs zero the format's page header (24 bytes in 1.0) and
  put the value after it.

All runs below with `RLMDB_DEFAULT_FORMAT` unset (so the bench's
environments are LMDB 0.9, as before).

Instructions per operation (macOS harness) against `c679eb2`, the same as
against `c9dc6d7` before: appends 2,555 -> 2,121 (0.83), overwrites
6,619 -> 6,065 (0.92), deletes 10,309 -> 9,572 (0.93), one-put
transaction begin 4,942 -> 3,756, put 15,489 -> 12,118, commit 62,379 ->
53,601, `put_64k` put 40,286 -> 23,499 and its commit 98,341 -> 26,021.

A/B against the bench of `c679eb2` (`--rounds 3 --reps 3 --only
seq_put,overwrite,overflow,small_txn,ls`; the machine was very busy when
it started, load average 21, and 3.3 at the end):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| seq_put_append | 165 (159) | 118 (117) | **0.72** | 0.74 |
| rand_overwrite | 3891 (3361) | 3242 (3171) | **0.83** | 0.94 |
| put_4k | 7249 (6679) | 12017 (6340) | (1.66) | 0.95 |
| get_4k | 540 (416) | 519 (399) | 0.96 | 0.96 |
| put_64k | 63895 (25108) | 37280 (13311) | (0.58) | 0.53 |
| get_64k | 220 (213) | 229 (208) | 1.04 | 0.98 |
| txn_nosync | 8235 (7714) | 7686 (6919) | **0.93** | 0.90 |
| txn_sync | 10369802 (9931825) | 10444470 (9914627) | 1.01 | 1.00 |
| ls_scan | 13.6 (13.3) | 13.8 (13.5) | 1.02 | 1.02 |
| ls_update | 5585 (5004) | 5219 (4214) | **0.93** | 0.84 |

A load burst hit the `overflow` group (both medians far above their
minima), so it was run again on its own right after (`--rounds 3 --only
overflow`, load 3.4):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| put_4k | 6900 (6399) | 6443 (6292) | **0.93** | 0.98 |
| get_4k | 443 (397) | 425 (393) | 0.96 | 0.99 |
| put_64k | 23191 (22095) | 13450 (13112) | **0.58** | 0.59 |
| get_64k | 210 (208) | 208 (205) | 0.99 | 0.99 |

The gains survive the rebase. (Against `25a3892`, at load 21-25 then 6-7,
the same A/B gave appends 0.76, `put_64k` 0.57-0.58, `txn_nosync` 0.91,
`put_4k` 0.91-1.04, `rand_overwrite` 1.04-1.21 by the median and
0.98-1.02 by the minimum, `ls_update` 1.00: on the M1 the overwrites and
`ls_update` wait on memory and move within the noise.)

Tests after the rebase onto `c679eb2`, from the repository root, with
`RLMDB_DEFAULT_FORMAT` unset:

| platform | command | outcome |
|---|---|---|
| macOS | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | clean |
| macOS | `cargo test --workspace --no-fail-fast` | 70 test binaries and doc-test runs: 787 passed, 0 failed, 8 ignored |
| macOS | `cargo test -p rlmdb --release -- --ignored` | passed: `diff_spill_full_size`, `replay`, `core::spill::tests::soak`, `write_soak` |
| Linux | `scripts/linux-test.sh test --workspace --no-fail-fast` | 70: 785 passed, 0 failed, 8 ignored |

On `25a3892`, two Linux workspace runs failed `c_databases::mtest3_dupsort`
once: C's own `cursorops` aborted (`mdb.c:5796: Assertion 'root > 1'`) on
the database that C's `mtest3`, seeded with the time, had written, and a
later round then differed after C's failed `count`. That test writes
with C only and compares read cursors; the same abort was seen once in
task W, before this work. Six reruns of the test and two full runs after
it passed. (A first Linux run had failed to link for lack of space on
the VM's disk, shared with other checkouts; this checkout's own scratch
there was removed, nothing else.)

<details><summary>The <code>test result</code> lines after the rebase</summary>

```
macOS, cargo test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 108 passed; 0 failed; 1 ignored; 0 filtered out; finished in 2.88s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.62s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.63s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    default_format tests/default_format.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.02s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.53s
    format_v10 tests/format_v10.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.66s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.09s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.07s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.21s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.26s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.11s
    robust_mutation tests/robust_mutation.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 21.59s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.02s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.86s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.26s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 12.52s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.53s
    write_review tests/write_review.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.28s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.05s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.21s
    lmdb src/lib.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    capi_format tests/capi_format.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.95s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.64s
    capi_version tests/capi_version.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.88s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.96s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.08s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.05s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.33s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.05s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.98s
    interop src/lib.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.86s
    rtools src/bin/rtools.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    audit10 tests/audit10.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.34s
    c_vs_c tests/c_vs_c.rs: ok. 40 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.53s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.35s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.32s
    capi10 tests/capi10.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 25.61s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.41s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.91s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.70s
    format10 tests/format10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 22.74s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    lockfile10 tests/lockfile10.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.17s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.30s
    rpeer tests/rpeer.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 18.00s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.17s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.69s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.72s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.40s
    tools10 tests/tools10.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.18s
    write_differential10 tests/write_differential10.rs: ok. 17 passed; 0 failed; 1 ignored; 0 filtered out; finished in 27.95s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.33s
    dump tests/dump.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.14s
    format tests/format.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.77s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.74s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.28s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.07s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.44s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
    doc-tests rlmdb_tools: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s

macOS, cargo test -p rlmdb --release -- --ignored:
    rlmdb src/lib.rs: ok. 1 passed; 0 failed; 0 ignored; 108 filtered out; finished in 20.11s
    write_differential tests/write_differential.rs: ok. 2 passed; 0 failed; 0 ignored; 18 filtered out; finished in 13.44s
    write_soak tests/write_soak.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 20.69s

Linux, scripts/linux-test.sh test --workspace --no-fail-fast:
    rlmdb src/lib.rs: ok. 105 passed; 0 failed; 1 ignored; 0 filtered out; finished in 3.15s
    c_concurrency tests/c_concurrency.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.06s
    c_databases tests/c_databases.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.59s
    core_api_c2 tests/core_api_c2.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    default_format tests/default_format.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    env_copy tests/env_copy.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.11s
    format_v10 tests/format_v10.rs: ok. 12 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.35s
    handbuilt tests/handbuilt.rs: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.07s
    model tests/model.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 30.12s
    nested_txn tests/nested_txn.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.10s
    pylmdb_patch_audit tests/pylmdb_patch_audit.rs: ok. 16 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.96s
    robust_io_errors tests/robust_io_errors.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.04s
    robust_mutation tests/robust_mutation.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 20.57s
    robust_send_sync tests/robust_send_sync.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_env tests/rust_env.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rust_writes tests/rust_writes.rs: ok. 23 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.36s
    write_api tests/write_api.rs: ok. 14 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.38s
    write_differential tests/write_differential.rs: ok. 18 passed; 0 failed; 2 ignored; 0 filtered out; finished in 21.37s
    write_interop tests/write_interop.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.53s
    write_review tests/write_review.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.13s
    write_smoke tests/write_smoke.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.06s
    write_soak tests/write_soak.rs: ok. 0 passed; 0 failed; 1 ignored; 0 filtered out; finished in 0.00s
    rlmdb_bench src/main.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.23s
    lmdb src/lib.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    capi_format tests/capi_format.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.78s
    capi_read tests/capi_read.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.13s
    capi_version tests/capi_version.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.87s
    capi_write tests/capi_write.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.77s
    concurrency tests/concurrency.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.83s
    cross_process tests/cross_process.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.86s
    mtest tests/mtest.rs: ok. 8 passed; 0 failed; 0 ignored; 0 filtered out; finished in 6.15s
    robust_capi tests/robust_capi.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.29s
    tools tests/tools.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.91s
    interop src/lib.rs: ok. 7 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    rpeer src/bin/rpeer/main.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.14s
    rtools src/bin/rtools.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    audit10 tests/audit10.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.23s
    c_vs_c tests/c_vs_c.rs: ok. 40 passed; 0 failed; 0 ignored; 0 filtered out; finished in 3.18s
    c_writes_conrust_reads tests/c_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 5.04s
    c_writes_rust_reads tests/c_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 4.76s
    capi10 tests/capi10.rs: ok. 5 passed; 0 failed; 0 ignored; 0 filtered out; finished in 29.32s
    conrust_vs_conrust tests/conrust_vs_conrust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 8.09s
    conrust_writes_c_reads tests/conrust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.81s
    conrust_writes_rust_reads tests/conrust_writes_rust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 11.14s
    format10 tests/format10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 21.31s
    layout tests/layout.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.01s
    lockfile10 tests/lockfile10.rs: ok. 4 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.09s
    oracle10 tests/oracle10.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.52s
    rpeer tests/rpeer.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 14.29s
    rust_vs_rust tests/rust_vs_rust.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 9.92s
    rust_writes_c_reads tests/rust_writes_c_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 7.11s
    rust_writes_conrust_reads tests/rust_writes_conrust_reads.rs: ok. 33 passed; 0 failed; 0 ignored; 0 filtered out; finished in 10.37s
    three_peers tests/three_peers.rs: ok. 1 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.35s
    tools10 tests/tools10.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 1.08s
    write_differential10 tests/write_differential10.rs: ok. 17 passed; 0 failed; 1 ignored; 0 filtered out; finished in 38.44s
    rlmdb_tools src/lib.rs: ok. 10 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_copy src/bin/mdb_copy.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_drop src/bin/mdb_drop.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_dump src/bin/mdb_dump.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_load src/bin/mdb_load.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    mdb_stat src/bin/mdb_stat.rs: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
    copy_drop tests/copy_drop.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 2.53s
    dump tests/dump.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.28s
    format tests/format.rs: ok. 6 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.46s
    load tests/load.rs: ok. 9 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.74s
    readers tests/readers.rs: ok. 2 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.09s
    stat tests/stat.rs: ok. 11 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.20s
    doc-tests rlmdb: ok. 3 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.26s
    doc-tests interop: ok. 0 passed; 0 failed; 3 ignored; 0 filtered out; finished in 0.00s
    doc-tests rlmdb_tools: ok. 0 passed; 0 failed; 0 ignored; 0 filtered out; finished in 0.00s
```

</details>
