//! In-place edits of a page buffer: the byte-shuffling halves of
//! `mdb_node_add`, `mdb_node_del`, `mdb_node_shrink` and `mdb_update_key`.
//!
//! Everything here works on one `&mut [u8]` that is exactly a page (or a
//! `P_SUBP` sub-page inside a leaf node: offsets in a sub-page are relative
//! to its start, so the same code applies). The B-tree logic around these
//! edits (choosing the node size, allocating overflow pages, splitting when
//! a page is full, fixing up cursors) lives in the write-transaction
//! modules.
//!
//! The functions mirror the C byte layout exactly, because C readers parse
//! these pages. They return `None` when the page is malformed (an offset
//! or count that points outside the buffer) instead of panicking; callers
//! turn that into `MDB_CORRUPTED`.

use crate::format::layout::*;
use crate::format::node::F_BIGDATA;
use crate::format::page::{P_LEAF, P_LEAF2};
use crate::format::{get_u16, put_u16, put_u64};

/// `EVEN(n)`: round up to an even number.
#[inline]
pub(crate) fn even(n: usize) -> usize {
    (n + 1) & !1
}

#[inline]
fn hdr(p: &[u8], off: usize) -> u16 {
    get_u16(p, off).unwrap_or(0)
}

/// `mp_lower`.
#[inline]
pub(crate) fn lower(p: &[u8]) -> usize {
    usize::from(hdr(p, PAGE_LOWER))
}

/// `mp_upper`.
#[inline]
pub(crate) fn upper(p: &[u8]) -> usize {
    usize::from(hdr(p, PAGE_UPPER))
}

/// `mp_flags`.
#[inline]
pub(crate) fn flags(p: &[u8]) -> u16 {
    hdr(p, PAGE_FLAGS)
}

/// Sets `mp_lower`.
#[inline]
pub(crate) fn set_lower(p: &mut [u8], v: usize) {
    put_u16(p, PAGE_LOWER, v as u16);
}

/// Sets `mp_upper`.
#[inline]
pub(crate) fn set_upper(p: &mut [u8], v: usize) {
    put_u16(p, PAGE_UPPER, v as u16);
}

/// Sets `mp_flags`.
#[inline]
pub(crate) fn set_flags(p: &mut [u8], v: u16) {
    put_u16(p, PAGE_FLAGS, v);
}

/// Sets `mp_pgno`.
#[inline]
pub(crate) fn set_pgno(p: &mut [u8], pgno: u64) {
    put_u64(p, PAGE_PGNO, pgno);
}

/// `NUMKEYS`.
#[inline]
pub(crate) fn num_keys(p: &[u8]) -> usize {
    lower(p).saturating_sub(PAGE_HDR_SIZE) >> 1
}

/// `SIZELEFT`.
#[inline]
pub(crate) fn size_left(p: &[u8]) -> usize {
    upper(p).saturating_sub(lower(p))
}

/// `mp_ptrs[i]`.
#[inline]
pub(crate) fn ptr(p: &[u8], i: usize) -> Option<usize> {
    get_u16(p, PAGE_PTRS + 2 * i).map(usize::from)
}

#[inline]
fn set_ptr(p: &mut [u8], i: usize, v: usize) -> Option<()> {
    let off = PAGE_PTRS + 2 * i;
    p.get_mut(off..off + 2)?
        .copy_from_slice(&(v as u16).to_ne_bytes());
    Some(())
}

/// Header fields of the node at offset `off`: `(ksize, flags, dsize)`.
#[inline]
fn node_hdr(p: &[u8], off: usize) -> Option<(usize, u16, usize)> {
    let lo = get_u16(p, off.checked_add(NODE_LO)?)?;
    let hi = get_u16(p, off + NODE_HI)?;
    let flags = get_u16(p, off + NODE_FLAGS)?;
    let ksize = get_u16(p, off + NODE_KSIZE)?;
    Some((
        usize::from(ksize),
        flags,
        (u32::from(lo) | (u32::from(hi) << 16)) as usize,
    ))
}

/// The byte range of the inline data of leaf node `i` (`NODEDATA`,
/// `NODEDSZ` bytes).
pub(crate) fn node_data_range(p: &[u8], i: usize) -> Option<std::ops::Range<usize>> {
    if i >= num_keys(p) {
        return None;
    }
    let off = ptr(p, i)?;
    let (ksize, _, dsize) = node_hdr(p, off)?;
    let start = off.checked_add(NODE_DATA + ksize)?;
    let end = start.checked_add(dsize)?;
    (end <= p.len()).then_some(start..end)
}

