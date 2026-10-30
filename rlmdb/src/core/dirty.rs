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
//! page being freed can never leave a dangling reference. Unlike C, new
//! buffers are always zeroed (as with C's default `!MDB_NOMEMINIT`, plus the
//! parts C leaves uninitialised), which only changes bytes no reader ever
//! looks at.
//!
//! # Recycled buffers
//!
//! The list creates and drops every buffer itself
//! ([`DirtyPages::alloc_zeroed`], [`DirtyPages::alloc_copy`]; removing,
//! clearing, merging), so that single-page buffers can be reused instead of
//! going back to the allocator: C keeps them on `me_dpages`
//! (`mdb_page_malloc`, `mdb_page_free`, `mdb_dpage_free`). Here a
//! [`PagePool`] travels with the dirty list: the environment keeps it
//! between write transactions (under the writer lock), a write transaction
//! holds it while it runs, and a nested transaction borrows it from its
//! parent. Committing a big transaction no longer frees thousands of pages
//! for the next one to allocate again; unlike C's, the pool shrinks to what
//! the last transaction used and has a cap, so a huge transaction does not
//! pin its memory forever. Recycling never shows in the data file: a reused
//! buffer is zero-filled or completely overwritten before it is used.
//!
//! # Nested transactions and spilled pages
//!
//! A nested transaction has its own dirty list; reads fall back to its
//! parent's, then the grandparent's, and so on (the `mt_parent` walk of
//! `mdb_page_get`). While a child runs, the parent cannot be used (the child
//! borrows it), so the child takes the parent's lists with it as [`Layer`]s
//! and gives them back when it ends.
//!
//! Pages written to the data file before commit to keep the dirty list
//! within `MDB_IDL_UM_MAX` (`mdb_page_spill`) leave the dirty list and are
//! recorded in the transaction's spill set ([`SpillSet`], C's
//! `mt_spill_pgs`); they are read back from the map. C keeps that list as
//! `pgno << 1` values with the low bit marking deleted entries; a set of
//! page numbers has the same meaning.

use std::cell::Cell;
use std::collections::BTreeSet;

use crate::idl::Id2List;

/// `mt_spill_pgs`: page numbers this transaction wrote to the data file
/// before commit (see the module documentation).
pub(crate) type SpillSet = BTreeSet<u64>;

/// The dirty and spilled pages of one ancestor of a nested transaction.
#[derive(Debug, Default)]
pub(crate) struct Layer {
    /// The ancestor's `mt_u.dirty_list`.
    pub(crate) dirty: DirtyPages,
    /// The ancestor's `mt_spill_pgs`.
    pub(crate) spilled: SpillSet,
}

/// Number of remembered positions in [`DirtyPages`].
const HINTS: usize = 16;

/// The most memory a [`PagePool`] keeps, however big the last write
/// transaction was (C's `me_dpages` keeps every page it was ever given
/// until the environment closes). A transaction that dirties more pages
/// than this still gets this many from the pool and allocates the rest.
const POOL_MAX_BYTES: usize = 256 << 20;

/// Spare single-page buffers (C's `me_dpages`). Their contents are
/// arbitrary; whoever takes one zero-fills or overwrites it completely.
///
/// Between write transactions the pool keeps as many buffers as the last
/// transaction used ([`PagePool::trim`]), up to `POOL_MAX_BYTES`: the next
/// transaction of the same kind then allocates nothing, and after a big one
/// the memory goes back to the allocator when the next, smaller one ends.
#[derive(Debug, Default)]
pub(crate) struct PagePool {
    /// The page size: only buffers of exactly this length are kept. The
    /// default pool (0) keeps none.
    page_size: usize,
    /// How many buffers it keeps at most.
    max: usize,
    /// The spare buffers.
    free: Vec<Box<[u8]>>,
    /// Single-page buffers asked for since the last [`PagePool::trim`]
    /// (from the pool or not).
    used: usize,
}

impl PagePool {
    /// An empty pool for buffers of `page_size` bytes.
    pub(crate) fn new(page_size: usize) -> PagePool {
        PagePool {
            page_size,
            max: POOL_MAX_BYTES / page_size.max(1),
            free: Vec::new(),
            used: 0,
        }
    }

    /// A spare buffer of `len` bytes, if there is one.
    fn take(&mut self, len: usize) -> Option<Box<[u8]>> {
        if len != self.page_size {
            return None;
        }
        self.used += 1;
        self.free.pop()
    }

    /// `mdb_page_free`: keeps `buf` if it is one page and the pool is not
    /// full, else frees it (overflow runs are freed, as by
    /// `mdb_dpage_free`).
    fn put(&mut self, buf: Box<[u8]>) {
        if buf.len() == self.page_size && self.free.len() < self.max {
            self.free.push(buf);
        }
    }

