# interop: C ⇄ Rust LMDB compatibility test bed

The Rust rewrite must share environments with C LMDB *concurrently*: same
data file, same lock file, same reader table and writer lock. This crate
proves it by running **peer programs** as separate processes against one
environment and checking what each sees.

* `cpeer` (C, `c/cpeer*.c`) speaks the protocol below, linked against
  `liblmdb` built from `../../libraries/liblmdb` (LMDB 0.9.36+) by `build.rs`
  with the upstream Makefile's flags and the platform's default lock mode.
* `rpeer` (Rust, `src/bin/rpeer/`, on the `rlmdb` crate) speaks the same
  protocol, with the same LMDB calls in the same order and the same output
  (see "Peer status" below).
* `cpeer_capi` is the C peer compiled against `../rlmdb-capi/include/lmdb.h`
  and linked against the Rust `liblmdb.a` of `rlmdb-capi` instead of the C
  library: the same C program on the Rust implementation (`Peer::COnRust`).
  `build.rs` compiles its objects; `interop::bins::cpeer_capi()` links them
  at test time (once per test process), because cargo builds `liblmdb.a`
  (`rlmdb-capi` is a dev-dependency of this crate) only after build scripts
  run.
* `layout_dump` (`c/layout_dump.c`) `#include`s `mdb.c` and prints
  `sizeof`/`offsetof`/`alignof` of every on-disk and lock-file struct.
* The upstream tools `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy` and
  `mtest`…`mtest5` are built too. (`mdb_drop` does not exist in 0.9, and
  `mtest6` needs an `MDB_DEBUG` build, as upstream.)

The C programs are compiled into `$OUT_DIR/bin` (`interop::bins` has the
paths); `rpeer` is a normal `[[bin]]` of this crate.

Three peers give nine ordered (writer, reader) pairs. Every scenario runs
for every pair, so each implementation writes what each implementation
reads, concurrently where the scenario says so; C always verifies and
audits too.

## Running

```sh
cargo test -p rlmdb-interop                       # everything: 9 pairs, three peers, layout, rpeer (~1.5 min)
cargo test -p rlmdb-interop --test c_vs_c         # every scenario with C on both sides
cargo test -p rlmdb-interop --test rust_writes_c_reads   # every scenario, Rust writes, C reads
cargo test -p rlmdb-interop --test rpeer          # rpeer vs cpeer outputs, checkers, exit codes
cargo test -p rlmdb-interop --test c_vs_c -- --nocapture mvcc   # one scenario, with its log
INTEROP_LOOP_SECS=20 cargo test -p rlmdb-interop --no-fail-fast  # stress: 20 s loops (~6 min)
rust/scripts/linux-test.sh test -p rlmdb-interop  # the same on Linux (Docker)
```

| test file | what |
|---|---|
| `tests/c_vs_c.rs` | `scenario_tests!(C, C)`: validates the harness and the C peer; negative tests of the checkers (`verify`, `check-freelist`, `pagecmp`) |
| `tests/c_writes_rust_reads.rs` | `scenario_tests!(C, Rust)` |
| `tests/c_writes_conrust_reads.rs` | `scenario_tests!(C, COnRust)` |
| `tests/rust_writes_c_reads.rs` | `scenario_tests!(Rust, C)` |
| `tests/rust_vs_rust.rs` | `scenario_tests!(Rust, Rust)` |
| `tests/rust_writes_conrust_reads.rs` | `scenario_tests!(Rust, COnRust)` |
| `tests/conrust_writes_c_reads.rs` | `scenario_tests!(COnRust, C)` |
| `tests/conrust_writes_rust_reads.rs` | `scenario_tests!(COnRust, Rust)` |
| `tests/conrust_vs_conrust.rs` | `scenario_tests!(COnRust, COnRust)` |
| `tests/three_peers.rs` | `scenarios::all_peers`: all three implementations at once |
| `tests/rpeer.rs` | `gen`, `stat`, `dump`, `verify`, `check-freelist`, and `create`/`fill`/`delete` byte-identical to C (also failure reports); unimplemented commands exit 4; usage errors |
| `tests/layout.rs` | the C struct layout against DESIGN.md, and every `layout_dump` value against `lmdb::consts`/`lmdb::format::layout` |

