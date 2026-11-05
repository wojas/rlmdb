# rlmdb-tools

Rust versions of the LMDB 0.9 command line tools, built on `rlmdb`:
`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`, plus `mdb_drop` (which
0.9.37 does not ship; see below).

They are drop-in replacements for the C tools of LMDB 0.9.37 (whose
sources the tests build from `../tests/lmdb-c/0.9`): same
options, same output byte for byte, same messages on standard error, same
exit codes, on the same platform. They work on environments that C
processes use at the same time, since `rlmdb` shares the C data and
lock file formats. The tests compare every tool with its C counterpart on
macOS and Linux.

They also work on LMDB 1.0 environments (`../DUAL_FORMAT.md`): an existing
environment is opened in its format, and for a 1.0 environment the tools
print what C 1.0's tools print (see "LMDB 1.0 environments" below).

```sh
cargo build --release -p rlmdb-tools     # target/release/mdb_{stat,dump,load,copy,drop}
```

## Usage

The documentation of the C tools applies: upstream LMDB's man pages
(`mdb_stat.1`, `mdb_dump.1`, `mdb_load.1`, `mdb_copy.1` in
`libraries/liblmdb` of <https://github.com/LMDB/lmdb>; not copied into this
repository). In short:

```text
mdb_stat [-V] [-n] [-e] [-r[r]] [-f[f[f]]] [-v] [-a|-s subdb] dbpath
mdb_dump [-V] [-f output] [-i txnid] [-l] [-n] [-p] [-v] [-a|-s subdb] dbpath
mdb_load [-V] [-a] [-f input] [-F 0.9|1.0] [-i] [-n] [-s name] [-N] [-T] dbpath
mdb_copy [-V] [-c] [-n] [-v] srcpath [dstpath]
mdb_drop [-V] [-n] [-d] [-s subdb] dbpath
```

* `mdb_stat`: `-e` environment info, `-f`/`-ff`/`-fff` free list (summary,
  per-record lines, page numbers), `-r` reader table (`-rr` also clears
  stale readers), `-a` all databases, `-s` one named database, `-n` the
  environment is a file (`MDB_NOSUBDIR`).
* `-v` (`mdb_stat`, `mdb_dump`, `mdb_copy`; C 1.0's option, here for
  environments of both formats): use the previous snapshot, the one
  before the latest commit (`MDB_PREVSNAPSHOT`).
* `mdb_dump`: writes the main DB (or `-s subdb`, or with `-a` every named
  database) in the BDB-compatible dump format; `-p` prints printable bytes
  as text, `-l` lists the named databases, `-f` writes to a file. `-i
  txnid` (LMDB 1.0 environments) writes an incremental dump instead: the
  pages written since transaction `txnid` (see "LMDB 1.0 environments").
* `mdb_load`: reads that format (or with `-T` plain pairs of text lines)
  into an environment, creating it if needed; `-s` names the target
  database (a `database=` header line overrides it), `-N` keeps existing
  records, `-a` appends records in input order without sorting (for dumps
  of databases with custom comparators), `-f` reads a file. `-F 0.9` or
  `-F 1.0` is the format of a new environment (rlmdb's own option; see
  below). `-i` loads an incremental dump of `mdb_dump -i` instead.
* `mdb_copy`: copies an environment to an empty directory, or to standard
  output without `dstpath`; `-c` compacts (only pages in use, renumbered).
* `mdb_drop`: empties a database (the main DB, or `-s subdb`); `-d`
  deletes a named database from the environment.

Arguments are taken as bytes, so database names and paths need not be
UTF-8 (as in C). Options are parsed like the platform's `getopt(3)`: on
Linux (glibc) options may follow the path (`mdb_stat db -e`) unless
`POSIXLY_CORRECT` is set; on macOS parsing stops at the first non-option,
and the error messages differ accordingly (`invalid option -- 'x'` vs.
`illegal option -- x`).

## LMDB 1.0 environments

Each C release's tools are the oracle for their own format: for an LMDB
1.0 environment the tools print and decide what C 1.0's do.

* `mdb_load -F 1.0` creates a 1.0 environment; without `-F` a new
  environment gets rlmdb's default format: LMDB 0.9, unless the
  **experimental** variable `RLMDB_DEFAULT_FORMAT=1.0` is set (it may be
  removed or restricted later; prefer `-F`). An existing environment keeps
  its format; `-F` naming the other one fails with
  `mdb_env_open failed, error -30794 MDB_VERSION_MISMATCH: …`. For a 1.0
  environment, `db_pagesize=` in the dump header sets the page size of a
  new environment (an invalid size is ignored, as C 1.0 ignores the result
  of `mdb_env_set_pagesize`) and a record with an empty key is skipped
  with `zero-length key(ignored)`; for a 0.9 environment `db_pagesize=` is
  an unrecognized keyword and an empty key fails, as in C 0.9. The usage
  line names `-F` (and `-i`, below), where it differs from C 0.9's.
