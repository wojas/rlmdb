# lmdb-bench results

Rust rewrite (`rlmdb`) against the C library (0.9.36), stage 4 task P:
measured, profiled, cheap wins fixed. Workloads and method: [`README.md`](README.md).

**Summary.**

* Before: on macOS (16 KiB pages) Rust reads were already at parity
  with C or faster (random gets, `GET_BOTH`, `NEXT_DUP`, 8 concurrent
  readers), scans 1.2-1.35x C's time; on Linux (4 KiB pages, deeper
  trees) every read was 1.07-1.4x. Writes were 1.2-5x slower on both.
* Five local, behaviour-preserving changes (below). Interleaved A/B
  against the unchanged code, macOS: sequential appends -44 %,
  `DUPSORT` puts -32 %, small-tree gets -19 %, scans -10 to -15 %,
  random gets, deletes, random puts, small transactions -7 to -13 %;
  overwrites and large-value puts unchanged within noise. Linux,
  interleaved too: appends -43 %, `DUPSORT` puts -33 %, scans -18 to
  -23 %, random gets and small transactions -15 %, random puts, deletes
  and overwrites -8 to -13 %, mixed readers -21 %.
* After: reads 0.7-1.25x C (Linux: scans and `DUPSORT` reads at
  parity, random gets 1.16x), except gets on a small cached tree with
  4 KiB pages (`get_64k` on Linux, ~1.4-1.6x); writes 1.2-1.8x
  (appends 2.5-3x). What is left is mostly architectural; see
  "Remaining gaps".

## macOS (Apple M1 Pro, 16 KiB pages)

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD; environments in `$TMPDIR`.
* rustc 1.93.1 (Homebrew), Apple clang 17.0.0 (clang-1700.4.4.1).
* Command (from `rust/`): `cargo build --release -p lmdb-bench &&
  target/release/lmdb-bench --raw` (all groups, n = 1 000 000, 5 reps).

Rust against C, before and after (ns/op: median (min); two separate runs,
each comparing both sides within the run):

| workload | ops/rep | C before | Rust before | Rust/C before | C after | Rust after | Rust/C after |
|---|---:|---:|---:|---:|---:|---:|---:|
| seq_put_append | 1000000 | 110 (85.2) | 558 (430) | 5.06 | 79.8 (76.1) | 251 (245) | **3.14** |
| rand_put | 1000000 | 3209 (2886) | 4763 (4309) | 1.48 | 2524 (2464) | 3418 (3143) | **1.35** |
| rand_get | 1000000 | 1559 (1393) | 1316 (1123) | 0.84 | 1142 (1078) | 957 (915) | **0.84** |
| scan_fwd | 1000000 | 18.2 (17.6) | 22.2 (20.7) | 1.22 | 15.8 (15.7) | 17.5 (17.2) | **1.11** |
| scan_bwd | 1000000 | 16.1 (14.4) | 21.7 (19.6) | 1.35 | 14.2 (13.8) | 16.4 (16.2) | **1.16** |
| readers8_get | 8000000 | 374 (285) | 360 (234) | 0.96 | 143 (130) | 119 (118) | **0.83** |
| rand_overwrite | 1000000 | 4170 (3687) | 6817 (6431) | 1.63 | 4402 (4048) | 5789 (5338) | **1.32** |
| dup_put | 1000000 | 798 (792) | 2093 (1874) | 2.62 | 828 (811) | 1326 (1278) | **1.60** |
| dup_get_both | 1000000 | 1015 (955) | 929 (915) | 0.92 | 1133 (1070) | 807 (797) | **0.71** |
| dup_next_dup | 1000000 | 109 (108) | 101 (97.0) | 0.93 | 120 (118) | 88.7 (87.7) | **0.74** |
| put_4k | 50000 | 6253 (6056) | 9911 (9608) | 1.58 | 5789 (5657) | 9156 (9131) | **1.58** |
| get_4k | 1000000 | 581 (557) | 645 (641) | 1.11 | 613 (516) | 503 (494) | **0.82** |
| put_64k | 5000 | 23887 (22976) | 27030 (25672) | 1.13 | 22314 (21904) | 29536 (24550) | **1.32** |
| get_64k | 1000000 | 227 (225) | 317 (306) | 1.40 | 220 (219) | 269 (258) | **1.22** |
| del_rand | 1000000 | 3231 (3128) | 5372 (4993) | 1.66 | 3193 (2948) | 4653 (4492) | **1.46** |
| txn_nosync | 50000 | 7062 (6998) | 12859 (11742) | 1.82 | 6014 (5793) | 8252 (8091) | **1.37** |
| txn_sync | 1000 | 15534350 (14066008) | 15778200 (15039310) | 1.02 | 12175944 (10451865) | 10314719 (9840057) | **0.85** |
| mixed_writer | 50000 | 5716 (5540) | 6300 (6199) | 1.10 | 5506 (5299) | 7162 (6915) | **1.30** |
| mixed_readers | 63238144 | 4.4 (4.4) | 5.6 (5.5) | 1.27 | 4.4 (4.3) | 4.8 (4.7) | **1.10** |

