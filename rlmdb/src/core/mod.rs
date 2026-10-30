//! The byte-level API: LMDB's C API in Rust clothing.
//!
//! Keys and values are byte slices in and out; lifetimes do the work that
//! the "do not do X" paragraphs of `lmdb.h` do in C (the crate
//! documentation lists the rules and has an example):
//!
//! * an [`Environment`] owns the files and maps; transactions borrow it, so
//!   it cannot be closed while they are alive;
//! * a [`RoTxn`] pins one snapshot; every slice it (or one of its
//!   [`RoCursor`]s) returns borrows the transaction and stays valid until it
//!   ends, because pages of a pinned snapshot never change;
//! * an [`RwTxn`] is the environment's single writer (the writer lock is
//!   shared with C processes); slices it returns borrow it, and changes
//!   need `&mut`, so they cannot outlive a change that moves a dirty page.
//!   [`RwTxn::begin_nested_txn`] starts a child that borrows the parent;
//! * cursors of a write transaction come in two forms: [`RwCursor`]
//!   borrows the transaction (the Rust way), a [`CursorId`] is a handle
//!   used through [`RwTxn::cursor_get`], [`RwTxn::cursor_put`], ... and
//!   stays valid across every change in the transaction, as C's tracked
//!   cursors do (the C API is built on these);
//! * a [`Dbi`] is a plain `Copy` handle, as in C; using it after
//!   [`Environment::close_db`] yields [`MdbError::BadDbi`](crate::MdbError).
//!
//! Read transactions are never bound to a thread (we always behave like
//! `MDB_NOTLS`, which is compatible with every C peer since the flag is a
//! per-process choice that the lock file does not record).
//!
//! Where C 0.9.36 has a plain bug that does not affect the files, this
//! module does the obviously correct thing instead; `rust/KNOWN_C_BUGS.md`
//! lists every such case, and the ones ported on purpose.
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
//! * [`txn`]: read transactions and the [`Transaction`] trait
//!   (`mdb_txn_begin`, `mdb_txn_renew0`, `mdb_txn_reset`, `mdb_txn_abort`,
//!   `mdb_get`);
//! * [`dbi`]: database handles (`mdb_dbi_open`, `mdb_dbi_close`,
//!   `mdb_dbi_flags`, `mdb_stat`, `mdb_default_cmp`, stale-record refresh in
//!   `mdb_page_search`);
//! * [`cursor`]: B-tree search and movement (`mdb_page_get`,
//!   `mdb_page_search*`, `mdb_node_search`, `mdb_cursor_*`,
//!   `mdb_xcursor_init*`), and [`ro_cursor`]: the public read cursor and
//!   [`CursorOp`] (`mdb_cursor_get`);
//! * [`txn_rw`]: write transactions and write cursors (`mdb_txn_begin`,
//!   `mdb_txn_commit`, `mdb_put`, `mdb_cursor_put`, `mdb_dbi_open` with
//!   `MDB_CREATE`, `mdb_drop`), built on private modules: `wcursor` (write
//!   cursors and their fixups, `mdb_page_touch`), `dirty` (the dirty page
//!   list), `page_ops` (byte-level node edits), `freelist`
//!   (`mdb_page_alloc`, `mdb_freelist_save`), `cursor_put`
//!   (`mdb_cursor_put`, `mdb_node_add`), `split` (`mdb_page_split`),
//!   `cursor_del` (`mdb_cursor_del`, `mdb_rebalance`, `mdb_page_merge`,
//!   `mdb_node_move`, `mdb_drop`), `commit` (`mdb_page_flush`,
//!   `mdb_env_write_meta`), `nested` (nested transactions:
//!   `mdb_txn_begin` with a parent, `mdb_cursor_shadow`, the nested commit
//!   and abort) and `spill` (`mdb_page_spill`, `mdb_page_unspill`);
//! * `copy`: [`Environment::copy_to_path`] and
//!   [`Environment::copy_to_fd`] (`mdb_env_copyfd0`, `mdb_env_copyfd1`,
//!   `mdb_env_cwalk`), byte-identical to `mdb_copy` and `mdb_copy -c`.

#[cfg(test)]
pub(crate) mod audit;
mod commit;
mod copy;
pub mod cursor;
mod cursor_del;
mod cursor_put;
pub mod dbi;
pub(crate) mod dirty;
pub mod env;
mod freelist;
mod nested;
pub(crate) mod page_ops;
pub mod reader;
pub mod ro_cursor;
mod spill;
mod split;
pub mod txn;
pub mod txn_rw;
pub(crate) mod wcursor;

pub use crate::cmp::{CmpFn, Comparator};
pub use crate::error::{Error, MdbError, Result};
pub use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};
pub use dbi::Dbi;
pub use env::{
    default_format, process_default_format, set_default_format, sniff_format, Environment,
    EnvironmentBuilder, DEFAULT_FORMAT_ENV,
};
pub use reader::{format_reader_list, ReaderInfo};
pub use ro_cursor::{CursorOp, Entry, RoCursor};
pub use txn::{InactiveTxn, RoTxn, Transaction};
pub use txn_rw::{CursorId, RwCursor, RwTxn};

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
    /// `me_mapaddr`: the map address recorded in the newest meta page
    /// (`mm_address`; non-zero only for C environments created with
    /// `MDB_FIXEDMAP`). C reports its own map address there; this crate
    /// never uses a fixed address, so it reports the recorded one.
    pub map_address: usize,
}
