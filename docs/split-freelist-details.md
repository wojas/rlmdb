# `split-freelist`: how a split commit writes the meta pages, and why

This page is the reasoning behind the `split-freelist` feature
([`split-freelist.md`](split-freelist.md)): what a split commit writes,
why it writes both meta pages, what every reader and writer sees, and
what remains. It documents what rlmdb does, and explains the directions
not taken.

Throughout, `T` is the ID a write transaction began with, the previous
commit is `T − 1`, and the examples use `T = 100`.

## 1. Background

### 1.1 Two meta pages

An LMDB file begins with two meta pages, page 0 and page 1. A meta page
names a snapshot: the ID of the transaction that committed it, the root
pages of the trees, and the last page in use. Even IDs go to page 0, odd
IDs to page 1. So the two pages always hold the last two commits:

```
before commit 100:   page 0: ID 98          page 1: ID 99  (current)
after  commit 100:   page 0: ID 100 (current)  page 1: ID 99
```

Readers and writers find the current snapshot through the lock file,
which names the newest ID. Crash recovery takes the meta page with the
higher ID.

### 1.2 The free list

A writer never changes a page that a committed snapshot uses. It copies
the page to a free or new one, and lists the old page as freed. At commit,
the freed pages go into the free list: a database inside the file, keyed
by transaction ID. Transaction 100 stores its freed pages under key 100.
Those pages were in use by snapshot 99 and earlier; snapshot 100 does not
use them.

### 1.3 The reuse rule, and what it protects

A writer with ID `W` may reuse the pages of a free-list record if its key
is below `W − 1` and below the snapshot of every active reader. With no
readers, that means keys up to `W − 2`. The record under `W − 1` is kept:

```
writer 101 begins; meta pages hold 100 and 99
records:  key 97   key 98   key 99  |  key 100
          reusable (≤ 99)           |  kept
```

Record 100 holds the pages that snapshot 99 used and transaction 100
stopped using. Snapshot 99 is still on meta page 1. If writer 101
overwrote those pages and then died, meta page 1 would point at garbage.
Records up to 99 hold pages of snapshots up to 98, which no meta page
names any more.

This gives the invariant everything else rests on: an ordinary writer
leaves every page that either meta page points at unchanged, until its
own commit replaces one of the meta pages. If the writer aborts, dies or
the machine loses power, both meta pages still describe intact snapshots.
(A writer opened with `MDB_PREVSNAPSHOT` is the deliberate exception: it
starts from the older snapshot and discards the newest.)

Every path that reads "the other meta page" relies on it:

* crash recovery, when the newest meta page did not reach the disk
  (`MDB_NOMETASYNC`);
* `MDB_PREVSNAPSHOT` (LMDB 1.0 and rlmdb): open the previous snapshot on
  purpose;
* `mdb_env_rollback` (LMDB 1.0 and rlmdb): undo the last commit;
* the tools' `-v` option (`mdb_stat -v`, `mdb_copy -v`);
* a reader that still holds an old cached ID without a lock file
  (`MDB_NOLOCK`).

## 2. What a split commit writes

### 2.1 The records

A transaction that frees more pages than fit in one page of record saves
them in `n` records of at most one page each, under consecutive keys
starting at its own ID. For `T = 100` and `n = 3`:

```
key 100 → the lowest third of the freed page numbers
key 101 → the middle third
key 102 → the highest third
```

One page of record holds `maxfree` page numbers: the page size divided
by 8, less the page header and the count. With 4 KiB pages that is 509
(508 in the 1.0 format, whose header is larger); with 16 KiB pages, 2045.
The lowest page numbers go under the lowest key, because a writer loads
records in key order and prefers low pages. Saving the free list can
itself free or use pages, so the save may run more than once. Full
records come first, so that a rerun rewrites them in place and only the
last record grows.

### 2.2 The two meta pages

Keys 100 to 102 are taken, so the commit needs an ID of at least 102. The
commit writes its snapshot into both meta pages:

* first under 102, into page 0. Page 0 holds 98, which no new reader can
  start on, because the lock file names 99. Like any ordinary commit,
  this leaves 99 intact if the write fails. This ID is called `P` below;
* then under 103, into page 1, replacing 99. This ID, `P + 1`, is called
  `D` below (for duplicate).

```
after the split commit:   page 0: ID 102      page 1: ID 103 (current)
                          both: the roots of the committed state
records:                  key 100, key 101, key 102 (the pages of snapshot 99)
```

Three rules decide the IDs:

| Rule | Why |
|---|---|
| `P` is even when `T` is even, odd when `T` is odd. | For 100: page 0, which holds 98. The lock file names 99, so no new reader starts on 98. This is what every ordinary commit does: if the write fails or tears, 99 is untouched. |
| `P` is at least the last key, `T + n − 1`. | Later transactions get IDs above `D`, so their own free-list keys never collide with the split keys. |
| `D = P + 1` goes to the other page with the same roots and last page. | It replaces `T − 1`. Afterwards the two meta pages are consecutive and both describe the committed state, which is what the reuse rule assumes. |

In numbers, for `T = 100`:

| Records `n` | Keys | `P` (first meta page) | `D` (second, published) | IDs that are only keys |
|---|---|---|---|---|
| 1 | 100 | 100 (an ordinary commit) | | none |
| 2 | 100, 101 | 102 | 103 | 100, 101 |
| 3 | 100, 101, 102 | 102 | 103 | 100, 101 |
| 4 | 100 … 103 | 104 | 105 | 100 … 103 |
| 5 | 100 … 104 | 104 | 105 | 100 … 103 |

With an odd number of records, `P` is also the key of the last record,
as an ordinary commit's ID is the key of its record. With an even number,
the last key has the wrong parity, so `P` is the next ID after it (102
after 101).

The ID the prepare reports, and the commit publishes, is `D`. The pages
of the 1.0 format are stamped with `P` (section 4.5).

### 2.3 Why both meta pages

If the commit wrote only the first meta page, the file would be:

```
page 0: ID 102 (current)   page 1: ID 99 (the old snapshot, still valid)
records: key 100, key 101 reusable by writer 103; key 102 kept
```

The reuse rule keeps only record 102. Records 100 and 101 hold pages of
snapshot 99 too, and meta page 1 still says that 99 is a complete
snapshot. The next writer, 103, reuses those pages. Normally it then
commits and its meta page replaces 99. But a writer can also write pages
without committing: when it spills dirty pages to make room, when it
prepares a two-phase commit and then aborts, or when its process dies
after writing. Then meta page 1 points at pages that hold the aborted
writer's data.

Crash recovery is fine either way: the first meta page is synced, so
recovery takes 102. But every path of section 1.3 that reads the
previous snapshot on purpose reads garbage. The left-hand column below
was observed once with a one-meta-page build, also with an unmodified
C 1.0 writer as the successor; `rlmdb/tests/split_freelist.rs` checks
the right-hand column:

| After a split commit and an aborted successor | One meta page | Both meta pages |
|---|---|---|
| Open with `PREV_SNAPSHOT`, read a key | the aborted writer's value | the committed state |
| `rollback` of the split commit, then read | the aborted writer's value, permanently | refused |
| `NO_LOCK`: a reader in another process with an old cached ID, while a local writer prepares | its data overwritten under it | intact |

One meta page would be enough for pure C 0.9, which has neither
`PREV_SNAPSHOT` nor `rollback`. It is not enough for rlmdb or LMDB 1.0.
With both meta pages written, nothing names snapshot 99 any more; its
pages are listed in records 100 to 102, and reusing them is as safe as
after any two ordinary commits. Readers that began on 99 keep it through
their reader slot, as always.

## 3. The commit, step by step

```
S1  Work. Reuse only records with keys ≤ 98, as any writer 100 would.
S2  Save the free list: records 100, 101, 102. Only now the ID becomes P = 102.
S3  Write every changed page to the file; sync the data file.
    (S1–S3 are the prepare phase; mdb_txn_prepare stops here and reports D = 103.)
S4  Write meta page 0: ID 102, the roots. Synced, also with MDB_NOMETASYNC.
S5  Publish 102 in the lock file. New readers now start on 102.
S6  Write meta page 1: ID 103, the same roots. Synced. Snapshot 99 is gone.
S7  Publish 103. Release the writer lock. The next writer begins as 104.
```

In every mode except `MDB_NOSYNC`, S4 and S6 are synchronous writes
(`O_DSYNC`; on macOS followed by `F_FULLFSYNC`), so the page is on disk
before the step ends. What each step prevents:

| Without this step | What would go wrong |
|---|---|
| S5: meta page 1 written while the lock file still names 99 | A reader beginning right then copies the page being written. rlmdb readers re-check the published ID after copying and start over if it changed; with no publication, the re-check passes and the torn copy is used. |
| The sync of S4 | If the process dies after S5, the next writer sees 102, reuses 99's pages, and a later power loss leaves 102 not on disk and 99 damaged: a corrupt database in a mode that promised integrity. |
| The sync of S6 | Crash recovery would still be fine: 102 is on disk. But the next writer may overwrite 99's pages before 103 reaches the disk; a power loss then leaves 102 and 99 on disk, with 99 damaged, for anyone who reads the previous snapshot on purpose. The second sync keeps both meta pages usable after a power loss, as stock LMDB does. |
| The writer lock held from S5 to S6 | Another writer could begin in the one-meta-page state of section 2.3. A process death there is section 5.2. |