`txn_sync` is dominated by `F_FULLFSYNC` (10-15 ms per commit on this
SSD, both sides). `mixed_writer` swings with whatever else the machine
does; the interleaved A/B below shows the new build 5 % faster there.
`put_64k` before/after differ within noise (A/B: 1.03 median, 0.93 min).

Rust after against Rust before, interleaved (`--baseline`, 2 rounds of
3 reps each; ns/op median (min)):

| workload | base ns/op | new ns/op | new/base (median) | new/base (min) |
|---|---:|---:|---:|---:|
| seq_put_append | 382 (373) | 215 (210) | **0.56** | 0.56 |
| rand_put | 3554 (3452) | 3245 (3144) | **0.91** | 0.91 |
| rand_get | 1005 (921) | 878 (846) | **0.87** | 0.92 |
| scan_fwd | 19.7 (19.5) | 17.7 (17.0) | **0.90** | 0.87 |
| scan_bwd | 18.8 (18.2) | 15.9 (15.6) | **0.85** | 0.86 |
| readers8_get | 128 (122) | 126 (114) | **0.98** | 0.93 |
| rand_overwrite | 5554 (5312) | 5700 (5152) | **1.03** | 0.97 |
| dup_put | 1889 (1864) | 1293 (1276) | **0.68** | 0.68 |
| dup_get_both | 986 (933) | 892 (866) | **0.90** | 0.93 |
| dup_next_dup | 110 (106) | 100 (96.8) | **0.91** | 0.92 |
| put_4k | 9998 (9609) | 9781 (9195) | **0.98** | 0.96 |
| get_4k | 654 (607) | 593 (549) | **0.91** | 0.91 |
| put_64k | 27061 (26060) | 27985 (24333) | **1.03** | 0.93 |
| get_64k | 333 (322) | 269 (264) | **0.81** | 0.82 |
| del_rand | 4986 (4811) | 4514 (4272) | **0.91** | 0.89 |
| txn_nosync | 9215 (9056) | 8553 (8018) | **0.93** | 0.89 |
| txn_sync (200 commits) | 10082077 (9849516) | 9264786 (9007664) | **0.92** | 0.91 |
| mixed_writer | 5923 (5753) | 5627 (5548) | **0.95** | 0.96 |
| mixed_readers | 5.4 (5.4) | 4.8 (4.8) | **0.89** | 0.89 |

## Linux (Debian bookworm container, 4 KiB pages)

* `Linux 6.8.0-146-generic #146-Ubuntu SMP PREEMPT_DYNAMIC aarch64`
  (the Lima VM behind `scripts/linux-test.sh`, 8 vCPUs and 12 GB on the
  same M1 Pro host), Debian 12 `rust:1-bookworm` container, glibc;
  environments in the container's `/tmp` (overlay on the VM disk).
