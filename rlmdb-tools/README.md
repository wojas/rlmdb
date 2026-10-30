# rlmdb-tools

Rust versions of the LMDB 0.9 command line tools, built on `rlmdb`:
`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`, plus `mdb_drop` (which
0.9.36 does not ship; see below).

They are drop-in replacements for the C tools in `libraries/liblmdb`: same
options, same output byte for byte, same messages on standard error, same
exit codes, on the same platform. They work on environments that C
processes use at the same time, since `rlmdb` shares the C data and
lock file formats. The tests compare every tool with its C counterpart on
macOS and Linux.

```sh
cargo build --release -p rlmdb-tools     # target/release/mdb_{stat,dump,load,copy,drop}
```

## Usage

The C man pages (`libraries/liblmdb/*.1`) apply. In short:

```text
mdb_stat [-V] [-n] [-e] [-r[r]] [-f[f[f]]] [-a|-s subdb] dbpath
mdb_dump [-V] [-f output] [-l] [-n] [-p] [-a|-s subdb] dbpath
mdb_load [-V] [-a] [-f input] [-n] [-s name] [-N] [-T] dbpath
mdb_copy [-V] [-c] [-n] srcpath [dstpath]
mdb_drop [-V] [-n] [-d] [-s subdb] dbpath
```

* `mdb_stat`: `-e` environment info, `-f`/`-ff`/`-fff` free list (summary,
  per-record lines, page numbers), `-r` reader table (`-rr` also clears
  stale readers), `-a` all databases, `-s` one named database, `-n` the
  environment is a file (`MDB_NOSUBDIR`).
* `mdb_dump`: writes the main DB (or `-s subdb`, or with `-a` every named
  database) in the BDB-compatible dump format; `-p` prints printable bytes
  as text, `-l` lists the named databases, `-f` writes to a file.
* `mdb_load`: reads that format (or with `-T` plain pairs of text lines)
  into an environment, creating it if needed; `-s` names the target
  database (a `database=` header line overrides it), `-N` keeps existing
  records, `-a` appends records in input order without sorting (for dumps
  of databases with custom comparators), `-f` reads a file.
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

## Differences from the C tools

C 0.9.36 bugs that are not reproduced (see `../KNOWN_C_BUGS.md`):

* **`mdb_load` keeps the flags of the first database** (#1). C 0.9.36
  ignores the header flags (`dupsort=1`, …) of the first database in its
  input (`flags = 0;` from ITS#8662), so e.g. a dumped `DUP_SORT` main DB
  is reloaded without duplicates.
* **`mdb_stat -r` exits with 0** (#2). With `-r`/`-rr` but none of `-a`,
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
* After loading the main DB with `-a`, the main DB's default comparator is
  restored. C leaves its "always greater" append comparator on the main
  DB, so a named database that follows in the same input is not found in
  the main DB (C then adds a second record under that name or crashes).

Other differences:

* **`mdb_drop`** is not part of LMDB 0.9.36. This one is LMDB's own
  `mdb_drop` as first added (2017, before the 1.0 options `-L` and the
  crypto options `-m`/`-w`): same options, messages and exit codes. The
  tests build that C version against the 0.9.36 library as the oracle.
* **Signals.** C `mdb_dump` catches `SIGINT`, `SIGTERM`, `SIGHUP` and
  `SIGPIPE`, finishes the current record and exits with "Interrupted
  system call"; C `mdb_copy` ignores them. The Rust tools handle a closed
  pipe the same way (`mdb_dump` exits with "Interrupted system call",
  `mdb_copy` reports the write error), but `SIGINT`, `SIGTERM` and `SIGHUP`
  terminate them at once: catching them would need `unsafe` code. The
  environment stays consistent either way (the tools are read-only or
  commit atomically).
* **Write errors.** C ignores errors writing standard output (or the
  `-f` file of `mdb_dump`) and exits with 0. The Rust tools report them
  (`mdb_dump: <path>: No space left on device`, or
  `<tool>: write error: …` when it shows only at the final flush) and exit
  with 1. A closed pipe at the final flush is ignored, as in C.
* **`mdb_load` with `mapaddr=`** in the header opens the environment with
  `MDB_FIXEDMAP` as C does, but `rlmdb` ignores that flag (it never
  maps at a fixed address), so a new environment records address 0 and its
  dumps have no `mapaddr=` line (C records wherever its map landed).

## Tests

`cargo test -p rlmdb-tools` (and `rust/scripts/linux-test.sh test -p
rlmdb-tools` for Linux) builds the C library and tools from
`libraries/liblmdb` with the `cc` crate, plus `tests/c/mdb_drop.c` (the C
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
  `MDB_BAD_VALSIZE` when `mdb_load` creates) give the same errors.
* `load.rs`: crafted dumps in both encodings, loads into existing
  environments, `-T`, `-N`, `-a` (sorted and unsorted input, several
  batches), malformed input (header errors, truncated lines, bad
  escapes, oversized keys, a full map): identical messages and exit codes,
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
