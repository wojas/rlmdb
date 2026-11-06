# The `split-freelist` feature

`split-freelist` is an opt-in feature of rlmdb. It changes how a write
transaction that frees many pages stores them in the free list: in
several small records instead of one big one. It is off by default. It
works with files of both formats (LMDB 0.9 and 1.0), and the file format
does not change: C LMDB, and rlmdb without the feature, read and write
these files as usual.

This page explains what the feature does, how to turn it on, and what
changes for your program. [`split-freelist-details.md`](split-freelist-details.md)
has the reasoning behind every detail, with the scenarios we checked.

## When it matters

Only a transaction that frees more pages than one page of free-list
record can hold splits its record: about 500 pages with 4 KiB pages,
about 2,000 with 16 KiB pages. Every other commit works exactly as
before.

## The problem it solves

LMDB never overwrites a page that a reader may still use. When a write
transaction stops using pages (it deletes data, or copies a page before
changing it), it lists their page numbers in the free list: a database
inside the file, keyed by transaction ID. Each transaction adds one
record. The record holds the numbers of all pages it freed, 8 bytes
each.

A big delete makes a big record. Deleting 4 GB of data with 4 KiB pages
frees about a million pages. Their record takes 8 MB, and LMDB stores a
value that large in one contiguous run of pages: about 2,000 pages in a
row. The free list rarely has a run that long, so the file grows to make
room. A transaction that only deletes data can then fail with
`MDB_MAP_FULL`.

## What the feature does

One page of free-list record holds about 500 page numbers with 4 KiB
pages (509), or 2,045 with 16 KiB pages. If a transaction frees more
pages than that, it splits the list into records of one page each.

