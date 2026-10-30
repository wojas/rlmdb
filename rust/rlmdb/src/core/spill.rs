//! Spilling dirty pages to disk before commit: `mdb_page_spill` (mdb.c line
//! 2080), `mdb_pages_xkeep` (line 1987) and `mdb_page_unspill` (line 2435).
//!
//! A write transaction keeps at most `MDB_IDL_UM_MAX` (131071) pages dirty.
//! Before each put or delete, `mdb_cursor_put`/`mdb_cursor_del` estimate how
//! many pages the operation may dirty; if the room left is not larger,
//! about an eighth of the dirty list is written to the data file now, at
//! the pages' final positions, and remembered in the transaction's spill
//! list (`mt_spill_pgs`, here `TxnCore::spilled`). A spilled page is read
//! back from the map; if the transaction writes it again it is copied into
//! the dirty list under the same page number (`mdb_page_unspill`), so
//! nothing that points at it has to change. Pages written early but not
//! referenced at commit are simply garbage on disk, as in C: no snapshot
//! refers to them.
//!
//! Which pages are spilled follows C exactly (the choice decides what a
//! later free goes to: a spilled page is no longer dirty, so freeing it puts
//! it on the free list instead of making it loose): walking the dirty list
//! from its end, skip loose pages, pages under a cursor of the transaction
//! or the root of a dirty database (C marks them `P_KEEP`), and pages an
//! ancestor already spilled (spilling them again would overwrite the
//! ancestor's version on disk, which a child cannot undo); take the others
//! until enough are found.

use crate::consts::*;
use crate::core::dbi::DB_DIRTY;
use crate::core::page_ops;
use crate::core::txn::PageSource;
use crate::core::txn_rw::{RwTxn, TXN_ERROR, TXN_SPILLS};
use crate::core::wcursor::{CRef, SUBPAGE};
use crate::error::{MdbError, Result};
use crate::format::page::{P_DIRTY, P_KEEP, P_LOOSE, P_OVERFLOW, P_SUBP};

impl<'env> RwTxn<'env> {
    /// `mdb_page_spill`: make room in the dirty list for an operation of
    /// cursor `m0` estimated to dirty up to `need` pages, by writing at
    /// least an eighth of the dirty list's capacity to disk.
    pub(crate) fn page_spill(&mut self, m0: CRef, need: usize) -> Result<()> {
        let rc = self.page_spill0(m0, need);
        self.w.flags |= if rc.is_ok() { TXN_SPILLS } else { TXN_ERROR };
        rc
    }

    fn page_spill0(&mut self, m0: CRef, need: usize) -> Result<()> {
        // Preserve pages which may soon be dirtied again.
        let keep = self.pages_to_keep(m0)?;

        // Less aggressive spill: C spills only 1/8th of the dirty pages,
        // which its tests found to be the best tradeoff.
        let mut need = need.max(self.core.env.dirty_limit() / 8);

        // Flush from the tail forward, this saves a lot of shifting later
        // on (in C).
        let pgnos: Vec<u64> = self.core.dirty.pgnos().collect();
        let mut chosen: Vec<u64> = Vec::new();
        for &pgno in pgnos.iter().rev() {
            if need == 0 {
                break;
            }
            let l = self.core.layout;
            let flags = self
                .core
                .dirty
                .get(pgno)
                .map_or(0, |p| page_ops::flags(l, p));
            if flags & (P_LOOSE | P_KEEP) != 0 || keep.binary_search(&pgno).is_ok() {
                continue;
            }
            // Can't spill twice, make sure it's not already in a parent's
            // spill list.
            if self.core.parents.iter().any(|l| l.spilled.contains(&pgno)) {
                continue;
            }
            chosen.push(pgno);
            need -= 1;
        }
        chosen.reverse();

        // Flush the spilled part of the dirty list.
        self.write_pages(&chosen)?;
        self.core.spilled.extend(chosen);
        Ok(())
    }