* rustc 1.99.1, gcc 12.2.0 (Debian 12.2.0-14+deb12u1).
* Command: the container of `scripts/linux-test.sh` (same image,
  volumes and `/work` bind mount), running
  `cargo build --release -p lmdb-bench && target/release/lmdb-bench
  --dir /tmp/lmdb-bench --raw` for the current tree, and the same for the
  unchanged tree (copied into the container's `/tmp` and built with its
  own `CARGO_TARGET_DIR`), one after the other in one container.

Rust against C, before and after (two runs in one container, each
comparing both sides within the run):

| workload | ops/rep | C before | Rust before | Rust/C before | C after | Rust after | Rust/C after |
|---|---:|---:|---:|---:|---:|---:|---:|
| seq_put_append | 1000000 | 105 (102) | 459 (453) | 4.38 | 107 (105) | 268 (263) | **2.51** |
| rand_put | 1000000 | 2438 (2359) | 3759 (3504) | 1.54 | 2581 (2451) | 3366 (3275) | **1.30** |
| rand_get | 1000000 | 796 (763) | 1102 (953) | 1.38 | 951 (848) | 1108 (995) | **1.16** |
| scan_fwd | 1000000 | 34.1 (32.6) | 43.1 (40.7) | 1.26 | 38.4 (37.2) | 37.1 (34.0) | **0.97** |
| scan_bwd | 1000000 | 30.4 (28.9) | 36.8 (35.3) | 1.21 | 30.9 (28.8) | 32.7 (32.0) | **1.06** |
| readers8_get | 8000000 | 140 (106) | 153 (129) | 1.10 | 136 (110) | 146 (140) | **1.07** |
| rand_overwrite | 1000000 | 3377 (3131) | 4647 (4441) | 1.38 | 3397 (3224) | 4492 (4449) | **1.32** |
| dup_put | 1000000 | 694 (659) | 1872 (1838) | 2.70 | 764 (715) | 1357 (1279) | **1.77** |
| dup_get_both | 1000000 | 882 (851) | 1059 (929) | 1.20 | 985 (948) | 1026 (923) | **1.04** |
| dup_next_dup | 1000000 | 94.0 (90.9) | 101 (97.9) | 1.07 | 108 (105) | 108 (98.5) | **1.00** |
| put_4k | 50000 | 4080 (3994) | 5663 (5552) | 1.39 | 4172 (3831) | 6373 (6240) | **1.53** |
| get_4k | 1000000 | 470 (420) | 611 (548) | 1.30 | 458 (417) | 569 (466) | **1.24** |
| put_64k | 5000 | 37121 (36725) | 44281 (42970) | 1.19 | 34015 (32450) | 46783 (45096) | **1.38** |
| get_64k | 1000000 | 217 (212) | 294 (291) | 1.36 | 202 (197) | 322 (304) | **1.59** |
| del_rand | 1000000 | 3311 (3295) | 5580 (5159) | 1.69 | 3396 (3150) | 4939 (4701) | **1.45** |
| txn_nosync | 50000 | 3438 (3348) | 6237 (5936) | 1.81 | 3797 (3701) | 5293 (5214) | **1.39** |
| txn_sync | 1000 | 276214 (262861) | 330436 (293948) | 1.20 | 281145 (253726) | 287165 (259105) | **1.02** |
| mixed_writer | 50000 | 4731 (4613) | 5340 (5224) | 1.13 | 4597 (4477) | 5421 (5279) | **1.18** |
| mixed_readers | 28262400 | 8.4 (8.3) | 11.7 (11.7) | 1.40 | 8.6 (8.5) | 9.5 (9.3) | **1.11** |

On 4 KiB pages the trees are deeper (4 levels for 1M keys) and the
per-level overhead shows: before, Rust was slower than C on every read
here too (1.07-1.4x); after, scans and `DUPSORT` reads are at parity and
random gets at 1.16x. 4 KiB values go to overflow pages here (2 pages
each). The two sequential runs disagree for the overflow workloads
(`put_4k`, `put_64k`, `get_64k` look slower after); the interleaved A/B
in the same container (`lmdb-bench --baseline <unchanged build> --rounds
3 --reps 3` for the read and overflow groups, `--rounds 2 --reps 3
--sync-commits 200` for the others) shows that was run-to-run noise, and
gives the same picture as on macOS:

