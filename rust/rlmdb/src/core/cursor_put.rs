//! Inserting and replacing items: `mdb_cursor_put` (mdb.c line 7244, here
//! `RwTxn::put_at`), `mdb_node_add` (line 7475), `mdb_page_new` (line
//! 7382), `mdb_leaf_size`/`mdb_branch_size` and `mdb_update_key` (line
//! 7962).
//!
//! `mdb_cursor_put` is one long C function with `goto`s between its stages;
//! here each stage is a function and the jumps are return values:
//!
//! 1. position the cursor (`MDB_SET`, `MDB_APPEND` → last key, or the
//!    current position for `MDB_CURRENT`); an empty tree gets a root leaf;
//! 2. touch the pages on the cursor's path ([`RwTxn::cursor_touch`]);
//! 3. for an existing key: overwrite in place where possible
//!    ([`RwTxn::put_current`]), or for `MDB_DUPSORT` prepare the node that
//!    will hold the duplicates ([`RwTxn::dup_prepare`]): convert a single
//!    item into a sub-page, grow a sub-page, or promote it to a sub-DB;
//! 4. insert the node, splitting the leaf if it is full
//!    ([`RwTxn::insert_leaf_node`]), and fix up the other cursors;
//! 5. for duplicates, store the data item(s) through the sub-cursor
//!    ([`RwTxn::put_sub`]) and write the sub-DB record back into the node.
//!
//! The DUPSORT stage reproduces C's scratch page (`me_pbuf`) with the write
//! state's scratch buffers ([`RwTxn::take_scratch`]): `fp` is the old
//! sub-page (or a header faking one), `mp` the new (sub-)page, `xdata` the
//! new node data.

use std::cmp::Ordering;

use crate::cmp::Comparator;
use crate::consts::*;
use crate::core::cursor::{
    corrupted, cursor_last_for_append, cursor_set, CursorOp, C_DEL, C_INITIALIZED,
};
use crate::core::dbi::DB_DIRTY;
use crate::core::page_ops::{self, even, KeyUpdate, NodeBody};
use crate::core::txn_rw::{FailOnPanic, RwTxn, TXN_ERROR};
use crate::core::wcursor::{CRef, SUBPAGE};
use crate::error::{Error, MdbError, Result};
use crate::format::layout::{Format, Layout, OverflowRef, NODE_SIZE};
use crate::format::node::{F_BIGDATA, F_DUPDATA, F_SUBDATA};
use crate::format::page::{P_BRANCH, P_DIRTY, P_LEAF, P_LEAF2, P_OVERFLOW, P_SUBP};
use crate::format::{Db, PageRef};

/// `MDB_NOSPILL`: do not spill pages to disk if the transaction is getting
/// full (internal put/del flag).
pub(crate) const MDB_NOSPILL: u32 = 0x8000;
/// `MDB_SPLIT_REPLACE`: the key being split in is not new.
pub(crate) const SPLIT_REPLACE: u32 = MDB_APPENDDUP;
/// `NODE_ADD_FLAGS`: the put flags `mdb_node_add` and `mdb_page_split` use.
const NODE_ADD_FLAGS: u32 = F_DUPDATA as u32 | F_SUBDATA as u32 | MDB_RESERVE | MDB_APPEND;
/// `MDB_NO_ROOT`: internal "tree is empty" result of the positioning step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pos {
    Found,
    NotFound,
    NoRoot,
}

/// The data argument of a put (`MDB_val *data`, or the `data[2]` array of
/// `MDB_MULTIPLE`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PutData<'a> {
    /// The bytes (all items for `MDB_MULTIPLE`; empty for `MDB_RESERVE`).
    pub(crate) bytes: &'a [u8],
    /// `data[0].mv_size`: the size of one item.
    pub(crate) size: usize,
    /// `data[1].mv_size` for `MDB_MULTIPLE`: the number of items.
    pub(crate) count: usize,
}

impl<'a> PutData<'a> {
    /// Plain data.
    pub(crate) fn bytes(bytes: &'a [u8]) -> PutData<'a> {
        PutData {
            bytes,
            size: bytes.len(),
            count: 1,
        }
    }

    /// `MDB_RESERVE` of `len` bytes.
    pub(crate) fn reserve(len: usize) -> PutData<'static> {
        PutData {
            bytes: &[],
            size: len,
            count: 1,
        }
    }

    /// `MDB_MULTIPLE`: `count` items of `size` bytes.
    pub(crate) fn multiple(bytes: &'a [u8], size: usize, count: usize) -> PutData<'a> {
        PutData { bytes, size, count }
    }

    /// Empty data (the data part of a sub-DB key).
    pub(crate) fn empty() -> PutData<'static> {
        PutData::bytes(&[])
    }

    /// Item `k` as node data.
    fn item(&self, k: usize) -> NodeVal<'a> {
        let bytes = if self.bytes.is_empty() {
            self.bytes
        } else {
            &self.bytes[k * self.size..(k + 1) * self.size]
        };
        NodeVal {
            size: self.size,
            bytes,
        }
    }
}

/// Node data for [`RwTxn::node_add`]: `size` bytes, given by `bytes` (empty
/// for `MDB_RESERVE`, the stored overflow reference for `F_BIGDATA`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeVal<'a> {
    pub(crate) size: usize,
    pub(crate) bytes: &'a [u8],
}

/// What the DUPSORT stage decided (the `goto` targets of C).
enum DupStep {
    /// `goto current`: overwrite the single item.
    Current,
    /// `goto put_sub`: the node already holds room for the new item.
    PutSub,
    /// `MDB_CURRENT` with an item that does not fit the sub-page: delete the
    /// current item, then insert the new one.
    Replace,
    /// `goto new_sub`: (re)insert the node with this data; then store the
    /// old single item `dkey` (if converting) and the new item in the
    /// sub-tree whose new root page is `sub_root` (if promoted).
    NewSub {
        xdata: Vec<u8>,
        sub_root: Option<u64>,
        dkey: Option<Vec<u8>>,
    },
}

impl<'env> RwTxn<'env> {
    /// For `MDB_CURRENT` on a duplicate in sub-page `fp`: does the new
    /// item of `size` bytes fit where the sub-cursor's current item is
    /// (after that one is deleted)?
    fn current_dup_fits(&self, mc: CRef, fp: &[u8], size: usize) -> bool {
        let l = self.layout();
        let x = self.cur(CRef::sub(mc.slot));
        if !x.is_init() || !x.subp || page_ops::flags(l, fp) & P_LEAF2 != 0 {
            return true; // C's own checks apply
        }
        let Some(fpr) = PageRef::with_layout(fp, l) else {
            return true;
        };
        let Some(node) = fpr.node(x.ki_top()) else {
            return true;
        };
        let oldk = node.key_size();
        // Room of the old node and its pointer against that of the new one
        // (sub-page nodes have no data).
        oldk == size
            || page_ops::size_left(l, fp) + even(l.leaf_size(oldk, 0)) + 2
                >= even(l.leaf_size(size, 0) + 2)
    }
}

