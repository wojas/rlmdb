//! Cursors of a write transaction, and how the write path reaches pages.
//!
//! # Why write cursors hold page numbers
//!
//! A C cursor is a stack of page *pointers*. Writes move pages around: a
//! clean page is copied to a dirty buffer (`mdb_page_touch`), a page is
//! split or merged, a node moves. C keeps every open cursor of the
//! transaction valid by walking `txn->mt_cursors[dbi]` after each such
//! change and patching their `mc_pg`/`mc_ki` (the "cursor fixups").
//!
//! In safe Rust a cursor cannot hold a reference into a buffer that the same
//! transaction then mutates. So a write cursor ([`WCursor`]) stores page
//! *numbers*, which identify pages exactly as C's pointers do within one
//! transaction (a touched page gets a new number, just as it gets a new
//! pointer in C), and every access looks the page up again: the dirty list
//! first (directly through the page's slot there, if the cursor knows it;
//! see below), then the map (`TxnCore::page`). The one page without a
//! number is a
//! `P_SUBP` sub-page stored inside a leaf node; a sub-cursor on it records
//! [`SUBPAGE`] and the page is found through its main cursor's current
//! node. That makes C's `XCURSOR_REFRESH` (re-pointing a sub-cursor after
//! its node moved) implicit.
//!
//! # Dirty-list slots
//!
//! Next to each page number a write cursor keeps the page's slot in the
//! dirty list (`WCursor::pg_slot`, a [`PageSlot`]): the stable place of the
//! page's buffer in the transaction's list (see `super::dirty`). The
//! accessors here ([`RwTxn::page_of`], [`RwTxn::page_mut_of`],
//! [`RwTxn::page_touch`], the reload of read cursors) try that slot first
//! and reach a page of the transaction's own list in O(1); C follows a
//! pointer there. The slot is a hint, checked against the page number on
//! every use, so it needs no upkeep for correctness: wherever a page number
//! changes without the slot (a split, a merge, a fixup copying another
//! cursor's stack, a read operation moving the cursor) the old slot simply
//! misses and the page is searched by number, which is exactly what
//! happened before slots existed. Slots are recorded where they are known
//! for free: `mdb_page_touch` finds or creates the dirty page of every
//! level of a cursor before each change and stores its slot (also in the
//! other cursors it moves to a new page), and a write through a cursor
//! stores the slot it found. Spills, nested transactions and commits need
//! no special handling: see the invalidation rules in `super::dirty`.
//!
//! # Tracking
//!
//! All cursors of a write transaction live in an [`Arena`] owned by the
//! transaction; a cursor is named by a [`CRef`] (slot index, main or
//! sub-cursor). A slot is *tracked* when C would link it into
//! `mt_cursors[dbi]`: user cursors ([`RwCursor`](crate::core::RwCursor)),
//! the temporary cursor of `mdb_put`, and the temporary copies that
//! `WITH_CURSOR_TRACKING` links in during splits and rebalances. Fixup loops
//! iterate the tracked slots of a DBI, exactly like C walks the list.
//! [`CursorId`](crate::core::txn_rw::CursorId)s handed out for the C API
//! carry a generation number so a stale id cannot reach a reused slot.
//!
//! # Reading through a write cursor
//!
//! Searching and moving are the read path's job (`super::cursor`, faithful
//! ports of `mdb_cursor_set` & co.). [`RwTxn::read_op`] rebuilds a read
//! `Cursor` from a write cursor's state (resolving each page number),
//! runs the operation, and stores the resulting positions back, page
//! numbers taken from the page headers. Values a read operation returns
//! borrow the transaction, so write code copies what it keeps.
//!
//! The positioning of every `mdb_put` and `mdb_del` (`MDB_SET`, and
//! `mdb_cursor_last` for `MDB_APPEND`, on the fresh cursor of a tree
//! without duplicates) runs on the write cursor's own stack instead
//! ([`RwTxn::seek_key`], [`RwTxn::seek_last`]): the same steps as the read
//! path, leaving the same state, without building and storing back a read
//! cursor, and finding each level's page through its slot.

use crate::cmp::Comparator;
use crate::consts::*;
use crate::core::cursor::{
    corrupted, empty_page, node_read, search_page, Cursor, C_DEL, C_EOF, C_INITIALIZED, C_SUB,
};
use crate::core::dbi::{DB_DIRTY, DB_DUPDATA, DB_USRVALID, DB_VALID};
use crate::core::dirty::PageSlot;
use crate::core::page_ops;
use crate::core::txn::TxnCore;
use crate::core::txn_rw::{RwTxn, TXN_ERROR, TXN_SPILLS};
use crate::error::{MdbError, Result};
use crate::format::node::{F_DUPDATA, F_SUBDATA};
use crate::format::page::P_DIRTY;
use crate::format::{Db, Layout, PageRef};

/// Page number recorded in `pg[0]` of a sub-cursor whose tree is a `P_SUBP`
/// sub-page: the page lives in the main cursor's current leaf node.
pub(crate) const SUBPAGE: u64 = P_INVALID - 1;

/// Where a cursor's `MDB_db` record lives (`mc_db`): the transaction's
/// table, or the `mx_db` of a slot's sub-cursor. Copies of a cursor
/// (`mdb_cursor_copy`) share the record, as in C.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DbRef {
    /// `&txn->mt_dbs[dbi]`.
    Txn(usize),
    /// `&mc->mc_xcursor->mx_db` of the slot.
    Sub(usize),
}

/// The state of a write cursor (`MDB_cursor` with page numbers instead of
/// page pointers).
#[derive(Clone, Debug)]
pub(crate) struct WCursor {
    /// `mc_dbi`.
    pub(crate) dbi: usize,
    /// `mc_db`.
    pub(crate) db: DbRef,
    /// `mc_pg`, as page numbers (`SUBPAGE` for a sub-page root).
    pub(crate) pg: [u64; CURSOR_STACK],
    /// Where each page of `pg` was last found in the dirty list: a hint,
    /// checked before use (see the module documentation).
    pub(crate) pg_slot: [PageSlot; CURSOR_STACK],
    /// `mc_ki`.
    pub(crate) ki: [u16; CURSOR_STACK],
    /// `mc_snum`.
    pub(crate) snum: usize,
    /// `mc_top`.
    pub(crate) top: usize,
    /// `mc_flags` (`C_*`).
    pub(crate) flags: u8,
    /// The tree is a `P_SUBP` sub-page (sub-cursors only).
    pub(crate) subp: bool,
}

impl WCursor {
    /// `mdb_cursor_init` / `mdb_xcursor_init0`: an unpositioned cursor.
    pub(crate) fn new(dbi: usize, db: DbRef, flags: u8) -> WCursor {
        WCursor {
            dbi,
            db,
            pg: [P_INVALID; CURSOR_STACK],
            pg_slot: [PageSlot::NONE; CURSOR_STACK],
            ki: [0; CURSOR_STACK],
            snum: 0,
            top: 0,
            flags,
            subp: false,
        }
    }

    /// Makes this cursor what [`WCursor::new`] returns, in place (cursors
    /// are large; building one and moving it into its arena slot was a
    /// measurable part of every `mdb_put`). The slot hints are left as they
    /// are: they are checked before use.
    fn reset(&mut self, dbi: usize, db: DbRef, flags: u8) {
        self.dbi = dbi;
        self.db = db;
        self.pg.fill(P_INVALID);
        self.ki.fill(0);
        self.snum = 0;
        self.top = 0;
        self.flags = flags;
        self.subp = false;
    }

    /// `C_INITIALIZED`.
    #[inline]
    pub(crate) fn is_init(&self) -> bool {
        self.flags & C_INITIALIZED != 0
    }

    /// `C_SUB`.
    #[inline]
    pub(crate) fn is_sub(&self) -> bool {
        self.flags & C_SUB != 0
    }

