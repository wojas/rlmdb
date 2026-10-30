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
        /// `MDB_WRITEMAP`. Accepted and ignored: writes always go through
        /// `pwrite`, which is compatible with C peers using a writable map.
        const WRITE_MAP = MDB_WRITEMAP;
        /// `MDB_MAPASYNC`. Accepted and ignored (only meaningful with
        /// `MDB_WRITEMAP`).
        const MAP_ASYNC = MDB_MAPASYNC;
        /// `MDB_NOTLS`. Accepted; this implementation always behaves this
        /// way (a reader slot belongs to a transaction, not a thread).
        const NO_TLS = MDB_NOTLS;
        /// `MDB_NOLOCK`: no lock file; the caller must serialise access.
        const NO_LOCK = MDB_NOLOCK;
        /// `MDB_NORDAHEAD`: advise the OS that access is random.
        const NO_READ_AHEAD = MDB_NORDAHEAD;
        /// `MDB_NOMEMINIT`. Accepted and ignored (we always zero buffers).
        const NO_MEM_INIT = MDB_NOMEMINIT;
        /// `MDB_PREVSNAPSHOT` (LMDB 1.0 only). Rejected with `EINVAL` at
        /// open time, as LMDB 0.9 rejects unknown flags.
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