/// What the overwrite stage decided.
enum CurStep {
    /// Done, return success.
    Done,
    /// The old node was deleted; insert the new one (`new_ksize`).
    Reinsert,
}

/// `mdb_leaf_size`: bytes a leaf node takes (data on overflow pages counts
/// as an overflow reference), including its pointer, `EVEN`.
pub(crate) fn leaf_size(l: &Layout, nodemax: usize, ksize: usize, dsize: usize) -> usize {
    let mut sz = l.leaf_size(ksize, dsize);
    if sz > nodemax {
        // put on overflow page
        sz -= dsize - l.ovpage_size;
    }
    even(sz + 2)
}

/// `mdb_branch_size`: bytes a branch node takes, including its pointer.
/// 0.9 does not round this up (the node itself is `EVEN`), 1.0 does
/// (mdb.c 1.0, line 9334); split decisions depend on it.
pub(crate) fn branch_size(l: &Layout, ksize: usize) -> usize {
    let sz = l.branch_size(ksize) + 2;
    if l.even_nodes {
        even(sz)
    } else {
        sz
    }
}

impl<'env> RwTxn<'env> {
    /// `_mdb_cursor_put`: store `key`/`data` at cursor `mc`. Returns the
    /// number of items stored for `MDB_MULTIPLE` (`data[1].mv_size`).
    pub(crate) fn put_at(
        &mut self,
        mc: CRef,
        key: &[u8],
        data: PutData<'_>,
        flags: u32,
    ) -> Result<usize> {
        let mut txn = FailOnPanic(self);
        let rc = txn.put_at0(mc, key, data, flags);
        if let Err(e) = &rc {
            txn.fail_unless_clean(e);
        }
        rc
    }

    fn put_at0(&mut self, mc: CRef, key: &[u8], data: PutData<'_>, flags: u32) -> Result<usize> {
        let mut flags = flags;
        let mut dcount = 0;
        // Check this first so counter will always be zero on any early
        // failures.
        if flags & MDB_MULTIPLE != 0 {
            if data.count == 0 {
                return Err(Error::einval());
            }
            dcount = data.count;
            if u32::from(self.db_of(mc).flags) & MDB_DUPFIXED == 0 {
                return Err(MdbError::Incompatible.into());
            }
            match data.size.checked_mul(dcount) {
                None => return Err(MdbError::BadValSize.into()),
                Some(total) if total > data.bytes.len() => return Err(Error::einval()),
                Some(_) => {}
            }
        }
        let nospill = flags & MDB_NOSPILL != 0;
        flags &= !MDB_NOSPILL;
        self.check_usable()?;
        // ENV_MAXKEY(env)
        let max_key = self.core.env.max_key_size();
        if key.len().wrapping_sub(1) >= max_key {
            return Err(MdbError::BadValSize.into());
        }
        let db = self.db_of(mc);
        let dupsort = u32::from(db.flags) & MDB_DUPSORT != 0;
        if !self.cur(mc).is_sub() && u32::from(db.flags) & MDB_DUPFIXED != 0 && !dupsort {
            // DUPFIXED needs DUPSORT (lmdb.h). C builds P_LEAF2 pages with a
            // key size of 0 and corrupts them (KNOWN_C_BUGS.md #17).
            return Err(MdbError::Incompatible.into());
        }
        if dupsort && data.size.wrapping_sub(1) >= max_key {
            return Err(MdbError::BadValSize.into());
        }
        if data.size > MAXDATASIZE {
            return Err(MdbError::BadValSize.into());
        }

        let pos = self.put_position(mc, key, db.root, flags)?;
        if self.cur(mc).flags & C_DEL != 0 {
            self.cur_mut(mc).flags ^= C_DEL;
        }
        if dupsort && u32::from(db.flags) & MDB_DUPFIXED != 0 && pos == Pos::Found {
            self.check_dupfixed_size(mc, data.size)?;
        }

        // Cursor is positioned, check for room in the dirty list.
        if !nospill {
            let rsize = if flags & MDB_MULTIPLE != 0 {
                data.size * dcount
            } else {
                data.size
            };
            self.spill_check(mc, db.depth, Some((key.len(), rsize)))?;
        }

        if pos == Pos::NoRoot {
            // New database, write a root leaf page.
            let np = self.page_new(mc, P_LEAF, 1)?;
            self.cur_mut(mc).push(np)?;
            let db = self.db_mut(mc);
            db.root = np;
            db.depth = db.depth.wrapping_add(1);
            let leaf2 = u32::from(db.flags) & (MDB_DUPSORT | MDB_DUPFIXED) == MDB_DUPFIXED;
            self.set_dbflag(mc, DB_DIRTY);
            if leaf2 {
                let l = self.layout();
                let p = self.dirty_mut(np)?;
                page_ops::set_flags(l, p, page_ops::flags(l, p) | P_LEAF2);
            }
            self.cur_mut(mc).flags |= C_INITIALIZED;
        } else {
            self.cursor_touch(mc)?;
        }

        let mut insert_key = pos != Pos::Found;
        let mut insert_data = insert_key;
        let mut mcount = 0;
        let mut first = true;
        loop {
            let item = data.item(mcount);
            let (done_rc, did_insert_data) = self.put_item(
                mc,
                &db,
                key,
                item,
                &mut flags,
                insert_key,
                insert_data,
                first,
            )?;
            insert_data = did_insert_data;
            if let Some(rc) = done_rc {
                return rc.map(|()| mcount);
            }
            if insert_data {
                let db = self.db_mut(mc);
                db.entries = db.entries.wrapping_add(1);
            }
            if insert_key {
                self.cur_mut(mc).flags |= C_INITIALIZED;
            }
            if flags & MDB_MULTIPLE != 0 {
                mcount += 1;
                if mcount < dcount {
                    insert_key = false;
                    insert_data = false;
                    first = false;
                    continue;
                }
            }
            return Ok(mcount);
        }
    }

