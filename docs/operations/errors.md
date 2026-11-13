# Errors and what to do

This page lists the errors you are most likely to meet, with what each
one means and how to get past it. They are grouped by when they happen.

In C, every function returns 0 or an error code, and `mdb_strerror`
gives its text. In Rust, the same errors are values of `rlmdb::Error`.
The headings below use the C names; the Rust name follows in the text.

Two results look like errors but are part of normal use:

- `MDB_NOTFOUND`: the key is not there, or a cursor has reached the end.
  In Rust these are `None`.
- `MDB_KEYEXIST`: the key is there already, and you asked not to
  overwrite it.

## When opening an environment

### No such file or directory (`ENOENT`)

The directory of the environment does not exist. rlmdb creates the files
in it, but not the directory. Create it first.

### Permission denied (`EACCES`)

The program cannot write to `data.mdb` or `lock.mdb`. A program that
only reads still needs write permission on `lock.mdb`: see
[Read-only environments](../locking/index.md#read-only-environments).

### Resource busy (`EBUSY`)

The process has this environment open already. Open it once and share
the handle between threads. In Rust this is `Error::EnvAlreadyOpened`.

### `MDB_VERSION_MISMATCH`

Something about the files does not match what the program expects.

- The program asked for one format and the environment has the other.
  An existing environment keeps its format: see
  [Choose a data format](../getting-started/c-program.md#choose-a-data-format).
- Another kind of process has the environment open, and its lock file
  does not fit. See
  [Who can share an environment](../locking/platforms.md#who-can-share-an-environment).

In Rust: `MdbError::VersionMismatch`.

### `MDB_INVALID`

The file is not an LMDB data file, or it was written on a machine with
the other byte order. C LMDB 0.9 also reports this for an environment
in format 1.0. In Rust: `MdbError::Invalid`.

### `RLMDB_UNKNOWN_FEATURE`

`RLMDB_FEATURES` or the program names a feature that this rlmdb does not
know. Check the spelling. See [Features](../features/index.md).

## When beginning a transaction

### `MDB_READERS_FULL`

Every slot of the reader table is taken. Clear stale readers first, and
raise the limit if you need more. See
[The reader table is full](../locking/index.md#the-reader-table-is-full).
In Rust: `MdbError::ReadersFull`.

### `MDB_MAP_RESIZED`

Another process made the environment larger than this process's map.
Set the map size to 0 and begin the transaction again. See
[When another process grows the map](map-size.md#when-another-process-grows-the-map).
In Rust: `MdbError::MapResized`.

### Permission denied (`EACCES`), for a write transaction

The environment was opened read-only. Open it without the read-only flag
to write.

### `MDB_PANIC`

An earlier commit failed in a way that left this handle unusable, such
as a disk error while writing. Close the environment and open it again.
In Rust: `MdbError::Panic`.

### The call does not return

A write transaction waits for the writer lock, without a timeout. See
[Write transactions wait forever](../locking/index.md#write-transactions-wait-forever).

## When writing

### `MDB_MAP_FULL`

The data file has reached the map size. Abort the transaction, set a
larger map size, and try again. See
[Map size and file growth](map-size.md). In Rust: `MdbError::MapFull`.

If the data did not grow, something keeps freed pages from being used
again: see [Why a data file grows](map-size.md#why-a-data-file-grows).

### `MDB_DBS_FULL`

The program opened more named databases than it allowed for. The default
is none. Set the number before you open the environment, with
`mdb_env_set_maxdbs` in C or `.max_dbs(n)` in Rust. In Rust:
`MdbError::DbsFull`.

### `MDB_BAD_VALSIZE`

A key is empty or too long. In format 0.9 a key can have 511 bytes at
most.

- Shorten the key. For long values such as URLs, store a hash as the
  key.
- Or use format 1.0, where the limit depends on the page size and is
  1,978 bytes with 4 KiB pages. `mdb_env_get_maxkeysize` tells the limit
  of an environment.

In Rust: `MdbError::BadValSize`.

### `MDB_BAD_TXN`

The transaction cannot be used any more. Either an earlier operation in
it failed, and it can only be aborted, or it has a nested transaction
that is still open. In Rust: `MdbError::BadTxn`.

### No space left on device (`ENOSPC`)

The disk is full. The commit fails and nothing of the transaction is
kept. Free some space and run the transaction again.

## When reading

### `MDB_CORRUPTED` or `MDB_PAGE_NOTFOUND`

The data file is damaged: a page is not what it should be. rlmdb reports
this instead of crashing or returning nonsense. In Rust:
`MdbError::Corrupted` and `MdbError::PageNotFound`.

The usual causes are outside LMDB:

- Two programs wrote the environment without coordinating: with
  [`MDB_NOLOCK`](../locking/nolock.md), after `lock.mdb` was deleted
  while the environment was open, or with
  [processes that do not fit together](../locking/platforms.md#who-can-share-an-environment).
- The environment is on a network file system.
- The disk or the file system lost or changed data.
- Another program overwrote or truncated `data.mdb`.

What to do:

1. Stop every program that uses the environment.
2. Keep the files as they are, in a copy.
3. Restore your last [backup](backups.md).

If you have no backup, the snapshot before the last commit may still be
intact. `mdb_copy -v` copies that snapshot into a new environment:

```sh
mkdir rescued
mdb_copy -v data rescued
```

`mdb_dump` can still write out the databases that are readable.
