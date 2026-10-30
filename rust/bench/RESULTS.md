# lmdb-bench results

Rust rewrite (`rlmdb`) against the C library (0.9.36) at `e84876f`,
after the second optimisation round (tasks W, R, S and L) and with the
Lightning Stream workloads (`ls` group). Workloads and method:
[`README.md`](README.md); every change has a report with its A/B
tables, profiles and tests under [`results/`](results/).

The suite runs the same workloads with the same data on both sides:
puts (sequential with `MDB_APPEND`, random, overwrites, `DUPSORT`,
4 KiB and 64 KiB values), gets, cursor scans, deletes, one-put
transactions with and without sync, a writer next to four scanning
readers, and a Lightning Stream style sync scan and update of a
5-million-record database. n = 1 000 000 keys (16-byte keys, 100-byte
values) unless stated otherwise; all environments use `MDB_NOSYNC`
except `txn_sync`.

**Summary.**

* Reads (other than `DUPSORT` lookups) are at parity with C or faster
  on the server and on macOS; in the arm64 VM `get_4k` is at 1.33x.
  `ls_scan`, the workload this project cares most about, is now ahead
  of C everywhere: 0.77x C on the x86-64 server, 0.81x on macOS, 0.89x
  in the arm64 VM (it was 1.03-1.25x before task L). Plain scans are at
  0.8-1.0x C, random gets at 0.72-0.99x.
* Writes: sequential appends are the largest gap (1.75-2.2x C), then
  one-put transactions (1.2-1.45x). On the 4 KiB-page server random
  overwrites and deletes are at 1.3x, random puts 1.15x, 64 KiB puts
  1.7x, 4 KiB puts 1.1x; on macOS (16 KiB pages) all of these are at
  0.94-1.1x. Lightning Stream updates are at 1.0-1.17x.
* This round, against the tree before it (`71e228d`), interleaved on
  the server: sequential appends -33 %, 4 KiB puts -47 %, gets of 4 KiB
  and 64 KiB values -28 to -30 %, scans -10 % and random gets -5 %
  (all before L). macOS, per task: random overwrites -30 %, random puts
  -22 % (W) and -5 % (S), deletes -24 % and -5 %, sequential appends
  -7 % and -19 %; `ls_scan` -19 to -22 % and plain scans -12 to -20 %
  (L).

## Rust against C, current tree

ns/op: median (min); ratio = Rust median / C median (above 1: Rust is
slower).

### Linux server (x86-64, 4 KiB pages): the reference

* x86-64 Hyper-V guest, 4 vCPUs (AMD EPYC Milan), 31 GB RAM, Rocky
  Linux 9 kernel 5.14, environments on a tmpfs in RAM (`/dev/shm`)
  except the second `txn_sync` row, which ran on ext4 on a virtio disk;
  gcc 11.5, rustc 1.99.1.
* The machine was idle during the runs. Run 1 (5 reps, one process per
  side and group) has most medians within 1-3 % of the minimums; run 2
  (3 rounds of 3 reps) spreads more between rounds, on both sides.
* Source: the rows of run 2 are at `e84876f` (`--only
  ls,read,dupsort --rounds 3 --reps 3`). The others are run 1, the full
  suite at `ccc9893` (`--reps 5`), before task L. L changes only cursor
  stepping, which none of those rows depends on except `mixed_readers`
  (four scanners), marked *pre-L*; run 1's numbers for the rows run 2
  replaced are in the notes below.