    /// The positioning step of `mdb_cursor_put`.
    fn put_position(&mut self, mc: CRef, key: &[u8], root: u64, flags: u32) -> Result<Pos> {
        if flags & MDB_CURRENT != 0 {
            if !self.cur(mc).is_init() {
                return Err(Error::einval());
            }
            // C does not check that the cursor is on an item and asserts
            // in mdb_node_del (or corrupts the page without assertions);
            // KNOWN_C_BUGS.md #13.
            if self.cur(mc).ki_top() >= self.top_page(mc)?.num_keys() {
                return Err(MdbError::NotFound.into());
            }
            return Ok(Pos::Found);
        }
        if root == P_INVALID {
            // New database, cursor has nothing to point to.
            let w = self.cur_mut(mc);
            w.snum = 0;
            w.top = 0;
            w.flags &= !C_INITIALIZED;
            return Ok(Pos::NoRoot);
        }
        let rc = if flags & MDB_APPEND != 0 {
            let cmp = self.cmp_of(mc);
            let ord = self.read_op(mc, |txn, c, mx| {
                Ok(match cursor_last_for_append(txn, c, mx)? {
                    Some(kv) => cmp.compare(key, kv.key),
                    // An empty tree (see `cursor_last_for_append`): the
                    // cursor is before its first slot, append there.
                    None => Ordering::Greater,
                })
            })?;
            if ord == Ordering::Greater {
                let w = self.cur_mut(mc);
                w.ki[w.top] = w.ki[w.top].wrapping_add(1);
                Err(Error::from(MdbError::NotFound))
            } else {
                // New key is <= last key.
                Err(MdbError::KeyExist.into())
            }
        } else {
            let dcmp = if self.cur(mc).is_sub() {
                None
            } else {
                self.dcmp_of(mc)
            };
            self.read_op(mc, |txn, c, mx| {
                cursor_set(txn, c, mx, dcmp, key, None, CursorOp::Set, true).map(|_| ())
            })
        };
        match rc {
            Ok(()) if flags & MDB_NOOVERWRITE != 0 => Err(MdbError::KeyExist.into()),
            Ok(()) => Ok(Pos::Found),
            Err(e) if e.not_found() => Ok(Pos::NotFound),
            Err(e) => Err(e),
        }
    }

    /// `MDB_DUPFIXED` items must all have one size. C does not check and
    /// copies `md_pad` bytes regardless (reading past a shorter item), so a
    /// mismatch would corrupt the sub-page; we fail with `MDB_BAD_VALSIZE`
    /// before changing anything (`KNOWN_C_BUGS.md` #12).
    fn check_dupfixed_size(&self, mc: CRef, size: usize) -> Result<()> {
        if self.cur(mc).is_sub() {
            return Ok(());
        }
        let page = self.top_page(mc)?;
        let Some(node) = page.node(self.cur(mc).ki_top()) else {
            return Ok(());
        };
        let existing = if !node.is_dupdata() {
            node.data_size()
        } else {
            let data = node.inline_data().ok_or_else(corrupted)?;
            if node.is_subdata() {
                Db::read(data).ok_or_else(corrupted)?.pad as usize
            } else {
                // A sub-page has the layout of the page it is stored in.
                PageRef::with_layout(data, node.layout())
                    .ok_or_else(corrupted)?
                    .pad() as usize
            }
        };
        if existing != size {
            return Err(MdbError::BadValSize.into());
        }
        Ok(())
    }

    /// The start of `mdb_page_spill`: estimate how many pages the operation
    /// of `mc` may dirty (`depth` = its tree's `md_depth`, `kd` = key and
    /// data size of a put) and spill dirty pages to disk if the dirty list
    /// does not have room for that many.
    pub(crate) fn spill_check(
        &mut self,
        mc: CRef,
        depth: u16,
        kd: Option<(usize, usize)>,
    ) -> Result<()> {
        if self.cur(mc).is_sub() {
            return Ok(());
        }
        let ps = self.psize();
        let mut i = usize::from(depth);
        // Named DBs also dirty the main DB.
        if self.cur(mc).dbi >= CORE_DBS as usize {
            i += usize::from(self.core.dbs.borrow()[MAIN_DBI as usize].db.depth);
        }
        // For puts, roughly factor in the key+data size.
        if let Some((k, d)) = kd {
            i += (self.layout().leaf_size(k, d) + ps) / ps;
        }
        i += i; // double it for good measure
        if self.w.dirty_room > i {
            return Ok(());
        }
        self.page_spill(mc, i)
    }

    /// One pass of `mdb_cursor_put` from the insert/overwrite decision on
    /// (the body that `MDB_MULTIPLE` repeats via `goto more`). `db` is the
    /// tree's record as the put began (only its flags and `md_pad` are
    /// used, which a put does not change). Returns `(Some(rc), _)` when the
    /// put is finished with `rc`, else `(None, insert_data)` for the
    /// caller's bookkeeping.
    #[allow(clippy::too_many_arguments)]
    fn put_item(
        &mut self,
        mc: CRef,
        db: &Db,
        key: &[u8],
        item: NodeVal<'_>,
        flags: &mut u32,
        insert_key: bool,
        insert_data: bool,
        first: bool,
    ) -> Result<(Option<Result<()>>, bool)> {
        let dupsort = u32::from(db.flags) & MDB_DUPSORT != 0;
        let mut do_sub = None;
        // The node data a `DupStep::NewSub` built (a scratch buffer).
        let mut new_data = None;

        if insert_key {
            // The key does not exist.
            let nodemax = self.core.env.node_max();
            if dupsort && self.layout().leaf_size(key.len(), item.size) > nodemax {
                // Too big for a node, insert in sub-DB.
                match self.dup_prepare(mc, item, flags, true, true)? {
                    DupStep::NewSub {
                        xdata,
                        sub_root,
                        dkey,
                    } => {
                        new_data = Some(xdata);
                        do_sub = Some((sub_root, dkey));
                    }
                    _ => return Err(corrupted()),
                }
            }
        } else {
            if first && self.top_page(mc)?.is_leaf2() {
                // There's only a key anyway, so this is a no-op.
                let ksize = db.pad as usize;
                if key.len() != ksize {
                    return Err(MdbError::BadValSize.into());
                }
                let ki = self.cur(mc).ki_top();
                let start = self.layout().page_hdr + ki * ksize;
                let page = self.top_page_mut(mc)?;
                page.get_mut(start..start + ksize)
                    .ok_or_else(corrupted)?
                    .copy_from_slice(key);
                return Ok((Some(self.fix_parent(mc, key)), insert_data));
            }
            let mut go_current = !dupsort;
            if dupsort {
                match self.dup_prepare(mc, item, flags, false, false)? {
                    DupStep::Current => go_current = true,
                    DupStep::PutSub => {
                        let (rc, ins) = self.put_sub(mc, item, *flags, None, None, false);
                        return self.after_sub(mc, rc, ins, false);
                    }
                    DupStep::Replace => {
                        self.del_at(mc, 0)?;
                        let rc = self.put_at(mc, key, PutData::bytes(item.bytes), 0);
                        return Ok((Some(rc.map(|_| ())), false));
                    }
                    DupStep::NewSub {
                        xdata,
                        sub_root,
                        dkey,
                    } => {
                        new_data = Some(xdata);
                        do_sub = Some((sub_root, dkey));
                    }
                }
            }
            if go_current {
                match self.put_current(mc, key, item, *flags)? {
                    // An item equal to the key's only one: C returns here
                    // even for MDB_MULTIPLE, so the later items are never
                    // written (and the count says so: KNOWN_C_BUGS.md #30).
                    CurStep::Done if *flags & MDB_MULTIPLE != 0 => return Ok((None, false)),
                    CurStep::Done => return Ok((Some(Ok(())), insert_data)),
                    CurStep::Reinsert => {}
                }
            }
        }

        // new_sub:
        let node_val = match &new_data {
            Some(xdata) => NodeVal {
                size: xdata.len(),
                bytes: xdata,
            },
            None => item,
        };
        let rc = self.insert_leaf_node(mc, key, node_val, *flags, insert_key);
        if let Some(xdata) = new_data {
            self.give_scratch(xdata);
        }
        if let Err(e) = rc {
            self.w.flags |= TXN_ERROR;
            return Ok((Some(Err(e)), insert_data));
        }
        match do_sub {
            None => Ok((None, insert_data)),
            Some((sub_root, dkey)) => {
                let (rc, ins) = self.put_sub(mc, item, *flags, dkey, sub_root, insert_key);
                self.after_sub(mc, rc, ins, insert_key)
            }
        }
    }

