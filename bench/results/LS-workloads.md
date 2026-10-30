# Lightning Stream style workloads (`ls` group)

Two synthetic workloads that loosely simulate
[Lightning Stream](https://github.com/PowerDNS/lightningstream) (LS)
syncing a PowerDNS Authoritative database, on the C library and on
`rlmdb` with identical data and logic. No Lightning Stream code is used,
linked or run; only its value header layout is copied. Code:
`ls_group` in `src/workloads.rs` and `c/cbench.c`, data generator
`ls_key` / `ls_value` / `ls_update_value` in `src/data.rs` and the top
of `cbench.c`. Workload table and method: [`../README.md`](../README.md).

**Summary.** On macOS a sync scan costs about 17-19 ns per record on
both sides, with Rust at 1.11x C (median; 1.07x by the minimum). Updates
of 1 % of the records are at 0.86x C. On Linux, in a VM that another
benchmark was loading at the same time, the scan was at 1.25x C by the
median but 1.03x by the minimum, and updates at 1.54x (1.27x). Rust's
scan runs about 1.35x C's instructions per record, mostly bounds checks
and one extra call level. The scan waits on memory most of the time, so
that shows as only about 10 % in time.

## What the workloads do

LS keeps every value of an LMDB database prefixed with a 24-byte header:
the timestamp of the last change (8 bytes, big-endian nanoseconds since
the Unix epoch), the LMDB transaction id of the change (8 bytes,
big-endian here), a version byte (0), the number of extra 8-byte blocks
(0), 2 bytes of flags (bit 0: deleted) and 4 bytes of padding; the
application payload follows. On every sync interval LS opens a read
transaction and walks the whole database with a cursor, looking for
records whose header transaction id is above the one it last synced.
The application updates a record by putting a new value with a new
header.

* **`ls_scan`** (the important one): one read transaction, a full
  `MDB_NEXT` cursor scan that decodes the header transaction id of every
  value (bytes 8..16, big-endian, the same byte decoding on both sides:
  one load and a byte swap) and counts the records above the "last
  synced" transaction id. The database is all clean, so the count is 0;
  both sides check it, and that the scan saw every record. Timed: the
  cursor open, the scan and the cursor close; reported per record.
* **`ls_update`**: puts a new value for `5n/100` records (1 %, 50 000 at
  the default `--n`) picked by a random index stream, duplicates
  possible, committing every 10 000 puts. Each new value has a fresh
  header (transaction id above the last sync, a new timestamp) and a
  payload of the record's usual length. Reported per put. As in
  `rand_overwrite`, every repetition writes the same keys with new
  values (repetition `it` writes transaction id `last sync + 1 + it`),
  and the database is not rebuilt between repetitions. Afterwards an
  untimed scan checks that exactly the distinct updated records are now
  above the last sync.

The database is built once per group, untimed: records `0..5n` put in
record order, which is random key order, committing every 10 000 puts,
with `MDB_NOSYNC` like every other workload except `txn_sync`.
`ls_scan` runs on the fresh database, then `ls_update`.

## Data shape

* `5n` records: 5 million at the default `--n` (1 000 000), 500 000 at
  `--n 100000`.
* Keys: 20-40 bytes, DNS-name-like, `<host>.<zone>.<tld>`. The host label
  is random lowercase letters followed by the record number in 6 base-36
  digits, so keys are unique and sort in random order; the zone label is
  random letters, the TLD one of 8 (`com`, `net`, `org`, `nl`, ...).
  Example: `kqkbtibyag000003.zxhdvlh.nl`. Average 30 bytes.
* Values: the 24-byte header and a 40-100 byte payload from the shared
  pseudo-random pool (average 94 bytes in all). Transaction ids are
  spread over `1..=1 000 000` (the last sync is 1 000 000) and
  timestamps follow them: 2 s per transaction id from November 2023,
  plus a random offset within those 2 s.
* Everything is a function of the record number and fixed seeds, so it
  is regenerated rather than stored: memory use stays flat (the update
  keys, 50 000 of them, are precomputed outside the timed loop).
  `--check-data` hashes every key and value, the update index stream and
  the values of the first update repetition on both sides.
* Data file: 1.30 GB with 16 KiB pages (macOS) and 1.14 GB with 4 KiB
  pages (Linux) after the build, 1.34 GB and 1.15 GB after the updates.
  The records take about 0.67 GB with their node headers and pointers.
  Random insertion leaves the pages only 50-60 % used.

## macOS (Apple M1 Pro, 16 KiB pages)

* macOS 15, Apple M1 Pro (10 cores), 32 GiB RAM, APFS on the internal
  SSD; environments in `$TMPDIR`.
* rustc 1.93.1 (Homebrew), Apple clang 17.0.0 (clang-1700.4.4.1).
* Command (from `rust/`): `cargo build --release -p rlmdb-bench &&
  target/release/rlmdb-bench --dir "$TMPDIR/lmdb-bench-ls" --only ls
  --rounds 3 --reps 3` (n = 1 000 000, 3 rounds alternating which side
  goes first, 3 timed reps after 1 warm-up each, so 9 samples per side).
  The machine was shared with other jobs (a benchmark in the Linux VM).

| workload | ops/rep | C ns/op median (min) | Rust ns/op median (min) | Rust/C median | Rust/C min |
|---|---:|---:|---:|---:|---:|
| ls_scan | 5000000 | 17.2 (16.7) | 19.0 (17.8) | **1.11** | 1.07 |
| ls_update | 50000 | 6496 (5781) | 5583 (4729) | **0.86** | 0.82 |

A quick run at `--n 100000` (500 000 records, same command otherwise)
gave `ls_scan` 19.4 (16.3) ns for C and 17.4 (16.8) ns for Rust, and
`ls_update` 3824 (3357) and 3217 (3113) ns.

## Linux (Debian bookworm container, 4 KiB pages)

* `Linux 6.8.0-146-generic #146-Ubuntu SMP PREEMPT_DYNAMIC aarch64`
  (the Lima VM behind `scripts/linux-test.sh`, 8 vCPUs and 12 GB on the
  same M1 Pro host), Debian 12 `rust:1-bookworm` container, glibc.
* Environments on a tmpfs in the VM's memory: `scripts/linux-test.sh`
  mounts a 4 GiB tmpfs at the container's `/tmp`. The database used at
  most 1.07 GiB of it.
* rustc 1.99.1, gcc 12.2.0 (Debian 12.2.0-14+deb12u1).
* Command (from `rust/`): `scripts/linux-test.sh run --release -p
  rlmdb-bench -- --dir /tmp/lmdb-bench --only ls --rounds 3 --reps 3`.
* Another benchmark was running in a container of the same VM at the
  same time, so these numbers are noisier than the macOS ones: the
  medians and minimums of the Rust side differ by 27 % (scan) and 35 %
  (update). Read the minimums as the better estimate.

| workload | ops/rep | C ns/op median (min) | Rust ns/op median (min) | Rust/C median | Rust/C min |
|---|---:|---:|---:|---:|---:|
| ls_scan | 5000000 | 45.5 (43.3) | 56.9 (44.7) | **1.25** | 1.03 |
| ls_update | 50000 | 5805 (5211) | 8967 (6622) | **1.54** | 1.27 |

A scan step costs about 2.5x as much as on macOS on both sides: with
4 KiB pages the same records take about 3.5 times as many pages, and the
VM adds its own overhead to memory accesses.

## Where `ls_scan` spends its time (macOS)

Profiled with Instruments' Time Profiler (`xcrun xctrace record
--template 'Time Profiler' --launch -- <worker> ls <dir> 1000000 64
1000`, then `xctrace export` of the `time-profile` table) on the
release builds above; only samples whose stack contains the scan
(`ls_changed`, `cursor_next`, `RoCursor::next`) are counted. Instructions
per record were counted by single-stepping `next` calls in the middle of
a leaf page under lldb.

| | C | Rust |
|---|---:|---:|
| scan samples (65 scans of 5M records) | 5496 | 5876 (1.07x) |
| instructions per record, library call | 93 (`mdb_cursor_get` → `mdb_cursor_next` → `mdb_node_read`) | 119 (`cursor_next` → `read_position`) |
| instructions per record, caller's loop | 16 (`ls_changed`) | 28 (`RoCursor::next`, `finish`, the loop) |
| total instructions per record | 109 | 147 (1.35x) |

Leaf samples, C: `mdb_cursor_next` 48 %, `mdb_cursor_get` 29 %,
`ls_changed` 12 %, `mdb_node_read` 11 %. Rust (with inlined frames
resolved): `cursor_next` itself 35 %, `read_position` /
`read_position_in` 17 %, the page and node accessors inlined into them
(`get_u16`, `PageRef`, slice range checks, `node_read`,
`NodeRef::data_size`, `with_layout`) about 40 %, `RoCursor::next` 7 %.
The hottest instructions are the same on both sides: the first loads of a
record's node (its flags and sizes) and of the page header after the
cursor's index is advanced. Both sides read the same bytes of an
identical file there. The scan mostly waits on those loads, which is why
35 % more instructions cost only 7-11 % more time.

Where Rust's extra instructions are (all in `rlmdb`, none changed in
this task):

* **One more call level per record.** `RoCursor::next`
  (`rlmdb/src/core/ro_cursor.rs:429`) calls `cursor_next`
  (`rlmdb/src/core/cursor.rs:758`), which saves and restores 12
  registers and calls `read_position` (`cursor.rs:650`) out of line.
  `read_position` has a frame of its own and returns the `Result<Kv>`
  through memory. C does the read inside `mdb_cursor_next` and tail-calls
  `mdb_node_read`.
* **The format dispatch twice.** Both `cursor_next` and `read_position`
  go through `per_format!` (`cursor.rs:71`) on `mc.format`, each with
  its own check of the stack depth (`top <= 31`, then
  `panic_bounds_check`).
* **Bounds checks on every page and node access.** `PageRef::num_keys`
  (`rlmdb/src/format/page.rs:208`) is computed twice per record (the
  `ki + 1 >= num_keys` test in `cursor_next_in`, `cursor.rs:827`, and
  `mp.node(ki)` in `read_position_in`, `cursor.rs:678`). Each time it
  falls back to a static header if the page is shorter than one and
  saturates an `mp_lower` below the header size.
  `PageRef::node` (`page.rs:267`), `NodeRef::with_layout`,
  `NodeRef::data_size`, `NodeRef::key` and `NodeRef::inline_data`
  (`rlmdb/src/format/node.rs:62`, `:121`, `:153`, `:179`) check the
  node offset, the node header and the key and data lengths against the
  page. C trusts the page. These checks are what keeps crafted files from
  causing out-of-bounds reads (`AGENTS.md` section 3), so they stay. The
  repeated `num_keys` and format dispatch could be done once per step.
* **The caller's loop.** `RoCursor::finish` (`ro_cursor.rs:210`)
  re-tests `C_DEL` and turns the `Result<Kv>` into
  `Result<Option<Entry>>`. In this benchmark the scan loop is inlined
  into the large `workloads::run` function, which keeps the cursor and
  the counter on the stack and reloads them every iteration (the C
  `ls_changed` is not inlined and keeps them in registers). That part is
  the benchmark's code generation, not the library's.

On Linux, the scan's minimum ratio was 1.03. The extra instructions
matter even less there, because more of each step is spent waiting on
memory.