    /// `mc_ki[mc_top]`.
    #[inline]
    pub(crate) fn ki_top(&self) -> usize {
        usize::from(self.ki[self.top])
    }

    /// `mc_pg[mc_top]`.
    #[inline]
    pub(crate) fn pg_top(&self) -> u64 {
        self.pg[self.top]
    }

    /// `mdb_cursor_push` for a page number.
    pub(crate) fn push(&mut self, pgno: u64) -> Result<()> {
        if self.snum >= CURSOR_STACK {
            return Err(MdbError::CursorFull.into());
        }
        self.top = self.snum;
        self.snum += 1;
        self.pg[self.top] = pgno;
        self.ki[self.top] = 0;
        Ok(())
    }

    /// `mdb_cursor_pop`.
    pub(crate) fn pop(&mut self) {
        if self.snum > 0 {
            self.snum -= 1;
            if self.snum > 0 {
                self.top -= 1;
            } else {
                self.flags &= !C_INITIALIZED;
            }
        }
    }
}

/// The sub-cursor of a `MDB_DUPSORT` cursor (`MDB_xcursor`).
#[derive(Clone, Debug)]
pub(crate) struct XState {
    /// `mx_cursor`.
    pub(crate) c: WCursor,
    /// `mx_db`.
    pub(crate) db: Db,
    /// `mx_dbflag`.
    pub(crate) dbflag: u8,
    /// `mx_dbx.md_cmp`: the duplicate order.
    pub(crate) cmp: Comparator,
}

/// One cursor of the arena: a main cursor and, for `MDB_DUPSORT`
/// databases, its sub-cursor.
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub(crate) main: WCursor,
    pub(crate) x: Option<XState>,
    /// Linked into `mt_cursors[dbi]`.
    pub(crate) tracked: bool,
    live: bool,
    /// Bumped whenever the slot is freed, for [`CursorId`](crate::core::CursorId) validation.
    pub(crate) gen: u32,
    /// Nesting depth of the transaction that opened the cursor (0 = the
    /// top-level transaction). A nested transaction keeps its ancestors'
    /// cursors up to date (C's `mdb_cursor_shadow`), but only its own are
    /// usable through it.
    pub(crate) owner: u32,
}

/// Names a cursor in the arena: the main cursor of a slot or its
/// sub-cursor (`&mc->mc_xcursor->mx_cursor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CRef {
    pub(crate) slot: usize,
    pub(crate) sub: bool,
}

impl CRef {
    /// The main cursor of `slot`.
    pub(crate) fn main(slot: usize) -> CRef {
        CRef { slot, sub: false }
    }

    /// The sub-cursor of `slot`.
    pub(crate) fn sub(slot: usize) -> CRef {
        CRef { slot, sub: true }
    }
}

/// The cursors of a write transaction. A nested transaction takes over
/// its parent's arena while it runs (`mdb_cursor_shadow`) and gives it back
/// when it ends (`mdb_cursors_close`).
#[derive(Debug, Default)]
pub(crate) struct Arena {
    slots: Vec<Slot>,
    free: Vec<usize>,
    /// Nesting depth of the transaction currently using the arena.
    depth: u32,
}

impl XState {
    /// `mdb_xcursor_init0`: the unpositioned sub-cursor of slot `slot` on
    /// `dbi`, ordering duplicates by `cmp`.
    fn new(dbi: usize, slot: usize, cmp: Comparator) -> XState {
        XState {
            c: WCursor::new(dbi, DbRef::Sub(slot), C_SUB),
            db: Db::empty(0),
            dbflag: 0,
            cmp,
        }
    }

    /// [`XState::new`] in place (see [`WCursor::reset`]).
    fn reset(&mut self, dbi: usize, slot: usize, cmp: Comparator) {
        self.c.reset(dbi, DbRef::Sub(slot), C_SUB);
        self.db = Db::empty(0);
        self.dbflag = 0;
        self.cmp = cmp;
    }
}

impl Arena {
    /// `mdb_cursor_init`: a new slot holding an unpositioned cursor on
    /// `dbi` and, for a `MDB_DUPSORT` database (`dup_cmp`, the duplicate
    /// order), its sub-cursor. A free slot is reset in place.
    pub(crate) fn open(&mut self, dbi: usize, dup_cmp: Option<Comparator>, tracked: bool) -> usize {
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i].main.reset(dbi, DbRef::Txn(dbi), 0);
                i
            }
            None => {
                self.slots.push(Slot {
                    main: WCursor::new(dbi, DbRef::Txn(dbi), 0),
                    x: None,
                    tracked,
                    live: true,
                    gen: 0,
                    owner: self.depth,
                });
                self.slots.len() - 1
            }
        };
        let s = &mut self.slots[i];
        match (dup_cmp, &mut s.x) {
            (Some(cmp), Some(x)) => x.reset(dbi, i, cmp),
            (Some(cmp), None) => s.x = Some(XState::new(dbi, i, cmp)),
            (None, _) => s.x = None,
        }
        s.tracked = tracked;
        s.live = true;
        s.owner = self.depth;
        i
    }

    /// Stores a new cursor and returns its slot.
    pub(crate) fn alloc(&mut self, main: WCursor, x: Option<XState>, tracked: bool) -> usize {
        let owner = self.depth;
        match self.free.pop() {
            Some(i) => {
                let s = &mut self.slots[i];
                s.main = main;
                s.x = x;
                s.tracked = tracked;
                s.live = true;
                s.owner = owner;
                i
            }
            None => {
                self.slots.push(Slot {
                    main,
                    x,
                    tracked,
                    live: true,
                    gen: 0,
                    owner,
                });
                self.slots.len() - 1
            }
        }
    }

    /// Nesting depth of the transaction using the arena.
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    /// Hands the arena to a transaction at nesting depth `depth`.
    pub(crate) fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    /// Copies of the live slots (`mc_backup` of `mdb_cursor_shadow`), to
    /// put back with [`Arena::restore`] if a nested transaction aborts.
    pub(crate) fn backup(&self) -> Vec<(usize, Slot)> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.live)
            .map(|(i, s)| (i, s.clone()))
            .collect()
    }

    /// Puts slots saved by [`Arena::backup`] back (`*mc = *bk` in
    /// `mdb_cursors_close` without merge). Their generation numbers have not
    /// changed: a slot is only freed by the transaction that owns it.
    pub(crate) fn restore(&mut self, backup: Vec<(usize, Slot)>) {
        for (i, s) in backup {
            if let Some(dst) = self.slots.get_mut(i) {
                *dst = s;
            }
        }
    }

    /// Frees every slot opened at nesting depth `depth` (the cursors of a
    /// nested transaction that ends).
    pub(crate) fn release_owned(&mut self, depth: u32) {
        for i in 0..self.slots.len() {
            if self.slots[i].owner == depth {
                self.release(i);
            }
        }
    }

    /// Frees a slot (`mdb_cursor_close`, or the end of a temporary cursor).
    pub(crate) fn release(&mut self, slot: usize) {
        if let Some(s) = self.slots.get_mut(slot) {
            if s.live {
                s.live = false;
                s.tracked = false;
                s.gen = s.gen.wrapping_add(1);
                self.free.push(slot);
            }
        }
    }

    /// Frees every slot (`mdb_cursors_close`).
    pub(crate) fn clear(&mut self) {
        for i in 0..self.slots.len() {
            self.release(i);
        }
    }

    /// Number of slots, live or not (bounds for fixup loops).
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// The slot, if it is live.
    pub(crate) fn slot(&self, i: usize) -> Option<&Slot> {
        self.slots.get(i).filter(|s| s.live)
    }

    /// Mutable access to a live slot.
    pub(crate) fn slot_mut(&mut self, i: usize) -> Option<&mut Slot> {
        self.slots.get_mut(i).filter(|s| s.live)
    }

    /// The cursor `c`. Panics on a dangling `CRef`, which would be a bug in
    /// the write path (ids from users are validated before use).
    pub(crate) fn cur(&self, c: CRef) -> &WCursor {
        let s = &self.slots[c.slot];
        if c.sub {
            &s.x.as_ref().expect("sub-cursor of a non-DUPSORT cursor").c
        } else {
            &s.main
        }
    }

    /// Mutable access to the cursor `c`.
    pub(crate) fn cur_mut(&mut self, c: CRef) -> &mut WCursor {
        let s = &mut self.slots[c.slot];
        if c.sub {
            &mut s.x.as_mut().expect("sub-cursor of a non-DUPSORT cursor").c
        } else {
            &mut s.main
        }
    }

    /// The sub-cursor of slot `slot`, for its record (`mc_db` of a
    /// sub-cursor). Panics if there is none, which would be a bug in the
    /// write path (only sub-cursors refer to it).
    fn sub_record(&self, slot: usize) -> &XState {
        self.slots[slot].x.as_ref().expect("sub-cursor record")
    }

    /// For a fixup loop over `mt_cursors[dbi]` on behalf of a cursor with
    /// (`sub`) or without `C_SUB`: the cursor of tracked slot `i` to adjust
    /// (`m3`), if slot `i` is tracked on `dbi`.
    pub(crate) fn peer(&self, i: usize, dbi: usize, sub: bool) -> Option<CRef> {
        let s = self.slot(i)?;
        if !s.tracked || s.main.dbi != dbi {
            return None;
        }
        if sub {
            s.x.as_ref().map(|_| CRef::sub(i))
        } else {
            Some(CRef::main(i))
        }
    }
}

