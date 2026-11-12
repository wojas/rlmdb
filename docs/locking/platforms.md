# Lock implementations by platform

The writer lock has to work between processes, and each operating system
offers something different for that. rlmdb uses what C LMDB uses on the
same platform, so that both can share an environment.

Find your platform, then read the page of its lock:

| Platform | Lock | After a writer crashes |
|---|---|---|
| Linux, Android, FreeBSD | [Robust pthread mutexes](pthread-mutexes.md) | The next writer recovers |
| macOS | [POSIX named semaphores](posix-semaphores.md) | Writers wait until you step in |
| OpenBSD, NetBSD, DragonFly with format 0.9 | [POSIX named semaphores](posix-semaphores.md) | Writers wait until you step in |
| OpenBSD, NetBSD, DragonFly with format 1.0 | [SysV semaphores](sysv-semaphores.md) | The next writer recovers |
| Windows | [Named mutexes](windows-mutexes.md) | The next writer recovers |

The last column is the difference that matters most in practice. On
macOS, and with format 0.9 on three of the BSDs, a process that is
killed in a write transaction leaves the writer lock taken.

Android, NetBSD and DragonFly are implemented but not tested yet.

## What is the same everywhere

Each implementation provides two locks: the writer lock, and a second
one that is held for a moment while a read transaction gets its slot in
the reader table. The rest of the protocol does not depend on the
platform:

- **Setting up.** The first process to open an environment takes a file
  lock on `lock.mdb`, creates the two locks and clears the reader table.
  Later processes attach.
- **The reader table.** One slot for each running read transaction, as
  described in [How locking works](index.md).
- **Finding dead readers.** On Unix, every process that reads holds a
  file lock on one byte of `lock.mdb`, at the position of its process
  ID. A reader check tests those locks. On Windows it asks the system
  whether the process still exists.

## Who can share an environment

Processes share an environment when they agree on the files. That means:

- **The same operating system.** The lock file of one platform means
  nothing on another.
- **The same byte order.** LMDB writes its files in the byte order of
  the machine. A big-endian and a little-endian machine cannot use the
  same files. To move data from one to the other, use `mdb_dump` and
  `mdb_load`.
- **The same C library.** On Linux the lock is a structure of the C
  library, stored in the lock file. A program built with glibc and one
  built with musl do not agree on it. Watch for this when a container
  based on Alpine shares a directory with the host or with a container
  based on Debian.
- **The same format.** The lock file of a 1.0 environment differs from
  that of a 0.9 environment. This takes care of itself, since both
  processes open the same data file.

C LMDB has the same rules between two C programs.

!!! warning "A mismatch is not always reported"

    Do not count on an error to tell you. Sometimes the open fails,
    with `MDB_VERSION_MISMATCH` or `MDB_INVALID`. Often it succeeds,
    and the processes then misread each other's locks. Writers may wait
    forever. Worse, two may write at the same time, which damages the
    environment, and the errors that follow, such as `MDB_CORRUPTED`,
    point nowhere near the cause. Check the list above yourself before
    you let two kinds of programs share an environment.

If you need none of this, because one process is the only user of the
environment, see [Running without locks](nolock.md).
