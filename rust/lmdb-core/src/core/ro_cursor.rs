//! The public read cursor: [`RoCursor`] and [`CursorOp`].
//!
//! [`RoCursor::get`] is `mdb_cursor_get` (mdb.c line 6458): it validates the
//! operation against the database type and dispatches to the movement
//! functions in `super::cursor`. Values returned borrow the transaction
//! (`'txn`): pages of a read snapshot never move or change while the
//! transaction is alive.
//!
//! Differences to the C signature: C's `key`/`data` are in/out parameters;
//! here the inputs are `Option<&[u8]>` and the position is returned as
//! `Some((key, data))`. `MDB_NOTFOUND` becomes `Ok(None)`. Operations that in
//! C leave `key` untouched (`MDB_SET`, `MDB_GET_BOTH`, `MDB_GET_MULTIPLE`,
//! …) return the key stored in the database.

use crate::cmp::Comparator;
use crate::consts::*;
use crate::core::cursor::{
    corrupted, cursor_first, cursor_get_current, cursor_last, cursor_next, cursor_prev, cursor_set,
    new_xcursor, node_read, not_found, Cursor, Kv, C_DEL, C_EOF, C_INITIALIZED,
};
use crate::core::dbi::{Dbi, TxnDb};
use crate::core::txn::TxnCore;
use crate::error::{Error, MdbError, Result};

/// Cursor operations (`MDB_cursor_op`), with the C enum values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum CursorOp {
    /// `MDB_FIRST`: position at the first key/data item.
    First = MDB_FIRST,
    /// `MDB_FIRST_DUP`: position at the first data item of the current key
    /// (`DUP_SORT` only).
    FirstDup = MDB_FIRST_DUP,
    /// `MDB_GET_BOTH`: position at the key/data pair (`DUP_SORT` only).
    GetBoth = MDB_GET_BOTH,
    /// `MDB_GET_BOTH_RANGE`: position at the key, nearest data item
    /// (`DUP_SORT` only).
    GetBothRange = MDB_GET_BOTH_RANGE,
    /// `MDB_GET_CURRENT`: return the key/data at the cursor position.
    GetCurrent = MDB_GET_CURRENT,
    /// `MDB_GET_MULTIPLE`: return up to a page of duplicate data items of
    /// the current key (`DUP_FIXED` only).
    GetMultiple = MDB_GET_MULTIPLE,
    /// `MDB_LAST`: position at the last key/data item.
    Last = MDB_LAST,
    /// `MDB_LAST_DUP`: position at the last data item of the current key.
    LastDup = MDB_LAST_DUP,
    /// `MDB_NEXT`: position at the next data item.
    Next = MDB_NEXT,
    /// `MDB_NEXT_DUP`: position at the next data item of the current key.
    NextDup = MDB_NEXT_DUP,
    /// `MDB_NEXT_MULTIPLE`: return the next page of duplicates of the
    /// current key (`DUP_FIXED` only).
    NextMultiple = MDB_NEXT_MULTIPLE,
    /// `MDB_NEXT_NODUP`: position at the first data item of the next key.
    NextNoDup = MDB_NEXT_NODUP,
    /// `MDB_PREV`: position at the previous data item.
    Prev = MDB_PREV,
    /// `MDB_PREV_DUP`: position at the previous data item of the current key.
    PrevDup = MDB_PREV_DUP,
    /// `MDB_PREV_NODUP`: position at the last data item of the previous key.
    PrevNoDup = MDB_PREV_NODUP,
    /// `MDB_SET`: position at the given key.
    Set = MDB_SET,
    /// `MDB_SET_KEY`: position at the given key, return key and data.
    SetKey = MDB_SET_KEY,
    /// `MDB_SET_RANGE`: position at the first key greater than or equal to
    /// the given key.
    SetRange = MDB_SET_RANGE,
    /// `MDB_PREV_MULTIPLE`: return the previous page of duplicates
    /// (`DUP_FIXED` only).
    PrevMultiple = MDB_PREV_MULTIPLE,
}

impl CursorOp {
    /// The `MDB_cursor_op` value.
    pub fn to_raw(self) -> u32 {
        self as u32
    }

    /// The operation for an `MDB_cursor_op` value, if valid.
    pub fn from_raw(op: u32) -> Option<CursorOp> {
        use CursorOp::*;
        const ALL: [CursorOp; 19] = [
            First,
            FirstDup,
            GetBoth,
            GetBothRange,
            GetCurrent,
            GetMultiple,
            Last,
            LastDup,
            Next,
            NextDup,
            NextMultiple,
            NextNoDup,
            Prev,
            PrevDup,
            PrevNoDup,
            Set,
            SetKey,
            SetRange,
            PrevMultiple,
        ];
        ALL.get(op as usize).copied()
    }
}