    /// The pages `mdb_pages_xkeep(m0, P_DIRTY, 1)` marks `P_KEEP`, sorted:
    /// this transaction's dirty pages (not loose, not sub-pages) on the
    /// stack of an initialised cursor — `m0` and every tracked cursor,
    /// including an ancestor's shadowed ones — or of its sub-cursor when that
    /// is in a sub-DB, and the root pages of the databases written in this
    /// transaction.
    fn pages_to_keep(&self, m0: CRef) -> Result<Vec<u64>> {
        const MASK: u16 = P_SUBP | P_DIRTY | P_LOOSE | P_KEEP;
        let mut keep = Vec::new();
        let l = self.core.layout;
        let mut mark = |pgno: u64| {
            if let Some(page) = self.core.dirty.get(pgno) {
                if page_ops::flags(l, page) & MASK == P_DIRTY {
                    keep.push(pgno);
                }
            }
        };
        let arena = &self.w.cursors;
        let slots = std::iter::once(m0.slot)
            .chain((0..arena.len()).filter(|&i| arena.slot(i).is_some_and(|s| s.tracked)));
        for i in slots {
            let Some(s) = arena.slot(i) else { continue };
            let m = &s.main;
            if !m.is_init() {
                continue;
            }
            for &pgno in &m.pg[..m.snum] {
                if pgno != SUBPAGE {
                    mark(pgno);
                }
            }
            // Proceed to the sub-cursor if it is at a sub-database.
            let Some(x) = s.x.as_ref().filter(|x| x.c.is_init()) else {
                continue;
            };
            let top = m.snum.saturating_sub(1);
            if m.snum == 0 || m.pg[top] == SUBPAGE {
                continue;
            }
            let in_subdb = self.core.page(m.pg[top]).is_ok_and(|leaf| {
                leaf.is_leaf()
                    && leaf
                        .node(usize::from(m.ki[top]))
                        .is_some_and(|n| n.is_subdata())
            });
            if in_subdb {
                for &pgno in &x.c.pg[..x.c.snum] {
                    if pgno != SUBPAGE {
                        mark(pgno);
                    }
                }
            }
        }
        // Mark dirty root pages (only this transaction's own dirty pages
        // qualify, `level <= 1` in C).
        for e in self.core.dbs.borrow().iter() {
            if e.state & DB_DIRTY != 0 && e.db.root != P_INVALID {
                mark(e.db.root);
            }
        }
        keep.sort_unstable();
        keep.dedup();
        Ok(keep)
    }

    /// `mdb_page_unspill`: if `pgno` (not dirty) was spilled by this
    /// transaction or an ancestor, copy it back into this transaction's
    /// dirty list under the same number and return `true`. It stays in an
    /// ancestor's spill list until this transaction commits.
    pub(crate) fn page_unspill(&mut self, pgno: u64) -> Result<bool> {
        let PageSource::Spilled(level) = self.core.locate(pgno) else {
            return Ok(false);
        };
        if self.w.dirty_room == 0 {
            return Err(MdbError::TxnFull.into());
        }
        let ps = self.psize();
        let first = self.core.page(pgno)?;
        let num = if first.flags() & P_OVERFLOW != 0 {
            (first.overflow_pages() as usize).max(1)
        } else {
            1
        };
        let off = usize::try_from(pgno)
            .ok()
            .and_then(|p| p.checked_mul(ps))
            .ok_or(MdbError::PageNotFound)?;
        let src = self
            .core
            .map
            .slice(off, num * ps)
            .ok_or(MdbError::PageNotFound)?;
        let l = self.core.layout;
        let mut buf = self.core.dirty.alloc_copy(src);
        let flags = page_ops::flags(l, &buf);
        page_ops::set_flags(l, &mut buf, flags | P_DIRTY);
        if level == 1 {
            // If in current txn, this page is no longer spilled.
            self.core.spilled.remove(&pgno);
        }
        // Otherwise, if belonging to a parent txn, the page remains spilled
        // until the child commits.
        if self.core.dirty.insert(pgno, buf).is_none() {
            return Err(MdbError::Corrupted.into());
        }
        self.w.dirty_room -= 1;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use crate::core::audit::audit;
    use crate::core::{CursorOp, Dbi, Environment, RwTxn, Transaction};
    use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};