What readers see:

| When | The lock file names | A new reader gets |
|---|---|---|
| Before S5 | 99 | snapshot 99, from meta page 1: the state before the transaction |
| Between S5 and S7 | 102 | snapshot 102, from meta page 0: the committed state |
| After S7 | 103 | snapshot 103, from meta page 1: the committed state |
| Never | | 100 or 101. They exist only as free-list keys. |

## 4. Scenarios

Every case we checked, with the outcome. "Integrity modes" means every
mode except `MDB_NOSYNC`, which promises nothing after a crash in stock
LMDB either.

### 4.1 Power loss (integrity modes)

| Power lost during | On disk afterwards | Opens at | Outcome |
|---|---|---|---|
| S1–S3 | 98 and 99 (the transaction's new pages are in unreferenced slots) | 99 | intact: transaction 100 reused only pages of snapshots up to 97 |
| S4 | page 0 is still 98 or fully 102 (LMDB assumes that a 120-byte write inside one sector does not tear; every meta write relies on that) | 99 or 102 | both consistent; 102's data pages were synced in S3 |
| S5, S6 | 102 and 99, or 102 and 103 | 102 or 103 | both describe the committed state |
| after S6 | 102 and 103 | 103 | the stock shape |
| the next writer, 104 | 102 and 103; some of 99's pages overwritten | 103 (or 104 once its meta page is on disk) | as after any two ordinary commits |

### 4.2 The process dies (the OS keeps running)

| Dies during | The next writer sees | Outcome |
|---|---|---|
| S1–S3 | 99 published; the transaction is gone | as any aborted transaction |
| S4, on Linux | the `O_DSYNC` write completes before the thread can exit: 102 is either absent or on disk | fine |
| S4 or S6 on macOS, between the write and `F_FULLFSYNC` | 102 (or 103) visible in the page cache, perhaps not yet on the drive | section 5.3 |
| after S4, before S6 completes | 102 current, page 1 still 99 | durable and consistent; the next writer begins as 103 and its commit replaces 99. See 5.2 |
| between S6 and S7 | pages 102 and 103; the lock file says 102 | robust-mutex recovery and a reopen both publish the newest meta page, 103 |

### 4.3 Writers after the split

| Case | What happens |
|---|---|
| Writer 104 (rlmdb, or C 0.9 / 1.0, which follow the same reuse rule) | reuses records with keys up to 102: all of 99's freed pages. No meta page names 99. Its own record goes under 104; its meta page replaces 102. |
| Writer 104 spills, or prepares and aborts, or dies after writing pages | meta pages still 102 and 103, both intact: it wrote only into pages of 99 and older, which neither names. This is the case that broke the single meta page. |
| Several split commits in a row; split commits mixed with commits of processes without the feature, or C processes | every commit leaves the stock shape; no writer needs to know what the previous one did. |
| A writer under `MDB_PREVSNAPSHOT` | starts from 102 (the committed state) as writer 103 and writes meta page 1. No record has key 103 yet, so its own record does not collide with a split record. It reuses keys up to 101: 99's pages, which nothing names. |

### 4.4 Readers

| Case | What happens |
|---|---|
| A reader on 99 from before the commit | its slot pins 99; every writer keeps the records with keys of 99 and above. As in stock. |
| A reader on 98 or older | S4 rewrites its meta page, but it copied that page at its begin and never reads it again. As in stock. |
| An rlmdb reader beginning during S4–S7 | it may copy a meta page while it is being rewritten, but it re-checks the published ID afterwards and throws the copy away if the ID changed. |
| A C reader (0.9 or 1.0) that pinned 99 just before S5 and copies meta page 1 while S6 rewrites it | C does not re-check after the copy: for that one transaction it can see 103's data under the ID 99, or a mix of records (section 5.1). |
| `NO_LOCK`: a reader in our process with an old cached ID, after another process did a split commit | its cached ID names meta page 1, which now holds 103's roots: a complete, intact snapshot. It reads the committed state (and may report the old ID: an existing quirk of `NO_LOCK`). Nothing it reads is reusable by any writer. |

### 4.5 LMDB 1.0 files

The 1.0 format stamps every page with the ID of the transaction that
wrote it, and a C 1.0 writer treats a page as its own, changeable in
place, if the stamp is at or above its ID. rlmdb stamps the pages of a
split commit with `P` (spilled pages keep `T`). A normal successor begins
as `D + 1`; a successor under `MDB_PREVSNAPSHOT` begins as `D`. Both are
above `P`, so both copy the pages instead of changing them. Stamping
with `D` would make the `PREV_SNAPSHOT` writer change committed pages in
place.

Incremental backups (`mdb_env_incr_dump`) take the pages whose stamp is
above the given ID: a dump since 99 includes the pages stamped 100 and
102 and both meta pages; a dump since 102 includes only meta page 103;
a dump since 103 includes nothing of this commit.

### 4.6 Everything else

| Case | What happens |
|---|---|
| `rollback(103)` in rlmdb | `MDB_CANT_ROLLBACK`: the previous meta page holds the same snapshot. `rollback(102)` and `rollback(100)` fail too: not the newest commit. |
| `mdb_env_rollback(103)` in C 1.0 | succeeds; 102 becomes current with the same data. C cannot know. |
| Aborting the prepared transaction | nothing published; the next writer begins at 100 and reuses the written pages. The way to undo a split commit: decide at the prepare. |
| S4 fails (I/O error) | as any failed meta write: the old ID is written back, nothing is published, the commit failed, the environment is unusable in this process (`MDB_PANIC`). S6 failing is section 5.4. |
| A pair left half written (section 4.2, row 4) | left as it is: the next writer begins as 103, as after any commit, and its own commit replaces the old page. Nothing at a writer's begin writes to the file. |
| Nested transactions | their freed pages join the parent's; only the top-level commit saves the free list and writes meta pages. |
| Spilling during the save | record pages are ordinary dirty pages; spilled ones land in free or new slots. |
| Transaction IDs near the end of the range | A transaction splits only if the writer after it would still have an ID at least 16 below 2⁶⁴. The highest ID marks a free reader slot, and near 2⁶⁴ C 1.0's per-writer work ID wraps to zero. If a save has already split and the IDs would run out on a rerun, the commit fails with `EOVERFLOW`; falling back to one record there would list pages twice. In practice this is unreachable: it needs about 10¹⁹ commits. |
| Unchanged | copies and `mdb_stat -ff` walk the free list record by record; `MDB_NOSYNC` syncs nothing and promises nothing, as in stock; with `MDB_WRITEMAP`, rlmdb writes all pages with `pwrite`, meta pages included, so the synced writes need nothing special. |

## 5. What remains

### 5.1 C readers copying a meta page that is being rewritten

C LMDB readers pin an ID, then copy the meta page it names, without
checking afterwards that the page still holds that ID. If a commit
rewrites the page in between, the reader uses what it copied
(`KNOWN_C_BUGS.md` row 31): the newer commit's data under the older ID,
or a mix of two commits' records. In stock LMDB that needs the reader to
stall for a whole write transaction between pinning and copying. With
the split commit, S6 rewrites the page that was published until S5, a
few microseconds earlier, so a C reader that pinned 99 just before S5
and was preempted before finishing its copy is hit. rlmdb readers
re-check and retry. The consequence for the C reader is one read
transaction with wrong results or errors; it writes nothing, and its
reader slot still protects the pages it may read. rlmdb's `rollback`
accepts the same race. Waiting for old reader slots to drain before S6
would avoid it, but would let a long-lived reader block every split
commit.

### 5.2 Death between the two meta writes

If the process dies after S4 and before S6 completes, the file holds
102 on one meta page and 99 on the other: consistent, and durable (102
is on disk). Nothing repairs this. The next writer begins as 103, as
after any commit, and its own commit replaces 99. Until that commit, the
window of the one-meta-page design exists for one transaction: if that
writer reuses 99's pages and then does not commit (a spill, a prepare
that aborts, a death), a read of the old snapshot on purpose
(`PREV_SNAPSHOT`, the tools' `-v`, C 1.0's `mdb_env_rollback`) gets
overwritten pages. The database itself is fine throughout.

