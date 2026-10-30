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
//!
//! Every function takes the environment's [`Layout`]. Offsets passed in and
//! out (`lower`, `upper`, `ptr`, node offsets) are from the start of the
//! buffer; the layout's `PAGEBASE` is added when a stored `mp_lower`,
//! `mp_upper` or `mp_ptrs[i]` is read and subtracted when one is written,
//! so the algorithms below are C's with `PAGEBASE` folded in.

pub(crate) use crate::format::layout::even;
use crate::format::layout::{
    Layout, OverflowRef, NODE_FLAGS, NODE_HI, NODE_KSIZE, NODE_LO, NODE_SIZE,
};
use crate::format::node::F_BIGDATA;
use crate::format::page::{P_LEAF, P_LEAF2};
use crate::format::{get_u16, put_u16, put_u64};

#[inline]
fn hdr(p: &[u8], off: usize) -> u16 {
    get_u16(p, off).unwrap_or(0)
}

/// `mp_lower + PAGEBASE`: the end of the pointer array.
#[inline]
pub(crate) fn lower(l: &Layout, p: &[u8]) -> usize {
    usize::from(hdr(p, l.lower_offset())) + l.ptr_base
}

/// `mp_upper + PAGEBASE`: the start of the node area.
#[inline]
pub(crate) fn upper(l: &Layout, p: &[u8]) -> usize {
    usize::from(hdr(p, l.upper_offset())) + l.ptr_base
}

/// `mp_flags`.
#[inline]
pub(crate) fn flags(l: &Layout, p: &[u8]) -> u16 {
    hdr(p, l.flags_offset())
}

/// `mp_pad`: the key size of a `P_LEAF2` page.
#[inline]
pub(crate) fn pad(l: &Layout, p: &[u8]) -> u16 {
    hdr(p, l.pad_offset())
}

/// Sets `mp_lower` to `v - PAGEBASE` (`v` is from the page start).
#[inline]
pub(crate) fn set_lower(l: &Layout, p: &mut [u8], v: usize) {
    put_u16(p, l.lower_offset(), (v - l.ptr_base) as u16);
}

/// Sets `mp_upper` to `v - PAGEBASE` (`v` is from the page start).
#[inline]
pub(crate) fn set_upper(l: &Layout, p: &mut [u8], v: usize) {
    put_u16(p, l.upper_offset(), (v - l.ptr_base) as u16);
}

/// Sets `mp_flags`.
#[inline]
pub(crate) fn set_flags(l: &Layout, p: &mut [u8], v: u16) {
    put_u16(p, l.flags_offset(), v);
}

/// Sets `mp_pad`.
#[inline]
pub(crate) fn set_pad(l: &Layout, p: &mut [u8], v: u16) {
    put_u16(p, l.pad_offset(), v);
}

/// Sets `mp_pages`, the length of an overflow run.
#[inline]
pub(crate) fn set_overflow_pages(l: &Layout, p: &mut [u8], pages: u32) {
    crate::format::put_u32(p, l.pages_offset(), pages);
}

/// `mp_pages` of the first page of an overflow run (`None` if `p` is
/// shorter than a header).
#[inline]
pub(crate) fn overflow_pages(l: &Layout, p: &[u8]) -> Option<u32> {
    crate::format::get_u32(p, l.pages_offset())
}

/// Sets `mp_pgno` (the first 8 bytes in every format).
#[inline]
pub(crate) fn set_pgno(p: &mut [u8], pgno: u64) {
    put_u64(p, 0, pgno);
}

/// `NUMKEYS`.
#[inline]
pub(crate) fn num_keys(l: &Layout, p: &[u8]) -> usize {
    l.num_keys(hdr(p, l.lower_offset()))
}

/// `SIZELEFT`.
#[inline]
pub(crate) fn size_left(l: &Layout, p: &[u8]) -> usize {
    upper(l, p).saturating_sub(lower(l, p))
}