| workload | base ns/op | new ns/op | new/base (median) |
|---|---:|---:|---:|
| seq_put_append | 487 (470) | 279 (256) | **0.57** |
| rand_put | 3752 (3429) | 3251 (3077) | **0.87** |
| rand_get | 1164 (1049) | 991 (953) | **0.85** |
| scan_fwd | 44.4 (42.3) | 36.3 (32.8) | **0.82** |
| scan_bwd | 39.6 (37.2) | 30.4 (29.5) | **0.77** |
| readers8_get | 141 (130) | 149 (129) | **1.06** |
| rand_overwrite | 4729 (4462) | 4348 (4146) | **0.92** |
| dup_put | 1950 (1806) | 1299 (1229) | **0.67** |
| dup_get_both | 1024 (946) | 968 (917) | **0.94** |
| dup_next_dup | 108 (102) | 112 (103) | **1.03** |
| put_4k | 7081 (6212) | 6554 (5259) | **0.93** |
| get_4k | 578 (475) | 534 (441) | **0.92** |
| put_64k | 49472 (39599) | 48889 (43318) | **0.99** |
| get_64k | 338 (308) | 289 (256) | **0.86** |
| del_rand | 5109 (4779) | 4547 (4402) | **0.89** |
| txn_nosync | 5699 (5555) | 4843 (4717) | **0.85** |
| txn_sync (200 commits) | 257297 (251175) | 233857 (204221) | **0.91** |
| mixed_writer | 5407 (5180) | 5247 (5154) | **0.97** |
| mixed_readers | 11.4 (11.4) | 9.0 (8.8) | **0.79** |

`txn_sync` on Linux is `fdatasync` per commit on the VM's virtual disk
(about 0.28 ms); on macOS it is `F_FULLFSYNC` (10-15 ms).


## How the numbers were taken

* Every workload: one warm-up and 5 timed repetitions (`--reps 5`),
  median and min of ns/op, n = 1 000 000 keys (16-byte keys, 100-byte
  values), environments with `MDB_NOSYNC` except `txn_sync`.
* "Before" is the tree at `a031e87` plus the concurrent reviewer edits
  present when the work started (bug fixes in the write path, no
  performance effect expected); "after" adds the changes below.
* The macOS machine was shared with other heavy jobs during all runs
  (load average up to ~990 from unrelated processes), so absolute
  numbers, and C's in particular, move by 20-40 % between runs. The
  ratio columns compare C and Rust within one run (they alternate per
  group), and the A/B tables interleave the old and new Rust builds
  (2-3 rounds of 3 reps; on Linux with `--baseline`, on macOS with a
  scratch script that did the same before `--baseline` existed), which
  is what the per-change claims rest on.

## Changes

All in `rlmdb`, all behaviour-preserving (same results, same file
bytes; `cargo test -p rlmdb`, `-p rlmdb-interop`, `-p rlmdb-capi --test
mtest` pass, see the end), no new `unsafe`.

1. **Write-path read cursors built in place** (`core/cursor.rs`
   `Cursor::load`, `core/wcursor.rs` `read_op`/`load_read`). Write
   cursors hold page numbers; every read operation on them (the
   positioning of each put and delete) rebuilds read `Cursor`s, about
   700 bytes each. They were built in a temporary array, copied into a
   `Cursor`, moved out through two `Result`s and a tuple: 15-18 % of a
   sequential put was `memmove`. Now `read_op` builds both cursors where
   they are used. `Cursor::from_parts` is gone.
2. **`mdb_node_search` resolves page layout and comparator once**
   (`core/cursor.rs` `node_search`/`bsearch`, `format/page.rs`
   `PageRef::node_key`): the binary search had a per-probe check for
   `P_LEAF2`, built a `NodeRef` and a key tail and dispatched on the
   comparator for every probe. The default (memcmp) order now has its own
   loop over `node_key`, which does the same bounds checks without
   building the node. The empty page that fills unused cursor slots is a
   `const` (`PageRef::new` is `const fn`), so filling a cursor's stack is
   plain stores.
