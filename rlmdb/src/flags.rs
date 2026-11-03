//! Typed flag sets for environments, databases and writes.
//!
//! The bit values are exactly those of `lmdb.h` (see [`crate::consts`]), so
//! `bits()` can be handed to C and flags read from disk (`MDB_db.md_flags`)
//! can be converted back with `from_bits_retain`.
//!
//! Flags of C LMDB that this implementation does not act on are still
//! representable; [`EnvironmentFlags`] documents per flag what happens.

use crate::consts::*;

bitflags::bitflags! {
    /// Environment flags (`mdb_env_open`, "mdb_env Environment Flags").
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct EnvironmentFlags: u32 {
        /// `MDB_FIXEDMAP`. Accepted and ignored: we never ask for a fixed
        /// map address. The bit is still recorded in the meta page of a new
        /// environment, as C does.
        const FIXED_MAP = MDB_FIXEDMAP;
        /// `MDB_NOSUBDIR`: `path` is the data file itself and the lock file
        /// is `path` + `"-lock"`.
        const NO_SUB_DIR = MDB_NOSUBDIR;
        /// `MDB_NOSYNC`: do not flush buffers to disk when committing.
        const NO_SYNC = MDB_NOSYNC;
        /// `MDB_RDONLY`: open the environment read-only.
        const READ_ONLY = MDB_RDONLY;
        /// `MDB_NOMETASYNC`: flush data but not the meta page on commit.
        const NO_META_SYNC = MDB_NOMETASYNC;
        /// `MDB_WRITEMAP`. The writable map is not used: pages are written
        /// with `pwrite`, as without the flag, and the files are byte for
        /// byte those written without it (C with the flag also extends the
        /// data file to the map size; rlmdb leaves it as long as a writer
        /// without the flag does). What the flag changes through the API
        /// is C's: nested transactions are refused
        /// ([`RwTxn::begin_nested_txn`](crate::RwTxn::begin_nested_txn)
        /// returns `MdbError::BadTxn`, C 0.9's code; C 1.0 returns
        /// `EINVAL`), [`Environment::set_flags`](crate::Environment::set_flags)
        /// cannot change it (`EINVAL`), and a read-only open drops it
        /// ([`Environment::flags`](crate::Environment::flags) does not
        /// report it).
        ///
        /// On OpenBSD an open for writing with it fails with `ENOTSUP`:
        /// that OS keeps a file's maps apart from what `pwrite` writes, so
        /// a process that took the flag but writes with `pwrite` must not
        /// share an environment with C processes that write through the
        /// map (`lmdb.h` forbids mixing processes with and without the
        /// flag). A read-only open drops it there too.
        const WRITE_MAP = MDB_WRITEMAP;
        /// `MDB_MAPASYNC`. Accepted and ignored: it only changes how C
        /// flushes a writable map (`MDB_WRITEMAP`); commits and
        /// [`Environment::sync`](crate::Environment::sync) sync as without
        /// it.
        const MAP_ASYNC = MDB_MAPASYNC;
        /// `MDB_NOTLS`. Accepted; this implementation always behaves this
        /// way (a reader slot belongs to a transaction, not a thread).
        const NO_TLS = MDB_NOTLS;
        /// `MDB_NOLOCK`: no lock file and no cross-process locking (no
        /// shared mutexes or semaphores, no `fcntl` locks), as in C. Other
        /// processes are the caller's business: none may write while this
        /// one uses the environment, nor read while this one writes.
        /// Within the process nothing changes: write transactions are
        /// serialised, and read transactions are tracked in memory so that
        /// writers never reuse their pages (C tracks none; see
        /// [`Environment::in_memory_readers`](crate::Environment::in_memory_readers)).
        const NO_LOCK = MDB_NOLOCK;
        /// `MDB_NORDAHEAD`: advise the OS that access is random.
        const NO_READ_AHEAD = MDB_NORDAHEAD;
        /// `MDB_NOMEMINIT`. Accepted and ignored (we always zero buffers).
        const NO_MEM_INIT = MDB_NOMEMINIT;
        /// `MDB_PREVSNAPSHOT` (LMDB 1.0's flag, for environments of both
        /// formats here): open the previous snapshot, the older of the two
        /// meta pages, rather than the latest one. The latest transaction
        /// is lost once a write transaction commits on top of the previous
        /// snapshot; this may help to get past some kinds of damage.
        ///
        /// The flag lasts until the first write transaction that changes
        /// something commits: from then on the environment works as if
        /// opened without it, and [`Environment::flags`] no longer reports
        /// it (an empty commit, an abort or a failed commit keep it). A
        /// read-only environment keeps it until it is closed.
        ///
        /// Opened for writing, the environment must be the only user of
        /// its files, as in C: the open fails with `MDB_ENV_BUSY` if
        /// another process has the environment open, and other processes
        /// that open it wait until the first commit (C's exclusive lock on
        /// the lock file is kept until then). Read-only, it may share the
        /// environment: if it is the only user it keeps the exclusive lock
        /// until it is closed, as C does; otherwise its read transactions
        /// take the older meta page while holding the writer lock for a
        /// moment, and leave the lock file as it is (`KNOWN_C_BUGS.md` row
        /// 68). With [`NO_LOCK`](Self::NO_LOCK) the handle has the data
        /// file to itself in this process until then: the open fails with
        /// `EBUSY` if the process has it open, and other opens of it fail
        /// with `EBUSY` meanwhile.
        ///
        /// [`Environment::flags`]: crate::Environment::flags
        const PREV_SNAPSHOT = MDB_PREVSNAPSHOT;
    }
}