An LMDB file starts with two meta pages. Each one names a committed
state: normally the newest commit and the one before it. A split commit
writes its result into both, so the state before it is gone from the
file. This is what makes it safe for the next writer to reuse the freed
pages ([details, section 2.3](split-freelist-details.md#23-why-both-meta-pages)).

Each record needs its own transaction ID, and the commit needs two more
for its two meta pages. So the commit lands on a higher ID than the one
the transaction started with:

| A transaction that began as 100 and needs … | commits as |
|---|---|
| 1 record (no split) | 100 |
| 2 or 3 records | 103 |
| 4 or 5 records | 105 |

IDs 100 and 101 only label free-list records. [Details, section 2.2](split-freelist-details.md#22-the-two-meta-pages)
explains the numbers.

## Turning it on

The feature is a setting of one environment handle. Turn it on before the
environment is opened. It is not stored in the files, and other processes
may have it off.

Rust:

```rust
use rlmdb::{Environment, Feature};

let env = Environment::builder()
    .enable(Feature::SplitFreelist)
    // or by name, for example from a configuration file:
    // .enable_feature("split-freelist")?
    .open(path)?;
```

C (with `rlmdb.h`):

```c
mdb_env_create(&env);
rlmdb_env_enable_feature(env, "split-freelist");
mdb_env_open(env, path, 0, 0644);
```

Without changing the program, through the variable `RLMDB_FEATURES`:

```sh
RLMDB_FEATURES=split-freelist ./prog
```

These read the variable:

* C programs;
* the rlmdb tools;
* Rust programs that call `read_environment(true)` on the builder.

## What changes for your program

### The transaction ID jumps

A split commit lands on a higher ID than the transaction reported while
it was open (see the table above). Code that assumes one commit adds 1
to the last transaction ID (`mdb_env_info`'s `me_last_txnid`,
`Environment::info().last_txnid`) is wrong with this feature.

A read transaction that begins during the commit may see the committed
state under the first of the two IDs (102 in the table). Later readers
see it under the second (103). No reader ever sees ID 100 or 101.

A program that compares "the last ID" with "my commit's ID", to tell its
own commits from those of other writers, needs the ID the commit
published. See [Getting the ID](#getting-the-id).

### Such a commit cannot be undone

`mdb_txn_prepare` (Rust: `RwTxn::prepare`) is the first half of a
two-phase commit: it writes the data but does not commit it yet. The
transaction decides whether to split while it saves the free list, which
happens in that first half. So after the prepare, the program knows.

A split commit writes its result into both meta pages, so the state
before it is no longer in the file. `Environment::rollback` and
`mdb_env_rollback` refuse it with `MDB_CANT_ROLLBACK`. A program that
needs to undo commits (two-phase commit) checks the ID after the
prepare: if it rose, the program aborts the prepared transaction instead
of committing. That always works, because nothing has been published
yet.

### One more synced write

A split commit writes two meta pages instead of one, and both are
written to disk before the commit returns, also with `MDB_NOMETASYNC`.
The cost is one extra synced write per split commit; it matters most
with `MDB_NOMETASYNC`, where ordinary commits sync nothing.
`MDB_NOSYNC` is not affected. [Details, section 3](split-freelist-details.md#3-the-commit-step-by-step)
explains why both writes are synced.

## Getting the ID

While the transaction is open, `Transaction::id` (C: `mdb_txn_id`) gives
the ID it began with. The transaction decides whether to split while it
saves the free list, in the prepare. So: prepare, read the ID, then
commit. Do not read it from `mdb_env_info` after the commit: another
writer may have committed in the meantime.

Rust:

```rust
use rlmdb::{Dbi, Transaction};

let mut txn = env.begin_rw_txn()?;
for key in keys_to_delete {
    txn.del(Dbi::MAIN, key, None)?;
}
let began_as = txn.id();
let prepared = txn.prepare()?;      // writes the data pages, not the meta pages
let published = prepared.id();      // the ID the commit publishes
if published != began_as {
    // The commit will split. It cannot be rolled back afterwards.
}
prepared.commit()?;
```

C:

```c
MDB_txn *txn;
size_t began_as, published;
int rc;

mdb_txn_begin(env, NULL, 0, &txn);
/* ... mdb_del() ... */
began_as = mdb_txn_id(txn);
rc = mdb_txn_prepare(txn);          /* writes the data pages, not the meta pages */
if (rc) {
	mdb_txn_abort(txn);
	return rc;
}
published = mdb_txn_id(txn);        /* the ID the commit publishes */
if (published != began_as) {
	/* The commit will split. It cannot be rolled back afterwards. */
}
rc = mdb_txn_commit(txn);
```

If the transaction changed nothing, the prepare reports the ID it began
with (100). Nothing is committed, and the environment's last ID stays
99.

### Which programs can get the ID

| Program | Can use the feature | Can get the published ID |
|---|---|---|
| Rust | yes | `PreparedTxn::id` |
| C, compiled against rlmdb's `lmdb.h` (the LMDB 1.0 header) and linked with rlmdb | yes | `mdb_txn_prepare`, then `mdb_txn_id`; with 0.9 and 1.0 files |
| C, compiled against C LMDB 0.9's `lmdb.h`, running on rlmdb | yes (`RLMDB_FEATURES`, or `rlmdb_env_enable_feature`) | yes, if it declares `mdb_txn_prepare` itself (below); otherwise only with a race |
| C, running on C LMDB | no | no |

`mdb_txn_prepare` is part of the LMDB 1.0 API, and every rlmdb library
exports it, for files of both formats. A program written for the 0.9
header can switch to rlmdb's `lmdb.h`, or declare the function itself:

```c
int mdb_txn_prepare(MDB_txn *txn);
```

## What other programs see

* Readers see the state before the transaction, then the committed
  state. A reader that begins during the commit may report the first of
  the two IDs.
* The last transaction ID jumps by more than one.
* The free list holds several records for the transaction, under
  consecutive keys (`mdb_stat -ff` lists them).
* The two meta pages hold consecutive IDs and the same committed state.
  `MDB_PREVSNAPSHOT` and the tools' `-v` therefore show the committed
  state, not the state before the transaction.
* Writers that follow, C or Rust, with or without the feature, work as
  usual.

## Remaining risks

Each is described with the exact sequence of events in the details page.

* A C LMDB reader that begins at the moment the commit writes its second
  meta page can get wrong results or errors in that one read
  transaction. It writes nothing, and the file stays intact. This is an
  existing race in C LMDB (`KNOWN_C_BUGS.md` row 31); the feature makes
  it easier to hit. rlmdb readers are not affected.
  ([5.1](split-freelist-details.md#51-c-readers-copying-a-meta-page-that-is-being-rewritten))
* If the writing process dies between the two meta writes, the database
  is fine as it is, but the state before the transaction stays on one
  meta page for one more transaction. An rlmdb writer writes the second
  meta page before it writes anything else, as extra protection: if a C
  writer comes first, writes pages and then aborts, a rollback to that
  old state (`MDB_PREVSNAPSHOT`, the tools' `-v`, C 1.0's
  `mdb_env_rollback`) would read overwritten pages.
  ([5.2](split-freelist-details.md#52-death-between-the-two-meta-writes))
* On macOS, a synced write is two system calls. A process death between
  them, followed by a power loss, can damage the previous snapshot on
  disk, where stock LMDB would only lose the last commit.
  ([5.3](split-freelist-details.md#53-macos))
* If the second meta write fails (a disk error), the commit is still
  durable and returns success, but the environment refuses further
  transactions until it is reopened. The commit's ID may then be 102
  instead of the 103 that the prepare reported.
  ([5.4](split-freelist-details.md#54-a-failed-second-meta-write))
* The tests do not run C LMDB over files written with the feature. The
  format is C's, and after a split commit the file looks like two
  ordinary commits.