3. **No zero fill for touched pages** (`core/freelist.rs`
   `page_alloc_copy`, `core/wcursor.rs` `page_touch`). `mdb_page_touch`
   allocated a zero-filled 16 KiB buffer (`calloc`: 20 % of a random
   delete was `memset`) and then copied the whole clean page over it. The
   buffer is now created as a copy of the clean page. Same bytes (the
   whole page is still copied, so even unused bytes are as before).
4. **Position hints for the dirty list** (`core/dirty.rs`,
   `idl.rs` `Id2List::get_at`/`get_at_mut`). The write path looks up the
   same few pages (root, branch, leaf) about ten times per put, each a
   binary search of the dirty list: 24 % of a sequential put. `DirtyPages`
   remembers where it last found a page (16 slots by page number); the
   page number at that position is checked before use, so a stale hint
   only costs the binary search it replaces.
5. **`RoCursor::next`/`prev` call `mdb_cursor_next`/`prev` directly**
   (`core/ro_cursor.rs`) instead of going through the 19-way dispatch of
   `mdb_cursor_get` once per item. Same code path otherwise (`finish`
   does what `get` did after the dispatch).

Measured step by step while making them (macOS, the Rust side of the
new build interleaved with the unchanged one, n = 1M, 3 reps x 2-4
rounds; the two scratch loops are a small program outside the repo
timing 40 full scans or 2M gets in one read transaction; cumulative, so
each row includes the changes above it):

| after change | workload | new/base (median) |
|---|---|---:|
| 1 + 2 | seq_put_append | 0.75 |
| 1 + 2 | dup_put | 0.83 |
| 1 + 2 | rand_get / dup_get_both / dup_next_dup | 0.93 / 0.93 / 0.89 |
| 1 + 2 | scan_fwd / scan_bwd | 0.98 / 0.94 |
| 1 - 3 | rand_put | 0.81 |
| 1 - 3 | txn_nosync / del_rand | 0.91 / 0.95 |
| 1 - 4 | seq_put_append | 0.55 |
| 1 - 4 | get, 64 KiB values, 5000 keys (scratch loop) | 0.89 (313 -> 280 ns) |
| 5 | full forward/backward scan, 1M keys (scratch loop) | 0.86 (21 -> 18 ns) |

The final interleaved A/B over all workloads is the second macOS table
above (and the Linux before/after tables).


## Profiles

Profiled with `samply record --save-only --unstable-presymbolicate`
(macOS; `sample` was not needed) and a small script aggregating self and
inclusive time per (inlined) function with `atos -i`. Main findings
before the changes, as share of samples:

* `seq_put_append`: `memmove` of cursor structs 15 %, dirty-list binary
  search 7 % self / 24 % inclusive (after fix 1), `Cursor::new` 3 %,
  `pwritev` 5 %.
* `del_rand`: `calloc` zero fill of the new page buffer in
  `page_touch` 20 %, the full-page copy of the touched page 22 %, the
  B-tree search itself (cache misses on a 1M-key tree) about 25 %.
* `txn_nosync`: `pwrite` about 50 % (as in C, which spends 70 % there),
  zero fill and `malloc` 7-9 %, the page copy 8-10 %.
* `dup_put`: `memmove` 14 % (cursor structs, page copies), `malloc`
  8 % (`dup_prepare` builds sub-pages in fresh `Vec`s, page buffers),
  commit I/O 13 %.
* `put_4k` (after the changes): commit I/O (`pwritev`/`pwrite`) 38 %,
  then malloc/free churn of page buffers: `madvise` 9 %, `calloc` memset
  7 %, `bzero` 4 %.
* scans: no single hot spot; the per-item path (`mdb_cursor_get`
  dispatch, header reads) costs 2-4 ns more than C's.
* `get_64k` (small tree, all in cache; after the changes): 17 % of the
  time is filling the 32-slot page stack of the temporary cursor
  (`Cursor::new`; C leaves `mc_pg` uninitialised).

## Remaining gaps and why

These need design changes, not local fixes, so they were left alone:

* **Write cursors hold page numbers** (by design: no references into
  buffers the same transaction mutates). Every operation re-resolves its
  stack through the dirty list and rebuilds read cursors, and every
  `page_touch`, `node_add`, `top_page` resolves the page again; C follows
  pointers. This is most of the remaining 3x of `seq_put_append` (C does
  ~80 ns per append; Rust ~10 page lookups plus two cursor rebuilds).
  A fix would give dirty pages stable handles (a slab of page buffers
  indexed by slot, with the dirty list mapping page numbers to slots),
  so cursors could hold slots instead of searching by number.
* **No page-buffer recycling.** C keeps freed page buffers on
  `me_dpages` and reuses them without zeroing; Rust allocates a `Box`
  per dirty page and frees it at commit (`dirty.rs` says so on purpose).
  `put_4k` spends ~20 % in malloc/free/madvise/memset because of it, the
  random-write workloads 5-10 %. An environment-level pool (writer-only,
  under the writer lock, like `me_dpages`) would remove it; new pages
  would still be zeroed so files stay deterministic.
* **Full-page copies on touch.** C's `mdb_page_copy` copies only the
  used parts of a page (header and pointers, then the nodes), leaving
  the gap with whatever the buffer held. Rust copies the whole page so
  the unused bytes stay identical to the source page. Copying only the
  used parts into a zeroed (pooled) buffer would cut the copy by the
  free space (~30 % on random-insert pages) but change unused bytes in
  the file (to zeros); not done because output must stay byte-identical
  to what the Rust writer produced before.
* **`mdb_put` opens an arena slot per call** (copying `WCursor`s of
  ~350 bytes, twice for `DUPSORT`), where C uses a stack cursor; part of
  the per-put constant cost, and tied to the write-cursor design above.
* **Cursor stack initialisation**: safe Rust must initialise the 32-slot
  page stack of every cursor (`mdb_get` creates one per call); C leaves
  `mc_pg` uninitialised. That is most of the remaining `get_64k` gap on
  a small, cached tree. On a large tree with 16 KiB pages (`rand_get` on
  macOS) cache misses hide it and Rust is faster than C; with 4 KiB pages
  (4 levels for 1M keys, Linux) the per-level costs add up to 1.16x:
  each level's `TxnCore::page` also consults the (empty) dirty and spill
  lists of the read transaction, and `Cursor::push` is bounds-checked. A
  smaller stack type (page offsets instead of fat slices) and a separate
  page lookup for read-only transactions would address both.
* `mixed_writer` is the writer path above (1.2-1.3x C, but this workload
  swings by 20 % between runs here; interleaved, the new build is 5 %
  faster than the old one).

Local changes that would help but were out of scope here (the files are
being fixed concurrently by the reviewer):

* **`DUPSORT` puts allocate**: `dup_prepare` (`cursor_put.rs`) builds
  the new sub-page in a fresh `Vec` per put and `page_split0`
  (`split.rs`) allocates zeroed scratch pages; C uses
  `env->me_pbuf`/stack buffers. A reusable scratch buffer in the write
  state would remove 5-8 % of `dup_put`.
* `RwTxn::put` re-validates the DBI and borrows the DBI table's
  `RefCell` several times per put (`dbi_entry`, `user_dbi`,
  `check_reserve`, `db_of`); a few percent of `seq_put_append`.

## Tests after the changes

Run on the final tree (with the reviewer's concurrent edits present at
the time), debug profile:

| platform | command | test binaries | passed | failed | ignored |
|---|---|---:|---:|---:|---:|
| macOS | `cargo test -p rlmdb` | 20 | 200 | 0 | 4 |
| macOS | `cargo test -p rlmdb-interop` | 15 | 333 | 0 | 3 |
| macOS | `cargo test -p rlmdb-capi --test mtest` | 1 | 8 | 0 | 0 |
| Linux | `scripts/linux-test.sh test -p rlmdb` | 20 | 198 | 0 | 4 |
| Linux | `scripts/linux-test.sh test -p rlmdb-interop` | 15 | 333 | 0 | 3 |
| Linux | `scripts/linux-test.sh test -p rlmdb-capi --test mtest` | 1 | 8 | 0 | 0 |