bitflags::bitflags! {
    /// Database flags (`mdb_dbi_open`, "mdb_dbi_open Database Flags").
    ///
    /// All but [`DatabaseFlags::CREATE`] are persistent and stored in the
    /// database record (`MDB_db.md_flags`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct DatabaseFlags: u32 {
        /// `MDB_REVERSEKEY`: compare keys from the last byte backwards.
        const REVERSE_KEY = MDB_REVERSEKEY;
        /// `MDB_DUPSORT`: allow sorted duplicate data items per key.
        const DUP_SORT = MDB_DUPSORT;
        /// `MDB_INTEGERKEY`: keys are native `unsigned int` or `size_t`.
        const INTEGER_KEY = MDB_INTEGERKEY;
        /// `MDB_DUPFIXED`: with `DUP_SORT`, all data items have one size.
        const DUP_FIXED = MDB_DUPFIXED;
        /// `MDB_INTEGERDUP`: with `DUP_SORT`, data items are native integers.
        const INTEGER_DUP = MDB_INTEGERDUP;
        /// `MDB_REVERSEDUP`: with `DUP_SORT`, compare data from the end.
        const REVERSE_DUP = MDB_REVERSEDUP;
        /// `MDB_CREATE`: create the database if it does not exist.
        const CREATE = MDB_CREATE;
    }
}

bitflags::bitflags! {
    /// Write flags (`mdb_put`, `mdb_cursor_put`, "mdb_put Write Flags").
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct WriteFlags: u32 {
        /// `MDB_NOOVERWRITE`: fail with `KeyExist` if the key exists.
        const NO_OVERWRITE = MDB_NOOVERWRITE;
        /// `MDB_NODUPDATA`: fail with `KeyExist` if the pair exists.
        const NO_DUP_DATA = MDB_NODUPDATA;
        /// `MDB_CURRENT`: replace the item at the cursor position.
        const CURRENT = MDB_CURRENT;
        /// `MDB_RESERVE`: reserve space for the data and return it.
        const RESERVE = MDB_RESERVE;
        /// `MDB_APPEND`: append to the end of the database.
        const APPEND = MDB_APPEND;
        /// `MDB_APPENDDUP`: append to the end of the key's duplicates.
        const APPEND_DUP = MDB_APPENDDUP;
        /// `MDB_MULTIPLE`: store several `DUP_FIXED` items at once.
        const MULTIPLE = MDB_MULTIPLE;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_match_c() {
        assert_eq!(EnvironmentFlags::NO_SUB_DIR.bits(), 0x4000);
        assert_eq!(EnvironmentFlags::READ_ONLY.bits(), 0x20000);
        assert_eq!(EnvironmentFlags::NO_READ_AHEAD.bits(), 0x800000);
        assert_eq!(DatabaseFlags::DUP_SORT.bits(), 0x04);
        assert_eq!(DatabaseFlags::CREATE.bits(), 0x40000);
        assert_eq!(WriteFlags::MULTIPLE.bits(), 0x80000);
        assert_eq!(WriteFlags::APPEND_DUP.bits(), 0x40000);
    }
}