A scenario step that needs something a peer lacks (none at the moment)
prints
`[interop] SKIPPED ...` on stderr (visible without `--nocapture`, never a
silent pass): either C runs that step instead (`capable()`), or the whole
scenario is skipped (its test passes). Three things count as lacking: the
peer's `commands` list says `implemented=0` (exit status 4 if run anyway);
an LMDB call fails with `ENOSYS` (`COnRust` while `rlmdb-capi` has
placeholders); or a peer completes a command but prints a
`NOT-IMPLEMENTED` line on stderr for a part it left out (reported as
`SKIPPED partly`).

| variable | meaning |
|---|---|
| `INTEROP_LOOP_SECS` | duration of `writer-loop`/`reader-loop` runs in the scenarios (default 1.5) |
| `INTEROP_TIMEOUT_SECS` | a peer running longer is killed and the test fails (default 120) |
| `INTEROP_RPEER` | path of the Rust peer (else found via `CARGO_BIN_EXE_rpeer` or next to the test binary) |
| `INTEROP_KEEP` | keep the temporary directories (`interop-*`) of the environments, to look at the files after a failure |

The tests print the page size of each environment they create (`--nocapture`):
16384 on Apple Silicon macOS, 4096 on Linux x86-64.

## Rust harness

* `Peer::{C, Rust, COnRust}`: `peer.on("fill", &env).args([...]).run()`
  runs a command and requires exit 0 (exit 4, or exit 3 with `ENOSYS`,
  gives an error that `is_not_implemented()`); `.output()` returns any
  status; `.spawn()`
  returns a `PeerProcess` for long-running commands (`wait_ready()`,
  `release()`, `finish()`, `kill()` for a SIGKILL; dropping it kills the
  process). `peer.supports(command)` tells whether the peer implements a
  command.
* `TestEnv`: an environment in a temp dir (`new()`, `new_nosubdir()`,
  `with_name()`, `derived(name)` for copy targets).
* `report`: parsers for every command's output. `tools`: runs and parses
  the C tools. `tools::c_layout()`: the parsed `layout_dump`.
* `scenarios`: every scenario as `fn(Pair) -> Result<()>` with
  `Pair { writer, reader }`; `scenario_tests!(writer, reader)` turns them
  all into `#[test]`s. `tests/c_vs_c.rs` is just
  `interop::scenario_tests!(Peer::C, Peer::C)` plus self-tests showing the
  checkers reject bad data and broken page accounting; the other eight
  pair files are the same line for their pair. `scenarios::bidir` holds
  the scenarios in which both peers write; `scenarios::all_peers` those
  that use all three peers at once.
* `pagecmp::compare_data_files(a, b)`: do two data files hold the same
  trees on the same pages? Compares a canonical form of every page
  reachable from the newest meta page (headers, node pointers, nodes,
  sub-pages, overflow values, `P_LEAF2` keys) and both meta pages, and
  ignores bytes C leaves uninitialised (node padding, free space inside
  pages and sub-pages, the tail of overflow runs) and stale free pages.

### Peer status

`rpeer` mirrors the C peer file by file (`args.rs` = `cpeer.c`, `dataset.rs`
= `dataset.c`, `data.rs` = `cpeer_data.c`, `loops.rs` = `cpeer_loops.c`,
`misc.rs` = `cpeer_misc.c`, `pagewalk.rs` = `cpeer_pagewalk.c`) and uses only
the public API of the `rlmdb` crate (no `unsafe`): `rlmdb::core`
for everything except `check-freelist`, which decodes the data file with
`rlmdb::format` alone. `cpeer_capi` is `cpeer` itself; what it can do
depends on what `rlmdb-capi` implements.

| command | C | Rust (`rpeer`) | COnRust (`cpeer_capi`) |
|---|---|---|---|
| `create`, `fill`, `delete`, `drop`, `verify`, `dump`, `stat`, `gen` | yes | yes, output identical to C (`tests/rpeer.rs`) | yes |
| `readers`, `reader-check`, `hold-reader`, `hold-env`, `hold-writer`, `reader-loop`, `check-freelist` | yes | yes | yes |
| `writer-loop`, `nested-test` | yes | yes (`RwTxn::begin_nested_txn`) | yes |
| `copy` | yes | yes (`Environment::copy_to_path`) | yes |

All three implement everything; no scenario is skipped on macOS or Linux.
Database names are bytes in all three (rpeer uses
`Transaction::open_db_bytes`).

`<peer> commands` lists what a peer implements (`command=<name>
implemented=<0|1>`); `Peer::supports(command)` asks it once. To enable a
command in rpeer, implement it and set `implemented: true` in `main.rs`;
scenarios then use it automatically.