Writing the missing second page at the next writer's begin was
considered and not taken: a begin that writes to the file is unwanted
magic in a transaction, and a program that expects its transaction to
move the ID by one would see it move by two.

### 5.3 macOS

On macOS a synced write is two system calls: the write and
`F_FULLFSYNC`. If the process dies between them, the meta page is
visible but perhaps not yet on the drive. Stock LMDB has this for every
meta write in the default mode; the consequence there is losing the last
commit. For a split commit the consequence is worse: a writer that takes
over may reuse 99's pages, and a power loss then leaves 99 damaged on
disk. A full sync when taking over from a dead writer would narrow this
for rlmdb writers, but on macOS a dead writer is only recovered by
reopening the environment, and a process cannot tell at open whether the
previous close was clean. The window stays, as it does for stock LMDB
there.

### 5.4 A failed second meta write

If S6 fails after S5 published 102, the data is committed and durable
under 102, and that cannot be taken back. The commit then:

1. does not write the old snapshot back over the page. If the duplicate
   reached the page cache, it is a complete, valid meta page with the
   same data as 102, and better than 99; the commit then publishes 103
   as planned. Otherwise the page still holds 99 and the lock file keeps
   naming 102;
2. returns success: the data is committed, and a two-phase coordinator
   that got an error would tell the other participants to roll back
   while this one is committed;