    /// The tail of `mdb_cursor_put` after storing into the sub-tree:
    /// count the item, handle a failure that left an empty sub-DB.
    fn after_sub(
        &mut self,
        mc: CRef,
        rc: Result<()>,
        insert_data: bool,
        insert_key: bool,
    ) -> Result<(Option<Result<()>>, bool)> {
        match rc {
            Ok(()) => Ok((None, insert_data)),
            Err(e) => {
                if insert_data {
                    let db = self.db_mut(mc);
                    db.entries = db.entries.wrapping_add(1);
                }
                if insert_key {
                    // bad_sub: invalidate txn if we created an empty sub-DB.
                    let e = if matches!(e, Error::Mdb(MdbError::KeyExist)) {
                        MdbError::Corrupted.into()
                    } else {
                        e
                    };
                    self.w.flags |= TXN_ERROR;
                    return Ok((Some(Err(e)), false));
                }
                Ok((Some(Err(e)), false))
            }
        }
    }

    /// `fix_parent:` in `mdb_cursor_put`: after overwriting slot 0 of a
    /// leaf, update the branch key above (slot 0 of a branch page has an
    /// empty key, so find the real slot).
    fn fix_parent(&mut self, mc: CRef, key: &[u8]) -> Result<()> {
        let w = self.cur(mc);
        if w.top > 0 && w.ki[w.top] == 0 {
            let mut dtop = 1;
            let mut top = w.top - 1;
            while top > 0 && w.ki[top] == 0 {
                top -= 1;
                dtop += 1;
            }
            let do_update = w.ki[top] != 0;
            self.cur_mut(mc).top = top;
            let rc = if do_update {
                self.update_key(mc, key)
            } else {
                Ok(())
            };
            self.cur_mut(mc).top += dtop;
            rc?;
        }
        Ok(())
    }

    /// The `current:` stage: replace the data of the existing node in place
    /// if possible.
    fn put_current(
        &mut self,
        mc: CRef,
        key: &[u8],
        item: NodeVal<'_>,
        flags: u32,
    ) -> Result<CurStep> {
        let ps = self.psize();
        let l = self.layout();
        let ki = self.cur(mc).ki_top();
        let (nflags, ksize, olddata_size, ovref) = {
            let page = self.top_page(mc)?;
            let n = page.node(ki).ok_or_else(corrupted)?;
            (n.flags(), n.key_size(), n.data_size(), n.overflow_ref())
        };
        // LMDB passes F_SUBDATA in 'flags' to write a DB record.
        if (u32::from(nflags) ^ flags) & u32::from(F_SUBDATA) != 0 {
            return Err(MdbError::Incompatible.into());
        }
        if nflags & F_BIGDATA != 0 {
            // Overflow page overwrites need special handling.
            let ovref = ovref.ok_or_else(corrupted)?;
            let pg = ovref.pgno;
            let dpages = l.overflow_pages_for(item.size, ps);
            let (ovpages, mut dirty) = {
                let omp = self.core.page(pg)?;
                // 1.0 takes the run length from the node's `op_pages`, 0.9
                // from the page (the same number in a sound file).
                let pages = match l.format {
                    Format::V09 => omp.overflow_pages() as usize,
                    Format::V10 => usize::try_from(ovref.pages).unwrap_or(usize::MAX),
                };
                (pages, omp.flags() & P_DIRTY != 0)
            };
            let mut level = self.core.page_level(pg);
            // Is the ov page large enough?
            if ovpages >= dpages && !dirty && level != 0 {
                // Spilled by this txn or an ancestor: make it dirty again.
                dirty = self.page_unspill(pg)?;
                level = 0; // dirty in this txn or clean
            }
            // Is it dirty? Yes, overwrite it. Note in this case we don't
            // bother to try shrinking the page if the new data is smaller
            // than the overflow threshold.
            if ovpages >= dpages && dirty {
                if level > 1 {
                    // It is writable only in a parent txn: copy it (it is
                    // already counted in the parent's dirty room).
                    self.copy_ancestor_page(pg)?;
                }
                let page = self.top_page_mut(mc)?;
                page_ops::set_node_dsize(l, page, ki, item.size).ok_or_else(corrupted)?;
                if flags & MDB_RESERVE == 0 {
                    let run = self.dirty_mut(pg)?;
                    run.get_mut(l.page_hdr..l.page_hdr + item.size)
                        .ok_or_else(corrupted)?
                        .copy_from_slice(item.bytes);
                }
                return Ok(CurStep::Done);
            }
            self.ovpage_free(mc, pg)?;
        } else if item.size == olddata_size {
            // Same size, just replace it. (Smaller data could reuse the
            // node too, but C opts to shrink the node in that case.)
            if flags & MDB_RESERVE != 0 {
                return Ok(CurStep::Done);
            }
            if !self.cur(mc).is_sub() {
                let page = self.top_page_mut(mc)?;
                let range = page_ops::node_data_range(l, page, ki).ok_or_else(corrupted)?;
                page[range].copy_from_slice(item.bytes);
                return Ok(CurStep::Done);
            }
            if key.len() == ksize {
                let page = self.top_page_mut(mc)?;
                // NODEKEY: the key follows the node header.
                let off = page_ops::ptr(l, page, ki).ok_or_else(corrupted)? + NODE_SIZE;
                page.get_mut(off..off + ksize)
                    .ok_or_else(corrupted)?
                    .copy_from_slice(key);
                self.fix_parent(mc, key)?;
                return Ok(CurStep::Done);
            }
        }
        // new_ksize:
        self.node_del(mc, 0)?;
        Ok(CurStep::Reinsert)
    }

