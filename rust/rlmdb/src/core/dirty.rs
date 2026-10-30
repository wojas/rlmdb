//! The dirty pages of a write transaction (`mt_u.dirty_list`, an `MDB_ID2L`).
//!
//! C keeps `malloc`ed page copies in a list sorted by page number and finds
//! them with `mdb_mid2l_search` in `mdb_page_get` before falling back to the
//! map. Here each dirty page (or overflow run) is an owned buffer of
//! `n * psize` bytes, and the list sorted by page number maps each page to
//! the *slot* that holds its buffer (see below). `TxnCore::page` consults
//! this list first, so every read operation sees the transaction's own
//! changes.
//!
//! Nothing hands out references into the buffers across mutations: write
//! code names pages by number (see `wcursor`), so a page being added,
//! freed or written out can never leave a dangling reference. Unlike C, new
//! buffers are always zeroed (as with C's default `!MDB_NOMEMINIT`, plus the
//! parts C leaves uninitialised), which only changes bytes no reader ever
//! looks at.
//!
//! # Slots
//!
//! The buffers live in a slab: a vector of slots that only grows while the
//! transaction runs (it is emptied with the list), with a free list of
//! empty slots. A page keeps its slot, and so its buffer, for as long as it
//! stays in the list, however many pages are added or removed meanwhile;
//! only the sorted index of `(page number, slot)` pairs, the part C
//! searches, shifts. That lets the write path remember where a page is:
//! a [`PageSlot`] reaches the buffer in O(1), without the binary search
//! (and without the parent and spill-list checks of `TxnCore::page`). Write
//! cursors keep the slot of every page on their stack next to its number,
//! and `mdb_page_touch` learns from it at once that a page is already
//! dirty.
//!
//! A slot is only ever used as a **hint**. Every access by slot
//! ([`DirtyPages::at`], [`DirtyPages::lookup`], ...) names the page number
//! it expects and checks it against the page the slot holds *now*; on a
//! mismatch (or [`PageSlot::NONE`]) the caller falls back to the search.
//! A hint that went stale therefore costs one search and never yields the
//! wrong buffer, and nothing has to be invalidated explicitly:
//!
//! * a page that leaves the list (written out by a spill or at commit,
//!   freed, or the whole list cleared when the transaction ends) empties
//!   its slot, which then fails every check until it is reused, and a
//!   reused slot holds another page number, or the same page again, which
//!   is then exactly the buffer the search would find;
//! * slots are per list. A nested transaction has its own list (see below)
//!   while its parent's waits unchanged, so a slot the parent recorded is
//!   checked against the child's list and simply misses there (or hits the
//!   child's own copy of that very page, which is what the child must
//!   use). When the child commits, the
//!   parent's pages that it replaced keep their slots in the parent's list
//!   (the child's buffer moves into the parent's slot) and its other pages
//!   get new slots in the parent's list; slots the child recorded in
//!   cursors miss there and are looked up again. When it aborts, the
//!   parent's list comes back unchanged, and so do the parent's cursors.
//!
//! Hints are refreshed where the slot is known anyway: when a page is
//! made dirty or found dirty (`mdb_page_touch`, which runs over a cursor's
//! whole stack before every change) and when a page is written through a
//! cursor.
//!
//! # Recycled buffers
//!
//! The list creates and drops every buffer itself
//! ([`DirtyPages::alloc_zeroed`], [`DirtyPages::alloc_copy`],
//! [`DirtyPages::alloc_data`]; removing, clearing, merging), so that
//! buffers can be reused instead of going back to the allocator: C keeps
//! single pages on `me_dpages` (`mdb_page_malloc`, `mdb_page_free`,
//! `mdb_dpage_free`); here overflow runs are kept too. A [`PagePool`]
//! travels with the dirty list: the environment keeps it between write
//! transactions (under the writer lock), a write transaction holds it while
//! it runs, and a nested transaction borrows it from its parent.
//! Committing a big transaction no longer frees thousands of pages for the
//! next one to allocate again; unlike C's, the pool shrinks to what the
//! last transaction used and has a cap, so a huge transaction does not pin
//! its memory forever. Recycling never shows in the data file: a reused
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
//! page numbers has the same meaning. A page is never both in a list and in
//! the spill set of the same transaction, so a page found through a slot of
//! the transaction's own list is the one `mdb_page_get` would return.

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

