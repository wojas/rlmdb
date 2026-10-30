# rlmdb-bench

Benchmarks of the Rust rewrite (`rlmdb`) against the C library
(`libraries/liblmdb`, 0.9.36), with identical workloads and data on both
sides. Results and analysis: [`RESULTS.md`](RESULTS.md).

* **C side**: `c/cbench.c`, linked with `mdb.c`/`midl.c` compiled by
  `build.rs` with the interop test bed's flags (the upstream Makefile's:
  `-O2 -g -pthread -W -Wall -Wno-unused-parameter`).
* **Rust side**: `src/workloads.rs` on `rlmdb`'s byte-level API, built
  with the workspace's release profile (`lto = "thin"`,
  `codegen-units = 1`).
* **Data**: `src/data.rs` and the top of `cbench.c` implement the same
  generator (`splitmix64` with fixed seeds): 16-byte keys, 100-byte values
  (zero-copy slices of a fixed random pool), the same random index streams
  and permutations, so both sides store the same data in the same
  order. `rlmdb-bench --check-data` verifies that: both sides hash
  everything the generator produces and the driver compares the hashes.
* **Timing**: hand-rolled. Every workload runs one warm-up iteration and
  `--reps` timed ones (`CLOCK_MONOTONIC` / `Instant`); the table shows the
  median and the minimum time per operation, operations per second (from
  the median) and the ratio of the medians (Rust / C; above 1 means Rust
  is slower). Each workload group runs in a fresh process per side; the
  side that goes first alternates between groups.

## Running

```sh
cd rust
cargo run --release -p rlmdb-bench               # everything, both sides (20-30 min)
cargo run --release -p rlmdb-bench -- --check-data   # C and Rust generate the same data?
cargo run --release -p rlmdb-bench -- --only read,dupsort --reps 9
cargo run --release -p rlmdb-bench -- --impl rust --only rand_put --raw
cargo run --release -p rlmdb-bench -- --help
# Linux (4 KiB pages), in the Docker container of scripts/linux-test.sh;
# environments go to the container's /tmp, not the bind-mounted workspace:
scripts/linux-test.sh run --release -p rlmdb-bench -- --dir /tmp/lmdb-bench
```

Options: `--n N` (keys, default 1 000 000; every other size scales with
it), `--reps R` (default 5), `--sync-commits K` (commits of `txn_sync`,
default 1000), `--dir D` (where the environments are created, default the
temp directory; everything is deleted afterwards), `--only G1,G2`,
`--impl both|c|rust`, `--raw` (also print every sample), `--rounds K`
(run every group K times per side, alternating which side goes first;
the samples are pooled).

### A/B of two versions of `rlmdb`

`--baseline EXE` replaces the C side with the Rust side of another build
of `lmdb-bench`, for measuring a change to the library: build the bench
at the old version, keep the binary, build the new version and compare,
interleaved:

```sh
cargo build --release -p rlmdb-bench && cp target/release/lmdb-bench /tmp/lmdb-bench-old
# ... change rlmdb ...
cargo run --release -p rlmdb-bench -- --baseline /tmp/lmdb-bench-old --rounds 3 --reps 3
```

The machine this was developed on was shared with other heavy jobs, so
interleaved rounds were much more repeatable than two separate runs;
`RESULTS.md` reports both.

The largest data sets are about 0.35 GB (`put_64k`) and 0.2 GB (the
1M-key databases); only one exists at a time.

## Workloads

All environments use `MDB_NOSYNC` except `txn_sync`, so the numbers
measure the libraries, not the disk; transactions are committed every
10 000 operations unless stated otherwise. `n` is `--n`.

| group | workload | what is timed |
|---|---|---|
| `seq_put` | `seq_put_append` | `n` puts of ascending keys with `MDB_APPEND` into a new DB |
| `rand_put` | `rand_put` | `n` puts of random keys into a new DB |
| `read` | `rand_get` | `n` `mdb_get` of random existing keys, one read txn |
| | `scan_fwd`, `scan_bwd` | a full `MDB_NEXT` / `MDB_PREV` cursor scan |
| | `readers8_get` | 8 threads, each its own read txn and `n` random gets (wall time, all gets) |
| `overwrite` | `rand_overwrite` | `n` puts overwriting every key once, random order |
| `dupsort` | `dup_put` | `n/10` keys x 10 duplicates (100 bytes) into a new `MDB_DUPSORT` DB |
| | `dup_get_both` | `n` `MDB_GET_BOTH` lookups of random pairs |
| | `dup_next_dup` | for every key in random order: `MDB_SET`, then `MDB_NEXT_DUP` to the end |
| `overflow` | `put_4k`, `put_64k` | `n/20` puts of 4 KiB values, `n/200` puts of 64 KiB values (txn per 1000) |
| | `get_4k`, `get_64k` | `n` random gets, touching the first and last byte |
| `delete` | `del_rand` | delete all `n` keys in random order (the DB is rebuilt untimed per rep) |
| `small_txn` | `txn_nosync` | `n/20` transactions of one put each, `MDB_NOSYNC` |
| | `txn_sync` | `--sync-commits` such transactions with the default (synchronous) commit |
| `mixed` | `mixed_writer` | `n/2000` commits of 100 random overwrites each, while 4 threads scan |
| | `mixed_readers` | entries those 4 scanners read during the writer's run (ns per entry over the writer's wall time) |

On macOS (16 KiB pages) 4 KiB values fit in a leaf page; on Linux (4 KiB
pages) they go to overflow pages. 64 KiB values are overflow pages
everywhere.

The C program runs read transactions in the default (TLS) mode; Rust read
transactions always behave like `MDB_NOTLS`. Both open the environment
with a 4 GiB map, 4 DBs, 126 readers.