/// `SETDSZ`: sets the data size of leaf node `i`.
pub(crate) fn set_node_dsize(p: &mut [u8], i: usize, size: usize) -> Option<()> {
    let off = ptr(p, i)?;
    node_hdr(p, off)?;
    put_u16(p, off + NODE_LO, size as u16);
    put_u16(p, off + NODE_HI, (size >> 16) as u16);
    Some(())
}

/// `SETPGNO`: points branch node `i` at `pgno`.
pub(crate) fn set_node_pgno(p: &mut [u8], i: usize, pgno: u64) -> Option<()> {
    if i >= num_keys(p) {
        return None;
    }
    let off = ptr(p, i)?;
    node_hdr(p, off)?;
    put_u16(p, off + NODE_LO, pgno as u16);
    put_u16(p, off + NODE_HI, (pgno >> 16) as u16);
    put_u16(p, off + NODE_FLAGS, (pgno >> 32) as u16);
    Some(())
}

/// What goes into a new node besides its key.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NodeBody<'a> {
    /// A branch node pointing at a child (`SETPGNO`).
    Branch(u64),
    /// A leaf node with `size` bytes of data stored inline, copied from the
    /// slice (which must be `size` long).
    Inline(&'a [u8]),
    /// A leaf node whose `size` inline bytes are left as they are
    /// (`MDB_RESERVE`).
    Reserve(usize),
    /// An `F_BIGDATA` leaf node: data of `size` bytes on the overflow run at
    /// `pgno`.
    Overflow { pgno: u64, size: usize },
}

/// The `update:` part of `mdb_node_add`: make room in the pointer array at
/// `indx`, carve `node_size` bytes (already `EVEN`) off the top of the free
/// space and write the node header, key and body there. The caller has
/// checked that the node fits. Returns the node's offset in the page.
pub(crate) fn insert_node(
    p: &mut [u8],
    indx: usize,
    node_size: usize,
    key: &[u8],
    flags: u16,
    body: NodeBody<'_>,
) -> Option<usize> {
    let nkeys = num_keys(p);
    if indx > nkeys {
        return None;
    }
    // Move higher pointers up one slot.
    for i in (indx..nkeys).rev() {
        let v = ptr(p, i)?;
        set_ptr(p, i + 1, v)?;
    }
    // Adjust free space offsets.
    let ofs = upper(p).checked_sub(node_size)?;
    if ofs < lower(p) + 2 || ofs + node_size > p.len() {
        return None;
    }
    set_ptr(p, indx, ofs)?;
    set_upper(p, ofs);
    set_lower(p, lower(p) + 2);

    // Write the node.
    put_u16(p, ofs + NODE_KSIZE, key.len() as u16);
    let (word, flags) = match body {
        NodeBody::Branch(pgno) => (pgno as u32, (pgno >> 32) as u16),
        NodeBody::Inline(d) => (d.len() as u32, flags),
        NodeBody::Reserve(size) => (size as u32, flags),
        NodeBody::Overflow { size, .. } => (size as u32, flags | F_BIGDATA),
    };
    put_u16(p, ofs + NODE_LO, word as u16);
    put_u16(p, ofs + NODE_HI, (word >> 16) as u16);
    put_u16(p, ofs + NODE_FLAGS, flags);
    let k = ofs + NODE_DATA;
    p.get_mut(k..k + key.len())?.copy_from_slice(key);
    let d = k + key.len();
    match body {
        NodeBody::Branch(_) | NodeBody::Reserve(_) => {}
        NodeBody::Inline(bytes) => p.get_mut(d..d + bytes.len())?.copy_from_slice(bytes),
        NodeBody::Overflow { pgno, .. } => {
            p.get_mut(d..d + 8)?.copy_from_slice(&pgno.to_ne_bytes())
        }
    }
    Some(ofs)
}