/// The most memory the overflow-run buffers of a [`PagePool`] take, on
/// top of its single pages.
const RUN_POOL_MAX_BYTES: usize = 128 << 20;

/// Spare page buffers: single pages (C's `me_dpages`) and overflow runs of
/// several pages. Their contents are arbitrary; whoever takes one
/// zero-fills or overwrites it completely.
///
/// Between write transactions the pool keeps as many single pages as the
/// last transaction used ([`PagePool::trim`]), up to `POOL_MAX_BYTES`: the
/// next transaction of the same kind then allocates nothing, and after a
/// big one the memory goes back to the allocator when the next, smaller
/// one ends. Overflow runs are kept the same way, per length (as many of
/// each length as the last transaction asked for), up to
/// `RUN_POOL_MAX_BYTES` in all. (C frees runs, `mdb_dpage_free`; reusing
/// them spares the allocator, and the page faults of fresh memory, a
/// large part of a put of a big value.)
#[derive(Debug, Default)]
pub(crate) struct PagePool {
    /// The page size: only buffers of a whole number of pages are kept.
    /// The default pool (0) keeps none.
    page_size: usize,
    /// How many single-page buffers it keeps at most.
    max: usize,
    /// The spare single-page buffers.
    free: Vec<Box<[u8]>>,
    /// Single-page buffers asked for since the last [`PagePool::trim`]
    /// (from the pool or not).
    used: usize,
    /// The spare run buffers by length (a workload has few lengths), each
    /// with how many runs of that length were asked for since the last
    /// [`PagePool::trim`].
    runs: Vec<RunBufs>,
    /// Bytes in `runs`.
    run_bytes: usize,
}

/// The spare overflow-run buffers of one length in a [`PagePool`].
#[derive(Debug)]
struct RunBufs {
    len: usize,
    bufs: Vec<Box<[u8]>>,
    used: usize,
}

impl PagePool {
    /// An empty pool for buffers of `page_size` bytes.
    pub(crate) fn new(page_size: usize) -> PagePool {
        PagePool {
            page_size,
            max: POOL_MAX_BYTES / page_size.max(1),
            ..PagePool::default()
        }
    }

    /// A spare buffer of `len` bytes, if there is one.
    fn take(&mut self, len: usize) -> Option<Box<[u8]>> {
        if len == self.page_size {
            self.used += 1;
            return self.free.pop();
        }
        if self.page_size == 0 || len % self.page_size != 0 {
            return None;
        }
        let i = match self.runs.iter().position(|r| r.len == len) {
            Some(i) => i,
            None => {
                self.runs.push(RunBufs {
                    len,
                    bufs: Vec::new(),
                    used: 0,
                });
                self.runs.len() - 1
            }
        };
        let r = &mut self.runs[i];
        r.used += 1;
        let buf = r.bufs.pop()?;
        self.run_bytes -= len;
        Some(buf)
    }

    /// `mdb_page_free` / `mdb_dpage_free`: keeps `buf` if it is a whole
    /// number of pages and the pool has room for it, else frees it.
    fn put(&mut self, buf: Box<[u8]>) {
        let len = buf.len();
        if len == self.page_size {
            if self.free.len() < self.max {
                self.free.push(buf);
            }
            return;
        }
        if self.page_size == 0
            || len % self.page_size != 0
            || self.run_bytes + len > RUN_POOL_MAX_BYTES
        {
            return;
        }
        self.run_bytes += len;
        match self.runs.iter_mut().find(|r| r.len == len) {
            Some(r) => r.bufs.push(buf),
            None => self.runs.push(RunBufs {
                len,
                bufs: vec![buf],
                used: 0,
            }),
        }
    }

