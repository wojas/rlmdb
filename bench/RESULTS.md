# lmdb-bench results

Rust rewrite (`rlmdb`) against the C library (0.9.36) after the second
optimisation round (tasks W, R, S and L, measured at `9b18deb`), with
the Lightning Stream workloads (`ls` group) and the write-path change P2
(`9aceeb0`; the server's run 3 below is at that commit). The
dual-format commits between `9b18deb` and `17c1c52` (LMDB 1.0 support
behind a per-environment layout) are in the measured tree, and their own
A/B measured them within noise on 0.9 files
([`results/dual-format-stage2.md`](results/dual-format-stage2.md)).
Workloads and method:
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
  `ls_scan`, the workload this project cares most about, is now at or
  ahead of C everywhere: about 0.8-1.0x C on the x86-64 server (C's
  time there varies between sessions; see below), 0.81-0.83x on macOS,
  0.89x in the arm64 VM (it was 1.03-1.25x before task L). Plain scans are at
  0.8-1.0x C, random gets at 0.72-0.99x.
* Writes, after P2: sequential appends are the largest gap (1.49x C on
  the server, 1.60x on macOS), then one-put transactions (1.32x and
  1.22x). On the 4 KiB-page server deletes are at 1.23x, random puts and
  overwrites 1.12-1.13x, 4 KiB and 64 KiB puts 1.01x; on macOS (16 KiB
  pages) these are at 0.95-1.01x, overwrites 0.80x and 64 KiB puts
  0.59x. Lightning Stream updates are at 1.09x on the server, 0.83x on
  macOS.
* This round, against the tree before it (`9f17f7e`), interleaved on
  the server: sequential appends -33 %, 4 KiB puts -47 %, gets of 4 KiB
  and 64 KiB values -28 to -30 %, scans -10 % and random gets -5 %
  (all before L). macOS, per task: random overwrites -30 %, random puts
  -22 % (W) and -5 % (S), deletes -24 % and -5 %, sequential appends
  -7 % and -19 %; `ls_scan` -19 to -22 % and plain scans -12 to -20 %
  (L).
* P2, against `c679eb2`, interleaved: on the server sequential appends
  -28 %, 64 KiB puts -40 %, 4 KiB puts -7 %, random puts and one-put
  transactions -3 to -4 %; on macOS appends -28 %, 64 KiB puts -42 %,
  overwrites -17 %, 4 KiB puts, one-put transactions and `ls_update`
  -7 %. Reads unchanged.

## Rust against C, current tree

ns/op: median (min); ratio = Rust median / C median (above 1: Rust is
slower).

### Linux server (x86-64, 4 KiB pages): the reference

* x86-64 Hyper-V guest, 4 vCPUs (AMD EPYC Milan), 31 GB RAM, Rocky
  Linux 9 kernel 5.14, environments on a tmpfs in RAM (`/dev/shm`)
  except the second `txn_sync` row, which ran on ext4 on a virtio disk;
  gcc 11.5, rustc 1.99.1.
* The machine was idle during the runs. Run 1 (5 reps, one process per
  side and group) has most medians within 1-3 % of the minimums; runs 2
  and 3 (3 rounds of 3 reps) spread more between rounds, on both sides
  (`mixed_writer` the most in run 3).
* Source: the rows of run 3 are at `9aceeb0`, with P2 (`--only
  seq_put,rand_put,overwrite,overflow,delete,small_txn,mixed,ls --rounds
  3 --reps 3`); those of run 2 at `9b18deb` (`--only ls,read,dupsort
  --rounds 3 --reps 3`). The others are run 1, the full suite at
  `90cc145` (`--reps 5`), before task L and P2; neither changed the code
  they measure (`get_4k` and `get_64k` were within noise in P2's A/B
  below). The numbers of the runs that later runs replaced are in the
  notes below the table. Run 3's `mixed_readers` count per rep was not
  recorded.

| workload | ops/rep | run | C ns/op | Rust ns/op | Rust/C |
|---|---:|:---:|---:|---:|---:|
| seq_put_append | 1000000 | 3 | 143 (139) | 213 (208) | **1.49** |
| rand_put | 1000000 | 3 | 2460 (2403) | 2761 (2715) | **1.12** |
| rand_get | 1000000 | 2 | 1009 (954) | 972 (882) | **0.96** |
| scan_fwd | 1000000 | 2 | 51.3 (46.0) | 41.7 (35.7) | **0.81** |
| scan_bwd | 1000000 | 2 | 43.3 (39.3) | 35.8 (31.5) | **0.83** |
| readers8_get | 8000000 | 2 | 259 (251) | 248 (220) | **0.96** |
| rand_overwrite | 1000000 | 3 | 3124 (3025) | 3530 (3438) | **1.13** |
| dup_put | 1000000 | 2 | 784 (731) | 1310 (1264) | **1.67** |
| dup_get_both | 1000000 | 2 | 1055 (966) | 1233 (1208) | **1.17** |
| dup_next_dup | 1000000 | 2 | 130 (123) | 162 (156) | **1.24** |
| put_4k | 50000 | 3 | 4650 (4332) | 4719 (4498) | **1.01** |
| get_4k | 1000000 | 1 | 481 (469) | 450 (441) | **0.94** |
| put_64k | 5000 | 3 | 38640 (36925) | 38983 (37163) | **1.01** |
| get_64k | 1000000 | 1 | 228 (227) | 226 (222) | **0.99** |
| del_rand | 1000000 | 3 | 3026 (2962) | 3720 (3630) | **1.23** |
| txn_nosync | 50000 | 3 | 6034 (5913) | 7983 (7739) | **1.32** |
| txn_sync (tmpfs) | 1000 | 3 | 4970 (4780) | 6382 (6213) | **1.28** |
| txn_sync (ext4) | 1000 | 1 | 311502 (308852) | 318443 (313707) | **1.02** |
| mixed_writer | 50000 | 3 | 10048 (7842) | 10929 (7544) | **1.09** |
| mixed_readers | – | 3 | 19.5 (18.6) | 15.9 (14.4) | **0.81** |
| ls_scan | 5000000 | 3 | 53.9 (51.9) | 43.6 (42.2) | **0.81** |
| ls_update | 50000 | 3 | 4702 (4270) | 5126 (4851) | **1.09** |

`ls_scan` on this machine: read the ratio as about 0.8-1.0, not 0.81.
C's time per record varies between sessions more than any other row:
60.6, 69.3, 58.1 and 53.9 ns in four sessions with the upstream flags
(55.1 with stronger flags, see "Compiler flags" below), while Rust's was
53-55 ns in every run after L until run 3, where both sides were faster
(43.6 against C's 53.9 ns, hence 0.81). Run 2 had 0.77 (C slow, its
median 13 % above its minimum); the session between runs 2 and 3 gave
0.92. The cause is not established: the scan is memory-bound over a
1.15 GB database, which makes it sensitive to page placement and cache
state in the guest. The plain scans moved with it (`scan_fwd` /
`scan_bwd` 0.93 / 0.95 in the session that gave 0.92). On macOS
`ls_scan` was 0.81 and 0.83 in two sessions, with little spread.

Before P2, run 1 had `seq_put_append` at 1.94, `rand_put` 1.15,
`rand_overwrite` 1.33, `put_4k` 1.10, `put_64k` 1.70, `del_rand` 1.29,
`txn_nosync` 1.34, `txn_sync` (tmpfs) 1.28, `mixed_writer` 1.18 and
`mixed_readers` 1.08 (before L too); run 2 had `ls_scan` at 0.77 and
`ls_update` at 1.05. Run 1 (before L) had `ls_scan` at 1.03,
`scan_fwd` 0.99, `scan_bwd` 1.11, `ls_update` 1.17 and the other rows
of run 2 within a few percent of the values above (`rand_get` 0.96,
`readers8_get` 0.94).
`txn_sync` on tmpfs measures the commit path with a near-free
`fdatasync`; on ext4 the sync dominates and both sides are equal. The
ext4 run also had `txn_nosync` at 1.31.

### macOS (Apple M1 Pro, 16 KiB pages)

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD, 16 KiB pages; environments in `$TMPDIR`. rustc 1.93.1
  (Homebrew), Apple clang 17.0.0 (clang-1700.4.4.1).
* At `9b18deb`: `target/release/rlmdb-bench --rounds 2 --reps 3` (all
  groups, 6 samples per side), started once the 1-minute load average
  had dropped to 5.5 from other jobs on the machine; during the
  10-minute run it stayed between 3.5 and 8.7 (10 cores). The `txn_*`
  and `mixed_writer` C medians sit well above their minimums, so read
  those rows with that in mind.
* The rows marked P2 are from the P2 report's Rust-against-C run
  (`--rounds 3 --reps 3`, the write groups plus `mixed` and `ls`, load
  average 4.5-7), with P2's final build before its rebase onto the
  dual-format commits; the A/B after the rebase, against `c679eb2`, gave
  the same gains ([`results/P2-write-path.md`](results/P2-write-path.md)).
  The read rows are from the `9b18deb` run; P2 does not change them.

| workload | ops/rep | C ns/op | Rust ns/op | Rust/C |
|---|---:|---:|---:|---:|
| seq_put_append (P2) | 1000000 | 75.6 (73.5) | 121 (117) | **1.60** |
| rand_put (P2) | 1000000 | 2458 (2359) | 2334 (2181) | **0.95** |
| rand_get | 1000000 | 1154 (1120) | 827 (775) | **0.72** |
| scan_fwd | 1000000 | 16.2 (15.6) | 13.2 (12.8) | **0.82** |
| scan_bwd | 1000000 | 14.1 (13.8) | 13.5 (13.3) | **0.96** |
| readers8_get | 8000000 | 138 (122) | 135 (105) | **0.98** |
| rand_overwrite (P2) | 1000000 | 4133 (3413) | 3321 (3124) | **0.80** |
| dup_put | 1000000 | 826 (817) | 1043 (1034) | **1.26** |
| dup_get_both | 1000000 | 1008 (966) | 830 (775) | **0.82** |
| dup_next_dup | 1000000 | 111 (109) | 87.1 (85.4) | **0.78** |
| put_4k (P2) | 50000 | 6641 (5685) | 6686 (6179) | **1.01** |
| get_4k | 1000000 | 652 (616) | 500 (489) | **0.77** |
| put_64k (P2) | 5000 | 24651 (21946) | 14591 (13825) | **0.59** |
| get_64k | 1000000 | 232 (222) | 221 (212) | **0.96** |
| del_rand (P2) | 1000000 | 3210 (3015) | 3248 (3072) | **1.01** |
| txn_nosync (P2) | 50000 | 5988 (5905) | 7282 (6941) | **1.22** |
| txn_sync (P2) | 1000 | 10607541 (9381046) | 10559032 (10220912) | **1.00** |
| mixed_writer (P2) | 50000 | 5289 (5040) | 5462 (4741) | **1.03** |
| mixed_readers | 92385280 | 4.6 (4.3) | 3.6 (3.4) | **0.79** |
| ls_scan | 5000000 | 17.8 (17.2) | 14.3 (13.5) | **0.81** |
| ls_update (P2) | 50000 | 5788 (5450) | 4806 (4403) | **0.83** |

With 16 KiB pages 4 KiB values fit in a leaf (only the 64 KiB values are
overflow runs, of 5 pages) and the trees are a level shallower; the
random writes are at parity with C here, unlike on 4 KiB pages
(`rand_overwrite` is 0.92 by the minimums; C's median sits well above
its minimum). `txn_sync` is `F_FULLFSYNC` (about 10 ms per commit, both
sides). `mixed_writer` swings with the load; by the minimums it is 0.94.
Before P2 (the `9b18deb` run): `seq_put_append` 2.16, `rand_put` 0.99,
`rand_overwrite` 0.96, `put_4k` 1.10, `put_64k` 1.02, `del_rand` 0.94,
`txn_nosync` 1.43, `txn_sync` 0.98, `mixed_writer` 0.85 (1.16 by the
minimums), `ls_update` 1.00.

### Linux VM (arm64, 4 KiB pages): indicative only

* The Lima VM behind `scripts/linux-test.sh`: `Linux 6.8.0-146-generic
  ... aarch64`, Debian 12 `rust:1-bookworm` container, 8 vCPUs, 12 GB,
  on the same M1 Pro host; rustc 1.99.1, gcc 12.2.0.
* Full suite at `8d8f612` (W, R and S; before the LS workloads and L),
  1 round of 3 reps. This run predates `0498cf1`, which put the
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
| hardening | `e33add5` | errors instead of panics or reuse of live pages for the corruptions py-lmdb patches against (checks in `page_touch` and the overflow-run length on delete) | correctness only; nothing measurable |
| W: write-path allocations | `68ae72b` | a pool of page buffers (C's `me_dpages`), scratch buffers for `DUPSORT` puts and splits, the DBI resolved once per put/del | macOS: `rand_overwrite` 0.70, `rand_put` 0.78, `del_rand` and `put_4k` 0.76, `seq_put_append` 0.93, `put_64k` and `txn_nosync` 0.96. [`results/W-write-alloc.md`](results/W-write-alloc.md) |
| R: read path | `fe3ac83` | 8-slot cursor stacks for `mdb_get`, a page lookup for read transactions that goes straight to the map, memcmp order compared inline for keys up to 32 bytes, smaller per-item and per-call costs | macOS: `get_64k` 0.78, `readers8_get` 0.86, `scan_bwd` 0.88, `rand_get` 0.91, `get_4k` 0.92, `mixed_readers` 0.87. [`results/R-read-path.md`](results/R-read-path.md) |
| page layout descriptor | `eb4fa08` | every format-dependent offset from a per-environment layout (dual-format stage 1, `DUAL_FORMAT.md`) | first cut cost 8-16 % on scans and cached reads; after the fixes in the same commit writes, point reads and scans are within noise (a few percent may remain on `DUPSORT` lookups). Details: `git show eb4fa08:bench/RESULTS.md` |
| S: stable dirty-page handles | `8d8f612` | dirty pages in a slab with checked slot handles that write cursors remember, so the write path stops searching the dirty list; the arena slot of `mdb_put` reset in place, fewer record copies | macOS: `seq_put_append` 0.81, `rand_put` and `del_rand` 0.95; one-put transactions and overwrites unchanged. [`results/S-dirty-handles.md`](results/S-dirty-handles.md) |
| LS workloads | `90cc145` | the `ls` group: Lightning Stream sync scan and update (bench only) | [`results/LS-workloads.md`](results/LS-workloads.md) |
| L: scan step | `9b18deb` | a cursor step within a leaf with one format dispatch, one `NUMKEYS` read and one bounds check for key and data, returned straight to the caller; the general path out of line | macOS: `ls_scan` 0.78-0.81 (147 to 78 instructions per record; C 113), `scan_fwd` 0.80, `scan_bwd` 0.88. [`results/L-ls-scan.md`](results/L-ls-scan.md) |
| LMDB 1.0 data file | `c11a5ca` | the 1.0 data file next to 0.9's (dual-format stage 2, `DUAL_FORMAT.md`): a format sniff at open, layout-aware dirty-slot lookups, format branches in rarely taken write paths, the `me_pbuf` pad of `DUPSORT` sub-pages | 0.9 workloads on macOS within noise (`seq_put_append` 0.97, `rand_put` 1.00, scans 1.03-1.05, `dup_get_both` 1.00, `mixed_writer` 1.01; read rows move both ways between runs): [`results/dual-format-stage2.md`](results/dual-format-stage2.md) |
| P2: write-path constant costs | `9aceeb0` | `mdb_put`/`mdb_del` positioned on the write cursor's own stack, fewer page lookups per put, overflow runs built with their data and recycled, spare per-transaction containers, cheaper commits and pointer moves (below) | macOS, against `c679eb2`: `put_64k` 0.58, `seq_put_append` 0.72, `rand_overwrite` 0.83, `put_4k`, `txn_nosync` and `ls_update` 0.93; `rand_put`, `del_rand` and `mixed_writer` 0.97 (before the rebase); server: the table below. [`results/P2-write-path.md`](results/P2-write-path.md) |

P2 removes the read-cursor rebuild of every put and delete (17.6 % of a
sequential put after S) and most of the overflow-run cost. Positioning for `mdb_put` and
`mdb_del` runs on the write cursor itself (`RwTxn::seek_key`,
`RwTxn::seek_last`, `descend` and `read_item` in `core/wcursor.rs`, used
by `put_position` in `core/cursor_put.rs` and `del0_with` in
`core/cursor_del.rs`), finding each level's page through the slot the
cursor already holds (`TxnCore::page_and_slot`); `node_add_to` looks its
page up once, and `page_ops::insert_node`/`delete_node` move the node
pointers in one pass. A new overflow run is built with the value in
place instead of zero-filled and then copied over (`PageInit::Data`,
`DirtyPages::alloc_data`), and `PagePool` (`core/dirty.rs`) recycles run
buffers per length. The containers of a write transaction are kept
between transactions (`SpareTxn` in `core/txn_rw.rs`, 13 allocations per
one-put transaction down to 2); `write_meta` (`core/commit.rs`) reads
only the two fields of the old meta page that C reads, the next write
transaction begins from the meta page this process wrote last
(`WriterState::last_meta`) while no other writer has committed, the
commit writes pages through their slots in one pass, and the free list
is encoded and merged in place (`Idl::encode_into`,
`Idl::xmerge_encoded`). Memory: an environment now keeps up to 128 MiB
of overflow-run buffers plus the existing 256 MiB page pool between
transactions, both trimmed to what the last transaction used (and the
emptied containers of that transaction). `DUPSORT` paths are unchanged.

On the server (the x86-64 Hyper-V guest above, 4 vCPUs AMD EPYC Milan,
Rocky Linux 9 kernel 5.14, environments on a tmpfs in RAM, 4 KiB pages;
idle), interleaved A/B of `9aceeb0` (new) against `c679eb2` (base),
3 rounds of 3 reps, ns/op median (min):

| workload | base ns/op | new ns/op | new/base |
|---|---:|---:|---:|
| seq_put_append | 282 (272) | 204 (197) | **0.72** |
| rand_put | 2808 (2758) | 2697 (2654) | 0.96 |
| rand_overwrite | 3496 (3320) | 3472 (3383) | 0.99 |
| put_4k | 5059 (4897) | 4685 (4522) | **0.93** |
| put_64k | 65090 (63133) | 38860 (37594) | **0.60** |
| del_rand | 3770 (3663) | 3695 (3595) | 0.98 |
| txn_nosync | 7922 (7860) | 7675 (7450) | 0.97 |
| txn_sync (tmpfs) | 6742 (6672) | 6476 (6275) | 0.96 |
| mixed_writer | 10493 (9554) | 10259 (8851) | 0.98 |
| ls_update | 4952 (4904) | 5107 (4930) | 1.03 |
| get_4k / get_64k / ls_scan / mixed_readers | | | 1.04 / 1.05 / 1.02 / 0.97 (no changed code; noise) |

On the server, the whole round before L (`90cc145` against `9f17f7e`,
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
eb4fa08:bench/RESULTS.md`).

## Remaining gaps and why

In order of importance for this project. `DUPSORT` performance is out
of scope; its rows stay in the tables for completeness.

* **`ls_scan`: done for now.** Rust is at or ahead of C on all three
  machines (0.81-0.89x on macOS and in the VM, about 0.8-1.0x on the
  server, where C's own time varies between sessions; with C built with
  `-O3 -flto -DNDEBUG` it is at parity there). The step still checks every page and node access against
  the page (`AGENTS.md` section 3), which C does not; the scan waits on
  memory most of the time, so those checks cost little. Left: a page
  change runs the general path after the fast step declines (about 60
  instructions more than before, once in ~65 records), and the
  `NUMKEYS` test could be fused with the pointer bounds check (2
  instructions per record); neither could be shown to help.
* **Sequential appends (`seq_put_append`, 1.49x C on the server, 1.60x
  on macOS; 1.75-2.2x before P2).** P2 positions puts on the write
  cursor's own stack, which removed the read-cursor rebuild (17.6 % of a
  sequential put after S). What is left is the put plumbing: per append
  Rust runs 2,061 user instructions against C's 1,405 (callgrind, 4 KiB
  pages), spent in node add, the put functions and the touch (2.2-2.5x
  C's each), `mdb_put` with its DBI checks (311 against 85) and the
  write-side page accessors (178), while the descent itself is cheaper
  than C's
  ([P2](results/P2-write-path.md), "Profiles"). Removing them means
  restructuring the put path around one borrowed view of the cursor and
  its record.
* **Small-transaction overhead (`txn_nosync` 1.32x on the server, 1.22x
  on macOS, `txn_sync` on tmpfs 1.28x; `mixed_writer` 1.09x and 1.03x).**
  P2 keeps the transaction's containers between transactions (13
  allocations per one-put transaction down to 2), reads only what C
  reads of the meta pages and writes the commit's pages in one pass:
  user instructions -24 %, time -3 % on the server and -7 % on macOS.
  Rust still runs about 2.5x C's user instructions per one-put
  transaction (17.7k against 7.0k, callgrind): the whole-page copies on
  touch (2.4k instructions against C's 0.9k, kept for byte-identical
  output, below), the read-cursor round trips of `mdb_freelist_save`
  (about 1.5k), the validated meta-page read at begin when another
  process committed last, and the writer lock's semaphores on macOS.
  That accounts for part of the time gap on Linux; the rest was not
  found.
* **Random overwrites and deletes (`rand_overwrite` 1.13x, `del_rand`
  1.23x, random puts 1.12x on the 4 KiB-page server; 0.80-1.01x on
  macOS).** Dominated by the binary searches in a 1M-key tree (cache
  misses; 40-50 % of the put and delete paths on macOS) and the copy of
  every page touched for the first time: Rust copies the whole page so
  the unused bytes match the source page, C's `mdb_page_copy` copies
  only the used parts (16-33 % of these workloads). Copying only the
  used parts into a zeroed buffer would change unused bytes in the file
  (to zeros), the one layout change that would help here; not made,
  because output stays byte-identical (`AGENTS.md` section 1 allows it
  as a recorded, deliberate divergence). On x86 the put plumbing above
  adds 600-900 instructions per operation that the M1 hides behind
  cache misses, and deletes run `mdb_rebalance` and `mdb_cursor_del0`
  at 2.3x and 4.4x C's instructions.
* **Overflow-value puts: closed by P2.** `put_64k` and `put_4k` are at
  1.01x C on the server (were 1.70x and 1.10x), `put_64k` at 0.59x and
  `put_4k` at 1.01x on macOS. The suspect named before was right: Rust
  zero-filled every new run (page faults included) before copying the
  value in. P2 builds a run with the value in place and recycles run
  buffers, so a 64 KiB put takes 0.97 page faults instead of 4.25 and
  the commit no longer frees runs with `madvise`; C frees its runs.

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
* `seq_put_append`, before P2: read-cursor rebuild 17.6 %, B-tree
  search and read path 15 %, page lookups by number or slot 13 %,
  `page_touch` 7 %. After P2 the time profile is flat; the per-function
  instruction counts against C are in [P2](results/P2-write-path.md).
* `txn_nosync`, before P2: `pwrite` 57-59 %, page copies 10 %,
  writer-lock semaphores 7 %, `malloc`/`free` 1-2 %. P2 removed most of
  the allocations and the full meta-page copies of the commit.
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
  `0498cf1` (except where stated), so the numbers measure the libraries,
  not the disks.
* Commands, from the repository root: `cargo build --release -p rlmdb-bench &&
  target/release/rlmdb-bench --rounds 2 --reps 3` (macOS);
  `rlmdb-bench --dir /dev/shm/rlmdb-bench --reps 5` and `--only
  ls,read,dupsort --rounds 3 --reps 3` (server; the ext4 row with
  `--only small_txn` and `--dir` on that file system); in the VM,
  `scripts/linux-test.sh run --release -p rlmdb-bench -- --dir
  /tmp/lmdb-bench --reps 3`.

**Compiler flags.** C is built with the upstream Makefile's flags (`-O2
-g -pthread -W -Wall -Wno-unused-parameter`, no `NDEBUG`), Rust with the
workspace's release profile (opt-level 3, thin LTO, `codegen-units = 1`);
neither side uses `-march=native` or `target-cpu=native`. Two
asymmetries remain: the C bench calls `mdb.c` in another translation
unit without LTO, while the Rust bench gets library calls inlined
through LTO; and C keeps its `mdb_assert` checks, since `NDEBUG` is not
defined. To see what that is worth, the C side was rebuilt on the
x86-64 server with `-O3 -flto -DNDEBUG` (through `CFLAGS`, which the
`cc` crate passes on), with the same Rust build, and the groups `ls`,
`read`, `seq_put`, `rand_put`, `overwrite`, `delete` and `small_txn` run
back to back with each set of C flags in one session:

| C flags | C `ls_scan` ns/record, median (min) | Rust `ls_scan` ns/record, median (min) | Rust/C (medians) |
|---|---:|---:|---:|
| upstream (`-O2`) | 58.1 (55.2) | 53.5 (52.5) | 0.92 |
| `-O3 -flto -DNDEBUG` | 55.1 (53.8) | 54.8 (53.6) | 1.00 |

The stronger flags make C's `ls_scan` about 5 % faster; every other row
(scans, gets and all the write workloads) moved by at most 4 %, in both
directions (`ls_update` the most, 5255 to 5024 ns), which is within
this session's noise. The upstream flags stay the documented default,
because they are what the C library ships with.
