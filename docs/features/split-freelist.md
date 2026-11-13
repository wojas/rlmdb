# split-freelist

`split-freelist` keeps a large delete from growing the data file. With
it, a write transaction that frees many pages records them in several
small pieces instead of one big one.

It is off by default. It works with both formats, and it does not change
the file format: C LMDB, and rlmdb without the feature, read and write
the files as before.

## The problem

LMDB does not overwrite a page that a reader may still use. When a write
transaction stops using pages, because it deletes data or replaces it,
it writes their page numbers into the *freelist*, so that later
transactions can use the pages again. Each transaction adds one record
to the freelist, with 8 bytes for every page it freed.

A big delete makes a big record. Take a delete of 4 GB of data with
4 KiB pages:

- It frees about a million pages.
- Their record takes 8 MB.
- LMDB stores a record of that size in one piece: about 2,000 pages in a
  row.

A file that has been in use for a while has plenty of free pages, but
rarely 2,000 in a row. So the file grows to make room for the record:

```text
The file. Free pages (.) lie scattered between used ones (#):

  ##.#..##.#.###..#.##.#

One big record (R) needs its pages in a row, so it goes at the end:

  ##.#..##.#.###..#.##.#RRRRRRRR

Small records fit into the gaps:

  ##R#RR##R#R###RR#.##.#
```

The result is odd: a transaction that only deletes data makes the file
larger, and it can fail with `MDB_MAP_FULL` when the file has reached
its map size.

## What the feature does

With `split-freelist`, a transaction that frees many pages writes
several records of one page each. They fit into single free pages, as in
the last line of the picture.

Only large transactions split their record: those that free more than
about 2 MB of pages, by deleting or replacing that much data. That
number is for 4 KiB pages, the usual size on Linux and Windows. With the
16 KiB pages of an Apple Silicon Mac it is about 32 MB. Every other
commit works exactly as without the feature.

## Turn it on

=== "C"

    ```c
    #include <rlmdb.h>              /* includes lmdb.h */

    mdb_env_create(&env);
    rlmdb_env_enable_feature(env, "split-freelist");
    mdb_env_open(env, "./data", 0, 0644);
    ```

=== "Rust"

    ```rust
    use rlmdb::{Environment, Feature};

    let env = Environment::builder()
        .enable(Feature::SplitFreelist)
        .open("data")?;
    ```

=== "Without changing the program"

    ```sh
    RLMDB_FEATURES=split-freelist ./prog
    ```

[Features](index.md) explains these three ways.

## What changes for your program

It is safe to turn on unless your program counts transaction IDs. Two
things change, and only for a commit that splits its record.

### The transaction ID jumps

Each record is stored under a transaction ID of its own, starting with
the one the transaction began with. The commit itself is written twice,
under two consecutive IDs. So it is published under a higher ID than the
transaction had while it was open:

```text
A transaction begins as 100 and needs two records:

  100   freelist record
  101   freelist record
  102   the commit, written to one meta page
  103   the commit, written to the other meta page   ← published
```

The next transaction is 104. With more records the jump is larger: the
ID rises by 3 for two or three records, by 5 for four or five, and so
on.

Code that expects every commit to add 1 to the last transaction ID is
wrong with this feature.

### Two synced writes

A data file has two meta pages. An ordinary commit writes one of them. A
commit that splits its record writes both, and both reach the disk
before it returns, also with `MDB_NOMETASYNC`. `MDB_NOSYNC` is not
affected.

## Get the ID your commit publishes

While a transaction is open, it reports the ID it began with. It decides
whether to split when it prepares the commit. So prepare first, read the
ID, then commit:

=== "C"

    ```c
    size_t published;

    rc = mdb_txn_prepare(txn);          /* writes the data, publishes nothing */
    if (rc) {
        mdb_txn_abort(txn);
        return rc;
    }
    published = mdb_txn_id(txn);        /* the ID the commit will have */
    rc = mdb_txn_commit(txn);
    ```

=== "Rust"

    ```rust
    let prepared = txn.prepare()?;      // writes the data, publishes nothing
    let published = prepared.id();      // the ID the commit will have
    prepared.commit()?;
    ```

In a test, a transaction that deleted about 3,000 large values began as
5 and committed as 8.

Do not read the ID from `mdb_env_info` after the commit. Another writer
may have committed in between.

!!! note "Programs written for the LMDB 0.9 header"

    `mdb_txn_prepare` is part of the LMDB 1.0 API. rlmdb has it for
    environments of both formats. A program that is compiled with the
    0.9 `lmdb.h` can declare it itself:

    ```c
    int mdb_txn_prepare(MDB_txn *txn);
    ```

## What other programs see

Other programs, on rlmdb or on C LMDB, with or without the feature, keep
working on the environment as usual.

- Readers see the state before the transaction, and then the committed
  state. There is nothing in between.
- The last transaction ID rises by more than one.
- `rlmdb_stat -ff` lists several freelist records for the transaction.

## Rare cases

Three cases are worth knowing. None of them damages the data file.

- **A C LMDB reader that begins at the moment the commit writes its
  second meta page** can get wrong results or errors in that one read
  transaction. This is a race that C LMDB has anyway; the feature makes
  it more likely. Readers on rlmdb are not affected.
- **A writer that dies between the two meta writes** leaves a database
  that is fine, with the commit in it. The next commit tidies up. Until
  then, looking at the previous state on purpose (`MDB_PREVSNAPSHOT`,
  the tools' `-v`) is not reliable.
- **Rolling back an already committed transaction is not possible
  after a split commit.** `mdb_env_rollback` takes the last commit out
  of the file again, after it was committed. A split commit fills both
  meta pages, so the commit before it is no longer in the file, and
  `mdb_env_rollback` returns `MDB_CANT_ROLLBACK`. `MDB_PREVSNAPSHOT`
  shows the split commit itself. Aborting a write transaction that is
  still open is something else, and it works as always.

The repository has the full reasoning, with every scenario that was
checked:
[how a split commit writes the meta pages, and why](https://github.com/wojas/rlmdb/blob/main/docs/split-freelist-details.md).
