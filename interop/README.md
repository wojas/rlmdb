# interop: C ⇄ Rust LMDB compatibility test bed

The Rust rewrite must share environments with C LMDB *concurrently*: same
data file, same lock file, same reader table and writer lock. This crate
proves it by running **peer programs** as separate processes against one
environment and checking what each sees.

* `cpeer` (C, `c/cpeer*.c`) speaks the protocol below, linked against
  `liblmdb` built from `../../libraries/liblmdb` (LMDB 0.9.36+) by `build.rs`
  with the upstream Makefile's flags and the platform's default lock mode.
* `rpeer` (Rust, on the `lmdb` crate) speaks the same protocol. It arrives in
  stage 2.
* `layout_dump` (`c/layout_dump.c`) `#include`s `mdb.c` and prints
  `sizeof`/`offsetof`/`alignof` of every on-disk and lock-file struct.
* The upstream tools `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy` and
  `mtest`…`mtest5` are built too. (`mdb_drop` does not exist in 0.9, and
  `mtest6` needs an `MDB_DEBUG` build, as upstream.)

Everything is compiled into `$OUT_DIR/bin`; `interop::bins` has the paths.

## Running

```sh
cargo test -p interop                       # all scenarios C-vs-C, layout, unit tests
cargo test -p interop --test c_vs_c -- --nocapture mvcc   # one scenario, with its log
```

| variable | meaning |
|---|---|
| `INTEROP_LOOP_SECS` | duration of `writer-loop`/`reader-loop` runs in the scenarios (default 1.5) |
| `INTEROP_TIMEOUT_SECS` | a peer running longer is killed and the test fails (default 120) |
| `INTEROP_RPEER` | path of the Rust peer (else found via `CARGO_BIN_EXE_rpeer` or next to the test binary) |

The tests print the page size of each environment they create (`--nocapture`):
16384 on Apple Silicon macOS, 4096 on Linux x86-64.

## Rust harness

* `Peer::{C, Rust}`: `peer.on("fill", &env).args([...]).run()` runs a
  command and requires exit 0; `.output()` returns any status; `.spawn()`
  returns a `PeerProcess` for long-running commands (`wait_ready()`,
  `release()`, `finish()`; dropping it kills the process).
* `TestEnv`: an environment in a temp dir (`new()`, `new_nosubdir()`,
  `with_name()`, `derived(name)` for copy targets).
* `report`: parsers for every command's output. `tools`: runs and parses
  the C tools. `tools::c_layout()`: the parsed `layout_dump`.
* `scenarios`: every scenario as `fn(Pair) -> Result<()>` with
  `Pair { writer, reader }`; `scenario_tests!(writer, reader)` turns them
  all into `#[test]`s. `tests/c_vs_c.rs` is just
  `interop::scenario_tests!(Peer::C, Peer::C)` plus self-tests showing the
  checkers reject bad data and broken page accounting.

### Adding the Rust peer (stage 2)

1. Add `src/bin/rpeer.rs` and a `[[bin]] name = "rpeer"` (with
   `required-features = ["with-lmdb"]`, or make `lmdb` a normal dependency).
   It implements this protocol on `lmdb::core`; the C files are the
   reference (`dataset.c` for the generator).
2. Add test files with one line each, e.g. `tests/c_vs_rust.rs`:
   `interop::scenario_tests!(interop::Peer::C, interop::Peer::Rust);`, and the
   same for `(Rust, C)` and `(Rust, Rust)`.
3. Enable `--features with-lmdb` for `tests/layout.rs::rust_layout_matches_c`
   (fix up the constant names there to match `lmdb::format::layout`).

In a pair the **writer** creates environments and writes; the **reader**
verifies, holds snapshots and runs reader loops; page audits always run on
the reader *and* on C (the reference). Some scenarios also run commands on
both peers (two concurrent writer loops, one per peer; reader loops on
both; `readers` both ways).

## The peer command protocol

```
<peer> <command> <positional args...> [options...]
```

Options start with `--` and may appear anywhere after the command. Exit
status: `0` success, `1` a check failed (data or invariant mismatch, message
on stderr), `2` usage error, `3` an LMDB call failed unexpectedly (message
names the call and the error). Output goes to stdout as lines of
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
`dump`, `stat`, `readers`, `hold-reader`, `reader-loop`, `check-freelist`,
`copy`, `nested-test --check`.

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

**`hold-reader <env> <seconds> [--until-stdin] [<db|-> <count> <seed> [data options]]`**
— begin a read txn, print `READY txnid=<snapshot> pid=<pid>`, wait
`<seconds>` (with `--until-stdin`: until a line or EOF on stdin, at most
`<seconds>`), then, if a data set is given, verify it *in the held
snapshot* (prints the `verified` line), then
`released txnid=<n> held=<seconds>`.

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
| `fill_verify_*` | plain/main DB, named DBs, `INTEGERKEY`, `REVERSEKEY`, `DUPSORT` (sub-pages, sub-DBs, `REVERSEDUP`), `DUPFIXED` (LEAF2), `INTEGERDUP` (4/8 bytes), overflow values (1 and many pages, `MDB_RESERVE`), `MDB_APPEND`/`APPENDDUP`; each verified by both peers and audited |
| `delete_verify` | deleting keys, single dups, overflow values, sub-DB values, then everything |
| `copy_dump_equal` | plain and compacting copies by both peers and `mdb_copy [-c]`: same dumps, valid page usage, compacted copy has no free pages and txnid 1 |
| `mvcc_hold_reader` | a reader pins a snapshot, the writer deletes and rewrites everything and commits 2000 more txns; the snapshot still verifies the old data; reader table shows the holder |
| `concurrent_writers_readers` | two writer loops (one per peer, synced and `--nosync`) and two reader loops at once; no lost updates, consistent snapshots |
| `freelist_after_writer_loop` | many commits with nested txns and aborts; audit; `mdb_stat -ff` and `stat` agree with the walk |
| `readers_list` | slots visible across processes both ways, also when the reader peer opened the lock file first; freed on exit |
| `nested_txns` | `nested-test` twice, checked by both peers |
| `nosubdir` | `MDB_NOSUBDIR` env and copy |
| `dump_load_roundtrip` | `mdb_dump -a` + `mdb_load` of the writer's data, verified by the reader |
| `mtest_databases` | databases written by upstream `mtest`…`mtest5` (incl. `MDB_FIXEDMAP`) audited and dumped |
| `gen_vectors` | the generator agrees between peers and with the pinned vectors |

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
* Keys and values point into the map and are not NUL-terminated; `sscanf`
  on them can read past the end of the mapped file (`SIGBUS` on macOS).
  `cpeer` copies before parsing.