In a pair the **writer** creates environments and writes; the **reader**
verifies, holds snapshots and runs reader loops; page audits always run on
the reader *and* on C (the reference), and data written by the writer is
verified by the writer, the reader and C. Many scenarios also run commands
on both peers (two concurrent writer loops, one per peer, with reader loops
of all three peers; `readers` both ways), and the scenarios of
`scenarios::bidir` have both peers write in turns.

## The peer command protocol

```
<peer> <command> <positional args...> [options...]
```

Options start with `--` and may appear anywhere after the command. Exit
status: `0` success, `1` a check failed (data or invariant mismatch, message
on stderr), `2` usage error, `3` an LMDB call failed unexpectedly (message
names the call and the error), `4` the peer does not implement the command
(yet). Output goes to stdout as lines of
space-separated `key=value` tokens, line-buffered. Messages on stderr start
with `cpeer: ` (or `rpeer: `).

### Environment options (every command)

| option | effect |
|---|---|
| `--nosubdir` | `MDB_NOSUBDIR`: `<env>` is the data file, lock file `<env>-lock` |
| `--notls` | `MDB_NOTLS` (default: TLS mode, as C's default) |
| `--nosync` / `--nometasync` | `MDB_NOSYNC` / `MDB_NOMETASYNC` |
| `--mapsize N` | `mdb_env_set_mapsize` (default: keep the environment's) |
| `--maxdbs N` | `mdb_env_set_maxdbs` (default 64) |
| `--maxreaders N` | `mdb_env_set_maxreaders` (default: LMDB's, 126) |

Commands that only read open the environment with `MDB_RDONLY`: `verify`,
`dump`, `stat`, `readers`, `reader-check`, `hold-reader`, `hold-env`,
`reader-loop`, `check-freelist`, `copy`, `nested-test --check`.

### Data options (`fill`, `verify`, `delete`, `hold-reader`, `gen`)

| option | effect |
|---|---|
| `--dupsort` | `MDB_DUPSORT` |
| `--dupfixed` | `MDB_DUPSORT \| MDB_DUPFIXED` |
| `--intkey` | `MDB_INTEGERKEY` (8-byte keys) |
| `--intdup` | `MDB_DUPSORT \| MDB_DUPFIXED \| MDB_INTEGERDUP` (4- or 8-byte values) |
| `--revkey` | `MDB_REVERSEKEY` |
| `--revdup` | `MDB_DUPSORT \| MDB_REVERSEDUP` |
| `--valsize N` | value size (max size for DUPSORT); default 32, DUPSORT 16, `--intdup` 8 |
| `--big N` | items with `i % 3 == 0` get `N`-byte values (overflow pages); not with DUPSORT |
| `--dups N` | max values per key (DUPSORT; default 5) |
| `--deleted-every N` | the data set as left by `delete --every N` |

### Commands

**`create <env> <mapsize> <maxdbs>`** — create/open the environment and exit.
`created psize=16384 mapsize=67108864 maxkeysize=511 last_txnid=0`

**`fill <env> <db|-> <count> <seed> [data options] [--per-txn N] [--append] [--reserve]`**
— write the data set (below) into `<db>` (`-` = main DB), creating it with
the data flags. Items in index order with `mdb_put` (random order for the
comparator), committing every `N` keys with `--per-txn`. `--append`: in
comparator order with `mdb_cursor_put(MDB_APPEND)` for a key's first value
and `MDB_APPENDDUP` for the others (the DB must not hold larger keys).
`--reserve`: `mdb_put(MDB_RESERVE)` then write the value (not with DUPSORT).
Overwrites existing equal keys, so filling twice is idempotent.
`filled db=<db> keys=<count> entries=<ms_entries> txns=<n> psize=.. depth=.. branch_pages=.. leaf_pages=.. overflow_pages=..`

**`verify <env> <db|-> <count> <seed> [data options]`** — check the DB holds
exactly the data set, in one read txn: DB flags equal the data flags;
`mdb_stat` entries; `mdb_get` of every present key returns its first
(smallest) value; deleted keys give `MDB_NOTFOUND`; a full `MDB_FIRST/NEXT`
walk and a full `MDB_LAST/PREV` walk equal the sorted model; for DUPSORT
per key `mdb_cursor_count`, `MDB_FIRST_DUP`, `MDB_LAST_DUP`, `MDB_GET_BOTH`
of every value and a `MDB_NEXT_NODUP` walk; for DUPFIXED
`MDB_GET_MULTIPLE`/`MDB_NEXT_MULTIPLE`; `MDB_SET_RANGE` on every 7th key.
With `-`, the main DB must hold nothing else (no named DBs).
`verified db=<db> keys=<present> entries=<pairs> absent=<deleted keys>`

**`delete <env> <db|-> <count> <seed> [--every N] [--per-txn N] [data options]`**
— delete a deterministic subset (default `N = 2`): key `i` entirely if
`i % N == 0` (alternating `mdb_del(key, NULL)` and `MDB_SET` +
`mdb_cursor_del`, with `MDB_NODUPDATA` for DUPSORT); for DUPSORT and
`N > 1`, the values `d % 2 == 0` of keys with `i % N == 1` one by one
(alternating `mdb_del(key, value)` and `MDB_GET_BOTH` + `mdb_cursor_del`).
Items already gone per `--deleted-every` are skipped; a missing item is an
error (exit 1). Verify afterwards with `--deleted-every N`.
`deleted db=<db> keys=<keys gone> entries=<pairs deleted> txns=<n>`

**`drop <env> <db|-> [--delete]`** — `mdb_drop`: empty the DB, or with
`--delete` delete it from the environment (the main DB can only be
emptied), in one transaction. `dropped db=<db> delete=0|1 entries=<n>
pages=<branch+leaf+overflow pages it had>`. A missing DB is a check
failure (exit 1).

**`dump <env> <db|->`** or **`dump <env> --all`** — print `db=<db>`, then
one line per key/value pair in cursor order: `<hex key>=<hex value>`.
Records of named DBs in the main DB print as `<hex key>=@db` (their value,
an `MDB_db` with page numbers, differs between copies). `--all` dumps the
main DB, then every named DB in main-DB order. Two environments with the
same content dump the same, whoever wrote them.

**`stat <env>`** — environment and DB statistics:

```
psize=16384
mapsize=268435456
last_pgno=940
last_txnid=12
maxreaders=126
numreaders=1
maxkeysize=511
free_pages=4                      (sum of the free DB's page lists)
db=@free depth=1 branch_pages=0 leaf_pages=1 overflow_pages=0 entries=2
db=- flags=0x0 depth=1 branch_pages=0 leaf_pages=1 overflow_pages=0 entries=6
db=<name> flags=0x4 depth=2 ...   (every named DB, as mdb_stat -a finds them)
```

(The free DB line has no flags: `mdb_dbi_flags` rejects DBI 0.)

**`readers <env>`** — the reader table via `mdb_reader_list` (this process
takes no slot): `reader pid=<pid> tid=0x<hex> txnid=<n|->` per used slot,
then `readers=<n> numreaders=<slots ever used> maxreaders=<n>`.

**`reader-check <env>`** — `mdb_reader_check`: clear the reader slots of
dead processes. `reader-check cleared=<n>`.

**`hold-reader <env> <seconds> [--until-stdin] [<db|-> <count> <seed> [data options]]`**
— begin a read txn, print `READY txnid=<snapshot> pid=<pid>`, wait
`<seconds>` (with `--until-stdin`: until a line or EOF on stdin, at most
`<seconds>`), then, if a data set is given, verify it *in the held
snapshot* (prints the `verified` line), then
`released txnid=<n> held=<seconds>`.

**`hold-env <env> <seconds> [--until-stdin]`** — open the environment
read-only and wait like `hold-reader`, but without a read transaction:
`READY pid=<pid>`, then `released held=<seconds>`. While it runs, other
openers are not exclusive, so they do not reset the reader table (which
would wipe the slots of dead processes before a reader check sees them).

**`hold-writer <env> <seconds> [--until-stdin]`** — begin a write
transaction (taking the writer lock), put `#held` = `<pid>` into the main
DB without committing, print `READY txnid=<txnid> pid=<pid>`, wait like
`hold-reader`, then abort: `released txnid=<n> held=<seconds>`. Tests kill
it with SIGKILL to leave the writer lock held by a dead process.

**`writer-loop <env> <db|-> <seconds> <seed> [--keys N] [--max-commits N]`**
— commit small random write transactions until `<seconds>` pass (at least
one; `0` just initialises the DBs) or `N` commits; prints `READY` after the
first commit, then
`commits=<n> aborts=<n> nested_commits=<n> nested_aborts=<n> last_commit=<#commits>`.
Each transaction: read the counters, 1–8 random operations, write the
counters; one in 6 adds a nested transaction doing the same (committed or
aborted at random); one in 10 (after the first) aborts the whole
transaction; otherwise set `#commits` and commit. Data, in DB `<db>` and
DUPSORT DB `<db>.dups` (`dups` for the main DB):

| key | value |
|---|---|
| `w%06u` (r < `--keys`, default 500) | `"<r>:<v>:<len>:"` + filler, `len` bytes in total: 40–139, or 20000 one time in 40 (overflow). `v` = the transaction's `#commits` value; filler = SplitMix64 seeded with `r << 32 ^ v`, little-endian words |
| `#count` | decimal number of `w` keys |
| `#dups` | decimal number of values in the dups DB |
| `#commits` | decimal number of committed loop transactions |
| `d%04u` (r < 64, dups DB) | 12 bytes: random `t` (u64 LE), `fmix32(lo32(t) ^ hi32(t) ^ r)` (u32 LE) |

Operations: 50% insert or overwrite `w<r>` (`MDB_NOOVERWRITE` first, to
count), 20% delete `w<r>`, 20% add a value to `d<r>` (`MDB_NODUPDATA`), 10%
delete one value of `d<r>` or (one time in 4) all of them via a cursor.
Writers of both peers may run at once; the writer lock serialises them,
and `#commits` ends at exactly 1 + the sum of their commits (no lost update).

**`reader-loop <env> <db|-> <seconds>`** — wait for writer-loop's DBs, then
repeatedly take a snapshot (alternating `mdb_txn_reset`/`renew` and
`abort`/`begin`) and check: every `w` value is well formed for its key; the
number of `w` keys equals `#count`; every dups value has a valid check
word; per key `mdb_cursor_count` equals the values walked; their total
equals `#dups` and the dups DB's `mdb_stat` entries; `#commits` never goes
down. Every third snapshot walks the main DB backwards. Prints `READY` after
the first snapshot, then `reads=<n> first_commit=<#commits> last_commit=<#commits>`.

**`check-freelist <env>`** — audit the page usage of a snapshot, like
`mdb_audit()` (mdb.c) but stricter. Pins a snapshot with a read txn, picks
the meta page with that txnid, then reads the data file with `pread` and
decodes pages *independently of LMDB* at the offsets of `DESIGN.md`. Every
page `0..=last_pgno` must be used exactly once: the 2 meta pages, a page of
a tree (free DB, main DB, named DBs, DUPSORT sub-DBs; branch, leaf and
overflow pages), or listed in a free DB record. It also checks each
`MDB_db` against its tree (depth = leaf level, page counts, entries; a
DUPSORT DB's branch/leaf counts include its sub-DBs' pages), page numbers in
headers, free lists sorted descending, sub-page headers. Output:

```
psize=16384
txnid=12
last_pgno=940
db=@free flags=0x8 depth=1 branch_pages=0 leaf_pages=1 overflow_pages=0 entries=2 subdbs=0 subpages=0
db=- flags=0x0 depth=1 ... subdbs=0 subpages=0
db=<name> ...                     (subdbs/subpages: DUPSORT keys with values in a sub-DB / sub-page)
pages_meta=2
pages_freedb=1                    (pages of the free DB's tree)
pages_free=4                      (pages listed as free)
pages_tree=934                    (pages of all other trees)
pages_leaked=0
pages_total=941
ok                                (or errors=<n>, exit 1, details on stderr)
```

**`nested-test <env> [--check]`** — nested write transactions: committed
child visible to parent and persisted; aborted child (with overwrites)
leaves no trace; overflow values and a committed grandchild inside an
aborted child; a DB created in a committed child of an aborted parent does
not exist; page splits in an aborted child; a committed grandchild deleting
half of its committed parent's 2000 keys; `mdb_drop` (empty) in an aborted
child, `mdb_drop` (delete) in a committed one. Each step checks its
intermediate states. The final state, which `--check` verifies read-only
(and the write mode reaches again on reruns):
DB `nested` = `k1=v1 k2=v2 k3=v3 k5=<40000 bytes>` and `n%05u = val%05u`
for odd `u < 2000`, in that cursor order; DBs `nested2` and `nested3` do not
exist. The 40000 bytes are SplitMix64 words (little-endian) from state 5.
`nested ok mode=write|check`

**`copy <env> <dest> [--compact]`** — `mdb_env_copy2` (with
`MDB_CP_COMPACT`). `<dest>` is a directory, created if missing (with
`--nosubdir` the new data file); the data file must not exist.
`copied compact=0|1`

**`gen <seed> <i> [data options]`** — print the generated item:
`key=<hex>`, `ndups=<n>`, `value<d>=<hex>` per value, in generation order.

**`commands`** — `command=<name> implemented=<0|1>` for every command of
the peer (C: all 1).

## Deterministic data set

All arithmetic is on `uint32_t`/`uint64_t` with wrap-around; "native" means
host byte order (little-endian on every supported target).

```
fmix32(h):  h ^= h >> 16; h *= 0x85ebca6b; h ^= h >> 13; h *= 0xc2b2ae35; h ^= h >> 16
splitmix64(state):  state += 0x9e3779b97f4a7c15; z = state;
                    z = (z ^ z >> 30) * 0xbf58476d1ce4e5b9; z = (z ^ z >> 27) * 0x94d049bb133111eb
                    return z ^ z >> 31

key_hash(i) = fmix32(i + seed * 0x9e3779b9)                 (a bijection: keys are unique)
key(i)      = "%08x.%u" % (key_hash(i), i)                     10..19 bytes
            | --intkey: u64 native (key_hash(i) << 32 | i)     8 bytes
ndups(i)    = 1 if not DUPSORT or dups <= 1
            = 1 + fmix32(i ^ (seed * 0x85ebca6b)) % dups
tag(i, d)   = fmix32(i * 0x9e3779b9 + seed) ^ d               (distinct for distinct d)
vlen(i, d)  = valsize                     for DUPFIXED/INTEGERDUP
            = 4 + fmix32(tag(i, d)) % (valsize - 3)   for other DUPSORT (4..valsize)
            = big if big and i % 3 == 0, else valsize   for plain DBs
value(i, d) = --intdup, 8 bytes: u64 native (tag << 32 | d); 4 bytes: u32 native tag
            = otherwise: tag as 4 bytes big-endian (truncated if vlen < 4), then
              splitmix64 words, each written little-endian, truncated at vlen,
              from state = seed * 0x9e3779b97f4a7c15 ^ (i << 16) ^ d
```

Item `i` (0 ≤ i < count) is key `key(i)` with values `value(i, d)` for
`d < ndups(i)` (one value, `d = 0`, unless DUPSORT). After
`delete --every N`, key `i` is gone if `i % N == 0`; for DUPSORT and
`N > 1`, value `d` of key `i` is gone if `i % N == 1 && d % 2 == 0`; a key
whose values are all gone is gone. The expected DB is this set sorted by the
DB's comparators: keys by `memcmp`-then-length, reversed bytes for
`--revkey`, native unsigned integers for `--intkey`; values likewise
(`--revdup`, `--intdup`).

### Test vectors (`gen`, pinned in `scenarios::GEN_VECTORS`)

```
gen 1 0                     key=39326361326630652e30  ("92ca2f0e.0")
                            value0=514e28b7f465b9a16a9e786e4f450980185dc406ec814c72a8b88bf89b74a851
gen 1 3 --intkey            key=03000000d3de81ed
gen 9 7 --dupfixed --dups 3 --valsize 12
                            key=34633335623464652e37 ndups=3
                            value0=533a22b44701441bf41bcea5 value1=533a22b5970b438dd36bef5d
                            value2=533a22b65d1ee65a0c759f65
gen 3 4 --intdup --dups 3   value0=000000005e89d359 value1=010000005f89d359 value2=020000005c89d359
```

## The scenarios

| scenario | what it shows |
|---|---|
| `create_stat` | new env: page size, data file = 2 meta pages, lock file = header + 126 × 64-byte slots, empty stats, `mdb_stat -e` |
| `fill_verify_*` | plain/main DB, named DBs, `INTEGERKEY`, `REVERSEKEY`, `DUPSORT` (sub-pages, sub-DBs, `REVERSEDUP`), `DUPFIXED` (LEAF2), `INTEGERDUP` (4/8 bytes), overflow values (1 and many pages, `MDB_RESERVE`), `MDB_APPEND`/`APPENDDUP`; each verified by the writer, the reader and C, and audited |
| `delete_verify` | deleting keys, single dups, overflow values, sub-DB values, then everything |
| `copy_dump_equal` | plain and compacting copies by both peers and `mdb_copy [-c]`: same dumps, valid page usage, compacted copy has no free pages and txnid 1 |
| `mvcc_hold_reader` | a reader pins a snapshot, the writer deletes and rewrites everything and commits 2000 more txns; the snapshot still verifies the old data; reader table shows the holder |
| `concurrent_writers_readers` | two writer loops (one per peer of the pair, synced and `--nosync`) and reader loops of all three peers at once; no lost updates, consistent snapshots |
| `freelist_after_writer_loop` | many commits with nested txns and aborts; audit; `mdb_stat -ff` and `stat` agree with the walk |
| `readers_list` | slots visible across processes both ways, also when the reader peer opened the lock file first; freed on exit |
| `nested_txns` | `nested-test` twice, checked by both peers |
| `nosubdir` | `MDB_NOSUBDIR` env and copy |
| `dump_load_roundtrip` | `mdb_dump -a` + `mdb_load` of the writer's data, verified by the reader |
| `mtest_databases` | databases written by upstream `mtest`…`mtest5` (incl. `MDB_FIXEDMAP`) audited and dumped |
| `gen_vectors` | the generator agrees between peers and with the pinned vectors |
| `stale_reader_cleared_by_reader` | the writer's `hold-reader` is SIGKILLed; its slot stays and pins the snapshot (a rewrite grows the file); a check while it was alive cleared nothing; the reader peer's `reader-check` clears exactly it; the next rewrite reuses the free pages (grows ~0); audit |
| `stale_reader_cleared_by_mdb_stat` | the same with the reader peer's `hold-reader` killed and `mdb_stat -rr` (C) clearing it |
| `reader_creates_lock_file` | the lock file is deleted; the reader peer opens first and creates it (reader table, semaphores/mutexes; size as C makes it); the writer then does all the writing (writer loop, delete) next to reader loops of both peers and the creator's held snapshot |
| `reader_loop_during_nested` | reader loops of both peers while the writer runs `nested-test` twice and a writer loop |
| `notls_readers` | `--notls` holder, reader loops and verify of both peers while the writer loops |
| `alternating_writers` | both peers write the same DBs in turns (fill, delete a third, refill, delete half, delete the rest, refill with `MDB_APPEND`; plain, DUPSORT, overflow, DUPFIXED), verified by all after every turn; then writer loops of both in turns: `#commits` continues exactly |
| `delete_heavy` | the reader deletes a third of six large DBs of every kind, the writer the rest, the reader refills and the writer deletes half; verified by the other peer and C, audited after every round; the refill reuses the freed pages |
| `drop_clear_dbs` | the writer `mdb_drop`s two DBs empty and deletes two (one with sub-DBs); the others see empty DBs with their flags and no deleted ones, every page of the dropped trees is free; the reader refills them, reusing the pages |
| `copy_cross` | plain and compacting copies of one snapshot by the writer, the reader and C, each verified, dumped and audited by the other implementations; plain copies byte-identical up to the last page, compacted copies too |
| `map_full` | a 4 MiB map: a fill too big fails with `MDB_MAP_FULL` in both peers and leaves nothing behind; committed batches of a `--per-txn` fill stay; a writer loop on the full map fails cleanly or not at all; the reader grows the map with `--mapsize`, the writer then opens with the size recorded in the meta page |
| `killed_writer` | `hold-writer` and writer loops of the writer SIGKILLed inside write transactions; the reader must get the writer lock and write (Linux: robust mutex, `EOWNERDEAD` recovery while another process keeps the env open; macOS: the next writer provably waits on the non-robust semaphore, then an exclusive re-open re-creates it); nothing of the killed transactions is visible; data, counters and pages consistent |
| `same_writes_same_pages` | writer and reader run the same script of writes (every fill variant, deletes, drops, a commit-bounded writer loop with nested transactions, `nested-test`) on environments of their own; after every step both files hold the same pages (`pagecmp`) |
| `three_writers` (`tests/three_peers.rs`) | writer loops of C, Rust and COnRust at once, reader loops of all three; `#commits` = 1 + the sum of their commits; audited by all three |

## C behaviour the Rust implementation must match (found by this harness)

* **Page size** is the OS page size: 16384 on Apple Silicon. A new data file
  is exactly `2 * psize` bytes (both meta pages); the lock file is
  `offsetof(mti_readers) + maxreaders * 64` bytes (128 + 126 × 64 = 8192 on
  macOS) and stays on disk after close.
* **DUPSORT page counts**: since ITS#9388 a DUPSORT DB's `md_branch_pages`
  and `md_leaf_pages` *include* the pages of its sub-DBs (sub-DBs never have
  overflow pages); `mdb_stat` reports these totals. `mdb_audit()` in mdb.c
  predates this and would double count. ITS#10643 (the two latest commits)
  fixes the counts when deleting DUPSORT records.
* **Compacting copy** (`mdb_env_copyfd1`): meta page 0 is a fresh meta with
  txnid 0, meta page 1 holds the data with txnid 1; the free DB is empty;
  pages are renumbered in tree-walk order and the main DB root is the last
  page. **Plain copy** (`mdb_env_copyfd0`) takes the *writer* lock while
  copying the two meta pages (so the snapshot is the newest), then copies
  pages `2..next_pgno` as they are (capped at the file size). Both open the
  target with `O_EXCL`: the file must not exist.
* **Exclusive re-open resets the reader table**: when the last process
  using an environment exits, the next opener gets the exclusive lock and
  reinitialises the table, so a dead reader's slot vanishes without a
  reader check. Scenarios about stale slots keep a `hold-env` process
  around.
* **A pinned snapshot stops all page reuse**: with a reader held, every
  commit of `writer-loop` takes new pages (about 9 per commit with the
  default data); an unbounded `--nosync` loop fills a 512 MiB map of 4 KiB
  pages in under a second on Linux. Scenarios that hold a reader during a
  writer loop pass `--max-commits`.
* **Reader table**: `mdb_reader_list` prints `"%10d %zx %zu"` (pid, thread
  as hex, txnid) or `"%10d %zx -"` for a slot that is not in a txn, after
  the header `"    pid     thread     txnid"`; `"(no active readers)"` if
  none. A process's slots are cleared (`pid = 0`) by `mdb_env_close`.
  `numreaders` only grows. A reader that dies without closing leaves its
  slot: writers do not clear stale slots by themselves, so the free list
  stops being reused and the map fills (`MDB_MAP_FULL`) until someone runs
  `mdb_reader_check` or an exclusive open resets the table.
* **`mdb_stat -r`** alone exits with status 1 on success (it returns the
  last `fputs` result of `mdb_reader_list` as its error code).
* **`mdb_load`** loses the DB flags (`dupsort=1`, …) of the *first* database
  in its input: `readhdr()` parses them before the load loop, whose first
  statement is `flags = 0;` (since ITS#8662, 2017). The Rust `mdb_load`
  should not copy this bug; the scenario avoids it by sorting a flag-less DB
  first.
* **`mdb_dump -a`** dumps only named DBs (not the main DB's records) and
  fails with "does not contain multiple databases" if there are none;
  `mdb_dump` and `mdb_stat -a` open with `maxdbs` 2 and 4 and close each DB
  after use. `mapaddr=` is printed only for `MDB_FIXEDMAP` envs.
* **`mdb_dbi_flags`** rejects DBI 0 (the free DB) with `EINVAL`, but
  cursors and `mdb_stat` on DBI 0 work. The free DB's `md_flags` is
  `MDB_INTEGERKEY` (0x8).
* `mtest6.c` calls `mdb_dkey()`, which exists only with `MDB_DEBUG`.
* **Uninitialised bytes**: C writes pages from reused page buffers without
  clearing them, so node padding bytes, the gap between `mp_lower` and
  `mp_upper` and the free space of sub-pages hold stale bytes; Rust writes
  zeros there. Files written by the same operations therefore differ in
  those bytes only (`same_writes_same_pages` compares everything else and
  finds no difference between C and Rust).
* **A killed writer** (SIGKILL inside a write transaction) leaves the
  writer lock held. On Linux the lock is a robust mutex: the next writer
  (C or Rust) gets `EOWNERDEAD`, resets `mti_txnid` from the newest meta
  page, runs a reader check, marks the mutex consistent and continues
  (`mdb_mutex_failed`). On macOS the lock is a POSIX semaphore, which is
  not robust: as long as any process keeps the environment open, every
  later writer waits forever (in C and in Rust alike); once all processes
  have closed it, the next opener is exclusive and re-creates the
  semaphores (`sem_unlink` + `O_CREAT|O_EXCL`). A killed transaction leaves
  nothing visible.
* **`MDB_MAP_FULL`** fails the operation and the transaction; the
  environment stays intact. A later opener with a larger `--mapsize` can
  write, and its commit records the new size in the meta page, which later
  openers without a map size use (`mdb_env_open`: no configured size means
  the meta page's `mm_mapsize`).
* **`mdb_drop`** puts every page of the dropped tree on the free list in the
  same transaction; with large trees the free list itself grows overflow
  pages, so the file can grow by a few pages even though the drop frees
  hundreds.
* Keys and values point into the map and are not NUL-terminated; `sscanf`
  on them can read past the end of the mapped file (`SIGBUS` on macOS).
  `cpeer` copies before parsing.