    /// The DUPSORT stage of `mdb_cursor_put` ("DB has dups?"): prepare the
    /// node that will hold the duplicates. With `big_new`, a new key whose
    /// item does not fit a node goes straight to a sub-DB. The buffers of a
    /// [`DupStep::NewSub`] are scratch buffers ([`RwTxn::take_scratch`]).
    fn dup_prepare(
        &mut self,
        mc: CRef,
        item: NodeVal<'_>,
        flags: &mut u32,
        insert_key: bool,
        big_new: bool,
    ) -> Result<DupStep> {
        // The current node's data is copied here (C reads it in place); a
        // step that keeps it takes it out, else it is given back.
        let mut olddata = self.take_scratch();
        let step = self.dup_prepare0(mc, item, flags, insert_key, big_new, &mut olddata);
        self.give_scratch(olddata);
        step
    }

    fn dup_prepare0(
        &mut self,
        mc: CRef,
        item: NodeVal<'_>,
        flags: &mut u32,
        insert_key: bool,
        big_new: bool,
        olddata: &mut Vec<u8>,
    ) -> Result<DupStep> {
        let ps = self.psize();
        let l = self.layout();
        let nodemax = self.core.env.node_max();
        let dbflags = u32::from(self.db_of(mc).flags);
        let dupfixed = dbflags & MDB_DUPFIXED != 0;
        let leaf_pgno = self.cur(mc).pg_top();
        let ki = self.cur(mc).ki_top();

        // fp: the old sub-page (or a header faking one); olddata_size its
        // size; offset: growth in page size; xsize: size of the new node data.
        let mut dkey = None;
        let mut offset = 0usize;
        let fp: Vec<u8>;
        let mut to_subdb = big_new;
        let xsize;
        if big_new {
            // Set up an empty "old sub-page" for prep_subDB to expand to a
            // full page.
            // (P_DIRTY: the sub-DB root built from it is a dirty page.)
            // C writes the size into its scratch page (`me_pbuf`), where
            // it stays for later sub-pages (`WriterState::pbuf_pad`).
            self.w.pbuf_pad = item.size as u16;
            let mut h = self.take_scratch();
            h.resize(l.page_hdr, 0);
            page_ops::set_flags(l, &mut h, P_LEAF | P_DIRTY);
            page_ops::set_pad(l, &mut h, self.w.pbuf_pad);
            page_ops::set_lower(l, &mut h, l.page_hdr);
            page_ops::set_upper(l, &mut h, l.page_hdr);
            fp = h;
            xsize = 0;
        } else {
            let (nflags, ksize) = {
                let page = self.top_page(mc)?;
                let n = page.node(ki).ok_or_else(corrupted)?;
                olddata.extend_from_slice(n.inline_data().ok_or_else(corrupted)?);
                (n.flags(), n.key_size())
            };
            if nflags & F_DUPDATA == 0 {
                // Was a single item before, must convert now.
                if *flags == MDB_CURRENT {
                    return Ok(DupStep::Current);
                }
                let mut dcmp = self.dcmp_of(mc).unwrap_or(Comparator::Memn);
                if dcmp.is_int() && olddata.len() == 8 {
                    dcmp = Comparator::Clong;
                }
                // Does data match?
                if dcmp.compare(item.bytes, olddata) == Ordering::Equal {
                    if *flags & (MDB_NODUPDATA | MDB_APPENDDUP) != 0 {
                        return Err(MdbError::KeyExist.into());
                    }
                    // Overwrite it.
                    return Ok(DupStep::Current);
                }
                // Make sub-page header for the dup items, with dummy body.
                let mut size = l.page_hdr + olddata.len() + item.size;
                let mut pflags = l.subpage_flags;
                if dupfixed {
                    pflags |= P_LEAF2;
                    self.w.pbuf_pad = item.size as u16;
                    size += 2 * item.size; // leave space for 2 more
                } else {
                    size += 2 * (2 + NODE_SIZE) + (olddata.len() & 1) + (item.size & 1);
                }
                let mut h = self.take_scratch();
                h.resize(size, 0);
                page_ops::set_pgno(&mut h, leaf_pgno);
                // 1.0 also stamps the sub-page with the current txnid.
                l.set_txnid(&mut h, self.core.txnid);
                // Without MDB_DUPFIXED C leaves mp_pad as its scratch page
                // has it: whatever an earlier put left there.
                page_ops::set_pad(l, &mut h, self.w.pbuf_pad);
                page_ops::set_flags(l, &mut h, pflags);
                page_ops::set_lower(l, &mut h, l.page_hdr);
                page_ops::set_upper(l, &mut h, size);
                fp = h;
                xsize = size;
                dkey = Some(std::mem::take(olddata));
                // mp == fp here: the fake page is the new node data.
                if NODE_SIZE + ksize + xsize > nodemax {
                    to_subdb = true;
                }
            } else if nflags & F_SUBDATA != 0 {
                // Data is on sub-DB, just store it.
                *flags |= u32::from(F_DUPDATA | F_SUBDATA);
                return Ok(DupStep::PutSub);
            } else {
                // Data is on sub-page.
                let mut fits = *flags == MDB_CURRENT;
                if fits && !self.current_dup_fits(mc, olddata, item.size) {
                    // C writes the new item in place and, if it does not fit,
                    // splits the sub-page as if it were a page (leaking pages
                    // and losing the item). Do what lmdb.h promises instead:
                    // delete the old item, then insert (KNOWN_C_BUGS.md #15).
                    return Ok(DupStep::Replace);
                }
                if !fits {
                    if !dupfixed {
                        let left = page_ops::size_left(l, olddata);
                        offset = even(NODE_SIZE + 2 + item.size);
                        // If there's enough space, just use it, else grow
                        // by whatever we need.
                        offset = offset.saturating_sub(left);
                    } else {
                        offset = usize::from(page_ops::pad(l, olddata));
                        if page_ops::size_left(l, olddata) < offset {
                            offset *= 4; // space for 4 more
                        } else {
                            // Big enough MDB_DUPFIXED sub-page.
                            fits = true;
                        }
                    }
                }
                if fits {
                    let page = self.top_page_mut(mc)?;
                    let range = page_ops::node_data_range(l, page, ki).ok_or_else(corrupted)?;
                    let sp = &mut page[range];
                    page_ops::set_flags(l, sp, page_ops::flags(l, sp) | l.subpage_dirty());
                    page_ops::set_pgno(sp, leaf_pgno);
                    let x = &mut self.cur_mut(CRef::sub(mc.slot));
                    x.pg[0] = SUBPAGE;
                    x.subp = true;
                    *flags |= u32::from(F_DUPDATA);
                    return Ok(DupStep::PutSub);
                }
                xsize = olddata.len() + offset;
                if NODE_SIZE + ksize + xsize > nodemax {
                    to_subdb = true;
                }
                fp = std::mem::take(olddata);
            }
        }
        let olddata_size = if big_new { l.page_hdr } else { fp.len() };
        let mut fp_flags = page_ops::flags(l, &fp);
        let fp_pad = page_ops::pad(l, &fp);

        let (xdata, sub_root) = if to_subdb {
            // Too big for a sub-page, convert to sub-DB (prep_subDB).
            fp_flags &= !P_SUBP;
            let mut dummy = Db::default();
            if dupfixed {
                fp_flags |= P_LEAF2;
                dummy.pad = u32::from(fp_pad);
                dummy.flags = MDB_DUPFIXED as u16;
                if dbflags & MDB_INTEGERDUP != 0 {
                    dummy.flags |= MDB_INTEGERKEY as u16;
                }
            }
            dummy.depth = 1;
            dummy.leaf_pages = 1;
            dummy.entries = page_ops::num_keys(l, &fp) as u64;
            let mp = self.page_alloc(1)?;
            offset = ps - olddata_size;
            *flags |= u32::from(F_DUPDATA | F_SUBDATA);
            dummy.root = mp;
            let db = self.db_mut(mc);
            db.leaf_pages = db.leaf_pages.wrapping_add(1);
            let page = self.dirty_mut(mp)?;
            let sub = SubpageSrc {
                fp: &fp,
                olddata_size,
                fp_flags,
                fp_pad,
            };
            // A regular dirty page: P_DIRTY in every format.
            build_subpage(l, &mut page[..ps], sub, offset, P_DIRTY).ok_or_else(corrupted)?;
            self.give_scratch(fp);
            let mut rec = self.take_scratch();
            rec.extend_from_slice(&dummy.to_bytes());
            (rec, Some(mp))
        } else if dkey.is_some() {
            // Converting a single item: the fake sub-page is the data.
            (fp, None)
        } else {
            // Grow the sub-page into a new buffer.
            // (C builds it in its scratch page, which keeps the pad.)
            self.w.pbuf_pad = fp_pad;
            let mut mp = self.take_scratch();
            mp.resize(xsize, 0);
            page_ops::set_pgno(&mut mp, leaf_pgno);
            l.set_txnid(&mut mp, self.core.txnid);
            let sub = SubpageSrc {
                fp: &fp,
                olddata_size,
                fp_flags,
                fp_pad,
            };
            build_subpage(l, &mut mp, sub, offset, l.subpage_dirty()).ok_or_else(corrupted)?;
            self.give_scratch(fp);
            (mp, None)
        };
        *flags |= u32::from(F_DUPDATA);
        if !insert_key {
            self.node_del(mc, 0)?;
        }
        Ok(DupStep::NewSub {
            xdata,
            sub_root,
            dkey,
        })
    }