| workload | ops/rep | run | C ns/op | Rust ns/op | Rust/C |
|---|---:|:---:|---:|---:|---:|
| seq_put_append | 1000000 | 1 | 141 (139) | 273 (271) | **1.94** |
| rand_put | 1000000 | 1 | 2861 (2840) | 3290 (3206) | **1.15** |
| rand_get | 1000000 | 2 | 1009 (954) | 972 (882) | **0.96** |
| scan_fwd | 1000000 | 2 | 51.3 (46.0) | 41.7 (35.7) | **0.81** |
| scan_bwd | 1000000 | 2 | 43.3 (39.3) | 35.8 (31.5) | **0.83** |
| readers8_get | 8000000 | 2 | 259 (251) | 248 (220) | **0.96** |
| rand_overwrite | 1000000 | 1 | 3146 (3065) | 4186 (4129) | **1.33** |
| dup_put | 1000000 | 2 | 784 (731) | 1310 (1264) | **1.67** |
| dup_get_both | 1000000 | 2 | 1055 (966) | 1233 (1208) | **1.17** |
| dup_next_dup | 1000000 | 2 | 130 (123) | 162 (156) | **1.24** |
| put_4k | 50000 | 1 | 4618 (4576) | 5066 (5001) | **1.10** |
| get_4k | 1000000 | 1 | 481 (469) | 450 (441) | **0.94** |
| put_64k | 5000 | 1 | 39409 (38892) | 66976 (64250) | **1.70** |
| get_64k | 1000000 | 1 | 228 (227) | 226 (222) | **0.99** |
| del_rand | 1000000 | 1 | 2884 (2839) | 3720 (3700) | **1.29** |
| txn_nosync | 50000 | 1 | 5915 (5876) | 7929 (7751) | **1.34** |
| txn_sync (tmpfs) | 1000 | 1 | 5152 (4717) | 6598 (6513) | **1.28** |
| txn_sync (ext4) | 1000 | 1 | 311502 (308852) | 318443 (313707) | **1.02** |
| mixed_writer | 50000 | 1 | 7771 (6979) | 9160 (8532) | **1.18** |
| mixed_readers | 23749632 | 1, *pre-L* | 16.1 (15.8) | 17.5 (16.2) | **1.08** |
| ls_scan | 5000000 | 2 | 69.3 (61.2) | 53.5 (50.2) | **0.77** |
| ls_update | 50000 | 2 | 5161 (4839) | 5414 (5242) | **1.05** |

Run 1 (before L) had `ls_scan` at 1.03, `scan_fwd` 0.99, `scan_bwd`
1.11, `ls_update` 1.17 and the other rows of run 2 within a few
percent of the values above (`rand_get` 0.96, `readers8_get` 0.94).
`txn_sync` on tmpfs measures the commit path with a near-free
`fdatasync`; on ext4 the sync dominates and both sides are equal. The
ext4 run also had `txn_nosync` at 1.31.

### macOS (Apple M1 Pro, 16 KiB pages)

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD, 16 KiB pages; environments in `$TMPDIR`. rustc 1.93.1
  (Homebrew), Apple clang 17.0.0 (clang-1700.4.4.1).
* At `e84876f`: `target/release/rlmdb-bench --rounds 2 --reps 3` (all
  groups, 6 samples per side), started once the 1-minute load average
  had dropped to 5.5 from other jobs on the machine; during the
  10-minute run it stayed between 3.5 and 8.7 (10 cores). The `txn_*`
  and `mixed_writer` C medians sit well above their minimums, so read
  those rows with that in mind.

| workload | ops/rep | C ns/op | Rust ns/op | Rust/C |
|---|---:|---:|---:|---:|
| seq_put_append | 1000000 | 75.2 (74.1) | 162 (159) | **2.16** |
| rand_put | 1000000 | 2560 (2460) | 2533 (2400) | **0.99** |
| rand_get | 1000000 | 1154 (1120) | 827 (775) | **0.72** |
| scan_fwd | 1000000 | 16.2 (15.6) | 13.2 (12.8) | **0.82** |
| scan_bwd | 1000000 | 14.1 (13.8) | 13.5 (13.3) | **0.96** |
| readers8_get | 8000000 | 138 (122) | 135 (105) | **0.98** |
| rand_overwrite | 1000000 | 3790 (3657) | 3641 (3545) | **0.96** |
| dup_put | 1000000 | 826 (817) | 1043 (1034) | **1.26** |
| dup_get_both | 1000000 | 1008 (966) | 830 (775) | **0.82** |
| dup_next_dup | 1000000 | 111 (109) | 87.1 (85.4) | **0.78** |
| put_4k | 50000 | 6593 (6110) | 7256 (6870) | **1.10** |
| get_4k | 1000000 | 652 (616) | 500 (489) | **0.77** |
| put_64k | 5000 | 26934 (23488) | 27503 (26417) | **1.02** |
| get_64k | 1000000 | 232 (222) | 221 (212) | **0.96** |
| del_rand | 1000000 | 3645 (3146) | 3419 (3318) | **0.94** |
| txn_nosync | 50000 | 6119 (5911) | 8731 (8109) | **1.43** |
| txn_sync | 1000 | 10539481 (10038662) | 10278517 (10038202) | **0.98** |
| mixed_writer | 50000 | 7975 (5396) | 6761 (6241) | **0.85** |
| mixed_readers | 92385280 | 4.6 (4.3) | 3.6 (3.4) | **0.79** |
| ls_scan | 5000000 | 17.8 (17.2) | 14.3 (13.5) | **0.81** |
| ls_update | 50000 | 5306 (5266) | 5301 (4495) | **1.00** |

