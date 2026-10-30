//! ID lists: sorted page-number lists (`MDB_IDL`) and dirty lists (`MDB_ID2L`).
//!
//! Ports of `midl.c`. LMDB keeps free page numbers in **descending** order
//! (`MDB_IDL`), both in memory (`me_pghead`, `mt_free_pgs`) and on disk, as
//! the data of free-DB records: `[count: u64, pgno_1, …, pgno_count]`, each
//! a native-endian 64-bit word (`mdb_freelist_save`). Dirty lists
//! (`MDB_ID2L`) are sorted **ascending** by page number.
//!
//! C stores the count in element 0 and uses 1-based indices. Here an [`Idl`]
//! is a `Vec<u64>` of the IDs only; [`Idl::search`] returns a 0-based index
//! with the same meaning as `mdb_midl_search` (whose result is that index + 1).
//! The behaviours that affect files are kept exactly: descending order,
//! `append_range` element order, `xmerge` and the encoding.

use crate::format::get_u64;

/// A list of page numbers sorted in descending order (`MDB_IDL`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Idl {
    ids: Vec<u64>,
}

impl Idl {
    /// An empty list.
    pub fn new() -> Idl {
        Idl { ids: Vec::new() }
    }

    /// An empty list with room for `n` IDs (`mdb_midl_alloc`).
    pub fn with_capacity(n: usize) -> Idl {
        Idl {
            ids: Vec::with_capacity(n),
        }
    }

    /// Wraps IDs without sorting them.
    pub fn from_vec(ids: Vec<u64>) -> Idl {
        Idl { ids }
    }

    /// The IDs (`ids[1..=ids[0]]` in C).
    pub fn as_slice(&self) -> &[u64] {
        &self.ids
    }

    /// The IDs as a vector.
    pub fn into_vec(self) -> Vec<u64> {
        self.ids
    }

    /// Number of IDs (`ids[0]` in C).
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// `MDB_IDL_IS_ZERO`.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Removes all IDs.
    pub fn clear(&mut self) {
        self.ids.clear();
    }

    /// Lowers the capacity to `max` IDs if it is larger (`Vec::shrink_to`):
    /// what a list kept between write transactions holds on to.
    pub fn shrink_to(&mut self, max: usize) {
        self.ids.shrink_to(max);
    }

    /// `mdb_midl_search`: the 0-based position of the first ID that is
    /// `<= id` (i.e. where `id` is or would be inserted to keep the
    /// descending order). Equal to `len()` if every ID is greater.
    pub fn search(&self, id: u64) -> usize {
        // Binary search in a descending list.
        self.ids.partition_point(|&x| x > id)
    }

    /// `true` if `id` is in the list.
    pub fn contains(&self, id: u64) -> bool {
        self.ids.get(self.search(id)) == Some(&id)
    }

    /// `mdb_midl_append`: append without keeping order (callers sort later).
    pub fn append(&mut self, id: u64) {
        self.ids.push(id);
    }

    /// `mdb_midl_append_list`: append all of `other` unsorted.
    pub fn append_list(&mut self, other: &Idl) {
        self.ids.extend_from_slice(&other.ids);
    }

    /// `mdb_midl_append_range`: append the `n` IDs `id .. id + n` in
    /// **descending** order (`id + n - 1` first, `id` last), as C does.
    pub fn append_range(&mut self, id: u64, n: usize) {
        self.ids.extend((0..n as u64).rev().map(|i| id + i));
    }

    /// `mdb_midl_xmerge`: merge the descending list `merge` into this
    /// descending list, keeping it sorted. Duplicates are kept.
    pub fn xmerge(&mut self, merge: &Idl) {
        self.xmerge_ids(merge.ids.len(), merge.ids.iter().rev().copied());
    }

    /// [`Idl::xmerge`] of the IDs of an encoded free-DB record (as
    /// [`Idl::to_bytes`] encodes them), without decoding it into a list
    /// first. Returns `None`, merging nothing, if `buf` is shorter than its
    /// count claims (as [`Idl::from_bytes`] does).
    pub fn xmerge_encoded(&mut self, buf: &[u8]) -> Option<()> {
        let ids = Idl::encoded_ids(buf)?;
        self.xmerge_ids(ids.len(), ids.rev());
        Some(())
    }