3. marks the environment unusable in this process (`MDB_PANIC` for every
   later transaction until it is reopened), as rlmdb does after any
   failed meta write: the disk failed a write, and the program must
   notice;
4. leaves the file as it is: the next writer's commit replaces the page
   (section 5.2).

What a program must know when a commit returns success and the
environment then fails with `MDB_PANIC`:

* the prepare reported 103. If the second write did not reach the file,
  the commit is 102, and 103 is still free;
* the next commit, by any writer, gets 103. A program that treats 103
  as its own would miss that commit;
* after reopening, read the last ID with `mdb_env_info` again.

This needs a disk that fails on the second of two writes a few
microseconds apart, or on the sync after it.

## 6. Rollback and two-phase commit

After a split commit the previous meta page holds the same data as the
current one. `MDB_PREVSNAPSHOT` and the tools' `-v` show the committed
state, not the state before the transaction; they stay safe, because the
page they read is intact. `rollback` cannot undo the transaction: the
state before it is not in the file any more. C 1.0's `mdb_env_rollback`
would return success and make 102 current with the data unchanged, which
a two-phase-commit program would misread as an undo. rlmdb detects the
pair (the previous meta page holds the same trees and last page as the
newest) and returns `MDB_CANT_ROLLBACK`. The check identifies "a rollback
that would change no data", not uniquely a split commit: `mdb_drop` of
an already empty main database produces the same pair, and refusing to
roll that back costs nothing either.

A program that needs reversibility aborts after the prepare if the ID
rose: nothing has been published yet, so that is always clean.

## 7. Decisions, and what we did not do

| Decision | Why |
|---|---|
| The ID the prepare reports is `D`, the second meta page's. | It is the last committed ID after the commit, which is what a program compares against `mdb_env_info`. |
| A transaction with nothing to write reports the ID it began with. | Nothing is published, so there is no commit ID. Reporting the previous commit's ID instead would make a program that rolls back on failure roll back the previous commit. |
| No separate function to get the published ID. | The two-phase commit already has it, for both data formats, through the existing API. |
| Both meta writes are synchronous in every mode except `MDB_NOSYNC`. | Section 3. |

Alternatives considered and not taken:

| Alternative | Why not |
|---|---|
| One meta write | Section 2.3. |
| One meta write, plus rlmdb refusing `PREV_SNAPSHOT` and `-v` on such a file | Protects rlmdb only; C 1.0's `PREV_SNAPSHOT` and rollback stay exposed. |
| Write the second meta page at the next writer's begin instead of in the commit | C writers would not, so the hole stays whenever a C writer follows. |
| Keys that the next writer cannot reuse | Impossible. With one meta write and a commit as 100, the next writer (101) may reuse every key up to 99. Only key 100 is kept, and keys from 101 up belong to later writers. So there is room for only one protected record. |
| A reuse rule only rlmdb follows | C writers ignore it. |
| One key with several values, or another record format | A format change; C could not read the file. |
| Writing the second meta page unsynced | Crash recovery stays correct, but a power loss can leave the old snapshot on disk and damaged (section 3). |
| Completing a half-written pair at the next writer's begin | A begin that writes to the file, and a transaction that moves the ID by two. The file is consistent as it is (section 5.2). |

## 8. Costs

| | Ordinary commit | Split commit |
|---|---|---|
| IDs used | 1 | `n + 1` (odd `n`) or `n + 2` (even `n`) |
| Meta pages written | 1 | 2 |
| Synced meta writes, default mode | 1 | 2 |
| Synced meta writes, `MDB_NOMETASYNC` | 0 | 2 |
| Rollback of the commit | undoes it | refused |

A split commit happens only when a transaction frees more than one
page's worth of page numbers: about 500 pages with 4 KiB pages, about
2,000 with 16 KiB. The cost of the second synced write is one more sync
per such commit; a bulk delete can free many pages while writing little
data, so it is not always hidden behind the data write.