* `mdb_stat` of a 1.0 environment prints the page size in every
  database's block (`  Page size: 4096`, as 1.0's `prstat` does), and
  `mdb_stat`/`mdb_dump` say `mdb_dbi_open failed` where 0.9's say
  `mdb_open failed`. `-a` (and `mdb_dump -l`) list the named databases
  by their records (`mdb_cursor_is_db`): 1.0 stores a name with its NUL,
  which the 0.9 rule (skip keys containing a NUL) would hide.
* `mdb_copy` copies pages, so a copy has the source's format (also with
  `-c`); there is no converting copy yet (`DUAL_FORMAT.md` §4.3).
* `mdb_drop` without `-s` on a 1.0 environment deletes the named databases
  first, as C 1.0 does, so their pages are freed (0.9 leaks them,
  `../KNOWN_C_BUGS.md` row 21).
* The options are 0.9's, 1.0's `-v` (see above), which the tools take
  for 0.9 environments too, and 1.0's `-i` (below); 1.0's `-L`, `-m`/`-w`
  are not there. The usage lines name `-v` and `-i`, as C 1.0's do.
* `-v` next to other processes: C 1.0's tools publish the previous
  snapshot in the lock file that the other processes use, so the next
  commit of one of them overwrites the latest one; the Rust tools leave
  the lock file alone (`../KNOWN_C_BUGS.md` row 68). Alone, a tool with
  `-v` keeps other processes from opening the environment until it exits,
  as C 1.0's does; they then see the latest snapshot, where after C 1.0's
  they see the previous one and their next commit overwrites the latest
  (row 70). And `mdb_copy -v`
  without `-c` is a copy of the previous snapshot, where C 1.0's keeps the
  latest commit's meta page and lacks pages it names (row 69).