/// The identity of a cursor's page at one level, for C's `mc_pg[i] == mp`
/// comparisons: a page number, or the sub-page in leaf `leaf` at node
/// `idx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageId {
    Pg(u64),
    Sub { leaf: u64, idx: u16 },
}

/// The sub-page a sub-cursor of `slot` walks: the data of the main
/// cursor's current node, or `None` if that node holds no sub-page (a
/// sub-cursor left over from an earlier key, as C allows; the read
/// operations set the sub-cursor up again before they use it).
fn subpage_of<'t>(
    core: &'t TxnCore<'_>,
    arena: &Arena,
    slot: usize,
) -> Result<Option<PageRef<'t>>> {
    let m = &arena.slots[slot].main;
    if m.snum == 0 || m.pg[m.top] == SUBPAGE {
        return Ok(None);
    }
    let leaf = core.page_by_slot(m.pg[m.top], m.pg_slot[m.top])?;
    match leaf.node(usize::from(m.ki[m.top])) {
        Some(n) if n.flags() & (F_DUPDATA | F_SUBDATA) == F_DUPDATA => {
            let data = n.inline_data().ok_or_else(corrupted)?;
            Ok(Some(
                PageRef::with_layout(data, n.layout()).ok_or_else(corrupted)?,
            ))
        }
        _ => Ok(None),
    }
}

/// The bytes of clean (mapped) page `pgno` of `map`.
fn clean_page(map: &crate::os::DataMap, ps: usize, pgno: u64) -> Result<&[u8]> {
    let off = usize::try_from(pgno)
        .ok()
        .and_then(|p| p.checked_mul(ps))
        .ok_or(MdbError::PageNotFound)?;
    map.slice(off, ps)
        .ok_or_else(|| MdbError::PageNotFound.into())
}

/// Rebuilds the read cursor for `c`.
fn build_read<'t>(
    core: &'t TxnCore<'_>,
    arena: &Arena,
    c: CRef,
    db: Db,
    cmp: Comparator,
) -> Result<Cursor<'t>> {
    let mut rc = Cursor::new(db, cmp, 0, core.layout);
    load_read(core, arena, c, &mut rc)?;
    Ok(rc)
}

/// Loads the state of cursor `c` into `out`, a fresh read cursor for its
/// tree (see [`build_read`]).
fn load_read<'t>(
    core: &'t TxnCore<'_>,
    arena: &Arena,
    c: CRef,
    out: &mut Cursor<'t>,
) -> Result<()> {
    let w = arena.cur(c);
    let sub = if w.subp {
        Some(
            subpage_of(core, arena, c.slot)?
                .unwrap_or_else(|| PageRef::from_parts(empty_page(), core.layout)),
        )
    } else {
        None
    };
    let snum = w.snum.min(CURSOR_STACK);
    out.load(w.flags, &w.ki, snum, w.top, sub, |i| match sub {
        Some(sp) if i == 0 => Ok(sp),
        _ => core.page_by_slot(w.pg[i], w.pg_slot[i]),
    })
}

/// Stores a read cursor's position back into `w`.
fn store_back(w: &mut WCursor, rc: &Cursor<'_>) {
    w.flags = rc.flags;
    w.snum = rc.depth();
    w.top = rc.top_level();
    w.ki = *rc.ki_array();
    w.subp = rc.subpage().is_some();
    for i in 0..w.snum {
        w.pg[i] = if i == 0 && w.subp {
            SUBPAGE
        } else {
            rc.page_at(i).pgno()
        };
    }
}

/// Builds the read cursors of slot `c.slot` (main and sub-cursor) for an
/// operation on the main cursor `c`, or only the sub-cursor if `c` is one.
pub(crate) fn read_cursors<'t>(
    core: &'t TxnCore<'_>,
    arena: &Arena,
    c: CRef,
) -> Result<(Cursor<'t>, Option<Cursor<'t>>)> {
    let (db, cmp) = db_and_cmp(core, arena, c);
    let mc = build_read(core, arena, c, db, cmp)?;
    let mx = match (&arena.slots[c.slot].x, c.sub) {
        (Some(x), false) => Some(build_read(core, arena, CRef::sub(c.slot), x.db, x.cmp)?),
        _ => None,
    };
    Ok((mc, mx))
}

/// Stores the state of read cursors built by [`read_cursors`] back.
pub(crate) fn write_back(arena: &mut Arena, c: CRef, mc: &Cursor<'_>, mx: Option<&Cursor<'_>>) {
    store_back(arena.cur_mut(c), mc);
    if let (Some(mx), Some(x)) = (mx, arena.slots[c.slot].x.as_mut()) {
        store_back(&mut x.c, mx);
        x.db = mx.db;
        x.cmp = mx.cmp;
    }
}