    /// An environment whose write transactions may hold only `limit` dirty
    /// pages (C: 131071), so that small transactions spill.
    fn env(dir: &std::path::Path, limit: usize) -> Environment {
        let env = Environment::builder()
            .map_size(256 << 20)
            .max_dbs(4)
            .flags(EnvironmentFlags::NO_SYNC)
            .open(dir)
            .unwrap();
        env.set_dirty_limit(limit);
        env
    }

    fn key(i: u32) -> [u8; 4] {
        i.to_be_bytes()
    }

    fn value(i: u32, round: u32, len: usize) -> Vec<u8> {
        let mut v = vec![(i ^ round) as u8; len];
        v[..4].copy_from_slice(&i.to_le_bytes());
        v
    }

    fn contents(env: &Environment, db: Dbi) -> Pairs {
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(db).unwrap();
        let mut out = Vec::new();
        while let Some((k, v)) = c.next().unwrap() {
            out.push((k.to_vec(), v.to_vec()));
        }
        out
    }

    /// The same writes with and without spilling must give the same data
    /// and a consistent page audit.
    type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

    fn run(limit: usize, work: impl Fn(&mut RwTxn<'_>, Dbi)) -> (Pairs, bool) {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), limit);
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(Some("s"), DatabaseFlags::CREATE).unwrap();
        work(&mut txn, db);
        let spilled = txn.w.flags & super::TXN_SPILLS != 0;
        txn.commit().unwrap();
        audit(&env);
        (contents(&env, db), spilled)
    }

    fn mixed(txn: &mut RwTxn<'_>, db: Dbi) {
        let ps = txn.psize();
        for round in 0..3u32 {
            for i in 0..600u32 {
                let len = match i % 5 {
                    0 => ps * 2 + 100, // overflow run
                    1 => ps / 2 + 10,  // overflow page or big node
                    _ => 50 + (i as usize % 300),
                };
                txn.put(db, &key(i), &value(i, round, len), WriteFlags::empty())
                    .unwrap();
            }
            for i in (round..600).step_by(7) {
                txn.del(db, &key(i), None).unwrap();
            }
        }
    }

    #[test]
    fn spilling_transaction_matches_unspilled() {
        let (big, spilled_big) = run(crate::consts::MDB_IDL_UM_MAX, mixed);
        assert!(!spilled_big);
        let (small, spilled_small) = run(64, mixed);
        assert!(spilled_small);
        assert_eq!(big, small);
    }

    #[test]
    fn large_transaction_commits_and_reuses_spilled_overflow_pages() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), 100);
        let ps = env.page_size();
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(Some("s"), DatabaseFlags::CREATE).unwrap();
        // Far more pages than the dirty list can hold.
        for i in 0..3000u32 {
            txn.put(db, &key(i), &value(i, 0, ps + 1), WriteFlags::empty())
                .unwrap();
        }
        assert!(txn.core.spilled.len() > 1000);
        assert!(txn.core.dirty.len() <= 100);
        // Overwrite spilled overflow pages in place (unspill), and free
        // some (spilled runs go back to the reclaimed pages if any).
        for i in (0..3000u32).step_by(3) {
            txn.put(db, &key(i), &value(i, 1, ps / 2 + 1), WriteFlags::empty())
                .unwrap();
        }
        for i in (1..3000u32).step_by(3) {
            txn.del(db, &key(i), None).unwrap();
        }
        txn.commit().unwrap();
        audit(&env);
        let got = contents(&env, db);
        assert_eq!(got.len(), 2000);
        for (k, v) in got {
            let i = u32::from_be_bytes(k.try_into().unwrap());
            let round = u32::from(i % 3 == 0);
            let len = if round == 1 { ps / 2 + 1 } else { ps + 1 };
            assert_eq!(v, value(i, round, len), "key {i}");
        }
        // A second large transaction reuses the freed pages.
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..3000u32 {
            txn.put(db, &key(i), &value(i, 2, 100), WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
        audit(&env);
    }

    #[test]
    fn cursors_survive_spills() {
        let dir = tempfile::tempdir().unwrap();
        // (With a much smaller limit, C's estimate of what one operation
        // dirties can be too small for deletes that rebalance a sub-DB and
        // the main tree: MDB_TXN_FULL, which C's comment on
        // mdb_page_spill mentions; at the real limit a spill frees 16383
        // pages.)
        let env = env(dir.path(), 256);
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn
            .create_db(Some("d"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
            .unwrap();
        let c1 = txn.open_cursor(db).unwrap();
        let c2 = txn.open_cursor(db).unwrap();
        let len = 400; // DUP_SORT items are at most 511 bytes
        for i in 0..8000u32 {
            for d in 0..(1 + i % 4) {
                txn.cursor_put(c1, &key(i), &value(d, i, len), WriteFlags::empty())
                    .unwrap();
            }
            if i == 500 {
                txn.cursor_get(c2, Some(&key(250)), None, CursorOp::Set)
                    .unwrap()
                    .unwrap();
            }
        }
        assert!(!txn.core.spilled.is_empty(), "no spill");
        let (k, _) = txn
            .cursor_get(c2, None, None, CursorOp::GetCurrent)
            .unwrap()
            .unwrap();
        assert_eq!(k, key(250));
        assert_eq!(txn.cursor_count(c2).unwrap(), 3);
        // Delete through the cursor while more spills happen.
        for _ in 0..3000 {
            match txn.cursor_del(c2, WriteFlags::empty()) {
                Ok(()) => {}
                Err(e) if e.not_found() => break,
                Err(e) => panic!("{e:?}"),
            }
        }
        txn.close_cursor(c1);
        txn.close_cursor(c2);
        txn.commit().unwrap();
        audit(&env);
    }

    /// Puts `key(i)` for `i` from `start` on until a put spills, plus `n`
    /// more; returns the next `i`. Right after a spill the transaction has
    /// room in its dirty list for a child.
    fn fill_until_spill(t: &mut RwTxn<'_>, db: Dbi, start: u32, len: usize) -> u32 {
        let mut i = start;
        loop {
            let before = t.core.spilled.len();
            t.put(db, &key(i), &value(i, 0, len), WriteFlags::empty())
                .unwrap();
            i += 1;
            if t.core.spilled.len() > before {
                return i;
            }
        }
    }

    #[test]
    fn nested_transactions_with_spills() {
        for commit_child in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let env = env(dir.path(), 512);
            let ps = env.page_size();
            let mut t = env.begin_rw_txn().unwrap();
            let db = t.create_db(Some("n"), DatabaseFlags::CREATE).unwrap();
            let mut i = 0;
            while i < 800 {
                i = fill_until_spill(&mut t, db, i, ps / 3);
            }
            let top = i;
            assert!(!t.core.spilled.is_empty(), "parent spilled");
            let n = {
                let mut c = t.begin_nested_txn().unwrap();
                // Touch pages the parent spilled or still has dirty, and
                // spill in the child too. (Not too many of the parent's
                // spilled pages: a child cannot spill them again, see
                // `child_of_a_full_parent_may_run_out_of_room`.)
                for i in (0..top).step_by(30) {
                    c.put(db, &key(i), &value(i, 1, ps / 3), WriteFlags::empty())
                        .unwrap();
                }
                let mut i = top;
                while i < top + 800 {
                    i = fill_until_spill(&mut c, db, i, 2 * ps);
                }
                let ctop = i;
                {
                    let mut g = c.begin_nested_txn().unwrap();
                    for i in (1..top).step_by(37) {
                        g.del(db, &key(i), None).unwrap();
                    }
                    for i in (top..ctop).step_by(5) {
                        g.del(db, &key(i), None).unwrap();
                    }
                    g.commit().unwrap();
                }
                assert!(!c.core.spilled.is_empty(), "child spilled");
                assert_eq!(c.get(db, &key(1)).unwrap(), None);
                if commit_child {
                    c.commit().unwrap();
                } else {
                    c.abort();
                }
                let n = if commit_child {
                    ctop as usize - (1..top).step_by(37).count() - (top..ctop).step_by(5).count()
                } else {
                    top as usize
                };
                assert_eq!(t.stat(db).unwrap().entries, n);
                n
            };
            t.commit().unwrap();
            audit(&env);
            let got = contents(&env, db);
            for (k, v) in &got {
                let i = u32::from_be_bytes(k[..].try_into().unwrap());
                let want = if i >= top {
                    value(i, 0, 2 * ps)
                } else if commit_child && i % 30 == 0 {
                    value(i, 1, ps / 3)
                } else {
                    value(i, 0, ps / 3)
                };
                assert_eq!(v, &want, "key {i} commit_child={commit_child}");
            }
            assert_eq!(got.len(), n);
        }
    }

    /// A database's contents, as the slot tests below expect them.
    type Model = std::collections::BTreeMap<Vec<u8>, Vec<u8>>;

    /// The key under cursor `c`.
    fn current(t: &mut RwTxn<'_>, c: crate::core::CursorId) -> Vec<u8> {
        let (k, _) = t
            .cursor_get(c, None, None, CursorOp::GetCurrent)
            .unwrap()
            .unwrap();
        k.to_vec()
    }

    /// Overwrites the item under each cursor (`MDB_CURRENT`, same size).
    fn overwrite(t: &mut RwTxn<'_>, cur: &[crate::core::CursorId], tag: u32, model: &mut Model) {
        for &c in cur {
            let k = current(t, c);
            let v = value(u32::from_be_bytes(k[..].try_into().unwrap()), tag, 200);
            t.cursor_put(c, &k, &v, WriteFlags::CURRENT).unwrap();
            model.insert(k, v);
        }
    }

    /// Moves cursor `c` `n` items with `op`, wrapping around at the ends.
    fn step(t: &mut RwTxn<'_>, c: crate::core::CursorId, op: CursorOp, n: usize) {
        for _ in 0..n {
            if t.cursor_get(c, None, None, op).unwrap().is_none() {
                let wrap = if op == CursorOp::Next {
                    CursorOp::First
                } else {
                    CursorOp::Last
                };
                t.cursor_get(c, None, None, wrap).unwrap().unwrap();
            }
        }
    }

    /// Puts new keys from `start` on until a put spills; returns the next
    /// key.
    fn fill(t: &mut RwTxn<'_>, db: Dbi, start: u32, model: &mut Model) -> u32 {
        let next = fill_until_spill(t, db, start, 200);
        for i in start..next {
            model.insert(key(i).to_vec(), value(i, 0, 200));
        }
        next
    }

    /// Write cursors remember where their pages are in the dirty list
    /// (slot hints, see the `dirty` module), and a hint must never lead
    /// to a wrong page. Here the parent's cursors write through children
    /// that copy the parent's pages, spill (their own pages, and pages the
    /// cursors held before), unspill what the parent spilled, and commit or
    /// abort; the parent spills pages its cursors touched before and writes
    /// through them again after each child. Every write is checked against
    /// a model, the cursors' positions after an abort against where they
    /// were, and the result by a page audit.
    #[test]
    fn cursor_slots_survive_spills_and_children() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), 128);
        let mut t = env.begin_rw_txn().unwrap();
        let db = t.create_db(Some("h"), DatabaseFlags::CREATE).unwrap();
        let mut model = Model::new();
        for i in 0..300u32 {
            t.put(db, &key(i), &value(i, 0, 200), WriteFlags::empty())
                .unwrap();
            model.insert(key(i).to_vec(), value(i, 0, 200));
        }
        let cur = [t.open_cursor(db).unwrap(), t.open_cursor(db).unwrap()];
        t.cursor_get(cur[0], Some(&key(10)), None, CursorOp::Set)
            .unwrap()
            .unwrap();
        t.cursor_get(cur[1], Some(&key(290)), None, CursorOp::Set)
            .unwrap()
            .unwrap();
        let mut next = 1000u32;
        for round in 0..8u32 {
            overwrite(&mut t, &cur, round, &mut model);
            // Leave the touched pages behind and spill.
            step(&mut t, cur[0], CursorOp::Next, 25);
            step(&mut t, cur[1], CursorOp::Prev, 25);
            next = fill(&mut t, db, next, &mut model);
            overwrite(&mut t, &cur, 10 + round, &mut model);

            let saved = model.clone();
            let at = [current(&mut t, cur[0]), current(&mut t, cur[1])];
            let commit = round % 2 == 0;
            {
                let mut c = t.begin_nested_txn().unwrap();
                overwrite(&mut c, &cur, 20 + round, &mut model);
                step(&mut c, cur[0], CursorOp::Next, 40);
                step(&mut c, cur[1], CursorOp::Prev, 40);
                overwrite(&mut c, &cur, 30 + round, &mut model);
                next = fill(&mut c, db, next, &mut model);
                step(&mut c, cur[0], CursorOp::Prev, 60);
                overwrite(&mut c, &cur, 40 + round, &mut model);
                // Delete under one cursor, then put it on a key again.
                let k = current(&mut c, cur[1]);
                c.cursor_del(cur[1], WriteFlags::empty()).unwrap();
                model.remove(&k);
                c.cursor_get(cur[1], Some(&k), None, CursorOp::SetRange)
                    .unwrap()
                    .unwrap();
                overwrite(&mut c, &cur, 50 + round, &mut model);
                if commit {
                    c.commit().unwrap();
                } else {
                    c.abort();
                }
            }
            if !commit {
                model = saved;
                assert_eq!(current(&mut t, cur[0]), at[0], "round {round}");
                assert_eq!(current(&mut t, cur[1]), at[1], "round {round}");
            }
            overwrite(&mut t, &cur, 60 + round, &mut model);
            step(&mut t, cur[0], CursorOp::Next, 3);
            overwrite(&mut t, &cur, 70 + round, &mut model);
            let mut c = t.cursor(db).unwrap();
            let mut got = Model::new();
            while let Some((k, v)) = c.next().unwrap() {
                got.insert(k.to_vec(), v.to_vec());
            }
            assert!(got == model, "round {round}");
        }
        assert!(!t.core.spilled.is_empty(), "no spill");
        for c in cur {
            t.close_cursor(c);
        }
        t.commit().unwrap();
        audit(&env);
        let want: Pairs = model.into_iter().collect();
        assert!(contents(&env, db) == want);
    }

    /// The case slot hints are checked for: a cursor leaves a dirty page
    /// by moving (its slot stays in the cursor), the page is spilled (its
    /// slot emptied), written again (unspilled into another slot) and the
    /// cursor comes back. Reading and writing through it must see the new
    /// version, not whatever its old slot holds.
    #[test]
    fn cursor_returns_to_a_spilled_page() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), 1024);
        let db = {
            let mut t = env.begin_rw_txn().unwrap();
            let db = t.create_db(Some("r"), DatabaseFlags::CREATE).unwrap();
            for i in 0..2000u32 {
                t.put(db, &key(i), &value(i, 0, 200), WriteFlags::empty())
                    .unwrap();
            }
            t.commit().unwrap();
            db
        };
        let mut model: Model = contents(&env, db).into_iter().collect();
        let mut t = env.begin_rw_txn().unwrap();
        let c = t.open_cursor(db).unwrap();
        for round in 0..4u32 {
            // Dirty the cursor's leaf through it, then move away.
            t.cursor_get(c, Some(&key(100 + 400 * round)), None, CursorOp::Set)
                .unwrap()
                .unwrap();
            overwrite(&mut t, &[c], round, &mut model);
            let left = current(&mut t, c);
            step(&mut t, c, CursorOp::Next, 150);
            // A page dirtied later, so that the old leaf's slot is not the
            // first one reused below.
            let last = key(1999).to_vec();
            let v = value(1999, round, 200);
            t.put(db, &last, &v, WriteFlags::empty()).unwrap();
            model.insert(last, v);
            // Spill everything not under a cursor, the old leaf too.
            let m0 = crate::core::wcursor::CRef::main(t.open_slot(db.0 as usize, false));
            t.page_spill(m0, 1 << 20).unwrap();
            t.close_slot(m0.slot);
            // Write the old leaf again: it comes back in another slot.
            let v = value(7, 20 + round, 200);
            t.put(db, &left, &v, WriteFlags::empty()).unwrap();
            model.insert(left.clone(), v.clone());
            // Back to it, through the cursor's old slot hint.
            step(&mut t, c, CursorOp::Prev, 150);
            let (k, got) = t
                .cursor_get(c, None, None, CursorOp::GetCurrent)
                .unwrap()
                .unwrap();
            assert_eq!((k, got), (&left[..], &v[..]), "round {round}");
            overwrite(&mut t, &[c], 40 + round, &mut model);
        }
        assert!(!t.core.spilled.is_empty(), "no spill");
        t.close_cursor(c);
        t.commit().unwrap();
        audit(&env);
        let want: Pairs = model.into_iter().collect();
        assert!(contents(&env, db) == want);
    }

    /// C's limitation, kept (`KNOWN_C_BUGS.md`): a child starts with its
    /// parent's remaining dirty room and can only spill its own pages, so
    /// right after a parent filled its dirty list a child may fail with
    /// `MDB_TXN_FULL`. The parent is not affected once the child is gone.
    #[test]
    fn child_of_a_full_parent_may_run_out_of_room() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), 64);
        let ps = env.page_size();
        let mut t = env.begin_rw_txn().unwrap();
        let db = t.create_db(Some("n"), DatabaseFlags::CREATE).unwrap();
        for i in 0..400u32 {
            t.put(db, &key(i), &value(i, 0, ps / 3), WriteFlags::empty())
                .unwrap();
        }
        let mut failed = false;
        for _ in 0..50 {
            let mut c = t.begin_nested_txn().unwrap();
            let rc = (0..400u32)
                .try_for_each(|i| c.put(db, &key(i), &value(i, 1, ps / 3), WriteFlags::empty()));
            match rc {
                Ok(()) => c.abort(),
                Err(e) => {
                    assert!(
                        matches!(e, crate::Error::Mdb(crate::MdbError::TxnFull)),
                        "{e:?}"
                    );
                    failed = true;
                    break;
                }
            }
            // Use up some room in the parent and try again.
            t.put(db, &key(1000), &value(0, 0, 10), WriteFlags::empty())
                .unwrap();
        }
        assert!(failed, "no child ran out of room");
        t.put(db, &key(2000), &value(0, 0, 10), WriteFlags::empty())
            .unwrap();
        t.commit().unwrap();
        audit(&env);
    }

    /// Soak of spilling (ignored by default; `SOAK_SECS`, default 20):
    ///
    /// ```sh
    /// SOAK_SECS=120 cargo test -p rlmdb --release --lib core::spill::tests::soak -- --ignored --nocapture
    /// ```
    ///
    /// Random transactions under a dirty-list limit of 256 pages, so that
    /// most transactions spill: puts of small values and of
    /// overflow runs, deletes, nested transactions (committed and aborted,
    /// up to three deep) and dropped or emptied databases, checked against
    /// a model and with the page audit after every commit.
    #[test]
    #[ignore]
    fn soak() {
        use std::collections::BTreeMap;
        type Model = BTreeMap<Vec<u8>, Vec<u8>>;
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % n as u64) as usize
            }
        }
        /// Returns `false` if a nested transaction ran out of dirty room
        /// (it must then be aborted: committing a failed child fails the
        /// parent too).
        fn run(
            txn: &mut RwTxn<'_>,
            rng: &mut Rng,
            m: &mut [Model; 2],
            depth: usize,
            ps: usize,
        ) -> bool {
            // A child may run out of dirty room (C's limitation, see
            // `child_of_a_full_parent_may_run_out_of_room`).
            macro_rules! ok {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(crate::Error::Mdb(crate::MdbError::TxnFull)) if depth > 0 => {
                            return false
                        }
                        Err(e) => panic!("{e:?}"),
                    }
                };
            }
            for _ in 0..1 + rng.below(30) {
                let which = rng.below(2);
                let name = ["one", "two"][which];
                let db = ok!(txn.create_db(Some(name), DatabaseFlags::CREATE));
                match rng.below(20) {
                    0 => {
                        ok!(txn.drop_db(db));
                        m[which].clear();
                    }
                    1 => {
                        ok!(txn.clear_db(db));
                        m[which].clear();
                    }
                    2..=3 if depth < 3 => {
                        let Ok(mut c) = txn.begin_nested_txn() else {
                            continue;
                        };
                        let mut m2 = m.clone();
                        if !run(&mut c, rng, &mut m2, depth + 1, ps) || rng.below(3) == 0 {
                            c.abort();
                        } else {
                            c.commit().unwrap();
                            *m = m2;
                        }
                    }
                    4..=6 => {
                        for _ in 0..rng.below(50) {
                            let k = format!("k{:05}", rng.below(5000)).into_bytes();
                            if ok!(txn.del(db, &k, None)) {
                                assert!(m[which].remove(&k).is_some());
                            }
                        }
                    }
                    _ => {
                        for _ in 0..rng.below(120) {
                            let k = format!("k{:05}", rng.below(5000)).into_bytes();
                            let len = if rng.below(8) == 0 {
                                ps * (1 + rng.below(3))
                            } else {
                                1 + rng.below(300)
                            };
                            let v = vec![rng.below(256) as u8; len];
                            ok!(txn.put(db, &k, &v, WriteFlags::empty()));
                            m[which].insert(k, v);
                        }
                    }
                }
            }
            true
        }
        let secs: u64 = std::env::var("SOAK_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path(), 256);
        let ps = env.page_size();
        let mut rng = Rng(0x5eed_1234_5678_9abc);
        let mut model: [Model; 2] = Default::default();
        let start = std::time::Instant::now();
        let (mut commits, mut spilled) = (0, 0);
        while start.elapsed() < std::time::Duration::from_secs(secs) {
            let mut txn = env.begin_rw_txn().unwrap();
            let mut m2 = model.clone();
            assert!(run(&mut txn, &mut rng, &mut m2, 0, ps));
            spilled += usize::from(txn.w.flags & super::TXN_SPILLS != 0);
            if rng.below(8) == 0 {
                txn.abort();
            } else {
                txn.commit().unwrap();
                model = m2;
                commits += 1;
            }
            audit(&env);
            let txn = env.begin_ro_txn().unwrap();
            for (which, name) in ["one", "two"].iter().enumerate() {
                let got: Model = match txn.open_db(Some(name)) {
                    Ok(db) => {
                        let mut c = txn.cursor(db).unwrap();
                        let mut out = Model::new();
                        while let Some((k, v)) = c.next().unwrap() {
                            out.insert(k.to_vec(), v.to_vec());
                        }
                        out
                    }
                    Err(e) if e.not_found() => Model::new(),
                    Err(e) => panic!("{e:?}"),
                };
                assert!(got == model[which], "{name} differs after commit {commits}");
            }
        }
        eprintln!("spill soak: {commits} commits, {spilled} transactions spilled");
        // (About half of them; fewer as the databases grow, so a faster
        // build that gets further in the time spills a smaller share.)
        assert!(spilled > commits / 4);
    }
}
