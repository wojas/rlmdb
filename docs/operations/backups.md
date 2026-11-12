# Backups

An environment can be copied while programs read and write it. The copy
is one consistent snapshot: the state of the last commit when the copy
began.

This page covers the copy, the compacting copy, a dump as text, and
incremental backups.

The commands on this page are rlmdb's own `mdb_*` tools. They have the
names and options of C LMDB's tools, and they work on environments of
both formats.
[The command-line tools](../getting-started/index.md#the-command-line-tools)
says how to build them.

## What to back up

Only `data.mdb`. The lock file holds no data and is set up again when
the environment is opened.

Do not copy `data.mdb` with `cp` or a file-level backup tool while a
program writes it. Such a copy can mix pages of different commits. Use
one of the ways below, which read a snapshot.

## Copy an environment

`mdb_copy` copies an environment into a directory. The directory must
exist and be empty:

```sh
mkdir backup
mdb_copy data backup
```

The backup is a complete environment. You can open it like any other.

The copy runs inside a read transaction. Writers continue in the
meantime, and the copy does not see their commits. For a large
environment the copy takes a while, and like any long read transaction
it keeps old pages alive until it is done.

## Make a compacting copy

A plain copy has the size of the original, free pages included. With
`-c`, `mdb_copy` leaves the free pages out:

```sh
mkdir compact
mdb_copy -c data compact
```

For an environment where most of the data had been deleted, the plain
copy had 22.1 MB and the compacting copy 2.3 MB. A compacting copy is
also the way to make a data file smaller: see
[Make a file smaller](map-size.md#make-a-file-smaller).

Without a destination, `mdb_copy` writes the copy to its standard
output, which is handy for compressing or sending it:

```sh
mdb_copy -c data | gzip > data.mdb.gz
```

## Copy from inside a program

A program can copy its own environment. This is the only safe way for
an environment that runs [without locks](../locking/nolock.md), where
the tools must not be used while the program runs.

=== "C"

    ```c
    rc = mdb_env_copy2(env, "./backup", MDB_CP_COMPACT);
    ```

=== "Rust"

    ```rust
    env.copy_to_path("backup", true)?; // true: compacting
    ```

The directory must exist and must not hold a `data.mdb` yet.

## Restore a copy

A copy is restored by putting it in place of the data file:

1. Stop every program that has the environment open.
2. Replace `data.mdb` in the environment's directory with the copy. A
   copy that was written to standard output is this file itself; name
   it `data.mdb`.
3. Start the programs again.

Leave `lock.mdb` alone. The first program to open the environment sets
it up again.

## Dump and load

`mdb_dump` writes the contents of an environment as text, and `mdb_load`
reads that text into an environment:

```sh
mdb_dump -a data > dump.txt
mkdir restored
mdb_load -f dump.txt restored
```

`-a` dumps every named database. A dump is larger and slower than a
copy, but it does not depend on the file format. Use it to move data:

- **to the other format.** `mdb_load -F 1.0` creates the new environment
  in format 1.0;
- **to a machine with the other byte order**, which cannot read the data
  file itself.

When it loads into an environment in format 0.9, `mdb_load` prints
`unrecognized keyword ignored: db_pagesize`. That is harmless.

## Incremental backups

An incremental backup holds only the pages that changed since an earlier
backup. It is much smaller than a copy when little has changed.

Incremental backups need an environment in format 1.0. In format 0.9
the pages do not record when they were written, and the commands fail
with "Operation not supported".

!!! warning "Incremental backups with the tools of C LMDB"

    At the time of writing, with LMDB 1.0.3, the tools of C LMDB have
    two bugs in incremental backups:

    - `mdb_dump -i` can leave out pages that are in use, or write old
      contents in their place, after a large value was deleted and its
      pages were partly used again. The restored environment is then
      not the source.
    - `mdb_load -i` takes a dump that was cut short for a complete one.

    rlmdb's tools do not have these bugs. A dump made with rlmdb's
    `mdb_dump` can be loaded with either `mdb_load`.

A chain starts with a plain copy and continues with one dump per backup:

```mermaid
flowchart LR
    base["Copy<br>up to transaction 2"] --> i1["Dump<br>changes since 2"] --> i2["Dump<br>changes since 3"]
```

### Take the backups

Start with a plain copy, and note its last transaction ID. Do not use a
compacting copy here: it starts its transaction IDs over.

```console
$ mkdir base
$ mdb_copy data base
$ mdb_stat -e base | grep 'Last transaction'
  Last transaction ID: 2
```

For each later backup, first note the environment's last transaction
ID, then dump the pages that changed since the ID of the backup before:

```console
$ mdb_stat -e data | grep 'Last transaction'
  Last transaction ID: 3
$ mdb_dump -i 2 -f incr1 data
```

The next dump then starts from 3:

```console
$ mdb_dump -i 3 -f incr2 data
```

The output file must not exist yet. In this example the environment had
6.8 MB, and each dump, after 2,000 new records, had 2.3 MB.

### Restore the chain

Load the dumps, in order, into a copy of the base:

```sh
mkdir restored
cp base/data.mdb restored/
mdb_load -i -f incr1 restored
mdb_load -i -f incr2 restored
```

Load into a copy, as here, and keep the base until the last load has
succeeded. A load that is interrupted can be run again.

### From inside a program

=== "C"

    ```c
    rc = mdb_env_incr_dump(env, "./incr1", 2);   /* changes since transaction 2 */
    ```

=== "Rust"

    ```rust
    env.incr_dump_to_path("incr1", 2)?; // changes since transaction 2
    ```