/// `mdb_page_search` and `mdb_page_search_root` (mdb.c line 5630) on the
/// stack of the unpositioned write cursor `w` itself (see
/// [`RwTxn::seeks_in_place`]): put the root of the tree with record `db`
/// (not `P_INVALID`) and key order `cmp` on the stack and descend to the
/// leaf for `key`, or to the last leaf (`None`, `MDB_PS_LAST`). Returns the
/// leaf.
///
/// These are the steps `Cursor::page_search` takes for an unpositioned
/// cursor, in the same order, so the stack, the indices and the flags end
/// up as [`RwTxn::read_op`] would store them back, also when a step fails
/// (page numbers are taken from the page headers, as there). What is
/// saved: building the read cursor and storing it back, and most page
/// lookups. Each level's page is first looked for through the slot the
/// cursor has for that level (from its last position: the cursor of
/// `mdb_put` keeps them from put to put, and consecutive operations mostly
/// pass the same root and branch pages), and the slot found is stored, so
/// the `mdb_page_touch` that follows needs no search either.
fn descend<'t>(
    core: &'t TxnCore<'_>,
    w: &mut WCursor,
    db: &Db,
    cmp: Comparator,
    key: Option<&[u8]>,
) -> Result<PageRef<'t>> {
    let (mut mp, slot) = core.page_and_slot(db.root, w.pg_slot[0])?;
    w.pg[0] = mp.pgno();
    w.pg_slot[0] = slot;
    w.snum = 1;
    w.top = 0;
    while mp.is_branch() {
        let nkeys = mp.num_keys();
        if nkeys == 0 {
            return Err(corrupted());
        }
        let i = match key {
            None => nkeys - 1,
            Some(key) => {
                let (i, found, exact) = search_page(mp, cmp, db.pad as usize, key)?;
                w.ki[w.top] = i as u16;
                let i = w.ki_top();
                if !found {
                    nkeys - 1
                } else if exact {
                    i
                } else {
                    i.checked_sub(1).ok_or_else(corrupted)?
                }
            }
        };
        let node = mp.node(i).ok_or_else(corrupted)?;
        let level = w.snum;
        let hint = w.pg_slot.get(level).copied().unwrap_or(PageSlot::NONE);
        let (child, slot) = core.page_and_slot(node.child_pgno(), hint)?;
        w.ki[w.top] = i as u16;
        // mdb_cursor_push
        if level >= CURSOR_STACK {
            return Err(MdbError::CursorFull.into());
        }
        w.pg[level] = child.pgno();
        w.pg_slot[level] = slot;
        w.ki[level] = 0;
        w.top = level;
        w.snum = level + 1;
        mp = child;
    }
    if !mp.is_leaf() {
        return Err(corrupted());
    }
    w.flags |= C_INITIALIZED;
    w.flags &= !C_EOF;
    Ok(mp)
}

/// The key of item `ki` of leaf `mp` of a tree without duplicates
/// (`ksize`: its `P_LEAF2` key size), read as `read_position` and the end
/// of `cursor_set` read it for a main cursor without a sub-cursor: a node
/// with duplicates is corrupt there, and the data is read too (following
/// `F_BIGDATA`) for its checks.
fn read_item<'t>(
    core: &'t TxnCore<'_>,
    mp: PageRef<'t>,
    ki: usize,
    ksize: usize,
) -> Result<&'t [u8]> {
    if !mp.is_leaf() {
        return Err(corrupted());
    }
    if mp.is_leaf2() {
        return mp.leaf2_key(ki, ksize).ok_or_else(corrupted);
    }
    let leaf = mp.node(ki).ok_or_else(corrupted)?;
    if leaf.is_dupdata() {
        return Err(corrupted());
    }
    node_read(core, leaf)?;
    leaf.key().ok_or_else(corrupted)
}

/// The record and key order of cursor `c`.
fn db_and_cmp(core: &TxnCore<'_>, arena: &Arena, c: CRef) -> (Db, Comparator) {
    match arena.cur(c).db {
        DbRef::Txn(dbi) => {
            let dbs = core.dbs.borrow();
            let e = &dbs[dbi];
            (e.db, e.key_cmp)
        }
        DbRef::Sub(slot) => {
            let x = arena.sub_record(slot);
            (x.db, x.cmp)
        }
    }
}

impl<'env> RwTxn<'env> {
    // --- cursor state --------------------------------------------------------

    /// The state of cursor `c`.
    pub(crate) fn cur(&self, c: CRef) -> &WCursor {
        self.w.cursors.cur(c)
    }

    /// Mutable state of cursor `c`.
    pub(crate) fn cur_mut(&mut self, c: CRef) -> &mut WCursor {
        self.w.cursors.cur_mut(c)
    }

    /// `*mc->mc_db` (a copy).
    pub(crate) fn db_of(&self, c: CRef) -> Db {
        match self.cur(c).db {
            DbRef::Txn(dbi) => self.core.dbs.borrow()[dbi].db,
            DbRef::Sub(slot) => self.w.cursors.sub_record(slot).db,
        }
    }

    /// `mc->mc_db`, for updating the record.
    pub(crate) fn db_mut(&mut self, c: CRef) -> &mut Db {
        match self.cur(c).db {
            DbRef::Txn(dbi) => &mut self.core.dbs.get_mut()[dbi].db,
            DbRef::Sub(slot) => {
                &mut self
                    .w
                    .cursors
                    .slot_mut(slot)
                    .and_then(|s| s.x.as_mut())
                    .expect("sub-cursor record")
                    .db
            }
        }
    }

    /// `mc->mc_dbx->md_cmp`.
    pub(crate) fn cmp_of(&self, c: CRef) -> Comparator {
        match self.cur(c).db {
            DbRef::Txn(dbi) => self.core.dbs.borrow()[dbi].key_cmp,
            DbRef::Sub(slot) => self.w.cursors.sub_record(slot).cmp,
        }
    }

    /// `mc->mc_dbx->md_dcmp` of a main cursor.
    pub(crate) fn dcmp_of(&self, c: CRef) -> Option<Comparator> {
        self.core.dbs.borrow()[self.cur(c).dbi].dup_cmp
    }

    /// `*mc->mc_dbflag`.
    pub(crate) fn dbflag(&self, c: CRef) -> u8 {
        if c.sub {
            self.w
                .cursors
                .slot(c.slot)
                .and_then(|s| s.x.as_ref())
                .map_or(0, |x| x.dbflag)
        } else {
            self.core.dbs.borrow()[self.cur(c).dbi].state
        }
    }

    /// `*mc->mc_dbflag |= bits` for a main cursor.
    pub(crate) fn set_dbflag(&mut self, c: CRef, bits: u8) {
        if !c.sub {
            let dbi = self.cur(c).dbi;
            self.core.dbs.get_mut()[dbi].state |= bits;
        }
    }

    /// `mdb_cursor_init`: a new cursor slot on `dbi`, linked into the
    /// tracked list if `tracked` (`mdb_cursor_open`, `mdb_put`). The DBI's
    /// record must be fresh.
    pub(crate) fn open_slot(&mut self, dbi: usize, tracked: bool) -> usize {
        let dup_cmp = self.core.dbs.borrow()[dbi].dup_order();
        self.w.cursors.open(dbi, dup_cmp, tracked)
    }

    /// Frees a cursor slot.
    pub(crate) fn close_slot(&mut self, slot: usize) {
        self.w.cursors.release(slot);
    }

    /// `mdb_cursor_copy(mc, &mn)` with `mn.mc_xcursor = NULL`: a temporary
    /// copy of `mc`, tracked if `tracked` (`WITH_CURSOR_TRACKING`). A copy of
    /// a sub-cursor is reached through a dummy main cursor, as in C.
    pub(crate) fn copy_cursor(&mut self, mc: CRef, tracked: bool) -> CRef {
        let copy = self.cur(mc).clone();
        if copy.is_sub() {
            let dummy = WCursor::new(copy.dbi, DbRef::Txn(copy.dbi), C_INITIALIZED);
            let x = XState {
                c: copy,
                db: Db::empty(0),
                dbflag: 0,
                cmp: Comparator::Memn,
            };
            CRef::sub(self.w.cursors.alloc(dummy, Some(x), tracked))
        } else {
            CRef::main(self.w.cursors.alloc(copy, None, tracked))
        }
    }

    /// Copies the state of `src` into `dst` (`mdb_cursor_copy(src, dst)`).
    pub(crate) fn copy_state(&mut self, src: CRef, dst: CRef) {
        let s = self.cur(src).clone();
        let d = self.cur_mut(dst);
        d.dbi = s.dbi;
        d.db = s.db;
        d.snum = s.snum;
        d.top = s.top;
        d.flags = s.flags;
        d.subp = s.subp;
        d.pg[..s.snum].copy_from_slice(&s.pg[..s.snum]);
        d.pg_slot[..s.snum].copy_from_slice(&s.pg_slot[..s.snum]);
        d.ki[..s.snum].copy_from_slice(&s.ki[..s.snum]);
    }