    /// The `new_sub:` stage: add the leaf node for `key` at the cursor
    /// position, splitting the page if it has no room, and move other
    /// cursors on the page past the new node.
    fn insert_leaf_node(
        &mut self,
        mc: CRef,
        key: &[u8],
        rdata: NodeVal<'_>,
        flags: u32,
        insert_key: bool,
    ) -> Result<()> {
        let mut nflags = flags & NODE_ADD_FLAGS;
        let nodemax = self.core.env.node_max();
        let (is_leaf2, room) = {
            let p = self.top_page(mc)?;
            (p.is_leaf2(), p.size_left())
        };
        let nsize = if is_leaf2 {
            key.len()
        } else {
            leaf_size(self.layout(), nodemax, key.len(), rdata.size)
        };
        if room < nsize {
            if flags & u32::from(F_DUPDATA | F_SUBDATA) == u32::from(F_DUPDATA) {
                nflags &= !MDB_APPEND; // sub-page may need room to grow
            }
            if !insert_key {
                nflags |= SPLIT_REPLACE;
            }
            return self.page_split(mc, key, Some(rdata), P_INVALID, nflags);
        }
        // There is room already in this leaf page.
        let ki = self.cur(mc).ki_top();
        self.node_add(mc, ki, key, Some(rdata), 0, nflags)?;
        // Adjust other cursors pointing to mp.
        let (i, snum, dbi, sub) = {
            let w = self.cur(mc);
            (w.top, w.snum, w.dbi, w.is_sub())
        };
        let mp = self.page_id(mc, i);
        let mc_ki = self.cur(mc).ki[i];
        for s in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(s, dbi, sub) else {
                continue;
            };
            if m3 == mc || self.cur(m3).snum < snum || self.page_id(m3, i) != mp {
                continue;
            }
            let w3 = self.cur_mut(m3);
            if w3.ki[i] >= mc_ki && insert_key {
                w3.ki[i] = w3.ki[i].wrapping_add(1);
            }
        }
        Ok(())
    }

    /// The `put_sub:` stage: store the item(s) in the duplicates' sub-tree
    /// through the sub-cursor, then write the sub-DB record back into the
    /// node. Returns the result and whether an item was added.
    fn put_sub(
        &mut self,
        mc: CRef,
        item: NodeVal<'_>,
        flags: u32,
        dkey: Option<Vec<u8>>,
        sub_root: Option<u64>,
        insert_key: bool,
    ) -> (Result<()>, bool) {
        match self.put_sub0(mc, item, flags, dkey, sub_root, insert_key) {
            Ok(r) => r,
            Err(e) => (Err(e), false),
        }
    }

    #[allow(clippy::type_complexity)]
    fn put_sub0(
        &mut self,
        mc: CRef,
        item: NodeVal<'_>,
        flags: u32,
        dkey: Option<Vec<u8>>,
        sub_root: Option<u64>,
        _insert_key: bool,
    ) -> Result<(Result<()>, bool)> {
        let mx = CRef::sub(mc.slot);
        let leaf_flags = {
            let p = self.top_page(mc)?;
            p.node(self.cur(mc).ki_top()).ok_or_else(corrupted)?.flags()
        };
        let mut xflags = if flags & (MDB_CURRENT | MDB_APPENDDUP) == MDB_CURRENT {
            // The sub-cursor keeps its position; make sure its record is
            // this key's (KNOWN_C_BUGS.md #14).
            self.refresh_xdb(mc)?;
            MDB_CURRENT | MDB_NOSPILL
        } else {
            self.xcursor_init1(mc)?;
            if flags & MDB_NODUPDATA != 0 {
                MDB_NOOVERWRITE | MDB_NOSPILL
            } else {
                MDB_NOSPILL
            }
        };
        if let Some(root) = sub_root {
            let x = self.cur_mut(mx);
            x.pg[0] = root;
            x.subp = false;
        }
        let new_dupdata = dkey.is_some();
        // Converted, write the original data first.
        if let Some(dk) = dkey {
            let rc = self.put_at(mx, &dk, PutData::empty(), xflags);
            self.give_scratch(dk);
            if let Err(e) = rc {
                let e = if matches!(e, Error::Mdb(MdbError::KeyExist)) {
                    MdbError::Corrupted.into()
                } else {
                    e
                };
                self.w.flags |= TXN_ERROR;
                return Err(e);
            }
        }
        if leaf_flags & F_SUBDATA == 0 || sub_root.is_some() {
            // Adjust other cursors pointing to mp.
            let (i, snum, dbi, mc_ki) = {
                let w = self.cur(mc);
                (w.top, w.snum, w.dbi, w.ki[w.top])
            };
            let mp = self.cur(mc).pg[i];
            for s in 0..self.w.cursors.len() {
                let Some(m2) = self.w.cursors.peer(s, dbi, false) else {
                    continue;
                };
                let w2 = self.cur(m2);
                if m2 == mc || w2.snum < snum || !w2.is_init() {
                    continue;
                }
                if w2.pg[i] == mp && w2.ki[i] == mc_ki {
                    self.xcursor_init2(s, mc.slot, new_dupdata);
                }
            }
        }
        let ecount = self.db_of(mx).entries;
        if flags & MDB_APPENDDUP != 0 {
            xflags |= MDB_APPEND;
        }
        let key = if item.bytes.len() == item.size {
            item.bytes
        } else {
            return Err(Error::einval());
        };
        let rc = self.put_at(mx, key, PutData::empty(), xflags).map(|_| ());
        if flags & u32::from(F_SUBDATA) != 0 {
            self.store_subdb_record(mc)?;
        }
        let insert_data = self.db_of(mx).entries != ecount;
        self.share_xdb(mc);
        Ok((rc, insert_data))
    }

    /// `mdb_subdb_adjust` + `memcpy(NODEDATA(leaf), &mx_db)`: write the
    /// sub-cursor's `MDB_db` into the node at `mc`, adding the change of its
    /// page counts to the main DB's (ITS#9388: a DUPSORT DB's counts
    /// include its sub-DBs' pages).
    pub(crate) fn store_subdb_record(&mut self, mc: CRef) -> Result<()> {
        let new = self.db_of(CRef::sub(mc.slot));
        let ki = self.cur(mc).ki_top();
        let old = {
            let p = self.top_page(mc)?;
            let n = p.node(ki).ok_or_else(corrupted)?;
            Db::read(n.inline_data().ok_or_else(corrupted)?).ok_or_else(corrupted)?
        };
        let db = self.db_mut(mc);
        db.branch_pages = db
            .branch_pages
            .wrapping_add(new.branch_pages.wrapping_sub(old.branch_pages));
        db.leaf_pages = db
            .leaf_pages
            .wrapping_add(new.leaf_pages.wrapping_sub(old.leaf_pages));
        let l = self.layout();
        let page = self.top_page_mut(mc)?;
        let range = page_ops::node_data_range(l, page, ki).ok_or_else(corrupted)?;
        new.write(&mut page[range]);
        Ok(())
    }

    // --- nodes and pages ---------------------------------------------------------

    /// `mdb_page_new`: allocate `num` pages and initialise them as an empty
    /// page of type `flags`, counting them in the cursor's DB record.
    pub(crate) fn page_new(&mut self, mc: CRef, flags: u16, num: usize) -> Result<u64> {
        let np = self.page_alloc(num)?;
        let ps = self.psize();
        let l = self.layout();
        let page = self.dirty_mut(np)?;
        page_ops::set_flags(l, page, flags | P_DIRTY);
        page_ops::set_lower(l, page, l.page_hdr);
        page_ops::set_upper(l, page, ps);
        if flags & P_OVERFLOW != 0 {
            page_ops::set_overflow_pages(l, page, num as u32);
        }
        let db = self.db_mut(mc);
        if flags & P_BRANCH != 0 {
            db.branch_pages = db.branch_pages.wrapping_add(1);
        } else if flags & P_LEAF != 0 {
            db.leaf_pages = db.leaf_pages.wrapping_add(1);
        } else if flags & P_OVERFLOW != 0 {
            db.overflow_pages = db.overflow_pages.wrapping_add(num as u64);
        }
        Ok(np)
    }

    /// `mdb_node_add` on the top page of `mc`.
    pub(crate) fn node_add(
        &mut self,
        mc: CRef,
        indx: usize,
        key: &[u8],
        data: Option<NodeVal<'_>>,
        pgno: u64,
        flags: u32,
    ) -> Result<()> {
        self.node_add_to(mc, None, indx, key, data, pgno, flags)
    }

    /// `mdb_node_add` (mdb.c line 7475): add a node at `indx` of the top
    /// page of `mc`, or of `scratch` (the temporary page of a split). Leaf
    /// data too big for a node goes to newly allocated overflow pages.
    /// `MDB_PAGE_FULL` (and `MDB_TXN_ERROR`) if the node does not fit; the
    /// callers check the room first, so that means corruption.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn node_add_to(
        &mut self,
        mc: CRef,
        scratch: Option<&mut [u8]>,
        indx: usize,
        key: &[u8],
        data: Option<NodeVal<'_>>,
        pgno: u64,
        flags: u32,
    ) -> Result<()> {
        let ps = self.psize();
        let l = self.layout();
        let nodemax = self.core.env.node_max();
        let (is_leaf2, is_leaf, room) = {
            let p = match scratch.as_deref() {
                Some(buf) => PageRef::with_layout(buf, l).ok_or_else(corrupted)?,
                None => self.top_page(mc)?,
            };
            (p.is_leaf2(), p.is_leaf(), p.size_left() as isize - 2)
        };
        if is_leaf2 {
            // Move higher keys up one slot and insert the new key.
            let ksize = self.db_of(mc).pad as usize;
            if key.len() != ksize {
                self.w.flags |= TXN_ERROR;
                return Err(MdbError::BadValSize.into());
            }
            let page = match scratch {
                Some(buf) => buf,
                None => self.top_page_mut(mc)?,
            };
            return page_ops::insert_leaf2(l, page, indx, key, ksize)
                .ok_or_else(|| MdbError::PageFull.into());
        }
        let mut node_size = NODE_SIZE + l.node_key_len(key.len());
        let mut flags = flags;
        let mut body = NodeBody::Branch(pgno);
        if is_leaf {
            let d = data.ok_or_else(corrupted)?;
            if flags & u32::from(F_BIGDATA) != 0 {
                // Data already on overflow page.
                node_size += l.ovpage_size;
                let r = l.read_overflow_ref(d.bytes).ok_or_else(corrupted)?;
                body = NodeBody::Overflow { r, size: d.size };
            } else if node_size + d.size > nodemax {
                // Put data on overflow page.
                let ovpages = l.overflow_pages_for(d.size, ps);
                node_size = even(node_size + l.ovpage_size);
                if node_size as isize > room {
                    return self.page_full();
                }
                let ofp = self.page_new(mc, P_OVERFLOW, ovpages)?;
                if flags & MDB_RESERVE == 0 {
                    let run = self.dirty_mut(ofp)?;
                    run.get_mut(l.page_hdr..l.page_hdr + d.size)
                        .ok_or_else(corrupted)?
                        .copy_from_slice(d.bytes);
                }
                flags |= u32::from(F_BIGDATA);
                let r = OverflowRef {
                    pgno: ofp,
                    txnid: self.core.txnid,
                    pages: ovpages as u64,
                };
                body = NodeBody::Overflow { r, size: d.size };
            } else {
                node_size += l.node_data_len(d.size);
                body = if flags & MDB_RESERVE != 0 {
                    NodeBody::Reserve(d.size)
                } else {
                    NodeBody::Inline(d.bytes)
                };
            }
        }
        node_size = even(node_size);
        if node_size as isize > room {
            return self.page_full();
        }
        let page = match scratch {
            Some(buf) => buf,
            None => self.top_page_mut(mc)?,
        };
        page_ops::insert_node(l, page, indx, node_size, key, flags as u16, body)
            .ok_or_else(corrupted)?;
        Ok(())
    }

    fn page_full(&mut self) -> Result<()> {
        self.w.flags |= TXN_ERROR;
        Err(MdbError::PageFull.into())
    }

    /// `mdb_node_del` on the top page of `mc`.
    pub(crate) fn node_del(&mut self, mc: CRef, ksize: usize) -> Result<()> {
        let ki = self.cur(mc).ki_top();
        let l = self.layout();
        let page = self.top_page_mut(mc)?;
        page_ops::delete_node(l, page, ki, ksize).ok_or_else(corrupted)
    }

    /// `mdb_update_key` (mdb.c line 7962): replace the key of the branch
    /// node at `mc`'s position; if the page has no room for a longer key,
    /// delete the node and re-insert it with a split.
    pub(crate) fn update_key(&mut self, mc: CRef, key: &[u8]) -> Result<()> {
        let ki = self.cur(mc).ki_top();
        let l = self.layout();
        let page = self.top_page_mut(mc)?;
        match page_ops::update_key(l, page, ki, key).ok_or_else(corrupted)? {
            KeyUpdate::Done => Ok(()),
            KeyUpdate::NeedsSplit => {
                // Not enough space left, do a delete and split.
                let pgno = self
                    .top_page(mc)?
                    .node(ki)
                    .ok_or_else(corrupted)?
                    .child_pgno();
                self.node_del(mc, 0)?;
                self.page_split(mc, key, None, pgno, SPLIT_REPLACE)
            }
        }
    }
}