    /// Adds the buffers of `other` (and takes its page size if this pool
    /// has none). Hand-offs between the environment and a transaction,
    /// where one side is empty, move the list without copying it.
    pub(crate) fn merge(&mut self, mut other: PagePool) {
        if self.page_size == 0 || other.free.len() > self.free.len() {
            std::mem::swap(self, &mut other);
        }
        self.used += other.used;
        for buf in other.free {
            self.put(buf);
        }
    }

    /// At the end of a write transaction: keep no more spare buffers than
    /// it used (see [`PagePool`]), and start counting again.
    pub(crate) fn trim(&mut self) {
        self.free.truncate(self.used);
        self.used = 0;
    }
}

/// The dirty-page list of a write transaction.
#[derive(Debug, Default)]
pub(crate) struct DirtyPages {
    list: Id2List<Box<[u8]>>,
    /// Where pages were last found in `list`, by `pgno % HINTS`. Only a
    /// hint: the page number stored at the position is checked before use
    /// (inserts and removals shift positions), so a stale hint just means
    /// a binary search. The write path looks up the same few pages (root,
    /// branch, leaf) many times per operation; this spares most searches.
    hint: [Cell<u32>; HINTS],
    /// Where buffers come from and go to (see the module documentation).
    pool: PagePool,
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
        let hint = &self.hint[pgno as usize % HINTS];
        if let Some(b) = self.list.get_at(hint.get() as usize, pgno) {
            return Some(b);
        }
        let i = self.list.search(pgno);
        let b = self.list.get_at(i, pgno)?;
        hint.set(i as u32);
        Some(b)
    }

    /// Mutable access to the buffer of dirty page `pgno`.
    #[inline]
    pub(crate) fn get_mut(&mut self, pgno: u64) -> Option<&mut [u8]> {
        let hint = &self.hint[pgno as usize % HINTS];
        let mut i = hint.get() as usize;
        if self.list.get_at(i, pgno).is_none() {
            i = self.list.search(pgno);
            hint.set(i as u32);
        }
        self.list.get_at_mut(i, pgno).map(|b| &mut b[..])
    }

    /// `true` if `pgno` is dirty.
    pub(crate) fn contains(&self, pgno: u64) -> bool {
        self.get(pgno).is_some()
    }

    /// `mdb_page_malloc` for a new page: a zero-filled buffer of `len`
    /// bytes (one page or an overflow run), recycled if possible. Hand it
    /// to [`DirtyPages::insert`].
    pub(crate) fn alloc_zeroed(&mut self, len: usize) -> Box<[u8]> {
        match self.pool.take(len) {
            Some(mut buf) => {
                buf.fill(0);
                buf
            }
            None => vec![0u8; len].into_boxed_slice(),
        }
    }

    /// A buffer holding a copy of `src` (a whole page or run that is about
    /// to be dirtied), recycled if possible. Hand it to
    /// [`DirtyPages::insert`].
    pub(crate) fn alloc_copy(&mut self, src: &[u8]) -> Box<[u8]> {
        match self.pool.take(src.len()) {
            Some(mut buf) => {
                buf.copy_from_slice(src);
                buf
            }
            None => src.into(),
        }
    }

    /// `mdb_page_dirty`: add a page buffer. Returns `false` (and recycles
    /// the buffer) if the page is already dirty, which would be a bug.
    pub(crate) fn insert(&mut self, pgno: u64, buf: Box<[u8]>) -> bool {
        match self.list.insert(pgno, buf) {
            Ok(()) => true,
            Err(buf) => {
                self.pool.put(buf);
                false
            }
        }
    }

    /// Removes a page from the list (its buffer is recycled). Returns
    /// `false` if it was not there.
    pub(crate) fn remove(&mut self, pgno: u64) -> bool {
        match self.list.remove(pgno) {
            Some(buf) => {
                self.pool.put(buf);
                true
            }
            None => false,
        }
    }

    /// The page numbers in ascending order.
    pub(crate) fn pgnos(&self) -> impl Iterator<Item = u64> + '_ {
        self.list.as_slice().iter().map(|(p, _)| *p)
    }

    /// `mdb_dlist_free`: removes all pages (their buffers are recycled).
    pub(crate) fn clear(&mut self) {
        let pool = &mut self.pool;
        self.list.retain(|_, buf| {
            pool.put(std::mem::take(buf));
            false
        });
    }

    /// Keeps the pages for which `keep` returns `true` (one pass); the
    /// buffers of the others are recycled.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(u64, &mut [u8]) -> bool) {
        let pool = &mut self.pool;
        self.list.retain(|pgno, buf| {
            if keep(pgno, buf) {
                return true;
            }
            pool.put(std::mem::take(buf));
            false
        });
    }

    /// Merges `other` in; its buffers replace ours for the same page (ours
    /// are recycled), and its spare buffers join ours.
    pub(crate) fn merge(&mut self, other: DirtyPages) {
        let DirtyPages { list, pool, .. } = other;
        self.pool.merge(pool);
        if list.is_empty() {
            return;
        }
        // Both lists are ascending: drop our copies of the pages `other`
        // replaces in one pass, so the merge sees no duplicates.
        let mut theirs = list.as_slice().iter().map(|(p, _)| *p).peekable();
        self.retain(|pgno, _| {
            while theirs.next_if(|&p| p < pgno).is_some() {}
            theirs.peek() != Some(&pgno)
        });
        self.list.merge(list);
    }

    /// Takes the spare buffers (for the environment to keep until the next
    /// write transaction, or for a nested transaction to use), leaving an
    /// empty pool that keeps nothing.
    pub(crate) fn take_pool(&mut self) -> PagePool {
        std::mem::take(&mut self.pool)
    }

    /// Adds spare buffers (see [`DirtyPages::take_pool`]).
    pub(crate) fn give_pool(&mut self, pool: PagePool) {
        self.pool.merge(pool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorted_and_unique() {
        let mut d = DirtyPages::new();
        assert!(d.is_empty());
        let b = d.alloc_zeroed(64);
        assert!(d.insert(9, b));
        let b = d.alloc_zeroed(128);
        assert!(d.insert(3, b));
        let b = d.alloc_zeroed(64);
        assert!(d.insert(5, b));
        let b = d.alloc_zeroed(64);
        assert!(!d.insert(5, b));
        assert_eq!(d.pgnos().collect::<Vec<_>>(), [3, 5, 9]);
        assert_eq!(d.get(3).map(<[u8]>::len), Some(128));
        d.get_mut(9).unwrap()[0] = 7;
        assert_eq!(d.get(9).unwrap()[0], 7);
        assert!(d.contains(5) && !d.contains(4));
        assert!(d.remove(5));
        assert!(!d.remove(5));
        assert_eq!(d.len(), 2);
        d.clear();
        assert!(d.get(3).is_none());
    }

    #[test]
    fn buffers_are_recycled_clean() {
        let mut d = DirtyPages::new();
        d.give_pool(PagePool::new(64));
        let mut b = d.alloc_zeroed(64);
        b.fill(0xaa);
        assert!(d.insert(1, b));
        let b = d.alloc_copy(&[0xbb; 64]);
        assert!(d.insert(2, b));
        let b = d.alloc_zeroed(192);
        assert!(d.insert(3, b));
        d.clear();
        // The single pages come back, the overflow run does not.
        assert_eq!(d.pool.free.len(), 2);
        assert_eq!(&*d.alloc_zeroed(64), &[0; 64]);
        assert_eq!(&*d.alloc_copy(&[5; 64]), &[5; 64]);
        assert_eq!(d.pool.free.len(), 0);
        // A pool without a page size keeps nothing.
        let mut pool = d.take_pool();
        let b = d.alloc_zeroed(64);
        assert!(d.insert(1, b));
        d.clear();
        assert_eq!(d.pool.free.len(), 0);
        pool.merge(d.take_pool());
        assert_eq!(pool.page_size, 64);
    }

    #[test]
    fn trim_keeps_what_was_used() {
        let mut d = DirtyPages::new();
        d.give_pool(PagePool::new(8));
        for p in 0..5 {
            let b = d.alloc_zeroed(8);
            assert!(d.insert(p, b));
        }
        d.clear();
        let mut pool = d.take_pool();
        assert_eq!(pool.free.len(), 5);
        pool.trim();
        assert_eq!(pool.free.len(), 5);
        // A smaller transaction: two buffers used, three left over.
        d.give_pool(pool);
        for p in 0..2 {
            let b = d.alloc_copy(&[1; 8]);
            assert!(d.insert(p, b));
        }
        d.clear();
        let mut pool = d.take_pool();
        assert_eq!(pool.free.len(), 5);
        pool.trim();
        assert_eq!(pool.free.len(), 2);
    }

    #[test]
    fn merge_replaces_and_recycles() {
        let mut parent = DirtyPages::new();
        parent.give_pool(PagePool::new(8));
        for p in [2, 4, 6] {
            let b = parent.alloc_copy(&[p as u8; 8]);
            assert!(parent.insert(p, b));
        }
        let mut child = DirtyPages::new();
        child.give_pool(parent.take_pool());
        for p in [1, 4, 7] {
            let b = child.alloc_copy(&[10 + p as u8; 8]);
            assert!(child.insert(p, b));
        }
        parent.merge(child);
        assert_eq!(parent.pgnos().collect::<Vec<_>>(), [1, 2, 4, 6, 7]);
        assert_eq!(parent.get(4).unwrap()[0], 14);
        assert_eq!(parent.get(2).unwrap()[0], 2);
        // The parent's old page 4 went to the pool.
        assert_eq!(parent.pool.free.len(), 1);
    }
}
