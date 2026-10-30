# LMDB 1.0 data file (dual-format stage 2)

`DUAL_FORMAT.md` stage 2: rlmdb reads and writes LMDB 1.0's data file. For
0.9 environments the change adds a format sniff at open, the layout-aware
lookups of the dirty-page slots, two branches on the format in rarely taken
write paths (`mdb_rebalance` of a root, overflow overwrites, `mdb_drop`,
the compacting copy) and the `me_pbuf` pad of `DUPSORT` sub-pages. Measured
on macOS (same machine, 16 KiB pages, 0.9 environments) against a baseline
built from the commit before the change, interleaved (`--baseline`, 3
rounds of 3 reps, `n` = 1000000). Other jobs kept the load average between
6 and 25 during the runs, so single rows move by up to ±10 % between runs.

The first run showed `seq_put_append` 1.05 (min 1.06): `MDB_APPEND` puts
searched the tree twice (the empty-tree check for `KNOWN_C_BUGS.md` #32
called `cursor_last`, which searched again). Fixed; the run after the fix:

| workload | base ns/op | new ns/op | new/base (median) | (min) |
|---|---:|---:|---:|---:|
| seq_put_append | 170 (166) | 165 (161) | 0.97 | 0.97 |
| rand_put | 2554 (2391) | 2556 (2440) | 1.00 | 1.02 |
| rand_get | 851 (789) | 951 (766) | 1.12 | 0.97 |
| scan_fwd | 16.7 (16.3) | 17.4 (16.8) | 1.05 | 1.03 |
| scan_bwd | 15.1 (15.0) | 15.6 (15.4) | 1.03 | 1.03 |
| readers8_get | 187 (145) | 158 (127) | 0.85 | 0.88 |
| rand_overwrite | 3608 (3368) | 3662 (3432) | 1.01 | 1.02 |
| dup_put | 1081 (1040) | 1077 (1026) | 1.00 | 0.99 |
| dup_get_both | 856 (793) | 855 (776) | 1.00 | 0.98 |
| dup_next_dup | 93.3 (89.0) | 91.6 (82.7) | 0.98 | 0.93 |
| put_4k | 7447 (6888) | 7220 (6965) | 0.97 | 1.01 |
| get_4k | 566 (515) | 563 (506) | 1.00 | 0.98 |
| put_64k | 27984 (26946) | 30385 (25888) | 1.09 | 0.96 |
| get_64k | 232 (219) | 229 (219) | 0.99 | 1.00 |
| del_rand | 3705 (3425) | 3595 (3339) | 0.97 | 0.97 |
| txn_nosync | 10054 (8348) | 8993 (8168) | 0.89 | 0.98 |
| txn_sync | 10548116 (9769399) | 10635869 (10149891) | 1.01 | 1.04 |
| mixed_writer | 5620 (5422) | 5694 (5433) | 1.01 | 1.00 |
| mixed_readers | 4.9 (4.8) | 4.9 (4.8) | 1.01 | 1.00 |
| ls_scan | 17.8 (17.5) | 18.0 (17.9) | 1.01 | 1.02 |
| ls_update | 5660 (4507) | 5520 (4736) | 0.98 | 1.05 |

The read path did not change; its rows moved in both directions from run
to run (the first run, before the fix, had `rand_get` 0.98, `scan_fwd`
1.00, `scan_bwd` 0.97). A rerun of the `read` and `overflow` groups (6
rounds, load average 7-12), new/base median (min): `rand_get` 0.99 (1.01),
`scan_fwd` 0.99 (0.96), `scan_bwd` 0.98 (0.98), `readers8_get` 1.04
(1.05), `put_4k` 1.03 (1.02), `get_4k` 0.98 (1.04), `put_64k` 0.98 (0.97),
`get_64k` 0.95 (0.99). Reading: 0.9 performance is unchanged within noise.

Not measured: the bench always creates 0.9 environments; a 1.0 run needs a
format option in the bench and in its `--baseline` protocol.