/// The old sub-page `fp` that [`build_subpage`] grows: its bytes (of which
/// the first `olddata_size` are the sub-page; a header only when a new key
/// goes straight to a sub-DB), and its flags and `mp_pad` as the caller
/// adjusted them.
struct SubpageSrc<'a> {
    fp: &'a [u8],
    olddata_size: usize,
    fp_flags: u16,
    fp_pad: u16,
}

/// Builds a (sub-)page `mp` from the old sub-page `fp` grown by `offset`
/// bytes (the `if (mp != fp)` block of `mdb_cursor_put`): same header with
/// `mp_upper += offset`, the nodes moved to the end and their pointers
/// shifted, or the `P_LEAF2` keys copied. `dirty` is the `P_DIRTY` bit the
/// new page gets: always for a sub-DB root (an ordinary dirty page),
/// [`Layout::subpage_dirty`] for a sub-page.
fn build_subpage(
    l: &Layout,
    mp: &mut [u8],
    src: SubpageSrc<'_>,
    offset: usize,
    dirty: u16,
) -> Option<()> {
    let SubpageSrc {
        fp,
        olddata_size,
        fp_flags,
        fp_pad,
    } = src;
    let nkeys = page_ops::num_keys(l, fp);
    let fp_upper = page_ops::upper(l, fp);
    page_ops::set_flags(l, mp, fp_flags | dirty);
    page_ops::set_pad(l, mp, fp_pad);
    page_ops::set_lower(l, mp, page_ops::lower(l, fp));
    let mp_upper = fp_upper + offset;
    page_ops::set_upper(l, mp, mp_upper);
    if fp_flags & P_LEAF2 != 0 {
        let len = nkeys * usize::from(fp_pad);
        let hdr = l.page_hdr;
        mp.get_mut(hdr..hdr + len)?
            .copy_from_slice(fp.get(hdr..hdr + len)?);
    } else {
        let tail = fp.get(fp_upper..olddata_size)?;
        mp.get_mut(mp_upper..mp_upper + tail.len())?
            .copy_from_slice(tail);
        for i in 0..nkeys {
            let v = page_ops::ptr(l, fp, i)? + offset;
            page_ops::set_ptr(l, mp, i, v)?;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::layout::{V09, V10};

    #[test]
    fn sizes() {
        let l = &V09;
        let nodemax = l.node_max(4096);
        assert_eq!(leaf_size(l, nodemax, 3, 5), even(8 + 3 + 5 + 2));
        // Data too big for the node: only a page number is stored inline.
        assert_eq!(leaf_size(l, nodemax, 3, 5000), even(8 + 3 + 8 + 2));
        assert_eq!(branch_size(l, 5), 15);
    }

    #[test]
    fn sizes_v10() {
        let l = &V10;
        let nodemax = l.node_max(4096);
        assert_eq!(leaf_size(l, nodemax, 3, 5), even(8 + 4 + 6 + 2));
        // LEAFSIZE - dsize + sizeof(MDB_ovpage): the rounding of an odd
        // dsize stays in, as in C.
        assert_eq!(leaf_size(l, nodemax, 3, 5001), even(8 + 4 + 1 + 24 + 2));
        assert_eq!(branch_size(l, 5), 16);
    }
}