    /// The IDs of an encoded free-DB record (see [`Idl::from_bytes`]).
    pub fn encoded_ids(
        buf: &[u8],
    ) -> Option<impl DoubleEndedIterator<Item = u64> + ExactSizeIterator + Clone + '_> {
        let n = usize::try_from(get_u64(buf, 0)?).ok()?;
        let end = n.checked_add(1)?.checked_mul(8)?;
        let ids = buf.get(8..end)?;
        Some(
            ids.chunks_exact(8)
                .map(|c| u64::from_ne_bytes(c.try_into().unwrap_or([0; 8]))),
        )
    }

    /// The merge of `mdb_midl_xmerge`, in place and from the back as in C:
    /// `merge` yields the `n` IDs to merge in ascending order (the list's
    /// end first). The list grows by `n` without a new allocation if it
    /// has the room.
    fn xmerge_ids(&mut self, n: usize, mut merge: impl Iterator<Item = u64>) {
        let mut j = self.ids.len();
        self.ids.resize(j + n, 0);
        let ids = &mut self.ids[..];
        let mut k = ids.len();
        for merge_id in merge.by_ref() {
            while j > 0 && ids[j - 1] < merge_id {
                ids[k - 1] = ids[j - 1];
                k -= 1;
                j -= 1;
            }
            ids[k - 1] = merge_id;
            k -= 1;
            if k == 0 {
                break;
            }
        }
    }

    /// The run search of `mdb_page_alloc`: the 1-based C index `i` of the
    /// lowest page of `num` contiguous page numbers (`ids[i-num+1..=i]` in C
    /// terms is `pgno+num-1, ..., pgno`), preferring the tail (lowest
    /// numbers). `None` if there is no such run.
    pub fn find_run(&self, num: usize) -> Option<usize> {
        let n2 = num.checked_sub(1)?;
        let mut i = self.ids.len();
        if i <= n2 {
            return None;
        }
        loop {
            let pgno = self.ids[i - 1];
            if self.ids[i - 1 - n2] == pgno.wrapping_add(n2 as u64) {
                return Some(i);
            }
            i -= 1;
            if i <= n2 {
                return None;
            }
        }
    }

    /// Removes the run found by [`Idl::find_run`] (C index `i`, `num` IDs)
    /// and returns its lowest page number.
    pub fn remove_run(&mut self, i: usize, num: usize) -> u64 {
        let pgno = self.ids[i - 1];
        self.ids.drain(i - num..i);
        pgno
    }

    /// Inserts the `n` page numbers `pgno..pgno+n` keeping the descending
    /// order, as `mdb_ovpage_free` does for `me_pghead`.
    pub fn insert_run(&mut self, pgno: u64, n: usize) {
        let at = self.ids.partition_point(|&x| x > pgno);
        self.ids
            .splice(at..at, (0..n as u64).rev().map(|i| pgno + i));
    }

    /// `mdb_midl_sort`: sort descending. (Sorting integers has a unique
    /// result, so the algorithm need not be C's quicksort.)
    pub fn sort(&mut self) {
        self.ids.sort_unstable_by(|a, b| b.cmp(a));
    }

    /// `true` if the IDs are in strictly descending order.
    pub fn is_sorted_desc(&self) -> bool {
        self.ids.windows(2).all(|w| w[0] > w[1])
    }

    /// Encodes as stored in the free DB: `[count, ids...]`, native endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.ids.len() + 1) * 8);
        Idl::encode_append(&self.ids, &mut out);
        out
    }

    /// Appends `ids` encoded as [`Idl::to_bytes`] encodes them to `out`.
    pub fn encode_append(ids: &[u64], out: &mut Vec<u8>) {
        out.extend_from_slice(&(ids.len() as u64).to_ne_bytes());
        for id in ids {
            out.extend_from_slice(&id.to_ne_bytes());
        }
    }

    /// Writes the IDs encoded as [`Idl::to_bytes`] encodes them into
    /// `out`, which must be exactly that long (C copies the list into the
    /// space it reserved for it). `false`, with nothing written, if it is
    /// not.
    pub fn encode_into(&self, out: &mut [u8]) -> bool {
        if out.len() != (self.ids.len() + 1) * 8 {
            return false;
        }
        let (count, rest) = out.split_at_mut(8);
        count.copy_from_slice(&(self.ids.len() as u64).to_ne_bytes());
        for (dst, id) in rest.chunks_exact_mut(8).zip(&self.ids) {
            dst.copy_from_slice(&id.to_ne_bytes());
        }
        true
    }

    /// Decodes a free-DB record. Returns `None` if the buffer is shorter than
    /// its count claims.
    pub fn from_bytes(buf: &[u8]) -> Option<Idl> {
        let n = usize::try_from(get_u64(buf, 0)?).ok()?;
        let end = n.checked_add(1)?.checked_mul(8)?;
        if buf.len() < end {
            return None;
        }
        let ids = buf[8..end]
            .chunks_exact(8)
            .map(|c| u64::from_ne_bytes(c.try_into().unwrap_or([0; 8])))
            .collect();
        Some(Idl { ids })
    }

    /// The count stored at the start of an encoded free-DB record
    /// (`*(size_t *)data`, what `mdb_stat -f` sums up).
    pub fn encoded_len(buf: &[u8]) -> Option<u64> {
        get_u64(buf, 0)
    }
}