/// The `IS_LEAF2` branch of `mdb_node_add`: insert a `ksize`-byte key at
/// `indx`, shifting the following keys up. `key` must be `ksize` long.
pub(crate) fn insert_leaf2(p: &mut [u8], indx: usize, key: &[u8], ksize: usize) -> Option<()> {
    let nkeys = num_keys(p);
    if indx > nkeys || key.len() != ksize {
        return None;
    }
    let start = PAGE_HDR_SIZE + indx * ksize;
    let end = PAGE_HDR_SIZE + nkeys * ksize;
    if end + ksize > p.len() {
        return None;
    }
    p.copy_within(start..end, start + ksize);
    p[start..start + ksize].copy_from_slice(key);
    // Just using these for counting.
    set_lower(p, lower(p) + 2);
    set_upper(p, upper(p).checked_sub(ksize.checked_sub(2)?)?);
    Some(())
}

/// The on-page size of node `i` (header, key and inline data or overflow
/// pgno), `EVEN`, as `mdb_node_del` computes it.
fn node_size(p: &[u8], off: usize, is_leaf: bool) -> Option<usize> {
    let (ksize, nflags, dsize) = node_hdr(p, off)?;
    let mut sz = NODE_SIZE + ksize;
    if is_leaf {
        sz += if nflags & F_BIGDATA != 0 { 8 } else { dsize };
    }
    Some(even(sz))
}

/// `mdb_node_del` (mdb.c line 7602): delete node `indx`, closing the gap in
/// the pointer array and in the node area. `ksize` is the key size of a
/// `P_LEAF2` page.
pub(crate) fn delete_node(p: &mut [u8], indx: usize, ksize: usize) -> Option<()> {
    let numkeys = num_keys(p);
    if indx >= numkeys {
        return None;
    }
    let pflags = flags(p);
    if pflags & P_LEAF2 != 0 {
        let x = numkeys - 1 - indx;
        let base = PAGE_HDR_SIZE + indx * ksize;
        let end = base + ksize + x * ksize;
        if end > p.len() {
            return None;
        }
        p.copy_within(base + ksize..end, base);
        set_lower(p, lower(p) - 2);
        set_upper(p, upper(p) + ksize.checked_sub(2)?);
        return Some(());
    }

    let node_ofs = ptr(p, indx)?;
    let sz = node_size(p, node_ofs, pflags & P_LEAF != 0)?;
    let mut j = 0;
    for i in 0..numkeys {
        if i != indx {
            let mut v = ptr(p, i)?;
            if v < node_ofs {
                v += sz;
            }
            set_ptr(p, j, v)?;
            j += 1;
        }
    }
    let base = upper(p);
    if base > node_ofs || node_ofs + sz > p.len() {
        return None;
    }
    p.copy_within(base..node_ofs, base + sz);
    set_lower(p, lower(p) - 2);
    set_upper(p, base + sz);
    Some(())
}

/// `mdb_node_shrink` (mdb.c line 7658): give the free space of the
/// sub-page in leaf node `indx` back to the main page `p`.
pub(crate) fn shrink_node(p: &mut [u8], indx: usize, page_pgno: u64) -> Option<()> {
    let node_ofs = ptr(p, indx)?;
    let (ksize, _, dsize) = node_hdr(p, node_ofs)?;
    let sp = node_ofs + NODE_DATA + ksize;
    let sub = p.get(sp..sp.checked_add(dsize)?)?;
    let delta = size_left(sub);
    let nsize = dsize.checked_sub(delta)?;
    let sub_keys = num_keys(sub);
    let sub_leaf2 = flags(sub) & P_LEAF2 != 0;

    // Prepare to shift upward, set len = length(subpage part to shift).
    let len = if sub_leaf2 {
        if nsize & 1 != 0 {
            return Some(()); // do not make the node uneven-sized
        }
        nsize
    } else {
        // The pointer array moves up by `delta` (into the destination
        // sub-page); its entries shrink by `delta` because the sub-page
        // now starts `delta` bytes later.
        let xp = sp + delta;
        for i in (0..sub_keys).rev() {
            let v = ptr(&p[sp..], i)?.checked_sub(delta)?;
            set_ptr(&mut p[xp..], i, v)?;
        }
        PAGE_HDR_SIZE
    };
    let sub_lower = lower(&p[sp..]);
    set_upper(&mut p[sp..], sub_lower);
    set_pgno(&mut p[sp..], page_pgno);
    set_node_dsize(p, indx, nsize)?;

    // Shift <lower nodes...initial part of subpage> upward.
    let base = upper(p);
    let end = sp + len;
    if base > end || end + delta > p.len() {
        return None;
    }
    p.copy_within(base..end, base + delta);
    for i in 0..num_keys(p) {
        let v = ptr(p, i)?;
        if v <= node_ofs {
            set_ptr(p, i, v + delta)?;
        }
    }
    set_upper(p, base + delta);
    Some(())
}

