//! The byte-level API: LMDB's C API in Rust clothing.
//!
//! Keys and values are byte slices in and out; lifetimes do the work that
//! the "do not do X" paragraphs of `lmdb.h` do in C:
//!
//! * an [`Environment`] owns the files and maps; transactions borrow it, so
//!   it cannot be closed while they are alive;
//! * a [`RoTxn`] pins one snapshot; every slice it (or one of its
//!   [`RoCursor`]s) returns borrows the transaction and stays valid until it
//!   ends, because pages of a pinned snapshot never change;
//! * a [`Dbi`] is a plain `Copy` handle, as in C; using it after
//!   [`Environment::close_db`] yields [`MdbError::BadDbi`](crate::MdbError).
//!
//! Read transactions are never bound to a thread (we always behave like
//! `MDB_NOTLS`, which is compatible with every C peer since the flag is a
//! per-process choice that the lock file does not record).
//!
//! Module map (C functions in parentheses):
//!
//! * [`env`](mod@env): open/close, meta pages, map size (`mdb_env_open`,
//!   `mdb_env_open2`, `mdb_env_read_header`, `mdb_env_init_meta`,
//!   `mdb_env_close0`, `mdb_env_stat`, `mdb_env_info`);
//! * [`reader`]: the lock file and reader table (`mdb_env_setup_locks`,
//!   `mdb_env_excl_lock`, `mdb_env_share_locks`, `mdb_reader_pid`,
//!   `mdb_reader_list`, `mdb_reader_check0`, `mdb_find_oldest`,
//!   `mdb_mutex_failed`);
//! * [`txn`]: read transactions (`mdb_txn_begin`, `mdb_txn_renew0`,
//!   `mdb_txn_reset`, `mdb_txn_abort`, `mdb_get`);
//! * [`dbi`]: database handles (`mdb_dbi_open`, `mdb_dbi_close`,
//!   `mdb_dbi_flags`, `mdb_stat`, `mdb_default_cmp`, stale-record refresh in
//!   `mdb_page_search`);
//! * [`cursor`]: B-tree search and movement (`mdb_page_get`,
//!   `mdb_page_search*`, `mdb_node_search`, `mdb_cursor_*`,
//!   `mdb_xcursor_init*`).

pub mod cursor;
pub mod dbi;
pub mod env;
pub mod reader;
pub mod ro_cursor;
pub mod txn;

pub use crate::cmp::{CmpFn, Comparator};
pub use crate::error::{Error, MdbError, Result};
pub use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};
pub use dbi::Dbi;
pub use env::{Environment, EnvironmentBuilder};
pub use reader::{format_reader_list, ReaderInfo};
pub use ro_cursor::{CursorOp, Entry, RoCursor};
pub use txn::{InactiveTxn, RoTxn, Transaction};

/// Statistics for a database (`MDB_stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// `ms_psize`: size of a database page.
    pub page_size: u32,
    /// `ms_depth`: depth (height) of the B-tree.
    pub depth: u32,
    /// `ms_branch_pages`: number of internal (non-leaf) pages.
    pub branch_pages: usize,
    /// `ms_leaf_pages`: number of leaf pages.
    pub leaf_pages: usize,
    /// `ms_overflow_pages`: number of overflow pages.
    pub overflow_pages: usize,
    /// `ms_entries`: number of data items.
    pub entries: usize,
}

impl Stat {
    pub(crate) fn from_db(page_size: usize, db: &crate::format::Db) -> Stat {
        Stat {
            page_size: page_size as u32,
            depth: u32::from(db.depth),
            branch_pages: db.branch_pages as usize,
            leaf_pages: db.leaf_pages as usize,
            overflow_pages: db.overflow_pages as usize,
            entries: db.entries as usize,
        }
    }
}

/// Information about the environment (`MDB_envinfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    /// `me_mapsize`: size of the data memory map.
    pub map_size: usize,
    /// `me_last_pgno`: ID of the last used page.
    pub last_pgno: usize,
    /// `me_last_txnid`: ID of the last committed transaction.
    pub last_txnid: usize,
    /// `me_maxreaders`: max reader slots in the environment.
    pub max_readers: u32,
    /// `me_numreaders`: max reader slots used in the environment.
    pub num_readers: u32,
}
