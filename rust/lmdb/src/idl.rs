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
        // C merges in place from the back; a forward merge yields the same
        // sequence for sorted inputs.
        let mut out = Vec::with_capacity(self.ids.len() + merge.ids.len());
        let (mut i, mut j) = (0, 0);
        let (a, b) = (&self.ids, &merge.ids);
        while i < a.len() && j < b.len() {
            if a[i] > b[j] {
                out.push(a[i]);
                i += 1;
            } else {
                out.push(b[j]);
                j += 1;
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        self.ids = out;
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
            if self.ids[i - 1 - n2] == pgno + n2 as u64 {
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
        out.extend_from_slice(&(self.ids.len() as u64).to_ne_bytes());
        for id in &self.ids {
            out.extend_from_slice(&id.to_ne_bytes());
        }
        out
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
/// (`MDB_ID2L`), used for dirty pages. `V` is the page buffer type.
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
}