With 16 KiB pages 4 KiB values fit in a leaf (only the 64 KiB values are
overflow runs, of 5 pages) and the trees are a level shallower; the
random writes are at parity with C here, unlike on 4 KiB pages. `txn_sync` is `F_FULLFSYNC`
(about 10 ms per commit, both sides). `mixed_writer` swings with the
load; by the minimums it is 1.16.

### Linux VM (arm64, 4 KiB pages): indicative only

* The Lima VM behind `scripts/linux-test.sh`: `Linux 6.8.0-146-generic
  ... aarch64`, Debian 12 `rust:1-bookworm` container, 8 vCPUs, 12 GB,
  on the same M1 Pro host; rustc 1.99.1, gcc 12.2.0.
* Full suite at `87b1a9b` (W, R and S; before the LS workloads and L),
  1 round of 3 reps. This run predates `bb96986`, which put the
  container's `/tmp` on a tmpfs in the VM's memory, so its environments
  were on the VM's virtual disk; and the host was shared. Read it as
  indicative; the server above is the reference for 4 KiB pages.

| workload | ops/rep | C ns/op | Rust ns/op | Rust/C |
|---|---:|---:|---:|---:|
| seq_put_append | 1000000 | 113 (109) | 197 (197) | **1.75** |
| rand_put | 1000000 | 2260 (2205) | 2557 (2554) | **1.13** |
| rand_get | 1000000 | 891 (863) | 879 (877) | **0.99** |
| scan_fwd | 1000000 | 35.3 (34.4) | 42.8 (39.6) | **1.21** |
| scan_bwd | 1000000 | 33.4 (33.0) | 35.3 (34.4) | **1.06** |
| readers8_get | 8000000 | 116 (105) | 117 (109) | **1.02** |
| rand_overwrite | 1000000 | 3141 (3138) | 3434 (3228) | **1.09** |
| dup_put | 1000000 | 654 (633) | 1049 (970) | **1.60** |
| dup_get_both | 1000000 | 858 (815) | 860 (835) | **1.00** |
| dup_next_dup | 1000000 | 103 (87.3) | 98.3 (96.0) | **0.96** |
| put_4k | 50000 | 3660 (3381) | 3938 (3805) | **1.08** |
| get_4k | 1000000 | 368 (364) | 490 (480) | **1.33** |
| put_64k | 5000 | 30568 (30453) | 43281 (42818) | **1.42** |
| get_64k | 1000000 | 212 (200) | 233 (231) | **1.10** |
| del_rand | 1000000 | 3146 (2963) | 3435 (3425) | **1.09** |
| txn_nosync | 50000 | 3824 (3616) | 4677 (4673) | **1.22** |
| txn_sync | 1000 | 288543 (266451) | 276385 (250578) | **0.96** |
| mixed_writer | 50000 | 4364 (4352) | 4728 (4698) | **1.08** |
| mixed_readers | 28639232 | 7.6 (7.6) | 8.2 (8.2) | **1.08** |

After L, on the tmpfs and a quiet host (`--only ls,read --rounds 3
--reps 3`, [`results/L-ls-scan.md`](results/L-ls-scan.md)): `ls_scan`
44.1 (40.6) against 39.1 (34.4) ns, **0.89**; `ls_update` **1.17**;
`scan_fwd` **0.83**; `scan_bwd` **0.99**. `get_4k` is the one read
above 1.1 here; on the server it is at 0.94.

## What changed in this round

All in `rlmdb`, behaviour- and layout-preserving (same results, same
errors on corrupt input, same file bytes), no new `unsafe`, public API
unchanged. In commit order:

| change | commit | what | what it bought (interleaved A/B, new/base of the medians) |
|---|---|---|---|
| hardening | `9eda847` | errors instead of panics or reuse of live pages for the corruptions py-lmdb patches against (checks in `page_touch` and the overflow-run length on delete) | correctness only; nothing measurable |
| W: write-path allocations | `f8fb670` | a pool of page buffers (C's `me_dpages`), scratch buffers for `DUPSORT` puts and splits, the DBI resolved once per put/del | macOS: `rand_overwrite` 0.70, `rand_put` 0.78, `del_rand` and `put_4k` 0.76, `seq_put_append` 0.93, `put_64k` and `txn_nosync` 0.96. [`results/W-write-alloc.md`](results/W-write-alloc.md) |
| R: read path | `30d5566` | 8-slot cursor stacks for `mdb_get`, a page lookup for read transactions that goes straight to the map, memcmp order compared inline for keys up to 32 bytes, smaller per-item and per-call costs | macOS: `get_64k` 0.78, `readers8_get` 0.86, `scan_bwd` 0.88, `rand_get` 0.91, `get_4k` 0.92, `mixed_readers` 0.87. [`results/R-read-path.md`](results/R-read-path.md) |
| page layout descriptor | `0e26884` | every format-dependent offset from a per-environment layout (dual-format stage 1, `DUAL_FORMAT.md`) | first cut cost 8-16 % on scans and cached reads; after the fixes in the same commit writes, point reads and scans are within noise (a few percent may remain on `DUPSORT` lookups). Details: `git show 0e26884:rust/bench/RESULTS.md` |
| S: stable dirty-page handles | `87b1a9b` | dirty pages in a slab with checked slot handles that write cursors remember, so the write path stops searching the dirty list; the arena slot of `mdb_put` reset in place, fewer record copies | macOS: `seq_put_append` 0.81, `rand_put` and `del_rand` 0.95; one-put transactions and overwrites unchanged. [`results/S-dirty-handles.md`](results/S-dirty-handles.md) |
| LS workloads | `ccc9893` | the `ls` group: Lightning Stream sync scan and update (bench only) | [`results/LS-workloads.md`](results/LS-workloads.md) |
| L: scan step | `e84876f` | a cursor step within a leaf with one format dispatch, one `NUMKEYS` read and one bounds check for key and data, returned straight to the caller; the general path out of line | macOS: `ls_scan` 0.78-0.81 (147 to 78 instructions per record; C 113), `scan_fwd` 0.80, `scan_bwd` 0.88. [`results/L-ls-scan.md`](results/L-ls-scan.md) |

On the server, the whole round before L (`ccc9893` against `71e228d`,
`--baseline`, 3 rounds of 3 reps, groups `read`, `dupsort`, `overflow`,
`seq_put`): `put_4k` 0.53, `seq_put_append` 0.67, `get_64k` 0.70,
`dup_put` 0.71, `get_4k` 0.72, `scan_fwd` and `scan_bwd` 0.90,
`rand_get` 0.95, `readers8_get` 0.97, `put_64k` 0.97.

The first optimisation round (stage 4 task P: read cursors built in
place on the write path, `node_search` with the layout and comparator
resolved once, no zero fill of touched pages, dirty-list position hints,
direct `next`/`prev`) took appends from 4.4-5x C to 2.5-3x, `DUPSORT`
puts from 2.6x to 1.6-1.8x and reads to 0.7-1.25x C. Its before/after
and per-change tables, profiles and test runs, and the descriptor's
measurements, are in the previous version of this file (`git show
0e26884:rust/bench/RESULTS.md`).

## Remaining gaps and why

In order of importance for this project. `DUPSORT` performance is out
of scope; its rows stay in the tables for completeness.

* **`ls_scan`: done for now.** Rust is ahead of C on all three machines
  (0.77-0.89x). The step still checks every page and node access against
  the page (`AGENTS.md` section 3), which C does not; the scan waits on
  memory most of the time, so those checks cost little. Left: a page
  change runs the general path after the fast step declines (about 60
  instructions more than before, once in ~65 records), and the
  `NUMKEYS` test could be fused with the pointer bounds check (2
  instructions per record); neither could be shown to help.
* **Sequential appends (`seq_put_append`, 1.75-2.2x C).** Every
  positioning of a put rebuilds two read cursors from the write cursor
  (`read_op`: 32 page slots to fill, the `ki` array copied back right
  after the read operation's stores, a store-forwarding stall) and
  descends by page number (three lookups per put). That rebuild is
  17.6 % of a sequential put after S. The fix is to run read operations
  on the write cursor's own stack of `(pgno, slot)` pairs (a read cursor
  generic over where its pages come from); 8-slot read cursors on the
  write path were tried and did not pay ([S](results/S-dirty-handles.md)).
* **Overflow-value puts (`put_64k` 1.4-1.7x with 4 KiB pages, 1.02x on
  macOS; `put_4k` 1.1x everywhere).** With 4 KiB pages both values are
  overflow runs (2 and 17 pages); with 16 KiB pages only the 64 KiB
  values are (5 pages), and 4 KiB values are leaf items. Not profiled in
  this round. Overflow runs are not pooled on either side, but Rust
  allocates a new run zero-filled (`DirtyPages::alloc_zeroed`) before
  the value is copied in, where C's `mdb_page_malloc` clears only the
  last page of a run. That is the first suspect for `put_64k` on Linux,
  to be confirmed by a profile; the 1.1x of `put_4k` on macOS, where no
  overflow page is involved, is unexplained.
* **Small-transaction overhead (`txn_nosync` 1.2-1.45x, `mixed_writer`
  1.1-1.2x on Linux, noisy on macOS).** The commit's `pwrite`s are
  57-59 % of Rust's one-put transaction (C's share was about 70 % in the
  first round's profile); the rest is fixed per-transaction cost: page
  copies on touch (10 %), the writer lock's semaphores (7 % on macOS),
  reading the meta page, and a dozen small `Vec`s (dirty list, free
  list, cursor arena, write lists) allocated per transaction (1-5 %
  depending on the profile). No single hot spot: the gap is the sum of
  these small costs.
* **Random overwrites and deletes (`rand_overwrite` 1.1-1.35x,
  `del_rand` 1.1-1.3x, random puts 1.1-1.15x with 4 KiB pages; all at
  0.94-0.99x on macOS).** Dominated by the binary searches in a 1M-key
  tree (cache misses; 40-50 % of the put and delete paths on macOS)
  and the copy of every page
  touched for the first time: Rust copies the whole page so the unused
  bytes match the source page, C's `mdb_page_copy` copies only the used
  parts (16-33 % of these workloads). Copying only the used parts into
  a zeroed buffer would change unused bytes in the file (to zeros), the
  one layout change that would help here; not made, because output
  stays byte-identical (`AGENTS.md` section 1 allows it as a recorded,
  deliberate divergence).

## Profiles

Current findings (macOS unless noted; tools and per-function tables in
the per-task reports):

* `ls_scan` and the plain scans: 78 instructions per record in Rust, 113
  in C (counted by single-stepping, same method on both sides). Both
  sides spend most of the time on the first loads of a record's node
  and of the page header; the time ratio follows the cache misses more
  than the instruction counts.
* `rand_put`, `del_rand` (shares of the put and delete paths): B-tree
  search and read path 43-49 %, whole-page copy on touch and node moves
  23-31 %, node edits 6-7 %; allocator frames below 1 % since W (were
  9 %). Overwrites were not profiled separately.
* `seq_put_append`: read-cursor rebuild 17.6 %, B-tree search and read
  path 15 %, page lookups by number or slot 13 %, `page_touch` 7 %.
* `txn_nosync`: `pwrite` 57-59 %, page copies 10 %, writer-lock
  semaphores 7 %, `malloc`/`free` 1-2 %.
* `get_64k` (small cached tree): the binary search (branch
  mispredictions on the probe direction) and the cache misses on the
  values; the stack fill and the `memcmp` calls that were 20 % and a
  third of it before R are gone.

## How the numbers were taken

* Every workload: one warm-up and `--reps` timed repetitions per round,
  median and min of ns/op over all samples of a side; each group runs
  in a fresh process per side and the side that goes first alternates
  between groups and rounds. Rust/C ratios are always from one run, so
  both sides saw the same conditions.
* Per-change claims rest on interleaved A/B runs of two Rust builds
  (`--baseline <old build> --rounds 3 --reps 3`), never on two separate
  runs. The macOS machine is shared with other jobs (the per-task
  reports give the load during each run), where separate runs move by
  10-40 %.
* Environments are on local storage only: the internal SSD on macOS
  (`$TMPDIR`), a tmpfs on the server (`/dev/shm`) and in the VM since
  `bb96986` (except where stated), so the numbers measure the libraries,
  not the disks.
* Commands, from `rust/`: `cargo build --release -p rlmdb-bench &&
  target/release/rlmdb-bench --rounds 2 --reps 3` (macOS);
  `rlmdb-bench --dir /dev/shm/rlmdb-bench --reps 5` and `--only
  ls,read,dupsort --rounds 3 --reps 3` (server; the ext4 row with
  `--only small_txn` and `--dir` on that file system); in the VM,
  `scripts/linux-test.sh run --release -p rlmdb-bench -- --dir
  /tmp/lmdb-bench --reps 3`.