/// A key/data pair borrowed from a transaction.
pub type Entry<'txn> = (&'txn [u8], &'txn [u8]);

/// A read cursor (`MDB_cursor`) over one database of a transaction.
///
/// Also usable on a write transaction (later stage), where the values it
/// returns may live in dirty pages; the transaction cannot be modified while
/// the cursor or anything it returned is alive.
pub struct RoCursor<'txn> {
    txn: &'txn TxnCore<'txn>,
    dbi: Dbi,
    /// The main cursor.
    mc: Cursor<'txn>,
    /// The sub-cursor over duplicates (`MDB_DUPSORT` only).
    mx: Option<Cursor<'txn>>,
    /// `md_dcmp`: the duplicate order.
    dcmp: Option<Comparator>,
}

impl std::fmt::Debug for RoCursor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoCursor")
            .field("dbi", &self.dbi)
            .field("depth", &self.mc.depth())
            .field("initialized", &self.mc.is_initialized())
            .finish()
    }
}

impl<'txn> RoCursor<'txn> {
    /// `mdb_cursor_init` for `entry` (a fresh, non-stale record of `dbi`).
    pub(crate) fn new(txn: &'txn TxnCore<'txn>, dbi: Dbi, entry: &TxnDb) -> RoCursor<'txn> {
        RoCursor {
            txn,
            dbi,
            mc: Cursor::new(entry.db, entry.key_cmp, 0),
            mx: new_xcursor(entry),
            dcmp: entry.dup_cmp,
        }
    }

    /// A cursor over existing positions (used to run read operations on a
    /// write cursor's state, see `wcursor`).
    pub(crate) fn from_parts(
        txn: &'txn TxnCore<'txn>,
        dbi: Dbi,
        mc: Cursor<'txn>,
        mx: Option<Cursor<'txn>>,
        dcmp: Option<Comparator>,
    ) -> RoCursor<'txn> {
        RoCursor {
            txn,
            dbi,
            mc,
            mx,
            dcmp,
        }
    }

    /// The main cursor and sub-cursor, for writing their state back.
    pub(crate) fn parts(&self) -> (&Cursor<'txn>, Option<&Cursor<'txn>>) {
        (&self.mc, self.mx.as_ref())
    }

    /// The database this cursor reads (`mdb_cursor_dbi`).
    pub fn dbi(&self) -> Dbi {
        self.dbi
    }

    /// `mdb_cursor_get`: perform `op`. `key` is required by `Set`, `SetKey`,
    /// `SetRange`, `GetBoth` and `GetBothRange`; `data` by `GetBoth` and
    /// `GetBothRange` (`EINVAL` if missing). Returns `Ok(None)` for
    /// `MDB_NOTFOUND`.
    pub fn get(
        &mut self,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Option<Entry<'txn>>> {
        let rc = self.get_inner(key, data, op);
        if self.mc.flags & C_DEL != 0 {
            self.mc.flags ^= C_DEL;
        }
        match rc {
            Ok(kv) => Ok(Some((kv.key, kv.data))),
            Err(e) if e.not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn require_dupfixed(&self) -> Result<()> {
        if u32::from(self.mc.db.flags) & MDB_DUPFIXED == 0 || self.mx.is_none() {
            return Err(MdbError::Incompatible.into());
        }
        Ok(())
    }

    /// The key at the main cursor's position.
    fn current_key(&self) -> Result<&'txn [u8]> {
        self.mc.current_node()?.key().ok_or_else(corrupted)
    }

    fn get_inner(
        &mut self,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: CursorOp,
    ) -> Result<Kv<'txn>> {
        use CursorOp::*;
        let txn = self.txn;
        match op {
            GetCurrent => cursor_get_current(txn, &mut self.mc, self.mx.as_mut()),
            GetBoth | GetBothRange => {
                let data = data.ok_or_else(Error::einval)?;
                if self.mx.is_none() {
                    return Err(MdbError::Incompatible.into());
                }
                let key = key.ok_or_else(Error::einval)?;
                let (kv, _) = cursor_set(
                    txn,
                    &mut self.mc,
                    self.mx.as_mut(),
                    self.dcmp,
                    key,
                    Some(data),
                    op,
                    true,
                )?;
                Ok(kv)
            }
            Set | SetKey | SetRange => {
                let key = key.ok_or_else(Error::einval)?;
                let (kv, _) = cursor_set(
                    txn,
                    &mut self.mc,
                    self.mx.as_mut(),
                    self.dcmp,
                    key,
                    data,
                    op,
                    op != SetRange,
                )?;
                Ok(kv)
            }
            GetMultiple => {
                if !self.mc.is_initialized() {
                    return Err(Error::einval());
                }
                self.require_dupfixed()?;
                let key = self.current_key().unwrap_or(&[]);
                let mx = self.mx.as_mut().ok_or(MdbError::Incompatible)?;
                if mx.is_initialized() && mx.flags & C_EOF == 0 {
                    Ok(Kv {
                        key,
                        data: mx.fetch_multiple()?,
                    })
                } else {
                    // Nothing to fetch (a key with a single item, or the
                    // sub-cursor past its end): C returns success and leaves
                    // `data` untouched; we return empty data.
                    Ok(Kv { key, data: &[] })
                }
            }
            NextMultiple => {
                self.require_dupfixed()?;
                let kv = cursor_next(txn, &mut self.mc, self.mx.as_mut(), NextDup)?;
                match self.mx.as_mut() {
                    Some(mx) if mx.is_initialized() => Ok(Kv {
                        key: kv.key,
                        data: mx.fetch_multiple()?,
                    }),
                    _ => Err(not_found()),
                }
            }
            PrevMultiple => {
                self.require_dupfixed()?;
                if !self.mc.is_initialized() {
                    cursor_last(txn, &mut self.mc, self.mx.as_mut())?;
                }
                let key = self.current_key()?;
                match self.mx.as_mut() {
                    Some(mx) if mx.is_initialized() => {
                        mx.sibling(txn, false)?;
                        Ok(Kv {
                            key,
                            data: mx.fetch_multiple()?,
                        })
                    }
                    _ => Err(not_found()),
                }
            }
            Next | NextDup | NextNoDup => cursor_next(txn, &mut self.mc, self.mx.as_mut(), op),
            Prev | PrevDup | PrevNoDup => cursor_prev(txn, &mut self.mc, self.mx.as_mut(), op),
            First => cursor_first(txn, &mut self.mc, self.mx.as_mut()),
            Last => cursor_last(txn, &mut self.mc, self.mx.as_mut()),
            FirstDup | LastDup => self.move_in_dups(op == FirstDup),
        }
    }

    /// `MDB_FIRST_DUP` / `MDB_LAST_DUP` (`mmove` in `mdb_cursor_get`).
    fn move_in_dups(&mut self, first: bool) -> Result<Kv<'txn>> {
        if !self.mc.is_initialized() {
            return Err(Error::einval());
        }
        if self.mx.is_none() {
            return Err(MdbError::Incompatible.into());
        }
        let nkeys = self.mc.top_page().num_keys();
        if self.mc.ki_top() >= nkeys {
            self.mc.set_ki_top(nkeys);
            return Err(not_found());
        }
        self.mc.flags &= !C_EOF;
        let leaf = self.mc.current_node()?;
        let key = leaf.key().ok_or_else(corrupted)?;
        if !leaf.is_dupdata() {
            return Ok(Kv {
                key,
                data: node_read(self.txn, leaf)?,
            });
        }
        let mx = self.mx.as_mut().ok_or(MdbError::Incompatible)?;
        if mx.flags & C_INITIALIZED == 0 {
            return Err(Error::einval());
        }
        let dup = if first {
            cursor_first(self.txn, mx, None)?
        } else {
            cursor_last(self.txn, mx, None)?
        };
        Ok(Kv { key, data: dup.key })
    }

    /// `mdb_cursor_count`: number of data items for the current key (1 for
    /// a key without duplicates). `MdbError::Incompatible` unless the
    /// database is `DUP_SORT`; `MdbError::NotFound` if the cursor is not on
    /// an item.
    pub fn count(&mut self) -> Result<u64> {
        if self.mx.is_none() {
            return Err(MdbError::Incompatible.into());
        }
        if !self.mc.is_initialized() {
            return Err(Error::einval());
        }
        if self.mc.depth() == 0 {
            return Err(not_found());
        }
        if self.mc.flags & C_EOF != 0 {
            if self.mc.ki_top() >= self.mc.top_page().num_keys() {
                return Err(not_found());
            }
            self.mc.flags ^= C_EOF;
        }
        // C only checks the index under C_EOF and otherwise reads
        // NODEPTR(mp, ki) unchecked, typically reporting 1 for a cursor left
        // past the end by a failed MDB_SET. We report MDB_NOTFOUND
        // (KNOWN_C_BUGS.md).
        if self.mc.ki_top() >= self.mc.top_page().num_keys() {
            return Err(not_found());
        }
        let leaf = self.mc.current_node()?;
        if !leaf.is_dupdata() {
            return Ok(1);
        }
        match &self.mx {
            Some(mx) if mx.is_initialized() => Ok(mx.db.entries),
            _ => Err(Error::einval()),
        }
    }

    // --- convenience wrappers -------------------------------------------------

    /// `MDB_FIRST`.
    pub fn first(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::First)
    }

    /// `MDB_LAST`.
    pub fn last(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::Last)
    }

    /// `MDB_NEXT` (the first item if the cursor is not positioned).
    ///
    /// Named after the C operation; a cursor is not an `Iterator` because
    /// each step can fail.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::Next)
    }

    /// `MDB_PREV` (the last item if the cursor is not positioned).
    pub fn prev(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::Prev)
    }

    /// `MDB_NEXT_DUP`.
    pub fn next_dup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::NextDup)
    }

    /// `MDB_PREV_DUP`.
    pub fn prev_dup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::PrevDup)
    }

    /// `MDB_NEXT_NODUP`.
    pub fn next_nodup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::NextNoDup)
    }

    /// `MDB_PREV_NODUP`.
    pub fn prev_nodup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::PrevNoDup)
    }

    /// `MDB_FIRST_DUP`.
    pub fn first_dup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::FirstDup)
    }

    /// `MDB_LAST_DUP`.
    pub fn last_dup(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::LastDup)
    }

    /// `MDB_GET_CURRENT`.
    pub fn get_current(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::GetCurrent)
    }

    /// `MDB_SET`: position at `key` (exact match).
    pub fn set(&mut self, key: &[u8]) -> Result<Option<Entry<'txn>>> {
        self.get(Some(key), None, CursorOp::Set)
    }

    /// `MDB_SET_KEY`: position at `key` (exact match).
    pub fn set_key(&mut self, key: &[u8]) -> Result<Option<Entry<'txn>>> {
        self.get(Some(key), None, CursorOp::SetKey)
    }

    /// `MDB_SET_RANGE`: position at the first key `>= key`.
    pub fn set_range(&mut self, key: &[u8]) -> Result<Option<Entry<'txn>>> {
        self.get(Some(key), None, CursorOp::SetRange)
    }

    /// `MDB_GET_BOTH`: position at the exact key/data pair (`DUP_SORT`).
    pub fn get_both(&mut self, key: &[u8], data: &[u8]) -> Result<Option<Entry<'txn>>> {
        self.get(Some(key), Some(data), CursorOp::GetBoth)
    }

    /// `MDB_GET_BOTH_RANGE`: position at `key` and its first data item
    /// `>= data` (`DUP_SORT`).
    pub fn get_both_range(&mut self, key: &[u8], data: &[u8]) -> Result<Option<Entry<'txn>>> {
        self.get(Some(key), Some(data), CursorOp::GetBothRange)
    }

    /// `MDB_GET_MULTIPLE`: the duplicates on the sub-cursor's current page,
    /// concatenated (`DUP_FIXED`). The data is empty if there is nothing to
    /// fetch (C reports success but leaves `data` untouched then).
    pub fn get_multiple(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::GetMultiple)
    }

    /// `MDB_NEXT_MULTIPLE` (`DUP_FIXED`).
    pub fn next_multiple(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::NextMultiple)
    }

    /// `MDB_PREV_MULTIPLE` (`DUP_FIXED`).
    pub fn prev_multiple(&mut self) -> Result<Option<Entry<'txn>>> {
        self.get(None, None, CursorOp::PrevMultiple)
    }
}

#[cfg(test)]
mod tests {
    use super::CursorOp;

    #[test]
    fn op_values_match_c() {
        assert_eq!(CursorOp::First.to_raw(), 0);
        assert_eq!(CursorOp::GetMultiple.to_raw(), 5);
        assert_eq!(CursorOp::NextNoDup.to_raw(), 11);
        assert_eq!(CursorOp::SetRange.to_raw(), 17);
        assert_eq!(CursorOp::PrevMultiple.to_raw(), 18);
        for v in 0..19 {
            assert_eq!(CursorOp::from_raw(v).unwrap().to_raw(), v);
        }
        assert_eq!(CursorOp::from_raw(19), None);
    }
}