/// `mp_ptrs[i] + PAGEBASE`: the offset of node `i` from the page start.
#[inline]
pub(crate) fn ptr(l: &Layout, p: &[u8], i: usize) -> Option<usize> {
    get_u16(p, l.ptrs_offset() + 2 * i).map(|v| usize::from(v) + l.ptr_base)
}

/// Sets `mp_ptrs[i]` to `v - PAGEBASE` (`v` is from the page start).
#[inline]
pub(crate) fn set_ptr(l: &Layout, p: &mut [u8], i: usize, v: usize) -> Option<()> {
    let off = l.ptrs_offset() + 2 * i;
    let stored = v.checked_sub(l.ptr_base)? as u16;
    p.get_mut(off..off + 2)?
        .copy_from_slice(&stored.to_ne_bytes());
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
pub(crate) fn node_data_range(l: &Layout, p: &[u8], i: usize) -> Option<std::ops::Range<usize>> {
    if i >= num_keys(l, p) {
        return None;
    }
    let off = ptr(l, p, i)?;
    let (ksize, _, dsize) = node_hdr(p, off)?;
    let start = off.checked_add(l.node_data_offset(ksize))?;
    let end = start.checked_add(dsize)?;
    (end <= p.len()).then_some(start..end)
}

/// Replaces the overflow reference stored as the data of `F_BIGDATA` leaf
/// node `i` with `r` (0.9 stores only `r.pgno`). The node's data size is
/// that of the value, not of the reference, so [`node_data_range`] does
/// not apply.
pub(crate) fn set_node_overflow_ref(
    l: &Layout,
    p: &mut [u8],
    i: usize,
    r: OverflowRef,
) -> Option<()> {
    if i >= num_keys(l, p) {
        return None;
    }
    let off = ptr(l, p, i)?;
    let (ksize, _, _) = node_hdr(p, off)?;
    let data = p.get_mut(off.checked_add(l.node_data_offset(ksize))?..)?;
    l.write_overflow_ref(data, r)
}

/// `SETDSZ`: sets the data size of leaf node `i`.
pub(crate) fn set_node_dsize(l: &Layout, p: &mut [u8], i: usize, size: usize) -> Option<()> {
    let off = ptr(l, p, i)?;
    node_hdr(p, off)?;
    put_u16(p, off + NODE_LO, size as u16);
    put_u16(p, off + NODE_HI, (size >> 16) as u16);
    Some(())
}

/// `SETPGNO`: points branch node `i` at `pgno`.
pub(crate) fn set_node_pgno(l: &Layout, p: &mut [u8], i: usize, pgno: u64) -> Option<()> {
    if i >= num_keys(l, p) {
        return None;
    }
    let off = ptr(l, p, i)?;
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
    /// An `F_BIGDATA` leaf node: data of `size` bytes on the overflow run
    /// that `r` refers to.
    Overflow { r: OverflowRef, size: usize },
}

/// The `update:` part of `mdb_node_add`: make room in the pointer array at
/// `indx`, carve `node_size` bytes (already `EVEN`) off the top of the free
/// space and write the node header, key and body there. The caller has
/// checked that the node fits. Returns the node's offset in the page.
pub(crate) fn insert_node(
    l: &Layout,
    p: &mut [u8],
    indx: usize,
    node_size: usize,
    key: &[u8],
    flags: u16,
    body: NodeBody<'_>,
) -> Option<usize> {
    let nkeys = num_keys(l, p);
    if indx > nkeys {
        return None;
    }
    // Move higher pointers up one slot (one move of the pointer bytes, as
    // C's loop over `mp_ptrs` does them one by one).
    let ptrs = l.ptrs_offset();
    p.get_mut(ptrs + 2 * indx..ptrs + 2 * (nkeys + 1))?
        .copy_within(..2 * (nkeys - indx), 2);
    // Adjust free space offsets.
    let ofs = upper(l, p).checked_sub(node_size)?;
    if ofs < lower(l, p) + 2 || ofs + node_size > p.len() {
        return None;
    }
    set_ptr(l, p, indx, ofs)?;
    set_upper(l, p, ofs);
    set_lower(l, p, lower(l, p) + 2);

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
    let k = ofs + NODE_SIZE;
    p.get_mut(k..k + key.len())?.copy_from_slice(key);
    let d = ofs + l.node_data_offset(key.len());
    match body {
        NodeBody::Branch(_) | NodeBody::Reserve(_) => {}
        NodeBody::Inline(bytes) => p.get_mut(d..d + bytes.len())?.copy_from_slice(bytes),
        NodeBody::Overflow { r, .. } => l.write_overflow_ref(p.get_mut(d..)?, r)?,
    }
    Some(ofs)
}

/// The `IS_LEAF2` branch of `mdb_node_add`: insert a `ksize`-byte key at
/// `indx`, shifting the following keys up. `key` must be `ksize` long.
pub(crate) fn insert_leaf2(
    l: &Layout,
    p: &mut [u8],
    indx: usize,
    key: &[u8],
    ksize: usize,
) -> Option<()> {
    let nkeys = num_keys(l, p);
    if indx > nkeys || key.len() != ksize {
        return None;
    }
    let start = l.page_hdr + indx * ksize;
    let end = l.page_hdr + nkeys * ksize;
    if end + ksize > p.len() {
        return None;
    }
    p.copy_within(start..end, start + ksize);
    p[start..start + ksize].copy_from_slice(key);
    // Just using these for counting.
    set_lower(l, p, lower(l, p) + 2);
    set_upper(l, p, upper(l, p).checked_sub(ksize.checked_sub(2)?)?);
    Some(())
}

/// The on-page size of node `i` (header, key and inline data or overflow
/// reference), `EVEN`, as `mdb_node_del` computes it.
fn node_size(l: &Layout, p: &[u8], off: usize, is_leaf: bool) -> Option<usize> {
    let (ksize, nflags, dsize) = node_hdr(p, off)?;
    let mut sz = NODE_SIZE + l.node_key_len(ksize);
    if is_leaf {
        sz += l.stored_data_len(dsize, nflags & F_BIGDATA != 0);
    }
    Some(even(sz))
}

/// `mdb_node_del` (mdb.c line 7602): delete node `indx`, closing the gap in
/// the pointer array and in the node area. `ksize` is the key size of a
/// `P_LEAF2` page.
pub(crate) fn delete_node(l: &Layout, p: &mut [u8], indx: usize, ksize: usize) -> Option<()> {
    let numkeys = num_keys(l, p);
    if indx >= numkeys {
        return None;
    }
    let pflags = flags(l, p);
    if pflags & P_LEAF2 != 0 {
        let x = numkeys - 1 - indx;
        let base = l.page_hdr + indx * ksize;
        let end = base + ksize + x * ksize;
        if end > p.len() {
            return None;
        }
        p.copy_within(base + ksize..end, base);
        set_lower(l, p, lower(l, p) - 2);
        set_upper(l, p, upper(l, p) + ksize.checked_sub(2)?);
        return Some(());
    }

    let node_ofs = ptr(l, p, indx)?;
    let sz = node_size(l, p, node_ofs, pflags & P_LEAF != 0)?;
    // Drop the pointer and move the nodes below the deleted one up by its
    // size: stored values, as C's loop over `mp_ptrs` does (`node_ofs` is
    // the stored value plus `PAGEBASE`; `as u16` truncates as C does).
    let first = l.ptrs_offset();
    let ptrs = p.get_mut(first..first + 2 * numkeys)?;
    let below = node_ofs - l.ptr_base;
    let mut j = 0;
    for i in 0..numkeys {
        if i != indx {
            let v = usize::from(u16::from_ne_bytes([ptrs[2 * i], ptrs[2 * i + 1]]));
            let v = if v < below { v + sz } else { v };
            ptrs[2 * j..2 * j + 2].copy_from_slice(&(v as u16).to_ne_bytes());
            j += 1;
        }
    }
    let base = upper(l, p);
    if base > node_ofs || node_ofs + sz > p.len() {
        return None;
    }
    p.copy_within(base..node_ofs, base + sz);
    set_lower(l, p, lower(l, p) - 2);
    set_upper(l, p, base + sz);
    Some(())
}

/// `mdb_node_shrink` (mdb.c line 7658): give the free space of the
/// sub-page in leaf node `indx` back to the main page `p`. The sub-page
/// has the layout of its page.
pub(crate) fn shrink_node(l: &Layout, p: &mut [u8], indx: usize, page_pgno: u64) -> Option<()> {
    let node_ofs = ptr(l, p, indx)?;
    let (ksize, _, dsize) = node_hdr(p, node_ofs)?;
    let sp = node_ofs + l.node_data_offset(ksize);
    let sub = p.get(sp..sp.checked_add(dsize)?)?;
    let delta = size_left(l, sub);
    let nsize = dsize.checked_sub(delta)?;
    let sub_keys = num_keys(l, sub);
    let sub_leaf2 = flags(l, sub) & P_LEAF2 != 0;

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
            let v = ptr(l, &p[sp..], i)?.checked_sub(delta)?;
            set_ptr(l, &mut p[xp..], i, v)?;
        }
        l.page_hdr
    };
    let sub_lower = lower(l, &p[sp..]);
    set_upper(l, &mut p[sp..], sub_lower);
    set_pgno(&mut p[sp..], page_pgno);
    set_node_dsize(l, p, indx, nsize)?;

    // Shift <lower nodes...initial part of subpage> upward.
    let base = upper(l, p);
    let end = sp + len;
    if base > end || end + delta > p.len() {
        return None;
    }
    p.copy_within(base..end, base + delta);
    for i in 0..num_keys(l, p) {
        let v = ptr(l, p, i)?;
        if v <= node_ofs {
            set_ptr(l, p, i, v + delta)?;
        }
    }
    set_upper(l, p, base + delta);
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
pub(crate) fn update_key(l: &Layout, p: &mut [u8], indx: usize, key: &[u8]) -> Option<KeyUpdate> {
    let mut node_ofs = ptr(l, p, indx)?;
    let (oksize, _, _) = node_hdr(p, node_ofs)?;
    let ksize = even(key.len());
    let oksize_even = even(oksize);
    if ksize != oksize_even {
        if ksize > oksize_even && size_left(l, p) < ksize - oksize_even {
            return Some(KeyUpdate::NeedsSplit);
        }
        let numkeys = num_keys(l, p);
        let base = upper(l, p);
        let len = node_ofs.checked_sub(base)? + NODE_SIZE;
        if ksize > oksize_even {
            // Grow: everything from `upper` to this node's header moves down.
            let delta = ksize - oksize_even;
            for i in 0..numkeys {
                let v = ptr(l, p, i)?;
                if v <= node_ofs {
                    set_ptr(l, p, i, v.checked_sub(delta)?)?;
                }
            }
            let dst = base.checked_sub(delta)?;
            p.copy_within(base..base + len, dst);
            set_upper(l, p, dst);
            node_ofs -= delta;
        } else {
            // Shrink: it moves up.
            let delta = oksize_even - ksize;
            for i in 0..numkeys {
                let v = ptr(l, p, i)?;
                if v <= node_ofs {
                    set_ptr(l, p, i, v + delta)?;
                }
            }
            if base + len + delta > p.len() {
                return None;
            }
            p.copy_within(base..base + len, base + delta);
            set_upper(l, p, base + delta);
            node_ofs += delta;
        }
    }
    // But even if no shift was needed, update ksize.
    put_u16(p, node_ofs + NODE_KSIZE, key.len() as u16);
    // NODEKEY: the key follows the node header.
    let k = node_ofs + NODE_SIZE;
    p.get_mut(k..k + key.len())?.copy_from_slice(key);
    Some(KeyUpdate::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::layout::V09;
    use crate::format::node::F_DUPDATA;
    use crate::format::page::{NodeData, PageMut, PageRef, P_BRANCH, P_DIRTY, P_SUBP};

    const PS: usize = 512;
    const L: &Layout = &V09;

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
        insert_node(L, &mut b, 1, sz, b"bb", 0, NodeBody::Inline(b"22")).unwrap();
        assert_eq!(a, b);
    }

    /// `insert_node` and `delete_node` as they were, moving one pointer at
    /// a time, for [`pointer_moves_match_the_per_pointer_loop`].
    fn insert_node_reference(
        l: &Layout,
        p: &mut [u8],
        indx: usize,
        node_size: usize,
        key: &[u8],
        body: NodeBody<'_>,
    ) -> Option<usize> {
        let nkeys = num_keys(l, p);
        if indx > nkeys {
            return None;
        }
        for i in (indx..nkeys).rev() {
            let v = ptr(l, p, i)?;
            set_ptr(l, p, i + 1, v)?;
        }
        let ofs = upper(l, p).checked_sub(node_size)?;
        if ofs < lower(l, p) + 2 || ofs + node_size > p.len() {
            return None;
        }
        set_ptr(l, p, indx, ofs)?;
        set_upper(l, p, ofs);
        set_lower(l, p, lower(l, p) + 2);
        put_u16(p, ofs + NODE_KSIZE, key.len() as u16);
        let NodeBody::Inline(d) = body else {
            return None;
        };
        put_u16(p, ofs + NODE_LO, d.len() as u16);
        put_u16(p, ofs + NODE_HI, 0);
        put_u16(p, ofs + NODE_FLAGS, 0);
        let k = ofs + NODE_SIZE;
        p.get_mut(k..k + key.len())?.copy_from_slice(key);
        let dofs = ofs + l.node_data_offset(key.len());
        p.get_mut(dofs..dofs + d.len())?.copy_from_slice(d);
        Some(ofs)
    }

    fn delete_node_reference(l: &Layout, p: &mut [u8], indx: usize) -> Option<()> {
        let numkeys = num_keys(l, p);
        if indx >= numkeys {
            return None;
        }
        let node_ofs = ptr(l, p, indx)?;
        let sz = node_size(l, p, node_ofs, flags(l, p) & P_LEAF != 0)?;
        let mut j = 0;
        for i in 0..numkeys {
            if i != indx {
                let mut v = ptr(l, p, i)?;
                if v < node_ofs {
                    v += sz;
                }
                set_ptr(l, p, j, v)?;
                j += 1;
            }
        }
        let base = upper(l, p);
        if base > node_ofs || node_ofs + sz > p.len() {
            return None;
        }
        p.copy_within(base..node_ofs, base + sz);
        set_lower(l, p, lower(l, p) - 2);
        set_upper(l, p, base + sz);
        Some(())
    }

    /// The pointer moves of `insert_node` (one `copy_within`) and
    /// `delete_node` (one pass over the stored values) leave the pages the
    /// per-pointer loops of C left, in both layouts, at every index; on a
    /// page whose `mp_lower` claims more pointers than fit, both fail.
    #[test]
    fn pointer_moves_match_the_per_pointer_loop() {
        for l in [&V09, &crate::format::layout::V10] {
            for n in 0..12usize {
                let mut page = vec![0u8; PS];
                let mut pm = PageMut::with_layout(&mut page, l);
                pm.init(7, P_LEAF);
                for i in 0..n {
                    let k = [b'a' + i as u8; 3];
                    let v = vec![i as u8; i % 5];
                    pm.add_node(i, &k[..1 + i % 3], NodeData::Inline(&v), 0)
                        .unwrap();
                }
                for indx in 0..=n + 1 {
                    let size = even(NODE_SIZE + l.node_key_len(2) + l.node_data_len(3));
                    let body = NodeBody::Inline(b"xyz");
                    let mut a = page.clone();
                    let mut b = page.clone();
                    let ra = insert_node(l, &mut a, indx, size, b"kk", 0, body);
                    let rb = insert_node_reference(l, &mut b, indx, size, b"kk", body);
                    assert_eq!(ra, rb, "insert {n} at {indx}");
                    if ra.is_some() {
                        assert_eq!(a, b, "insert {n} at {indx}");
                    }
                    let mut a = page.clone();
                    let mut b = page.clone();
                    let ra = delete_node(l, &mut a, indx, 0);
                    let rb = delete_node_reference(l, &mut b, indx);
                    assert_eq!(ra, rb, "delete {n} at {indx}");
                    assert_eq!(a, b, "delete {n} at {indx}");
                }
                // `mp_lower` beyond the page: both refuse.
                let mut bad = page.clone();
                set_lower(l, &mut bad, PS + 64);
                assert!(
                    insert_node(l, &mut bad.clone(), 0, 16, b"k", 0, NodeBody::Inline(b""))
                        .is_none()
                );
                assert!(delete_node(l, &mut bad, n.saturating_sub(1), 0).is_none() || n == 0);
            }
        }
    }

    #[test]
    fn delete_keeps_others() {
        let mut buf = leaf(&[(b"a", b"1"), (b"bb", b"22"), (b"ccc", b"333")]);
        let before = upper(L, &buf);
        delete_node(L, &mut buf, 1, 0).unwrap();
        assert_eq!(
            pairs(&buf),
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"ccc".to_vec(), b"333".to_vec())
            ]
        );
        assert_eq!(upper(L, &buf), before + even(NODE_SIZE + 4));
        delete_node(L, &mut buf, 0, 0).unwrap();
        delete_node(L, &mut buf, 0, 0).unwrap();
        assert_eq!(num_keys(L, &buf), 0);
        assert_eq!(upper(L, &buf), PS);
        assert!(delete_node(L, &mut buf, 0, 0).is_none());
    }

    #[test]
    fn leaf2_insert_and_delete() {
        let mut buf = vec![0u8; PS];
        PageMut::new(&mut buf).init(3, P_LEAF | P_LEAF2);
        insert_leaf2(L, &mut buf, 0, b"bbbb", 4).unwrap();
        insert_leaf2(L, &mut buf, 0, b"aaaa", 4).unwrap();
        insert_leaf2(L, &mut buf, 2, b"cccc", 4).unwrap();
        assert_eq!(&buf[16..28], b"aaaabbbbcccc");
        delete_node(L, &mut buf, 1, 4).unwrap();
        assert_eq!(&buf[16..24], b"aaaacccc");
        assert_eq!(num_keys(L, &buf), 2);
        assert_eq!(upper(L, &buf), PS - 2 * 2);
        assert!(insert_leaf2(L, &mut buf, 0, b"xx", 4).is_none());
    }

    #[test]
    fn update_branch_key_grow_and_shrink() {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(9, P_BRANCH);
        p.add_node(0, b"", NodeData::Child(10), 0).unwrap();
        p.add_node(1, b"mm", NodeData::Child(11), 0).unwrap();
        p.add_node(2, b"zz", NodeData::Child(12), 0).unwrap();
        assert_eq!(update_key(L, &mut buf, 1, b"mmmmm"), Some(KeyUpdate::Done));
        let check = |buf: &[u8], k1: &[u8]| {
            let p = PageRef::new(buf).unwrap();
            assert_eq!(p.node(0).unwrap().child_pgno(), 10);
            assert_eq!(p.node(1).unwrap().key(), Some(k1));
            assert_eq!(p.node(1).unwrap().child_pgno(), 11);
            assert_eq!(p.node(2).unwrap().key(), Some(&b"zz"[..]));
            assert_eq!(p.node(2).unwrap().child_pgno(), 12);
        };
        check(&buf, b"mmmmm");
        assert_eq!(update_key(L, &mut buf, 1, b"a"), Some(KeyUpdate::Done));
        check(&buf, b"a");
        assert_eq!(update_key(L, &mut buf, 0, b""), Some(KeyUpdate::Done));
        check(&buf, b"a");
        let big = vec![b'q'; PS];
        assert_eq!(
            update_key(L, &mut buf, 1, &big),
            Some(KeyUpdate::NeedsSplit)
        );
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
        assert_eq!(size_left(L, &sub), 20);
        let mut buf = leaf(&[(b"a", b"1")]);
        PageMut::new(&mut buf)
            .add_node(1, b"key", NodeData::Inline(&sub), F_DUPDATA)
            .unwrap();
        PageMut::new(&mut buf)
            .add_node(2, b"z", NodeData::Inline(b"9"), 0)
            .unwrap();
        let up = upper(L, &buf);
        shrink_node(L, &mut buf, 1, 7).unwrap();
        assert_eq!(upper(L, &buf), up + 20);
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

    // --- the 1.0 layout: the same edits through PAGEBASE-relative fields ---

    const V10L: &Layout = &crate::format::layout::V10;

    fn page_v10(flags: u16) -> Vec<u8> {
        let mut buf = vec![0u8; PS];
        PageMut::with_layout(&mut buf, V10L).init(7, flags);
        buf
    }

    fn pairs_v10(buf: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let p = PageRef::with_layout(buf, V10L).unwrap();
        (0..p.num_keys())
            .map(|i| {
                let n = p.node(i).unwrap();
                (n.key().unwrap().to_vec(), n.inline_data().unwrap().to_vec())
            })
            .collect()
    }

    #[test]
    fn v10_insert_and_delete() {
        let l = V10L;
        let items: [(&[u8], &[u8]); 3] = [(b"a", b"1"), (b"bb", b"222"), (b"ccc", b"4444")];
        // insert_node builds what PageMut builds.
        let mut a = page_v10(P_LEAF);
        for (i, (k, v)) in items.iter().enumerate() {
            let sz = even(l.leaf_size(k.len(), v.len()));
            insert_node(l, &mut a, i, sz, k, 0, NodeBody::Inline(v)).unwrap();
        }
        let mut b = page_v10(P_LEAF);
        for (i, (k, v)) in items.iter().enumerate() {
            PageMut::with_layout(&mut b, l)
                .add_node(i, k, NodeData::Inline(v), 0)
                .unwrap();
        }
        assert_eq!(a, b);
        assert_eq!(num_keys(l, &a), 3);
        // Absolute offsets in, PAGEBASE-relative values stored.
        assert_eq!(lower(l, &a), 24 + 3 * 2);
        assert_eq!(get_u16(&a, l.lower_offset()), Some(3 * 2));
        assert_eq!(ptr(l, &a, 0), Some(PS - 12));
        assert_eq!(get_u16(&a, l.ptrs_offset()), Some((PS - 12 - 24) as u16));
        let range = node_data_range(l, &a, 1).unwrap();
        assert_eq!(&a[range], b"222");

        let before = upper(l, &a);
        delete_node(l, &mut a, 1, 0).unwrap();
        assert_eq!(
            pairs_v10(&a),
            [
                (b"a".to_vec(), b"1".to_vec()),
                (b"ccc".to_vec(), b"4444".to_vec())
            ]
        );
        // 8 + EVEN(2) + EVEN(3)
        assert_eq!(upper(l, &a), before + 14);
        delete_node(l, &mut a, 0, 0).unwrap();
        delete_node(l, &mut a, 0, 0).unwrap();
        assert_eq!(num_keys(l, &a), 0);
        assert_eq!(upper(l, &a), PS);
        assert_eq!(get_u16(&a, l.upper_offset()), Some((PS - 24) as u16));
    }

    #[test]
    fn v10_overflow_ref_and_leaf2() {
        let l = V10L;
        let mut a = page_v10(P_LEAF);
        let r = OverflowRef {
            pgno: 9,
            txnid: 5,
            pages: 2,
        };
        let sz = even(NODE_SIZE + l.node_key_len(3) + l.ovpage_size);
        insert_node(
            l,
            &mut a,
            0,
            sz,
            b"big",
            0,
            NodeBody::Overflow { r, size: 6000 },
        )
        .unwrap();
        let moved = OverflowRef {
            pgno: 11,
            txnid: 1,
            ..r
        };
        set_node_overflow_ref(l, &mut a, 0, moved).unwrap();
        let n = PageRef::with_layout(&a, l).unwrap().node(0).unwrap();
        assert!(n.is_bigdata());
        assert_eq!(n.data_size(), 6000);
        assert_eq!(n.overflow_ref(), Some(moved));
        delete_node(l, &mut a, 0, 0).unwrap();
        assert_eq!(upper(l, &a), PS);

        let mut k = page_v10(P_LEAF | P_LEAF2);
        insert_leaf2(l, &mut k, 0, b"bbbb", 4).unwrap();
        insert_leaf2(l, &mut k, 0, b"aaaa", 4).unwrap();
        assert_eq!(&k[24..32], b"aaaabbbb");
        delete_node(l, &mut k, 0, 4).unwrap();
        assert_eq!(&k[24..28], b"bbbb");
        assert_eq!(num_keys(l, &k), 1);
    }

    #[test]
    fn v10_update_key() {
        let l = V10L;
        let mut buf = page_v10(P_BRANCH);
        let mut p = PageMut::with_layout(&mut buf, l);
        p.add_node(0, b"", NodeData::Child(10), 0).unwrap();
        p.add_node(1, b"mm", NodeData::Child(11), 0).unwrap();
        p.add_node(2, b"zz", NodeData::Child(12), 0).unwrap();
        let check = |buf: &[u8], k1: &[u8]| {
            let p = PageRef::with_layout(buf, l).unwrap();
            assert_eq!(p.node(0).unwrap().child_pgno(), 10);
            assert_eq!(p.node(1).unwrap().key(), Some(k1));
            assert_eq!(p.node(1).unwrap().child_pgno(), 11);
            assert_eq!(p.node(2).unwrap().key(), Some(&b"zz"[..]));
            assert_eq!(p.node(2).unwrap().child_pgno(), 12);
        };
        assert_eq!(update_key(l, &mut buf, 1, b"mmmmm"), Some(KeyUpdate::Done));
        check(&buf, b"mmmmm");
        assert_eq!(update_key(l, &mut buf, 1, b"a"), Some(KeyUpdate::Done));
        check(&buf, b"a");
    }

    #[test]
    fn v10_shrink_subpage_node() {
        let l = V10L;
        // A 1.0 sub-page (P_LEAF|P_SUBP, 24-byte header) of two duplicates
        // with 20 bytes of free space.
        let mut sub = vec![0u8; 24 + 2 * 2 + 2 * even(l.leaf_size(2, 0)) + 20];
        let len = sub.len();
        let mut s = PageMut::with_layout(&mut sub, l);
        s.init(0, l.subpage_flags);
        s.add_node(0, b"d1", NodeData::Inline(b""), 0).unwrap();
        s.add_node(1, b"d2", NodeData::Inline(b""), 0).unwrap();
        assert_eq!(size_left(l, &sub), 20);
        let mut buf = page_v10(P_LEAF);
        let mut p = PageMut::with_layout(&mut buf, l);
        p.add_node(0, b"a", NodeData::Inline(b"1"), 0).unwrap();
        p.add_node(1, b"key", NodeData::Inline(&sub), F_DUPDATA)
            .unwrap();
        p.add_node(2, b"z", NodeData::Inline(b"9"), 0).unwrap();
        let up = upper(l, &buf);
        shrink_node(l, &mut buf, 1, 7).unwrap();
        assert_eq!(upper(l, &buf), up + 20);
        let p = PageRef::with_layout(&buf, l).unwrap();
        let n = p.node(1).unwrap();
        assert_eq!(n.data_size(), len - 20);
        let sp = PageRef::with_layout(n.inline_data().unwrap(), l).unwrap();
        assert_eq!(sp.flags(), P_LEAF | P_SUBP);
        assert_eq!(sp.size_left(), 0);
        assert_eq!(sp.pgno(), 7);
        assert_eq!(sp.node(0).unwrap().key(), Some(&b"d1"[..]));
        assert_eq!(sp.node(1).unwrap().key(), Some(&b"d2"[..]));
        assert_eq!(p.node(0).unwrap().inline_data(), Some(&b"1"[..]));
        assert_eq!(p.node(2).unwrap().inline_data(), Some(&b"9"[..]));
    }
}
