//! `MDB_db`: the 48-byte record describing one B-tree.
//!
//! Two of them live in every meta page (`mm_dbs[FREE_DBI]`, `mm_dbs[MAIN_DBI]`),
//! named databases store one as the data of an `F_SUBDATA` node in the main
//! DB, and `MDB_DUPSORT` keys whose duplicates outgrew a sub-page store one
//! as the data of an `F_DUPDATA|F_SUBDATA` node.

use super::layout::*;
use super::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};
use crate::consts::P_INVALID;

/// A decoded `MDB_db` record (mdb.c line 1058).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Db {
    /// `md_pad`: in `mm_dbs[FREE_DBI]` the page size (`mm_psize`); in a
    /// `MDB_DUPFIXED` sub-DB the fixed item size (`P_LEAF2` key size).
    pub pad: u32,
    /// `md_flags`: persistent database flags. In `mm_dbs[FREE_DBI]` this is
    /// `mm_flags`, the persistent environment flags.
    pub flags: u16,
    /// `md_depth`: depth of the tree (0 when empty).
    pub depth: u16,
    /// `md_branch_pages`: number of internal pages.
    pub branch_pages: u64,
    /// `md_leaf_pages`: number of leaf pages.
    pub leaf_pages: u64,
    /// `md_overflow_pages`: number of overflow pages.
    pub overflow_pages: u64,
    /// `md_entries`: number of data items.
    pub entries: u64,
    /// `md_root`: root page number, or `P_INVALID` for an empty tree.
    pub root: u64,
}

impl Db {
    /// An empty tree with the given persistent flags.
    pub fn empty(flags: u16) -> Db {
        Db {
            flags,
            root: P_INVALID,
            ..Db::default()
        }
    }

    /// Decodes a record from the first [`DB_SIZE`] bytes of `buf`.
    /// Returns `None` if `buf` is too short.
    pub fn read(buf: &[u8]) -> Option<Db> {
        if buf.len() < DB_SIZE {
            return None;
        }
        Some(Db {
            pad: get_u32(buf, DB_PAD)?,
            flags: get_u16(buf, DB_FLAGS)?,
            depth: get_u16(buf, DB_DEPTH)?,
            branch_pages: get_u64(buf, DB_BRANCH_PAGES)?,
            leaf_pages: get_u64(buf, DB_LEAF_PAGES)?,
            overflow_pages: get_u64(buf, DB_OVERFLOW_PAGES)?,
            entries: get_u64(buf, DB_ENTRIES)?,
            root: get_u64(buf, DB_ROOT)?,
        })
    }

    /// Encodes the record into the first [`DB_SIZE`] bytes of `buf`.
    ///
    /// # Panics
    /// If `buf` is shorter than [`DB_SIZE`].
    pub fn write(&self, buf: &mut [u8]) {
        put_u32(buf, DB_PAD, self.pad);
        put_u16(buf, DB_FLAGS, self.flags);
        put_u16(buf, DB_DEPTH, self.depth);
        put_u64(buf, DB_BRANCH_PAGES, self.branch_pages);
        put_u64(buf, DB_LEAF_PAGES, self.leaf_pages);
        put_u64(buf, DB_OVERFLOW_PAGES, self.overflow_pages);
        put_u64(buf, DB_ENTRIES, self.entries);
        put_u64(buf, DB_ROOT, self.root);
    }

    /// The record as its 48 on-disk bytes.
    pub fn to_bytes(&self) -> [u8; DB_SIZE] {
        let mut b = [0u8; DB_SIZE];
        self.write(&mut b);
        b
    }

    /// `true` if the tree has no root (`md_root == P_INVALID`).
    pub fn is_empty_tree(&self) -> bool {
        self.root == P_INVALID
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let db = Db {
            pad: 4096,
            flags: 0x14,
            depth: 3,
            branch_pages: 2,
            leaf_pages: 17,
            overflow_pages: 5,
            entries: 1234,
            root: 99,
        };
        let bytes = db.to_bytes();
        assert_eq!(&bytes[0..4], &4096u32.to_ne_bytes());
        assert_eq!(&bytes[4..6], &0x14u16.to_ne_bytes());
        assert_eq!(&bytes[40..48], &99u64.to_ne_bytes());
        assert_eq!(Db::read(&bytes), Some(db));
        assert_eq!(Db::read(&bytes[..47]), None);
        assert!(Db::empty(0).is_empty_tree());
    }
}