* Incremental backups, 1.0's `mdb_dump -i txnid` and `mdb_load -i`, as
  C 1.0's tools do them: the same dump on standard output
  or with `-f` in a new file (which must not exist); the same messages
  and exit codes, among them `<prog>: <arg>: invalid txnid: <strerror>`
  for a txnid that is 0 or not a number (it is read as `sscanf("%zu")`
  reads it: `5x` is 5, `-1` the largest); the other options are ignored
  but `-n` (and `-f`, `-F` of `mdb_load`). A chain is a copy
  (`mdb_copy`), then dumps since its txnid and since each dump's (`mdb_stat
  -e` shows the last txnid), loaded in order into a copy of the copy. C
  1.0's dump reads the headers of free pages, and after a large value is
  deleted and its pages partly reused it dumps the old value's bytes as
  pages, which no load restores correctly, or skips live pages
  (`../KNOWN_C_BUGS.md` row 72); rlmdb's dump holds only pages whose
  header names their own position (so it differs from C's there, and C
  1.0's `mdb_load -i` restores the source from it), and its load refuses
  C's stale pages where their number is impossible. With `-v` the dump is
  one of the previous snapshot (C 1.0's holds the latest commit's meta
  page and pages). Not as in C:
  `mdb_load -i` reports a dump cut short (`mdb_env_incr_load failed,
  error 5 Input/output error`; C reports success, or reads for ever, row
  66; on Windows C 1.0's fails at the end of a pipe with "error -1", having
  loaded the dump, where rlmdb's reads a pipe to its end) and fails with
  `EBUSY` while a reader of another process pins a snapshot (row 65).
  `-i` always means 1.0: `mdb_load -i` creates a new environment in
  format 1.0 whatever the experimental variable says (C 1.0's creates 1.0
  files), and `-F 0.9` with `-i` is a usage error. Like
  C's it opens the environment with `MDB_NOSYNC`, but its load writes
  each dump's meta pages after the data pages, syncing before and after
  them (C writes them first, unsynced; row 65): an interrupted load
  leaves the base's meta pages, and the base snapshot whole if the dump
  spans at most two commits (a third may reuse pages the base tree
  names). A dump cut at a record boundary looks whole in C's format. So
  load onto a copy you can throw away, or keep the base until the load
  has succeeded; loading the same dump again completes an interrupted
  one. On an LMDB 0.9 environment both fail with `ENOTSUP` (0.9 pages
  record no txnid; C 1.0's tools do not open 0.9 files).
* Not reproduced from C 1.0: its `mdb_env_info` reports the process's map
  address when the file records none, so `mdb_stat10 -e` prints it and
  every `mdb_dump10` output has a `mapaddr=` line (which makes `mdb_load`
  of it use `MDB_FIXEDMAP`). The Rust tools print the recorded address (0:
  `Map address: 0x0`, no `mapaddr=`) as 0.9 does.

## Differences from the C tools

C 0.9.37 bugs that are not reproduced (see `../KNOWN_C_BUGS.md`):

* **`mdb_load` keeps the flags of the first database** (row 1). C 0.9.37
  ignores the header flags (`dupsort=1`, …) of the first database in its
  input (`flags = 0;` from ITS#8662), so e.g. a dumped `DUP_SORT` main DB
  is reloaded without duplicates.
* **`mdb_stat -r` exits with 0** (row 2). With `-r`/`-rr` but none of `-a`,
  `-s`, `-f`, C exits with 1 after printing the reader table (it returns
  the value of the last `fputs`).

Further `mdb_load` input handling bugs that are fixed (only malformed or
unusual input is affected; `mdb_dump` output loads identically):

* `format=` applies to the database whose header contains it. C keeps
  `print` mode for all later databases once one header selected it.
* A header line without `=` is an error ("unexpected format"). C searches
  its whole 4 KiB line buffer for `=`, so leftovers of an earlier, longer
  line can turn this into "unrecognized keyword ignored" (with an extra
  empty line) and the load continues.
* Lines of any length are read whole, and NUL bytes in a line are data. C
  reads header lines in 4 KiB pieces and mixes up lines that contain a NUL
  byte (it continues the line in the buffer with the next input line).
* `maxreaders=`, `mapsize=` and `db_pagesize=` take only the plain
  decimal `mdb_dump` writes (0 leaves the environment's default, as in
  C). C reads them with `sscanf`, which also takes a sign, blanks and
  trailing junk, and uses the result unchecked: a `maxreaders=-1` line
  makes C create an environment whose lock file is 256 GiB, a
  `mapsize=-1` line one whose recorded map size nothing opens again
  (row 53). Here they are "invalid maxreaders -1", the message C gives a
  value `sscanf` cannot read at all. So is a `maxreaders=` above
  1,048,576, which the library refuses (row 60).
* A flag line (`dupsort=`, `integerkey=`, …) takes only the `1` that
  `mdb_dump` writes for a set flag. C sets the flag for any value, so
  `dupsort=0` in a hand-edited dump turns duplicates on, silently and for
  good (row 55). Here it is "invalid dupsort 0".
* After loading the main DB with `-a`, the main DB's default comparator is
  restored. C leaves its "always greater" append comparator on the main
  DB, so a named database that follows in the same input is not found in
  the main DB (C then adds a second record under that name or crashes).

Other differences:

* **`-V`** prints `LMDB 0.9.85: (rlmdb 0.0.0)` (the rlmdb version in the
  parentheses) where C prints `LMDB 0.9.37: (Oct 21, 2026)`: the string
  `mdb_version()` of rlmdb's LMDB 0.9 stub returns (`librlmdb` returns
  rlmdb's own version line). Its patch level is above every C 0.9
  release so that rlmdb can be told apart (`../rlmdb-capi/DETAILS.md`,
  "`rlmdb.h`", "Version").
* **`mdb_drop`** is not part of LMDB 0.9.37. This one is LMDB's own
  `mdb_drop` as first added (2017, before the 1.0 options `-L` and the
  crypto options `-m`/`-w`): same options, messages and exit codes. The
  tests build that C version against the 0.9.37 library as the oracle.
* **Signals.** C `mdb_dump` catches `SIGINT`, `SIGTERM`, `SIGHUP` and
  `SIGPIPE`, finishes the current record and exits with "Interrupted
  system call"; C `mdb_copy` ignores them. The Rust tools handle a closed
  pipe the same way (`mdb_dump` exits with "Interrupted system call",
  `mdb_copy` reports the write error), but `SIGINT`, `SIGTERM` and `SIGHUP`
  terminate them at once: catching them would need `unsafe` code. The
  environment stays consistent either way (the tools are read-only or
  commit atomically). Windows has no `SIGPIPE`: there C `mdb_dump` writes
  on to a closed pipe and exits with 0, and so does the Rust one.
* **Write errors.** C ignores errors writing standard output (or the
  `-f` file of `mdb_dump`) and exits with 0. The Rust tools report them
  (`mdb_dump: <path>: No space left on device`, or
  `<tool>: write error: …` when it shows only at the final flush) and exit
  with 1. A closed pipe at the final flush is ignored, as in C.
* **Windows.** The tools write and read text as the C runtime's text mode
  does (`\r\n` line ends; `mdb_copy`'s copy on standard output is
  binary), print `%p` values as the C runtime of the toolchain does (16
  hex digits: upper case with MSVC, lower case with MinGW-w64), and parse
  options as MinGW-w64's `getopt` does (MSVC has none; the C tools built
  with it here get a shim that behaves the same,
  `tests/compat/msvc/unistd.h`).
* **The C runtime's numbers.** `mdb_load` reads `VERSION=` with `atoi`, as
  C does, and the C runtimes differ past the range of `int`: glibc, macOS
  and MinGW's msvcrt keep the low 32 bits (`VERSION=99999999999` is
  "unsupported VERSION 1215752191"), MSVC's UCRT saturates (2147483647).
  The Rust tools follow the runtime of the toolchain they are built with,
  as the C tools they are compared with do (`cfmt::atoi`). The other
  numbers of a dump header are read like `sscanf("%zu")`/`"%u"`/`"%p"`:
  the low bits of a value that fits in 64 bits, as every runtime does;
  past 64 bits `strtoull`'s saturation, which glibc and macOS have (not
  measured against the Windows runtimes).
* **`mdb_load` with `mapaddr=`** in the header opens the environment with
  `MDB_FIXEDMAP` as C does, but `rlmdb` ignores that flag (it never
  maps at a fixed address), so a new environment records address 0 and its
  dumps have no `mapaddr=` line (C records wherever its map landed).

## Tests

`cargo test -p rlmdb-tools` (and `scripts/linux-test.sh test -p
rlmdb-tools` for Linux) builds the C library and tools from
`../tests/lmdb-c/0.9` with the `cc` crate, plus `tests/c/mdb_drop.c` (the C
oracle for `mdb_drop`) and `tests/c/holder.c` (keeps a reader open). Test
environments come from `mtest`..`mtest5` and from crafted dump files
loaded with the C `mdb_load`: every database flag combination, overflow
values, named databases (including non-UTF-8 names and names with spaces),
keys and values with every byte value, empty databases, `DUP_SORT` with
sub-pages and sub-databases, a `MDB_NOSUBDIR` environment.

* `stat.rs`, `dump.rs`: every option combination on every environment,
  plus usage errors and `getopt` edge cases: identical stdout, stderr and
  exit code (`-f` output files too). Database names C refuses (empty:
  `MDB_BAD_VALSIZE`; over 511 bytes: `MDB_NOTFOUND` when opening,
  `MDB_BAD_VALSIZE` when `mdb_load` creates) give the same errors. The one
  exception, in every test that compares the tools: where the C tool
  prints its version line for `-V`, the Rust tool must print its own
  (`version_line` and `assert_runs_equal` in `tests/common/mod.rs`).
* `load.rs`: crafted dumps in both encodings, loads into existing
  environments, `-T`, `-N` (the records it skips do not count toward a
  batch), `-a` (sorted and unsorted input, several batches), malformed
  input (header errors, truncated lines, bad escapes, oversized keys, a
  full map): identical messages and exit codes,
  and identical environments afterwards as the C tools see them (dumps,
  `mdb_stat -efa`, free list). Round trips: Rust `mdb_dump` → C `mdb_load`
  → C `mdb_dump`, and C `mdb_dump` → Rust `mdb_load` → C `mdb_dump`, both
  equal to the source.
* `copy_drop.rs`: plain and compacting copies, to a directory and to
  standard output, byte-identical to the C copies and read back by the C
  tools like the source; `mdb_drop` (empty and delete, named and main
  DBs) leaves exactly the environment the C `mdb_drop` leaves.
* `readers.rs`: `mdb_stat -r`/`-rr` with a C reader, a Rust reader (in the
  test process) and the stale slot of a killed reader.
* `format.rs`: `mdb_load -F 0.9|1.0` and `RLMDB_DEFAULT_FORMAT` (set for
  the tool's process only) in every combination: the format of the new
  file, the same messages and environment as C 0.9's `mdb_load` for 0.9,
  the same records for 1.0; `-F` against an existing file of the other
  format, `db_pagesize=` (valid and invalid), empty keys, bad `-F` values;
  and that `RLMDB_DEFAULT_FORMAT=1.0` changes nothing for `mdb_stat`,
  `mdb_dump`, `mdb_copy` and `mdb_drop` on existing 0.9 files (C 0.9's
  output and files).

The comparison with C 1.0's tools needs the 1.0 oracle, which only
`../interop` builds: `interop/tests/tools10.rs` runs these tools (as
`rtools <tool>`, one executable built from this crate's library) next to
`mdb_load10`, `mdb_stat10`, `mdb_dump10`, `mdb_drop10` and `mdb_copy10`,
`-i` included (`interop/tests/incr10.rs` restores chains of incremental
dumps with either side's tools).