`cargo fmt --check`, `cargo clippy -p rlmdb -p lmdb-bench
--all-targets -D warnings` and `cargo doc -p rlmdb -p lmdb-bench
--no-deps` with `-D warnings` are clean.

<details><summary>The <code>test result</code> lines</summary>

```
macOS, cargo test -p rlmdb:
    test result: ok. 62 passed; 0 failed; 1 ignored; finished in 1.93s
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 2.47s
    test result: ok. 12 passed; 0 failed; 0 ignored; finished in 5.56s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.01s
    test result: ok. 6 passed; 0 failed; 0 ignored; finished in 5.65s
    test result: ok. 2 passed; 0 failed; 0 ignored; finished in 0.08s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 30.19s
    test result: ok. 9 passed; 0 failed; 0 ignored; finished in 0.21s
    test result: ok. 4 passed; 0 failed; 0 ignored; finished in 0.06s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 14.87s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 13 passed; 0 failed; 0 ignored; finished in 0.02s
    test result: ok. 23 passed; 0 failed; 0 ignored; finished in 2.91s
    test result: ok. 14 passed; 0 failed; 0 ignored; finished in 0.13s
    test result: ok. 18 passed; 0 failed; 2 ignored; finished in 18.67s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.03s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.07s
    test result: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    test result: ok. 3 passed; 0 failed; 0 ignored; finished in 1.35s

macOS, cargo test -p rlmdb-interop:
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 39 passed; 0 failed; 0 ignored; finished in 6.56s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.45s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.46s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 8.72s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 8.22s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 9.11s
    test result: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    test result: ok. 10 passed; 0 failed; 0 ignored; finished in 15.36s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 8.81s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 7.86s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 9.33s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 2.40s
    test result: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s

macOS, cargo test -p rlmdb-capi --test mtest:
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 12.27s

Linux, scripts/linux-test.sh test -p rlmdb:
    test result: ok. 60 passed; 0 failed; 1 ignored; finished in 1.96s
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.71s
    test result: ok. 12 passed; 0 failed; 0 ignored; finished in 6.41s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.01s
    test result: ok. 6 passed; 0 failed; 0 ignored; finished in 4.73s
    test result: ok. 2 passed; 0 failed; 0 ignored; finished in 0.08s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 30.07s
    test result: ok. 9 passed; 0 failed; 0 ignored; finished in 0.07s
    test result: ok. 4 passed; 0 failed; 0 ignored; finished in 0.02s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 13.66s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 13 passed; 0 failed; 0 ignored; finished in 0.19s
    test result: ok. 23 passed; 0 failed; 0 ignored; finished in 2.22s
    test result: ok. 14 passed; 0 failed; 0 ignored; finished in 0.22s
    test result: ok. 18 passed; 0 failed; 2 ignored; finished in 20.76s
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 3.52s
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 0.08s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 0.04s
    test result: ok. 0 passed; 0 failed; 1 ignored; finished in 0.00s
    test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.48s

Linux, scripts/linux-test.sh test -p rlmdb-interop:
    test result: ok. 5 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 3 passed; 0 failed; 0 ignored; finished in 0.00s
    test result: ok. 39 passed; 0 failed; 0 ignored; finished in 3.85s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.57s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.20s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.05s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.68s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.48s
    test result: ok. 11 passed; 0 failed; 0 ignored; finished in 0.01s
    test result: ok. 10 passed; 0 failed; 0 ignored; finished in 14.95s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.85s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 5.46s
    test result: ok. 33 passed; 0 failed; 0 ignored; finished in 6.68s
    test result: ok. 1 passed; 0 failed; 0 ignored; finished in 2.32s
    test result: ok. 0 passed; 0 failed; 3 ignored; finished in 0.00s

Linux, scripts/linux-test.sh test -p rlmdb-capi --test mtest:
    test result: ok. 8 passed; 0 failed; 0 ignored; finished in 6.58s

```

</details>