    /// Adds the buffers of `other` (and takes its page size if this pool
    /// has none). Hand-offs between the environment and a transaction,
    /// where one side is empty, move the lists without copying them.
    pub(crate) fn merge(&mut self, mut other: PagePool) {
        if self.page_size == 0
            || other.free.len() + other.runs.len() > self.free.len() + self.runs.len()
        {
            std::mem::swap(self, &mut other);
        }
        self.used += other.used;
        for buf in other.free {
            self.put(buf);
        }
        for r in other.runs {
            for buf in r.bufs {
                self.put(buf);
            }
            match self.runs.iter_mut().find(|m| m.len == r.len) {
                Some(m) => m.used += r.used,
                None if r.used > 0 => self.runs.push(RunBufs {
                    len: r.len,
                    bufs: Vec::new(),
                    used: r.used,
                }),
                None => {}
            }
        }
    }

    /// At the end of a write transaction: keep no more spare buffers than
    /// it used (see [`PagePool`]), and start counting again.
    pub(crate) fn trim(&mut self) {
        self.free.truncate(self.used);
        self.used = 0;
        for r in &mut self.runs {
            if r.bufs.len() > r.used {
                self.run_bytes -= (r.bufs.len() - r.used) * r.len;
                r.bufs.truncate(r.used);
            }
            r.used = 0;
        }
        self.runs.retain(|r| !r.bufs.is_empty());
    }
}

/// Where a dirty page's buffer is in its [`DirtyPages`] list: an index into
/// the list's slab. Only a hint (see the module documentation): accesses by
/// slot also name the page they expect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageSlot(u32);

impl PageSlot {
    /// No slot known: an access through it always falls back to the search.
    pub(crate) const NONE: PageSlot = PageSlot(u32::MAX);
}

impl Default for PageSlot {
    fn default() -> PageSlot {
        PageSlot::NONE
    }
}

/// One slot of the slab: a dirty page's number and buffer, or nothing.
#[derive(Debug, Default)]
struct Entry {
    pgno: u64,
    buf: Option<Box<[u8]>>,
}