/// A list of `(pgno, value)` pairs sorted ascending by page number
/// (`MDB_ID2L`), used for dirty pages. `V` is what a page number maps to
/// (for dirty pages, the slot holding the page's buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id2List<V> {
    items: Vec<(u64, V)>,
}

impl<V> Default for Id2List<V> {
    fn default() -> Self {
        Id2List { items: Vec::new() }
    }
}

impl<V> Id2List<V> {
    /// An empty list.
    pub fn new() -> Self {
        Id2List::default()
    }

    /// Number of entries (`ids[0].mid`).
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// `true` if empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The entries in ascending pgno order.
    pub fn as_slice(&self) -> &[(u64, V)] {
        &self.items
    }

    /// `mdb_mid2l_search`: the 0-based position of the first entry whose
    /// pgno is `>= id`.
    pub fn search(&self, id: u64) -> usize {
        self.items.partition_point(|(k, _)| *k < id)
    }

    /// The value stored for `id`, if any.
    pub fn get(&self, id: u64) -> Option<&V> {
        let i = self.search(id);
        match self.items.get(i) {
            Some((k, v)) if *k == id => Some(v),
            _ => None,
        }
    }

    /// Mutable access to the value stored for `id`, if any.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut V> {
        let i = self.search(id);
        self.get_at_mut(i, id)
    }

    /// The value at position `i`, if the ID stored there is `id` (for
    /// callers that remember where they found an ID last time).
    #[inline]
    pub fn get_at(&self, i: usize, id: u64) -> Option<&V> {
        match self.items.get(i) {
            Some((k, v)) if *k == id => Some(v),
            _ => None,
        }
    }

    /// Mutable [`Id2List::get_at`].
    #[inline]
    pub fn get_at_mut(&mut self, i: usize, id: u64) -> Option<&mut V> {
        match self.items.get_mut(i) {
            Some((k, v)) if *k == id => Some(v),
            _ => None,
        }
    }

    /// `mdb_mid2l_insert`: insert keeping the order. Fails with the value
    /// handed back if `id` is already present (C returns -1).
    pub fn insert(&mut self, id: u64, value: V) -> Result<(), V> {
        let i = self.search(id);
        if matches!(self.items.get(i), Some((k, _)) if *k == id) {
            return Err(value);
        }
        self.items.insert(i, (id, value));
        Ok(())
    }

    /// `mdb_mid2l_append`: append without checking the order.
    pub fn append(&mut self, id: u64, value: V) {
        self.items.push((id, value));
    }

    /// Removes and returns the entry for `id`.
    pub fn remove(&mut self, id: u64) -> Option<V> {
        let i = self.search(id);
        match self.items.get(i) {
            Some((k, _)) if *k == id => Some(self.items.remove(i).1),
            _ => None,
        }
    }

    /// Removes all entries.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Lowers the capacity to `max` entries if it is larger
    /// (`Vec::shrink_to`).
    pub fn shrink_to(&mut self, max: usize) {
        self.items.shrink_to(max);
    }

    /// Keeps only the entries for which `keep` returns `true`, in one pass
    /// (C compacts the list in place the same way in `mdb_page_flush`).
    pub fn retain(&mut self, mut keep: impl FnMut(u64, &mut V) -> bool) {
        self.items.retain_mut(|(k, v)| keep(*k, v));
    }

    /// Merges `other` into this list in one pass; where both have an entry
    /// for a page number, `other`'s replaces this one's (the dirty-list
    /// merge of a nested commit, `mdb_txn_commit`).
    pub fn merge(&mut self, other: Id2List<V>) {
        if other.items.is_empty() {
            return;
        }
        let mine = std::mem::take(&mut self.items);
        let mut out = Vec::with_capacity(mine.len() + other.items.len());
        let mut a = mine.into_iter().peekable();
        let mut b = other.items.into_iter().peekable();
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) if x.0 < y.0 => a.next(),
                (Some(x), Some(y)) if x.0 == y.0 => {
                    a.next();
                    b.next()
                }
                (Some(_), Some(_)) | (None, Some(_)) => b.next(),
                (Some(_), None) => a.next(),
                (None, None) => break,
            };
            out.extend(next);
        }
        self.items = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal port of `mdb_midl_search` (1-based, `ids[0]` = count).
    fn c_midl_search(ids: &[u64], id: u64) -> usize {
        let (mut base, mut cursor, mut val) = (0usize, 1usize, 0i32);
        let mut n = ids[0] as usize;
        while n > 0 {
            let pivot = n >> 1;
            cursor = base + pivot + 1;
            val = match ids[cursor].cmp(&id) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            if val < 0 {
                n = pivot;
            } else if val > 0 {
                base = cursor;
                n -= pivot + 1;
            } else {
                return cursor;
            }
        }
        if val > 0 {
            cursor += 1;
        }
        cursor
    }

    #[test]
    fn search_matches_c() {
        let ids = [20u64, 15, 10, 9, 3];
        let mut c = vec![ids.len() as u64];
        c.extend_from_slice(&ids);
        let idl = Idl::from_vec(ids.to_vec());
        for id in 0..25 {
            assert_eq!(idl.search(id) + 1, c_midl_search(&c, id), "id {id}");
        }
        assert!(idl.contains(10) && !idl.contains(11));
        assert_eq!(Idl::new().search(5), 0);
    }

    #[test]
    fn append_range_is_descending() {
        let mut idl = Idl::from_vec(vec![100]);
        idl.append_range(10, 3);
        assert_eq!(idl.as_slice(), &[100, 12, 11, 10]);
        assert!(idl.is_sorted_desc());
    }

    /// The in-place merge from the back gives what a forward merge into a
    /// new list gives, for lists of every length up to 6 (with equal IDs
    /// too, as a corrupt free DB could hold), from a list or straight from
    /// an encoded record; a record shorter than its count merges nothing.
    #[test]
    fn xmerge_in_place_matches_a_forward_merge() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        for la in 0..7 {
            for lb in 0..7 {
                for _ in 0..20 {
                    let mut list = |n: usize| {
                        let mut v: Vec<u64> = (0..n)
                            .map(|_| {
                                rng ^= rng << 13;
                                rng ^= rng >> 7;
                                rng ^= rng << 17;
                                rng % 16
                            })
                            .collect();
                        v.sort_unstable_by(|a, b| b.cmp(a));
                        v
                    };
                    let (a, b) = (list(la), list(lb));
                    let mut want = a.clone();
                    want.extend_from_slice(&b);
                    want.sort_unstable_by(|x, y| y.cmp(x));
                    let mut got = Idl::from_vec(a.clone());
                    got.xmerge(&Idl::from_vec(b.clone()));
                    assert_eq!(got.as_slice(), &want[..], "{a:?} + {b:?}");
                    let mut enc = Idl::from_vec(a.clone());
                    let rec = Idl::from_vec(b.clone()).to_bytes();
                    assert_eq!(enc.xmerge_encoded(&rec), Some(()));
                    assert_eq!(enc.as_slice(), &want[..]);
                    let mut short = Idl::from_vec(a.clone());
                    if !b.is_empty() {
                        assert_eq!(short.xmerge_encoded(&rec[..rec.len() - 1]), None);
                        assert_eq!(short.as_slice(), &a[..]);
                    }
                }
            }
        }
    }

    #[test]
    fn xmerge_and_sort() {
        let mut a = Idl::from_vec(vec![50, 30, 10]);
        a.xmerge(&Idl::from_vec(vec![40, 20, 5]));
        assert_eq!(a.as_slice(), &[50, 40, 30, 20, 10, 5]);
        let mut b = Idl::from_vec(vec![3, 9, 1, 7]);
        b.sort();
        assert_eq!(b.as_slice(), &[9, 7, 3, 1]);
    }

    /// Literal port of the run search and removal in `mdb_page_alloc`.
    fn c_alloc_run(mop: &mut Vec<u64>, num: usize) -> Option<u64> {
        // mop[0] is the count, 1-based.
        let n2 = num - 1;
        let mut mop_len = mop[0] as usize;
        if mop_len <= n2 {
            return None;
        }
        let mut i = mop_len;
        let pgno;
        loop {
            let p = mop[i];
            if mop[i - n2] == p + n2 as u64 {
                pgno = p;
                break;
            }
            i -= 1;
            if i <= n2 {
                return None;
            }
        }
        mop_len -= num;
        mop[0] = mop_len as u64;
        let mut j = i - num;
        while j < mop_len {
            j += 1;
            i += 1;
            mop[j] = mop[i];
        }
        mop.truncate(mop_len + 1);
        Some(pgno)
    }

    #[test]
    fn runs_match_c() {
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..300 {
            let mut ids: Vec<u64> = Vec::new();
            let n = 1 + round % 60;
            for _ in 0..n {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ids.push(3 + (rng >> 40) % 200);
            }
            ids.sort_unstable_by(|a, b| b.cmp(a));
            ids.dedup();
            for num in 1..6 {
                let mut idl = Idl::from_vec(ids.clone());
                let mut c = vec![ids.len() as u64];
                c.extend_from_slice(&ids);
                let got = idl.find_run(num).map(|i| idl.remove_run(i, num));
                assert_eq!(got, c_alloc_run(&mut c, num), "{ids:?} num {num}");
                assert_eq!(idl.as_slice(), &c[1..]);
                if let Some(p) = got {
                    // Putting the run back restores the list.
                    idl.insert_run(p, num);
                    assert_eq!(idl.as_slice(), &ids[..]);
                }
            }
        }
        let mut idl = Idl::from_vec(vec![20, 9, 8, 7, 3]);
        assert_eq!(idl.find_run(3), Some(4));
        assert_eq!(idl.remove_run(4, 3), 7);
        assert_eq!(idl.as_slice(), &[20, 3]);
        idl.insert_run(10, 2);
        assert_eq!(idl.as_slice(), &[20, 11, 10, 3]);
        assert_eq!(Idl::new().find_run(1), None);
    }

    #[test]
    fn encoding() {
        let idl = Idl::from_vec(vec![9, 7, 3]);
        let bytes = idl.to_bytes();
        assert_eq!(bytes.len(), 32);
        assert_eq!(&bytes[..8], &3u64.to_ne_bytes());
        assert_eq!(Idl::from_bytes(&bytes), Some(idl));
        assert_eq!(Idl::encoded_len(&bytes), Some(3));
        assert_eq!(Idl::from_bytes(&bytes[..31]), None);
        let mut huge = bytes.clone();
        huge[..8].copy_from_slice(&u64::MAX.to_ne_bytes());
        assert_eq!(Idl::from_bytes(&huge), None);
    }

    #[test]
    fn encode_into_and_append_match_to_bytes() {
        for ids in [vec![], vec![5], vec![9, 7, 3]] {
            let idl = Idl::from_vec(ids.clone());
            let want = idl.to_bytes();
            let mut out = vec![0xaa; want.len()];
            assert!(idl.encode_into(&mut out));
            assert_eq!(out, want);
            let mut wrong = vec![0xaa; want.len() + 8];
            assert!(!idl.encode_into(&mut wrong));
            assert!(wrong.iter().all(|&b| b == 0xaa));
            let mut app = vec![1, 2];
            Idl::encode_append(&ids, &mut app);
            assert_eq!(&app[2..], &want[..]);
        }
    }

    #[test]
    fn id2l() {
        let mut l = Id2List::new();
        assert!(l.insert(5, "five").is_ok());
        assert!(l.insert(2, "two").is_ok());
        assert!(l.insert(9, "nine").is_ok());
        assert_eq!(l.insert(5, "again"), Err("again"));
        assert_eq!(
            l.as_slice().iter().map(|e| e.0).collect::<Vec<_>>(),
            [2, 5, 9]
        );
        assert_eq!(l.get(9), Some(&"nine"));
        assert_eq!(l.get(4), None);
        assert_eq!(l.search(6), 2);
        assert_eq!(l.remove(2), Some("two"));
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn id2l_merge_and_retain() {
        let mut a = Id2List::new();
        for (k, v) in [(1, "a1"), (4, "a4"), (6, "a6"), (9, "a9")] {
            a.append(k, v);
        }
        let mut b = Id2List::new();
        for (k, v) in [(0, "b0"), (4, "b4"), (7, "b7"), (9, "b9"), (12, "b12")] {
            b.append(k, v);
        }
        a.merge(b);
        let got: Vec<_> = a.as_slice().to_vec();
        assert_eq!(
            got,
            [
                (0, "b0"),
                (1, "a1"),
                (4, "b4"),
                (6, "a6"),
                (7, "b7"),
                (9, "b9"),
                (12, "b12")
            ]
        );
        a.retain(|k, _| k % 2 == 0);
        assert_eq!(
            a.as_slice().iter().map(|e| e.0).collect::<Vec<_>>(),
            [0, 4, 6, 12]
        );
        a.merge(Id2List::new());
        assert_eq!(a.len(), 4);
    }
}