/// Outcome of [`update_key`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyUpdate {
    /// The key was replaced in place.
    Done,
    /// The page has no room for the longer key: the caller must delete the
    /// node and re-insert it with a split (`mdb_update_key`'s fallback).
    NeedsSplit,
}

/// The in-place part of `mdb_update_key` (mdb.c line 7962): replace the key
/// of branch node `indx`, shifting the node area when `EVEN(key length)`
/// changes.
pub(crate) fn update_key(p: &mut [u8], indx: usize, key: &[u8]) -> Option<KeyUpdate> {
    let mut node_ofs = ptr(p, indx)?;
    let (oksize, _, _) = node_hdr(p, node_ofs)?;
    let ksize = even(key.len());
    let oksize_even = even(oksize);
    if ksize != oksize_even {
        if ksize > oksize_even && size_left(p) < ksize - oksize_even {
            return Some(KeyUpdate::NeedsSplit);
        }
        let numkeys = num_keys(p);
        let base = upper(p);
        let len = node_ofs.checked_sub(base)? + NODE_SIZE;
        if ksize > oksize_even {
            // Grow: everything from `upper` to this node's header moves down.
            let delta = ksize - oksize_even;
            for i in 0..numkeys {
                let v = ptr(p, i)?;
                if v <= node_ofs {
                    set_ptr(p, i, v.checked_sub(delta)?)?;
                }
            }
            let dst = base.checked_sub(delta)?;
            p.copy_within(base..base + len, dst);
            set_upper(p, dst);
            node_ofs -= delta;
        } else {
            // Shrink: it moves up.
            let delta = oksize_even - ksize;
            for i in 0..numkeys {
                let v = ptr(p, i)?;
                if v <= node_ofs {
                    set_ptr(p, i, v + delta)?;
                }
            }
            if base + len + delta > p.len() {
                return None;
            }
            p.copy_within(base..base + len, base + delta);
            set_upper(p, base + delta);
            node_ofs += delta;
        }
    }
    // But even if no shift was needed, update ksize.
    put_u16(p, node_ofs + NODE_KSIZE, key.len() as u16);
    let k = node_ofs + NODE_DATA;
    p.get_mut(k..k + key.len())?.copy_from_slice(key);
    Some(KeyUpdate::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::node::F_DUPDATA;
    use crate::format::page::{NodeData, PageMut, PageRef, P_BRANCH, P_DIRTY, P_SUBP};

    const PS: usize = 512;

    fn leaf(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(7, P_LEAF);
        for (i, (k, v)) in entries.iter().enumerate() {
            p.add_node(i, k, NodeData::Inline(v), 0).unwrap();
        }
        buf
    }

    fn pairs(buf: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let p = PageRef::new(buf).unwrap();
        (0..p.num_keys())
            .map(|i| {
                let n = p.node(i).unwrap();
                (n.key().unwrap().to_vec(), n.inline_data().unwrap().to_vec())
            })
            .collect()
    }

    #[test]
    fn insert_matches_page_mut() {
        let mut a = leaf(&[(b"a", b"1"), (b"c", b"333")]);
        let mut b = a.clone();
        PageMut::new(&mut a)
            .add_node(1, b"bb", NodeData::Inline(b"22"), 0)
            .unwrap();
        let sz = even(NODE_SIZE + 2 + 2);
        insert_node(&mut b, 1, sz, b"bb", 0, NodeBody::Inline(b"22")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn delete_keeps_others() {
        let mut buf = leaf(&[(b"a", b"1"), (b"bb", b"22"), (b"ccc", b"333")]);
        let before = upper(&buf);
        delete_node(&mut buf, 1, 0).unwrap();
        assert_eq!(
            pairs(&buf),
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"ccc".to_vec(), b"333".to_vec())
            ]
        );
        assert_eq!(upper(&buf), before + even(NODE_SIZE + 4));
        delete_node(&mut buf, 0, 0).unwrap();
        delete_node(&mut buf, 0, 0).unwrap();
        assert_eq!(num_keys(&buf), 0);
        assert_eq!(upper(&buf), PS);
        assert!(delete_node(&mut buf, 0, 0).is_none());
    }

    #[test]
    fn leaf2_insert_and_delete() {
        let mut buf = vec![0u8; PS];
        PageMut::new(&mut buf).init(3, P_LEAF | P_LEAF2);
        insert_leaf2(&mut buf, 0, b"bbbb", 4).unwrap();
        insert_leaf2(&mut buf, 0, b"aaaa", 4).unwrap();
        insert_leaf2(&mut buf, 2, b"cccc", 4).unwrap();
        assert_eq!(&buf[16..28], b"aaaabbbbcccc");
        delete_node(&mut buf, 1, 4).unwrap();
        assert_eq!(&buf[16..24], b"aaaacccc");
        assert_eq!(num_keys(&buf), 2);
        assert_eq!(upper(&buf), PS - 2 * 2);
        assert!(insert_leaf2(&mut buf, 0, b"xx", 4).is_none());
    }

    #[test]
    fn update_branch_key_grow_and_shrink() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(9, P_BRANCH);
        p.add_node(0, b"", NodeData::Child(10), 0).unwrap();
        p.add_node(1, b"mm", NodeData::Child(11), 0).unwrap();
        p.add_node(2, b"zz", NodeData::Child(12), 0).unwrap();
        assert_eq!(update_key(&mut buf, 1, b"mmmmm"), Some(KeyUpdate::Done));
        let check = |buf: &[u8], k1: &[u8]| {
            let p = PageRef::new(buf).unwrap();
            assert_eq!(p.node(0).unwrap().child_pgno(), 10);
            assert_eq!(p.node(1).unwrap().key(), Some(k1));
            assert_eq!(p.node(1).unwrap().child_pgno(), 11);
            assert_eq!(p.node(2).unwrap().key(), Some(&b"zz"[..]));
            assert_eq!(p.node(2).unwrap().child_pgno(), 12);
        };
        check(&buf, b"mmmmm");
        assert_eq!(update_key(&mut buf, 1, b"a"), Some(KeyUpdate::Done));
        check(&buf, b"a");
        assert_eq!(update_key(&mut buf, 0, b""), Some(KeyUpdate::Done));
        check(&buf, b"a");
        let big = vec![b'q'; PS];
        assert_eq!(update_key(&mut buf, 1, &big), Some(KeyUpdate::NeedsSplit));
    }

    #[test]
    fn shrink_subpage_node() {
        // A sub-page of two duplicates with 20 bytes of free space.
        let mut sub = vec![0u8; 16 + 2 * 2 + 2 * even(NODE_SIZE + 2) + 20];
        let len = sub.len();
        let mut s = PageMut::new(&mut sub);
        s.init(0, P_LEAF | P_SUBP | P_DIRTY);
        s.add_node(0, b"d1", NodeData::Inline(b""), 0).unwrap();
        s.add_node(1, b"d2", NodeData::Inline(b""), 0).unwrap();
        assert_eq!(size_left(&sub), 20);
        let mut buf = leaf(&[(b"a", b"1")]);
        PageMut::new(&mut buf)
            .add_node(1, b"key", NodeData::Inline(&sub), F_DUPDATA)
            .unwrap();
        PageMut::new(&mut buf)
            .add_node(2, b"z", NodeData::Inline(b"9"), 0)
            .unwrap();
        let up = upper(&buf);
        shrink_node(&mut buf, 1, 7).unwrap();
        assert_eq!(upper(&buf), up + 20);
        let p = PageRef::new(&buf).unwrap();
        let n = p.node(1).unwrap();
        assert_eq!(n.data_size(), len - 20);
        let sp = PageRef::new(n.inline_data().unwrap()).unwrap();
        assert_eq!(sp.size_left(), 0);
        assert_eq!(sp.pgno(), 7);
        assert_eq!(sp.node(0).unwrap().key(), Some(&b"d1"[..]));
        assert_eq!(sp.node(1).unwrap().key(), Some(&b"d2"[..]));
        assert_eq!(p.node(0).unwrap().inline_data(), Some(&b"1"[..]));
        assert_eq!(p.node(2).unwrap().inline_data(), Some(&b"9"[..]));
    }
}