/// The dirty-page list of a write transaction.
#[derive(Debug, Default)]
pub(crate) struct DirtyPages {
    /// `mt_u.dirty_list`: page number to slot, ascending by page number.
    index: Id2List<PageSlot>,
    /// The buffers, by slot.
    slots: Vec<Entry>,
    /// Empty slots, reused before the slab grows.
    vacant: Vec<PageSlot>,
    /// Where pages were last found, by `pgno % HINTS`. Checked like any
    /// slot; lookups by number go to the same few pages (root, branch,
    /// leaf) many times per operation, and this spares most searches.
    hint: [Cell<PageSlot>; HINTS],
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
        self.index.len()
    }

    /// `true` if there are no dirty pages.
    pub(crate) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The buffer of `slot` if it holds page `pgno` (the whole run for
    /// overflow pages), without a search.
    #[inline]
    pub(crate) fn at(&self, slot: PageSlot, pgno: u64) -> Option<&[u8]> {
        match self.slots.get(slot.0 as usize) {
            Some(Entry {
                pgno: p,
                buf: Some(buf),
            }) if *p == pgno => Some(buf),
            _ => None,
        }
    }

    /// The slot of dirty page `pgno` by binary search (`mdb_mid2l_search`),
    /// remembered for the next lookup of a page with the same hint index.
    fn search(&self, pgno: u64) -> Option<PageSlot> {
        let slot = *self.index.get_at(self.index.search(pgno), pgno)?;
        self.hint[pgno as usize % HINTS].set(slot);
        Some(slot)
    }

    /// The slot of dirty page `pgno`.
    #[inline]
    pub(crate) fn find(&self, pgno: u64) -> Option<PageSlot> {
        let hint = self.hint[pgno as usize % HINTS].get();
        if self.at(hint, pgno).is_some() {
            return Some(hint);
        }
        self.search(pgno)
    }

    /// The slot and buffer of dirty page `pgno`: through `hint` if it
    /// still holds the page, else as [`DirtyPages::find`] finds it.
    #[inline]
    pub(crate) fn lookup(&self, hint: PageSlot, pgno: u64) -> Option<(PageSlot, &[u8])> {
        if let Some(buf) = self.at(hint, pgno) {
            return Some((hint, buf));
        }
        let slot = self.find(pgno)?;
        Some((slot, self.at(slot, pgno)?))
    }

    /// The buffer of dirty page `pgno` (the whole run for overflow pages).
    #[inline]
    pub(crate) fn get(&self, pgno: u64) -> Option<&[u8]> {
        if let Some(buf) = self.at(self.hint[pgno as usize % HINTS].get(), pgno) {
            return Some(buf);
        }
        self.at(self.search(pgno)?, pgno)
    }

    /// Mutable [`DirtyPages::at`].
    #[inline]
    pub(crate) fn at_mut(&mut self, slot: PageSlot, pgno: u64) -> Option<&mut [u8]> {
        match self.slots.get_mut(slot.0 as usize) {
            Some(Entry {
                pgno: p,
                buf: Some(buf),
            }) if *p == pgno => Some(buf),
            _ => None,
        }
    }

    /// The slot of dirty page `pgno`: `hint` if it still holds the page,
    /// else by search.
    #[inline]
    pub(crate) fn resolve(&self, hint: PageSlot, pgno: u64) -> Option<PageSlot> {
        if self.at(hint, pgno).is_some() {
            return Some(hint);
        }
        self.find(pgno)
    }

    /// Mutable access to the buffer of dirty page `pgno`.
    #[inline]
    pub(crate) fn get_mut(&mut self, pgno: u64) -> Option<&mut [u8]> {
        let slot = self.find(pgno)?;
        self.at_mut(slot, pgno)
    }

    /// `true` if `pgno` is dirty.
    pub(crate) fn contains(&self, pgno: u64) -> bool {
        self.find(pgno).is_some()
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

    /// A buffer of `len` bytes holding `data` at offset `at` and zeros
    /// everywhere else (a new overflow run), recycled if possible; each
    /// byte is written once. Hand it to [`DirtyPages::insert`]. `None` if
    /// `data` does not fit.
    pub(crate) fn alloc_data(&mut self, len: usize, at: usize, data: &[u8]) -> Option<Box<[u8]>> {
        let end = at.checked_add(data.len()).filter(|&end| end <= len)?;
        Some(match self.pool.take(len) {
            Some(mut buf) => {
                buf[..at].fill(0);
                buf[at..end].copy_from_slice(data);
                buf[end..].fill(0);
                buf
            }
            None => {
                let mut buf = Vec::with_capacity(len);
                buf.resize(at, 0);
                buf.extend_from_slice(data);
                buf.resize(len, 0);
                buf.into_boxed_slice()
            }
        })
    }

    /// Puts `buf` into an empty slot for page `pgno` (not yet indexed).
    fn occupy(&mut self, pgno: u64, buf: Box<[u8]>) -> PageSlot {
        let entry = Entry {
            pgno,
            buf: Some(buf),
        };
        match self.vacant.pop() {
            Some(slot) => {
                self.slots[slot.0 as usize] = entry;
                slot
            }
            None => {
                // A dirty list holds at most `MDB_IDL_UM_MAX` pages, far
                // below `u32::MAX`.
                let slot = PageSlot(self.slots.len() as u32);
                self.slots.push(entry);
                slot
            }
        }
    }

    /// `mdb_page_dirty`: add a page buffer and return its slot. Returns
    /// `None` (and recycles the buffer) if the page is already dirty, which
    /// would be a bug.
    pub(crate) fn insert(&mut self, pgno: u64, buf: Box<[u8]>) -> Option<PageSlot> {
        let slot = self.occupy(pgno, buf);
        if self.index.insert(pgno, slot).is_err() {
            vacate(&mut self.slots, &mut self.vacant, &mut self.pool, slot);
            return None;
        }
        self.hint[pgno as usize % HINTS].set(slot);
        Some(slot)
    }

    /// Removes a page from the list (its buffer is recycled). Returns
    /// `false` if it was not there.
    pub(crate) fn remove(&mut self, pgno: u64) -> bool {
        match self.index.remove(pgno) {
            Some(slot) => {
                vacate(&mut self.slots, &mut self.vacant, &mut self.pool, slot);
                true
            }
            None => false,
        }
    }

    /// The page numbers in ascending order.
    pub(crate) fn pgnos(&self) -> impl Iterator<Item = u64> + '_ {
        self.index.as_slice().iter().map(|(p, _)| *p)
    }

    /// `mdb_dlist_free`: removes all pages (their buffers are recycled).
    pub(crate) fn clear(&mut self) {
        for (_, slot) in self.index.as_slice() {
            if let Some(buf) = self.slots[slot.0 as usize].buf.take() {
                self.pool.put(buf);
            }
        }
        self.index.clear();
        self.slots.clear();
        self.vacant.clear();
    }

    /// Calls `f` with every page (number, slot, buffer), in page order.
    pub(crate) fn for_each_mut(&mut self, mut f: impl FnMut(u64, PageSlot, &mut [u8])) {
        let DirtyPages { index, slots, .. } = self;
        for &(pgno, slot) in index.as_slice() {
            if let Some(buf) = slots[slot.0 as usize].buf.as_deref_mut() {
                f(pgno, slot, buf);
            }
        }
    }

    /// Empties the list for the next write transaction (see
    /// `txn_rw::SpareTxn`): the pages are recycled, the pool is trimmed to
    /// what this transaction used ([`PagePool::trim`]), and the list's own
    /// vectors keep room for at most `max` pages.
    pub(crate) fn recycle(&mut self, max: usize) {
        self.clear();
        self.pool.trim();
        self.index.shrink_to(max);
        self.slots.shrink_to(max);
        self.vacant.shrink_to(max);
    }

    /// Keeps the pages for which `keep` returns `true` (one pass, in page
    /// order); the buffers of the others are recycled.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(u64, &mut [u8]) -> bool) {
        let DirtyPages {
            index,
            slots,
            vacant,
            pool,
            ..
        } = self;
        index.retain(|pgno, slot| {
            let Some(buf) = slots[slot.0 as usize].buf.as_deref_mut() else {
                return false;
            };
            if keep(pgno, buf) {
                return true;
            }
            vacate(slots, vacant, pool, *slot);
            false
        });
    }

    /// Merges `other` (a committing child's list) in: its buffers replace
    /// ours for the same page, in the slot that page has here (ours are
    /// recycled), its other pages get slots here, and its spare buffers
    /// join ours.
    pub(crate) fn merge(&mut self, other: DirtyPages) {
        let DirtyPages {
            index: theirs,
            slots: mut their_slots,
            pool,
            ..
        } = other;
        self.pool.merge(pool);
        if theirs.is_empty() {
            return;
        }
        let mine = std::mem::take(&mut self.index);
        let mut merged = Id2List::new();
        let mut a = mine.as_slice().iter().copied().peekable();
        for &(pgno, slot) in theirs.as_slice() {
            while let Some((p, s)) = a.next_if(|&(p, _)| p < pgno) {
                merged.append(p, s);
            }
            let Some(buf) = their_slots[slot.0 as usize].buf.take() else {
                continue;
            };
            let slot = match a.next_if(|&(p, _)| p == pgno) {
                Some((_, s)) => {
                    let old = self.slots[s.0 as usize].buf.replace(buf);
                    if let Some(old) = old {
                        self.pool.put(old);
                    }
                    s
                }
                None => self.occupy(pgno, buf),
            };
            merged.append(pgno, slot);
        }
        for (p, s) in a {
            merged.append(p, s);
        }
        self.index = merged;
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

/// Empties `slot` of a slab, recycling its buffer.
fn vacate(slots: &mut [Entry], vacant: &mut Vec<PageSlot>, pool: &mut PagePool, slot: PageSlot) {
    if let Some(buf) = slots[slot.0 as usize].buf.take() {
        pool.put(buf);
        vacant.push(slot);
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
        assert!(d.insert(9, b).is_some());
        let b = d.alloc_zeroed(128);
        assert!(d.insert(3, b).is_some());
        let b = d.alloc_zeroed(64);
        assert!(d.insert(5, b).is_some());
        let b = d.alloc_zeroed(64);
        assert!(d.insert(5, b).is_none());
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
        assert!(d.insert(1, b).is_some());
        let b = d.alloc_copy(&[0xbb; 64]);
        assert!(d.insert(2, b).is_some());
        let mut b = d.alloc_zeroed(192);
        b.fill(0xcc);
        assert!(d.insert(3, b).is_some());
        let mut b = d.alloc_zeroed(192);
        b.fill(0xdd);
        assert!(d.insert(4, b).is_some());
        // Not a whole number of pages: not kept.
        assert!(d.insert(5, vec![0xee; 100].into_boxed_slice()).is_some());
        d.clear();
        // The single pages and the overflow runs come back.
        assert_eq!(d.pool.free.len(), 2);
        assert_eq!(d.pool.run_bytes, 2 * 192);
        assert_eq!(&*d.alloc_zeroed(64), &[0; 64]);
        assert_eq!(&*d.alloc_copy(&[5; 64]), &[5; 64]);
        assert_eq!(d.pool.free.len(), 0);
        // A recycled run is zero-filled, or holds the data and zeros only.
        assert_eq!(&*d.alloc_zeroed(192), &[0; 192]);
        let run = d.alloc_data(192, 16, &[7; 100]).unwrap();
        assert!(run[..16].iter().all(|&b| b == 0));
        assert!(run[16..116].iter().all(|&b| b == 7));
        assert!(run[116..].iter().all(|&b| b == 0));
        assert_eq!(d.pool.run_bytes, 0);
        // A fresh one too, and data that does not fit is refused.
        let run = d.alloc_data(128, 16, &[9; 112]).unwrap();
        assert!(run[..16].iter().all(|&b| b == 0) && run[16..].iter().all(|&b| b == 9));
        assert!(d.alloc_data(128, 16, &[9; 113]).is_none());
        // A pool without a page size keeps nothing.
        let mut pool = d.take_pool();
        let b = d.alloc_zeroed(64);
        assert!(d.insert(1, b).is_some());
        d.clear();
        assert_eq!(d.pool.free.len(), 0);
        pool.merge(d.take_pool());
        assert_eq!(pool.page_size, 64);
    }

    #[test]
    fn runs_are_kept_as_used_and_within_budget() {
        let mut pool = PagePool::new(64);
        for _ in 0..4 {
            pool.put(vec![0; 192].into_boxed_slice());
        }
        pool.put(vec![0; 128].into_boxed_slice());
        assert_eq!(pool.run_bytes, 4 * 192 + 128);
        // The last transaction used one 192-byte run: that is what stays.
        assert!(pool.take(192).is_some());
        pool.trim();
        assert_eq!(pool.run_bytes, 192);
        assert!(pool.take(128).is_none());
        assert!(pool.take(192).is_some());
        assert_eq!(pool.run_bytes, 0);
        // Beyond the budget, runs are freed.
        let big = RUN_POOL_MAX_BYTES / 64 * 64;
        pool.put(vec![0; big].into_boxed_slice());
        pool.put(vec![0; 64 * 3].into_boxed_slice());
        assert_eq!(pool.run_bytes, big);
    }

    #[test]
    fn trim_keeps_what_was_used() {
        let mut d = DirtyPages::new();
        d.give_pool(PagePool::new(8));
        for p in 0..5 {
            let b = d.alloc_zeroed(8);
            assert!(d.insert(p, b).is_some());
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
            assert!(d.insert(p, b).is_some());
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
            assert!(parent.insert(p, b).is_some());
        }
        let mut child = DirtyPages::new();
        child.give_pool(parent.take_pool());
        for p in [1, 4, 7] {
            let b = child.alloc_copy(&[10 + p as u8; 8]);
            assert!(child.insert(p, b).is_some());
        }
        parent.merge(child);
        assert_eq!(parent.pgnos().collect::<Vec<_>>(), [1, 2, 4, 6, 7]);
        assert_eq!(parent.get(4).unwrap()[0], 14);
        assert_eq!(parent.get(2).unwrap()[0], 2);
        // The parent's old page 4 went to the pool.
        assert_eq!(parent.pool.free.len(), 1);
    }

    /// A page keeps its slot while others come and go; a slot that no
    /// longer holds the page it was recorded for never matches.
    #[test]
    fn slots_are_stable_hints() {
        let mut d = DirtyPages::new();
        let slot = |d: &mut DirtyPages, p: u64| {
            let b = d.alloc_copy(&[p as u8; 32]);
            d.insert(p, b).unwrap()
        };
        let s7 = slot(&mut d, 7);
        let s3 = slot(&mut d, 3);
        let s9 = slot(&mut d, 9);
        assert_eq!(d.at(s7, 7).unwrap()[0], 7);
        assert!(d.at(s7, 3).is_none() && d.at(PageSlot::NONE, 7).is_none());
        // Inserting below and removing others moves the index, not slots.
        for p in [1, 2, 4, 5, 6] {
            slot(&mut d, p);
        }
        assert!(d.remove(3) && d.remove(5));
        assert_eq!(d.at(s7, 7).unwrap()[0], 7);
        assert_eq!(d.at(s9, 9).unwrap()[0], 9);
        assert!(d.at(s3, 3).is_none());
        assert_eq!(d.lookup(s3, 7).map(|(s, b)| (s, b[0])), Some((s7, 7)));
        assert!(d.lookup(s7, 3).is_none());
        // An emptied slot is reused: for another page it does not match
        // the old one, for the same page again it is just right.
        let s5 = slot(&mut d, 5);
        assert!(d.remove(5));
        let s10 = slot(&mut d, 10);
        assert_eq!(s10, s5, "the last emptied slot is reused first");
        assert!(d.at(s5, 5).is_none());
        assert_eq!(d.at(s10, 10).unwrap()[0], 10);
        assert!(d.remove(10));
        let s5b = slot(&mut d, 5);
        assert_eq!(s5b, s5);
        assert_eq!(d.at(s5, 5).unwrap()[0], 5);
        assert!(d.remove(5));
        d.at_mut(s7, 7).unwrap()[1] = 70;
        assert_eq!(d.get(7).unwrap()[1], 70);
        assert_eq!(d.resolve(PageSlot::NONE, 9), Some(s9));
        // Empty slots match no page number, not even P_INVALID.
        assert!(d.get(u64::MAX).is_none() && d.find(u64::MAX).is_none());
        assert_eq!(d.pgnos().collect::<Vec<_>>(), [1, 2, 4, 6, 7, 9]);
        d.retain(|p, _| p != 9);
        assert!(d.at(s9, 9).is_none() && d.find(9).is_none());
        d.clear();
        assert!(d.at(s7, 7).is_none() && d.find(7).is_none());
    }

    /// A child's commit: the parent's pages that the child replaced keep
    /// their slots (holding the child's buffers), the child's new pages
    /// get slots of the parent's list.
    #[test]
    fn merge_keeps_the_parents_slots() {
        let mut parent = DirtyPages::new();
        let mut slots = Vec::new();
        for p in [2u64, 4, 6] {
            let b = parent.alloc_copy(&[p as u8; 8]);
            slots.push(parent.insert(p, b).unwrap());
        }
        let mut child = DirtyPages::new();
        let mut child_slots = Vec::new();
        for p in [1u64, 4, 7, 8] {
            let b = child.alloc_copy(&[10 + p as u8; 8]);
            child_slots.push(child.insert(p, b).unwrap());
        }
        parent.merge(child);
        assert_eq!(parent.pgnos().collect::<Vec<_>>(), [1, 2, 4, 6, 7, 8]);
        assert_eq!(parent.at(slots[0], 2).unwrap()[0], 2);
        assert_eq!(parent.at(slots[1], 4).unwrap()[0], 14);
        assert_eq!(parent.at(slots[2], 6).unwrap()[0], 6);
        for p in [1u64, 7, 8] {
            let (s, b) = parent.lookup(PageSlot::NONE, p).unwrap();
            assert_eq!(b[0], 10 + p as u8);
            assert_eq!(parent.at(s, p).unwrap()[0], 10 + p as u8);
        }
        // Slots the child handed out are only hints in the parent's list.
        for (&s, p) in child_slots.iter().zip([1u64, 4, 7, 8]) {
            if let Some(b) = parent.at(s, p) {
                assert_eq!(b[0], 10 + p as u8);
            }
        }
    }
}