    // --- pages -----------------------------------------------------------------

    /// The page size.
    #[inline]
    pub(crate) fn psize(&self) -> usize {
        self.core.env.page_size()
    }

    /// The environment's page layout (a `&'static`, so it can be held
    /// across a mutable borrow of a page). Re-derived from the format so
    /// that the page edits it is passed to fold its numbers into constants
    /// (see `PageRef::l`).
    #[inline]
    pub(crate) fn layout(&self) -> &'static Layout {
        self.core.layout.format.layout()
    }

    /// Identity of the page of `c` at `level` (`mc->mc_pg[level]`).
    pub(crate) fn page_id(&self, c: CRef, level: usize) -> PageId {
        let w = self.cur(c);
        if level == 0 && w.subp {
            match self.w.cursors.slot(c.slot) {
                Some(s) if s.main.snum > 0 => PageId::Sub {
                    leaf: s.main.pg[s.main.top],
                    idx: s.main.ki[s.main.top],
                },
                _ => PageId::Sub {
                    leaf: P_INVALID,
                    idx: 0,
                },
            }
        } else {
            PageId::Pg(w.pg[level])
        }
    }

    /// The page of `c` at `level`, for reading.
    pub(crate) fn page_of(&self, c: CRef, level: usize) -> Result<PageRef<'_>> {
        let w = self.cur(c);
        if level == 0 && w.subp {
            return subpage_of(&self.core, &self.w.cursors, c.slot)?.ok_or_else(corrupted);
        }
        self.core.page_by_slot(w.pg[level], w.pg_slot[level])
    }

    /// The top page of `c` (`mc->mc_pg[mc->mc_top]`), for reading.
    pub(crate) fn top_page(&self, c: CRef) -> Result<PageRef<'_>> {
        self.page_of(c, self.cur(c).top)
    }

    /// The bytes of the page of `c` at `level`, for writing. The page must
    /// be dirty (touched); a sub-page is written inside its dirty leaf.
    /// The slot of the page is stored back into the cursor.
    pub(crate) fn page_mut_of(&mut self, c: CRef, level: usize) -> Result<&mut [u8]> {
        let ps = self.psize();
        let l = self.layout();
        let w = self.w.cursors.cur(c);
        if level == 0 && w.subp {
            let m = &mut self.w.cursors.slot_mut(c.slot).ok_or_else(corrupted)?.main;
            let (leaf, idx) = (m.pg[m.top], usize::from(m.ki[m.top]));
            let slot = self
                .core
                .dirty
                .resolve(m.pg_slot[m.top], leaf)
                .ok_or_else(corrupted)?;
            m.pg_slot[m.top] = slot;
            let buf = self.core.dirty.at_mut(slot, leaf).ok_or_else(corrupted)?;
            let range = page_ops::node_data_range(l, &buf[..ps], idx).ok_or_else(corrupted)?;
            return Ok(&mut buf[range]);
        }
        let pgno = w.pg[level];
        let slot = self
            .core
            .dirty
            .resolve(w.pg_slot[level], pgno)
            .ok_or_else(corrupted)?;
        self.cur_mut(c).pg_slot[level] = slot;
        let buf = self.core.dirty.at_mut(slot, pgno).ok_or_else(corrupted)?;
        Ok(&mut buf[..ps])
    }

    /// The top page of `c`, for writing.
    pub(crate) fn top_page_mut(&mut self, c: CRef) -> Result<&mut [u8]> {
        let top = self.cur(c).top;
        self.page_mut_of(c, top)
    }

    /// A dirty page by number, for writing (`ps` bytes, or the whole run).
    pub(crate) fn dirty_mut(&mut self, pgno: u64) -> Result<&mut [u8]> {
        self.core.dirty.get_mut(pgno).ok_or_else(corrupted)
    }

    // --- read operations on write cursors ----------------------------------------

    /// Runs a read-path operation on cursor `c` (and, for a main cursor,
    /// its sub-cursor), then stores the resulting positions back. The
    /// state is stored back even if the operation fails, since C cursors
    /// keep the position a failed search left them at (`mdb_cursor_put`
    /// relies on that to find the insertion point).
    pub(crate) fn read_op<R>(
        &mut self,
        c: CRef,
        f: impl for<'t> FnOnce(&'t TxnCore<'env>, &mut Cursor<'t>, Option<&mut Cursor<'t>>) -> Result<R>,
    ) -> Result<R> {
        // `read_cursors`, with both cursors built in place on this stack
        // frame (they are large; moving them is measurably slow).
        let core = &self.core;
        let arena = &self.w.cursors;
        let (db, cmp) = db_and_cmp(core, arena, c);
        let mut mc = Cursor::new(db, cmp, 0, core.layout);
        load_read(core, arena, c, &mut mc)?;
        let mut x_store;
        let mut mx = match (&arena.slots[c.slot].x, c.sub) {
            (Some(x), false) => {
                x_store = Cursor::new(x.db, x.cmp, 0, core.layout);
                load_read(core, arena, CRef::sub(c.slot), &mut x_store)?;
                Some(&mut x_store)
            }
            _ => None,
        };
        let res = f(core, &mut mc, mx.as_deref_mut());
        write_back(&mut self.w.cursors, c, &mc, mx.as_deref());
        res
    }

    // --- positioning on a write cursor's own stack -------------------------------

    /// Whether `mc` can be positioned on its own stack ([`RwTxn::seek_key`],
    /// [`RwTxn::seek_last`]) instead of through [`RwTxn::read_op`]: an
    /// unpositioned main cursor of a tree without duplicates (no
    /// sub-cursor). That is the cursor `mdb_put` and `mdb_del` open for
    /// every call; everything else takes the read path.
    #[inline]
    pub(crate) fn seeks_in_place(&self, mc: CRef) -> bool {
        let s = &self.w.cursors.slots[mc.slot];
        !mc.sub && s.x.is_none() && !s.main.is_init() && matches!(s.main.db, DbRef::Txn(_))
    }

    /// `mdb_cursor_set` with `MDB_SET` for a cursor that
    /// [`RwTxn::seeks_in_place`], in a tree with record `db` and key order
    /// `cmp`: what `cursor_set(.., CursorOp::Set, true)` through
    /// [`RwTxn::read_op`] does for such a cursor, step for step (see
    /// [`descend`]), with the same errors and the same cursor state
    /// afterwards, also when it fails. Returns `false` where that returns
    /// `MDB_NOTFOUND` (the cursor then sits where `key` would go).
    pub(crate) fn seek_key(
        &mut self,
        mc: CRef,
        db: &Db,
        cmp: Comparator,
        key: &[u8],
    ) -> Result<bool> {
        if key.is_empty() {
            return Err(MdbError::BadValSize.into());
        }
        if db.root == P_INVALID {
            return Ok(false);
        }
        let core = &self.core;
        let w = self.w.cursors.cur_mut(mc);
        let mp = descend(core, w, db, cmp, Some(key))?;
        let ksize = db.pad as usize;
        let (i, _, exact) = search_page(mp, cmp, ksize, key)?;
        w.ki[w.top] = i as u16;
        if !exact {
            return Ok(false);
        }
        // set1: the descent left C_INITIALIZED set and C_EOF clear. Read
        // the item as `cursor_set` does, for its checks.
        read_item(core, mp, w.ki_top(), ksize)?;
        Ok(true)
    }

    /// `mdb_cursor_last` as `mdb_cursor_put` uses it for `MDB_APPEND`, for
    /// a cursor that [`RwTxn::seeks_in_place`], in a tree with record `db`:
    /// what `cursor_last_for_append` through [`RwTxn::read_op`] does for
    /// such a cursor (see [`RwTxn::seek_key`]). Returns `f` of the last key,
    /// or of `None` for an empty leaf, where the cursor is left before its
    /// first slot (`mc_ki` 0xffff) as there.
    pub(crate) fn seek_last<R>(
        &mut self,
        mc: CRef,
        db: &Db,
        cmp: Comparator,
        f: impl FnOnce(Option<&[u8]>) -> R,
    ) -> Result<R> {
        if db.root == P_INVALID {
            return Err(MdbError::NotFound.into());
        }
        let core = &self.core;
        let w = self.w.cursors.cur_mut(mc);
        let mp = descend(core, w, db, cmp, None)?;
        let nkeys = mp.num_keys();
        if nkeys == 0 {
            if !mp.is_leaf() {
                return Err(corrupted());
            }
            w.ki[w.top] = u16::MAX;
            w.flags |= C_INITIALIZED | C_EOF;
            return Ok(f(None));
        }
        w.ki[w.top] = (nkeys - 1) as u16;
        w.flags |= C_INITIALIZED | C_EOF;
        Ok(f(Some(read_item(core, mp, w.ki_top(), db.pad as usize)?)))
    }

    /// `mdb_page_search` with `MDB_PS_MODIFY`: descend to the leaf for
    /// `key` (or the first/last leaf with `PS_FIRST`/`PS_LAST`) and touch
    /// every page on the way, root first, as C does while descending.
    pub(crate) fn page_search_modify(
        &mut self,
        mc: CRef,
        key: Option<&[u8]>,
        flags: u8,
    ) -> Result<()> {
        if self.w.flags & crate::core::txn_rw::TXN_BLOCKED != 0 {
            return Err(MdbError::BadTxn.into());
        }
        self.read_op(mc, |txn, c, _| c.page_search(txn, key, flags))?;
        let snum = self.cur(mc).snum;
        for level in 0..snum {
            self.cur_mut(mc).top = level;
            self.page_touch(mc)?;
        }
        self.cur_mut(mc).top = snum.saturating_sub(1);
        Ok(())
    }

    // --- touching ------------------------------------------------------------------

    /// `mdb_page_touch` (mdb.c line 2494): make the top page of `mc`
    /// writable. A clean page is copied to a newly allocated page number,
    /// the old number goes on the transaction's free list, the parent
    /// branch node (or the DB root) is pointed at the copy, and every other
    /// cursor on the old page moves to the copy.
    ///
    /// Sub-pages live inside a dirty leaf and are always `P_DIRTY` in C, so
    /// touching them is a no-op. A page this transaction (or an ancestor)
    /// spilled is copied back under its own number (`mdb_page_unspill`); in
    /// a nested transaction, a page dirty in an ancestor is copied into
    /// this transaction's dirty list under its own number. Neither moves a
    /// cursor: they hold page numbers, which do not change (C's fixups only
    /// replace the page pointer there).
    ///
    /// The page's slot in the dirty list is stored in the cursor (and in
    /// the cursors moved to a copy), so that later accesses through the
    /// cursor need no search. A page already in this transaction's list,
    /// the common case, is recognised through the slot the cursor already
    /// has, without a search.
    #[inline]
    pub(crate) fn page_touch(&mut self, mc: CRef) -> Result<()> {
        let top = self.cur(mc).top;
        match self.touch_needed(mc, top) {
            None => Ok(()),
            Some(in_list) => self.page_touch_slow(mc, in_list),
        }
    }

    /// The common case of `mdb_page_touch` for the page of `mc` at `level`:
    /// `None` if there is nothing to do because it is a sub-page or already
    /// dirty in this transaction's own list (found through the cursor's
    /// slot, which is then stored; a page there is what `TxnCore::page`
    /// returns for it). Otherwise whether the page is in that list, for
    /// [`RwTxn::page_touch_slow`].
    #[inline]
    fn touch_needed(&mut self, mc: CRef, level: usize) -> Option<bool> {
        let l = self.layout();
        let w = self.w.cursors.cur_mut(mc);
        if level == 0 && w.subp {
            return None;
        }
        match self.core.dirty.lookup(w.pg_slot[level], w.pg[level]) {
            Some((slot, page)) if page_ops::flags(l, page) & P_DIRTY != 0 => {
                w.pg_slot[level] = slot;
                None
            }
            found => Some(found.is_some()),
        }
    }

    /// [`RwTxn::page_touch`] for a page that is not dirty in this
    /// transaction's own list yet (`in_list`: whether it is in that list at
    /// all, as [`RwTxn::touch_needed`] found).
    #[inline(never)]
    fn page_touch_slow(&mut self, mc: CRef, in_list: bool) -> Result<()> {
        let (top, mp, snum, is_sub) = {
            let w = self.cur(mc);
            if w.top == 0 && w.subp {
                return Ok(());
            }
            (w.top, w.pg[w.top], w.snum, w.is_sub())
        };
        let page = if in_list {
            self.core.page(mp)?
        } else {
            self.core.page_not_dirty_here(mp)?
        };
        if page.flags() & P_DIRTY != 0 {
            if self.has_parent() && !in_list {
                // If txn has a parent, make sure the page is in our dirty
                // list. No - copy it (it is already counted in the parent's
                // dirty room).
                let slot = self.copy_ancestor_page(mp)?;
                self.cur_mut(mc).pg_slot[top] = slot;
            }
            return Ok(());
        }
        if self.w.flags & TXN_SPILLS != 0 {
            match self.page_unspill(mp) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => {
                    self.w.flags |= TXN_ERROR;
                    return Err(e);
                }
            }
        }
        // The new page starts as a copy of the clean one (`mdb_page_copy`,
        // done by the allocation; see `page_alloc_copy`).
        let map = std::sync::Arc::clone(&self.core.map);
        let src = clean_page(&map, self.psize(), mp)?;
        let (np, np_slot) = match self.page_alloc_copy(src) {
            Ok(p) => p,
            Err(e) => {
                self.w.flags |= TXN_ERROR;
                return Err(e);
            }
        };
        // A corrupt file can hand us the page we are copying: a free-DB
        // record naming a live page, or a root on the free list. C goes on
        // and later reuses the live page; refuse instead.
        if np == mp {
            self.w.flags |= TXN_ERROR;
            return Err(corrupted());
        }
        self.w.free_pgs.append(mp);
        // Update the parent page, if any, to point to the new page.
        if top > 0 {
            let pki = usize::from(self.cur(mc).ki[top - 1]);
            let l = self.layout();
            let parent = self.page_mut_of(mc, top - 1)?;
            page_ops::set_node_pgno(l, parent, pki, np).ok_or_else(corrupted)?;
        } else {
            self.db_mut(mc).root = np;
        }
        self.own_copied_page(np, np_slot)?;

        // Adjust cursors pointing to mp.
        let w = self.cur_mut(mc);
        w.pg[top] = np;
        w.pg_slot[top] = np_slot;
        let dbi = w.dbi;
        for i in 0..self.w.cursors.len() {
            let Some(m3) = self.w.cursors.peer(i, dbi, is_sub) else {
                continue;
            };
            if !is_sub && m3 == mc {
                continue;
            }
            let w3 = self.w.cursors.cur_mut(m3);
            if w3.snum < snum {
                continue;
            }
            if w3.pg[top] == mp && !(top == 0 && w3.subp) {
                w3.pg[top] = np;
                w3.pg_slot[top] = np_slot;
            }
        }
        Ok(())
    }

    /// The nested branch of `mdb_page_touch`: copy the page (`ps` bytes, or
    /// the whole overflow run) that an ancestor has dirty into this
    /// transaction's dirty list under the same number. Returns its slot.
    pub(crate) fn copy_ancestor_page(&mut self, pgno: u64) -> Result<PageSlot> {
        let crate::core::txn::PageSource::Dirty(_, level) = self.core.locate(pgno) else {
            return Err(corrupted());
        };
        // Level 2 is the parent's layer (level 1, our own list, cannot be:
        // the callers checked).
        let core = &mut self.core;
        let src = level
            .checked_sub(2)
            .and_then(|i| core.parents.get(i))
            .and_then(|layer| layer.dirty.get(pgno))
            .ok_or_else(corrupted)?;
        let buf = core.dirty.alloc_copy(src);
        core.dirty.insert(pgno, buf).ok_or_else(corrupted)
    }

    /// The tail of `mdb_page_touch` for the new dirty page `dst` (in
    /// `slot`), which holds a copy of the clean page (`page_alloc_copy`):
    /// `np->mp_pgno = pgno; np->mp_flags |= P_DIRTY`.
    fn own_copied_page(&mut self, dst: u64, slot: PageSlot) -> Result<()> {
        let l = self.layout();
        let to = self.core.dirty.at_mut(slot, dst).ok_or_else(corrupted)?;
        page_ops::set_pgno(to, dst);
        let f = page_ops::flags(l, to);
        page_ops::set_flags(l, to, f | P_DIRTY);
        Ok(())
    }

    /// `mdb_cursor_touch` (mdb.c line 6650): touch every page on the
    /// cursor's stack, root first. For a named DB not yet written in this
    /// transaction, first touch the path to its record in the main DB (so
    /// the record can be updated at commit) and mark it `DB_DIRTY`.
    pub(crate) fn cursor_touch(&mut self, mc: CRef) -> Result<()> {
        let dbi = self.cur(mc).dbi;
        if dbi >= CORE_DBS as usize && self.dbflag(mc) & (DB_DIRTY | DB_DUPDATA) == 0 {
            // Touch DB record of named DB.
            let entry = self.core.dbs.borrow()[dbi];
            if entry.seq != self.core.env.dbis.seq_of(dbi) {
                return Err(MdbError::BadDbi.into());
            }
            let name = self.core.env.dbis.name_of(dbi).ok_or(MdbError::BadDbi)?;
            let slot = self.open_slot(MAIN_DBI as usize, false);
            let rc = self.page_search_modify(CRef::main(slot), Some(&name), 0);
            self.close_slot(slot);
            rc?;
            self.set_dbflag(mc, DB_DIRTY);
        }
        let snum = self.cur(mc).snum;
        self.cur_mut(mc).top = 0;
        if snum > 0 {
            // Most of the time every page on the stack is already dirty in
            // this transaction's list and the cursor's slots say where:
            // check that in one pass over the stack. From the first level
            // where it does not hold, take the general path; the levels
            // before it are exactly those for which `touch_needed` would
            // have found nothing to do.
            let first = {
                let l = self.layout();
                let dirty = &self.core.dirty;
                let w = self.w.cursors.cur(mc);
                let n = snum.min(CURSOR_STACK);
                (0..n)
                    .find(|&level| {
                        !(level == 0 && w.subp)
                            && !dirty
                                .at(w.pg_slot[level], w.pg[level])
                                .is_some_and(|page| page_ops::flags(l, page) & P_DIRTY != 0)
                    })
                    .unwrap_or(snum)
            };
            let mut rc = Ok(());
            for level in first..snum {
                // `page_touch` with `mc_top` at each level in turn; the
                // common case does not look at `mc_top`.
                let Some(in_list) = self.touch_needed(mc, level) else {
                    continue;
                };
                self.cur_mut(mc).top = level;
                rc = self.page_touch_slow(mc, in_list);
                if rc.is_err() {
                    break;
                }
            }
            self.cur_mut(mc).top = snum - 1;
            rc?;
        }
        Ok(())
    }

    /// `mdb_xcursor_init1` (mdb.c line 7735) for the write path: point the
    /// sub-cursor of main cursor `mc` at the duplicates in its current node.
    pub(crate) fn xcursor_init1(&mut self, mc: CRef) -> Result<()> {
        let parent_flags = u32::from(self.db_of(mc).flags);
        let (sub_db, subpage) = {
            let leaf = self.top_page(mc)?;
            let node = leaf.node(self.cur(mc).ki_top()).ok_or_else(corrupted)?;
            let data = node.inline_data().ok_or_else(corrupted)?;
            if node.is_subdata() {
                (Db::read(data).ok_or_else(corrupted)?, false)
            } else {
                // A sub-page has the layout of the page it is stored in.
                let fp = PageRef::with_layout(data, node.layout()).ok_or_else(corrupted)?;
                let mut db = Db {
                    pad: 0,
                    flags: 0,
                    depth: 1,
                    branch_pages: 0,
                    leaf_pages: 1,
                    overflow_pages: 0,
                    entries: fp.num_keys() as u64,
                    root: fp.pgno(),
                };
                if parent_flags & MDB_DUPFIXED != 0 {
                    db.flags = MDB_DUPFIXED as u16;
                    db.pad = u32::from(fp.pad());
                    if parent_flags & MDB_INTEGERDUP != 0 {
                        db.flags |= MDB_INTEGERKEY as u16;
                    }
                }
                (db, true)
            }
        };
        let slot = mc.slot;
        let x = self
            .w
            .cursors
            .slot_mut(slot)
            .and_then(|s| s.x.as_mut())
            .ok_or_else(corrupted)?;
        x.db = sub_db;
        x.c.snum = 0;
        x.c.top = 0;
        x.c.flags = C_SUB;
        x.c.subp = false;
        if subpage {
            x.c.snum = 1;
            x.c.flags = C_INITIALIZED | C_SUB;
            x.c.pg[0] = SUBPAGE;
            x.c.ki[0] = 0;
            x.c.subp = true;
        }
        x.dbflag = DB_VALID | DB_USRVALID | DB_DUPDATA;
        // `#if UINT_MAX < SIZE_MAX`: 8-byte integer duplicates.
        if x.cmp.is_int() && x.db.pad == 8 {
            x.cmp = Comparator::Clong;
        }
        Ok(())
    }

    /// Re-reads the duplicates' record of the node at main cursor `mc` into
    /// its sub-cursor (`mx_db`), keeping the sub-cursor's position. C trusts
    /// the cached copy, which goes stale when another cursor changes the
    /// same key's duplicates (KNOWN_C_BUGS.md #14).
    pub(crate) fn refresh_xdb(&mut self, mc: CRef) -> Result<()> {
        let parent_flags = u32::from(self.db_of(mc).flags);
        let db = {
            let leaf = self.top_page(mc)?;
            let Some(node) = leaf.node(self.cur(mc).ki_top()) else {
                return Ok(());
            };
            if !node.is_dupdata() {
                return Ok(());
            }
            let data = node.inline_data().ok_or_else(corrupted)?;
            if node.is_subdata() {
                Db::read(data).ok_or_else(corrupted)?
            } else {
                // A sub-page has the layout of the page it is stored in.
                let fp = PageRef::with_layout(data, node.layout()).ok_or_else(corrupted)?;
                let mut db = Db {
                    pad: 0,
                    flags: 0,
                    depth: 1,
                    branch_pages: 0,
                    leaf_pages: 1,
                    overflow_pages: 0,
                    entries: fp.num_keys() as u64,
                    root: fp.pgno(),
                };
                if parent_flags & MDB_DUPFIXED != 0 {
                    db.flags = MDB_DUPFIXED as u16;
                    db.pad = u32::from(fp.pad());
                    if parent_flags & MDB_INTEGERDUP != 0 {
                        db.flags |= MDB_INTEGERKEY as u16;
                    }
                }
                db
            }
        };
        if let Some(x) = self.w.cursors.slot_mut(mc.slot).and_then(|s| s.x.as_mut()) {
            x.db = db;
        }
        Ok(())
    }

    /// After the duplicates of `mc`'s current key changed: give every other
    /// tracked cursor on the same key whose sub-cursor is set up the new
    /// record (their positions are kept up to date by the fixups; C leaves
    /// their `mx_db` stale, KNOWN_C_BUGS.md #14).
    pub(crate) fn share_xdb(&mut self, mc: CRef) {
        let (dbi, top, pg, ki) = {
            let w = self.cur(mc);
            (w.dbi, w.top, w.pg[w.top], w.ki[w.top])
        };
        let Some(db) = self
            .w
            .cursors
            .slot(mc.slot)
            .and_then(|s| s.x.as_ref())
            .map(|x| x.db)
        else {
            return;
        };
        for s in 0..self.w.cursors.len() {
            if s == mc.slot {
                continue;
            }
            let Some(m2) = self.w.cursors.peer(s, dbi, false) else {
                continue;
            };
            let w2 = self.cur(m2);
            if !w2.is_init() || w2.top != top || w2.pg[top] != pg || w2.ki[top] != ki {
                continue;
            }
            if let Some(x) = self.w.cursors.slot_mut(s).and_then(|sl| sl.x.as_mut()) {
                if x.c.is_init() {
                    x.db = db;
                }
            }
        }
    }

    /// `mdb_xcursor_init2` (mdb.c line 7786): bring the sub-cursor of
    /// `m2` up to date with `src`'s after `src` changed the duplicates both
    /// cursors are on.
    pub(crate) fn xcursor_init2(&mut self, m2: usize, src: usize, new_dupdata: bool) {
        let Some(sx) = self.w.cursors.slot(src).and_then(|s| s.x.clone()) else {
            return;
        };
        let Some(slot) = self.w.cursors.slot_mut(m2) else {
            return;
        };
        let main_flags = slot.main.flags;
        let Some(x) = slot.x.as_mut() else {
            return;
        };
        if new_dupdata {
            x.c.snum = 1;
            x.c.top = 0;
            // On the key's first item now. C ORs in C_INITIALIZED, so a
            // C_DEL or C_EOF left from an earlier delete or move makes the
            // next MDB_NEXT return the item again (KNOWN_C_BUGS.md #29);
            // only a main cursor that sits before the item (after a
            // delete, C_DEL) must not skip it.
            x.c.flags = C_SUB | C_INITIALIZED | (main_flags & C_DEL);
            x.c.ki[0] = 0;
            x.dbflag = DB_VALID | DB_USRVALID | DB_DUPDATA;
            x.cmp = sx.cmp;
        } else if !x.c.is_init() {
            return;
        }
        x.db = sx.db;
        x.c.pg[0] = sx.c.pg[0];
        x.c.pg_slot[0] = sx.c.pg_slot[0];
        x.c.subp = sx.c.subp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cursor::{cursor_last_for_append, cursor_set, CursorOp};
    use crate::core::{Dbi, Environment, Transaction};
    use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};
    use crate::format::layout::Format;

    /// Keys of 400 bytes, so that a few thousand make a three-level tree
    /// on any page size; ordered by `i`.
    fn key(i: u32) -> Vec<u8> {
        let mut k = vec![b'k'; 400];
        k[..4].copy_from_slice(&i.to_be_bytes());
        k
    }

    fn same_state(a: &WCursor, b: &WCursor) {
        assert_eq!(
            (a.snum, a.top, a.flags, a.subp),
            (b.snum, b.top, b.flags, b.subp)
        );
        assert_eq!(a.ki, b.ki);
        assert_eq!(a.pg[..a.snum], b.pg[..b.snum]);
    }

    /// Positions two fresh cursors of `dbi`, one on its own stack and one
    /// through the read path, for `MDB_SET` on `probe` and for
    /// `mdb_cursor_last`, and checks that results and cursor states agree.
    fn check(txn: &mut RwTxn<'_>, dbi: usize, probe: &[u8]) {
        let (a, b) = (txn.open_slot(dbi, false), txn.open_slot(dbi, false));
        let (ca, cb) = (CRef::main(a), CRef::main(b));
        assert!(txn.seeks_in_place(ca));
        let (db, cmp) = (txn.db_of(ca), txn.cmp_of(ca));
        let got = txn
            .seek_key(ca, &db, cmp, probe)
            .map_err(|e| e.to_err_code());
        let want = match txn.read_op(cb, |t, c, mx| {
            cursor_set(t, c, mx, None, probe, None, CursorOp::Set, true).map(|_| ())
        }) {
            Ok(()) => Ok(true),
            Err(e) if e.not_found() => Ok(false),
            Err(e) => Err(e.to_err_code()),
        };
        assert_eq!(got, want, "MDB_SET {:?}", &probe[..probe.len().min(4)]);
        same_state(txn.cur(ca), txn.cur(cb));
        txn.close_slot(a);
        txn.close_slot(b);

        let (a, b) = (txn.open_slot(dbi, false), txn.open_slot(dbi, false));
        let (ca, cb) = (CRef::main(a), CRef::main(b));
        let got = txn
            .seek_last(ca, &db, cmp, |k| k.map(<[u8]>::to_vec))
            .map_err(|e| e.to_err_code());
        let want = txn
            .read_op(cb, |t, c, mx| {
                cursor_last_for_append(t, c, mx).map(|kv| kv.map(|kv| kv.key.to_vec()))
            })
            .map_err(|e| e.to_err_code());
        assert_eq!(got, want, "last");
        same_state(txn.cur(ca), txn.cur(cb));
        txn.close_slot(a);
        txn.close_slot(b);
    }

    fn probes() -> Vec<Vec<u8>> {
        let mut p: Vec<Vec<u8>> = [0, 1, 2, 777, 1500, 3001, 5998, 5999, 6000, 9999]
            .into_iter()
            .map(key)
            .collect();
        p.push(Vec::new());
        p.push(b"a".to_vec());
        p.push(vec![0]);
        p
    }

    fn check_all(txn: &mut RwTxn<'_>) {
        for probe in probes() {
            check(txn, Dbi::MAIN.0 as usize, &probe);
        }
    }

    #[test]
    fn positioning_in_place_matches_the_read_path() {
        positioning_matches(Format::V09);
        // LMDB 1.0 (its layout and page header), on 4 KiB pages.
        positioning_matches(Format::V10);
    }

    fn positioning_matches(format: Format) {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = Environment::builder();
        builder
            .map_size(256 << 20)
            .flags(EnvironmentFlags::NO_SYNC)
            .format(format);
        if format == Format::V10 {
            builder.page_size(4096);
        }
        let env = builder.open(dir.path()).unwrap();
        assert_eq!(env.layout().format, format);
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
        // An empty tree.
        check_all(&mut txn);
        for i in 0..3000u32 {
            txn.put(db, &key(2 * i), b"v", WriteFlags::empty()).unwrap();
        }
        let depth = txn.stat(db).unwrap().depth;
        assert!(depth >= 3, "depth {depth}");
        // Dirty pages.
        check_all(&mut txn);
        txn.commit().unwrap();

        // Clean pages, then some dirty again.
        let mut txn = env.begin_rw_txn().unwrap();
        check_all(&mut txn);
        for i in 0..300u32 {
            txn.put(db, &key(20 * i + 1), b"w", WriteFlags::empty())
                .unwrap();
        }
        check_all(&mut txn);
        // Pages dirty in the parent, then in the child too.
        {
            let mut child = txn.begin_nested_txn().unwrap();
            check_all(&mut child);
            child.put(db, &key(3), b"c", WriteFlags::empty()).unwrap();
            check_all(&mut child);
        }
        txn.abort();

        // Spilled pages.
        env.set_dirty_limit(64);
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..3000u32 {
            txn.put(db, &key(2 * i), b"spill", WriteFlags::empty())
                .unwrap();
        }
        assert!(!txn.core.spilled.is_empty());
        check_all(&mut txn);
    }
}
