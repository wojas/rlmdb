//! The dirty pages of a write transaction (`mt_u.dirty_list`, an `MDB_ID2L`).
//!
//! C keeps `malloc`ed page copies in a list sorted by page number and finds
//! them with `mdb_mid2l_search` in `mdb_page_get` before falling back to the
//! map. Here each dirty page (or overflow run) is an owned, zero-filled
//! `Box<[u8]>` of `n * psize` bytes keyed by its first page number, in an
//! [`Id2List`]. `TxnCore::page` consults this list first, so every read
//! operation sees the transaction's own changes.
//!
//! Buffers are never handed out across mutations: write code looks pages up
//! by number every time it needs them (see `wcursor`), so a `Vec` move or a
//! page being freed can never leave a dangling reference. C's buffer
//! recycling (`me_dpages`, `mdb_page_malloc`/`mdb_page_free`) is not
//! reproduced; the allocator does that job. Unlike C, new buffers are always
//! zeroed (as with C's default `!MDB_NOMEMINIT`, plus the parts C leaves
//! uninitialised), which only changes bytes no reader ever looks at.

use crate::idl::Id2List;

/// The dirty-page list of a write transaction.
#[derive(Debug, Default)]
pub(crate) struct DirtyPages {
    list: Id2List<Box<[u8]>>,
}

impl DirtyPages {
    /// An empty list.
    pub(crate) fn new() -> DirtyPages {
        DirtyPages::default()
    }

    /// Number of dirty pages (runs count once, `dl[0].mid`).
    pub(crate) fn len(&self) -> usize {
        self.list.len()
    }

    /// `true` if there are no dirty pages.
    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The buffer of dirty page `pgno` (the whole run for overflow pages).
    #[inline]
    pub(crate) fn get(&self, pgno: u64) -> Option<&[u8]> {
        if self.list.is_empty() {
            return None;
        }
        self.list.get(pgno).map(|b| &b[..])
    }

    /// Mutable access to the buffer of dirty page `pgno`.
    #[inline]
    pub(crate) fn get_mut(&mut self, pgno: u64) -> Option<&mut [u8]> {
        self.list.get_mut(pgno).map(|b| &mut b[..])
    }

    /// `true` if `pgno` is dirty.
    pub(crate) fn contains(&self, pgno: u64) -> bool {
        self.get(pgno).is_some()
    }

    /// `mdb_page_dirty`: add a page buffer. Returns `false` (and drops the
    /// buffer) if the page is already dirty, which would be a bug.
    pub(crate) fn insert(&mut self, pgno: u64, buf: Box<[u8]>) -> bool {
        self.list.insert(pgno, buf).is_ok()
    }

    /// Removes a page from the list, returning its buffer.
    pub(crate) fn remove(&mut self, pgno: u64) -> Option<Box<[u8]>> {
        self.list.remove(pgno)
    }

    /// The page numbers in ascending order.
    pub(crate) fn pgnos(&self) -> impl Iterator<Item = u64> + '_ {
        self.list.as_slice().iter().map(|(p, _)| *p)
    }

    /// `mdb_dlist_free`: drop all buffers.
    pub(crate) fn clear(&mut self) {
        self.list.clear();
    }
}

/// A zero-filled buffer for `npages` pages (`mdb_page_malloc`).
pub(crate) fn alloc_pages(page_size: usize, npages: usize) -> Box<[u8]> {
    vec![0u8; page_size * npages].into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_and_unique() {
        let mut d = DirtyPages::new();
        assert!(d.is_empty());
        assert!(d.insert(9, alloc_pages(64, 1)));
        assert!(d.insert(3, alloc_pages(64, 2)));
        assert!(d.insert(5, alloc_pages(64, 1)));
        assert!(!d.insert(5, alloc_pages(64, 1)));
        assert_eq!(d.pgnos().collect::<Vec<_>>(), [3, 5, 9]);
        assert_eq!(d.get(3).map(<[u8]>::len), Some(128));
        d.get_mut(9).unwrap()[0] = 7;
        assert_eq!(d.get(9).unwrap()[0], 7);
        assert!(d.contains(5) && !d.contains(4));
        assert!(d.remove(5).is_some());
        assert_eq!(d.len(), 2);
        d.clear();
        assert!(d.get(3).is_none());
    }
}
