//! Nested write transactions: `mdb_txn_begin` with a parent (mdb.c line
//! 2912), the nested branch of `mdb_txn_commit` (line 3594), `mdb_txn_end`
//! for a child (line 3064), `mdb_cursor_shadow` (line 2622) and
//! `mdb_cursors_close` (line 2665).
//!
//! A child sees everything its parent did, and its own changes; when it
//! commits they become the parent's, when it aborts they vanish. Nothing is
//! written to the data file at a nested commit (except pages a transaction
//! spills, see `super::spill`).
//!
//! # How the state moves
//!
//! In C the child points at its parent (`mt_parent`) and the parent may not
//! be used until the child ends (`MDB_TXN_HAS_CHILD`). Here the child
//! borrows the parent mutably, which enforces the same rule at compile
//! time, and takes the parent's state with it for its lifetime:
//!
//! * the parent's dirty and spilled pages become the child's first
//!   ancestor [`Layer`]; reads walk the child's own pages, then the layers
//!   (`mdb_page_get`), and a write to a page dirty in an ancestor first
//!   copies it into the child's dirty list under the same page number
//!   (`mdb_page_touch`);
//! * the parent's cursor arena moves to the child, so the child's page
//!   moves, splits and merges keep the parent's cursors up to date as C's
//!   fixup loops do on the shadowed cursors; a copy of their state
//!   (`mc_backup`) is kept to restore them if the child aborts;
//! * the DB table, `mt_next_pgno`, `mt_dirty_room`, the flags and a copy of
//!   the reclaimed free pages (`me_pghead`, saved in C's `MDB_ntxn`) are
//!   copied; the child starts with an empty free list and no loose pages.
//!
//! When the child ends, everything goes back. A commit merges: the free
//! list is appended to the parent's, the child's dirty pages replace the
//! parent's (and leave the parent's spill list), pages the child spilled
//! leave the parent's dirty list and join its spill list, loose pages are
//! appended, the DB table, next page number, dirty room, flags and free-page
//! state replace the parent's (keeping the parent's `DB_NEW` marks). An
//! abort restores the parent's cursors and closes the handles of databases
//! the child created (`mdb_dbis_update`).

use std::sync::Arc;

use crate::consts::*;
use crate::core::dbi::{name_id, TxnDb, DB_NEW, DB_VALID};
use crate::core::dirty::{DirtyPages, Layer, SpillSet};
use crate::core::txn::TxnCore;
use crate::core::txn_rw::{RwTxn, WriteState, TXN_ERROR, TXN_FINISHED, TXN_HAS_CHILD};
use crate::core::wcursor::Slot;
use crate::error::Result;
use crate::idl::Idl;

/// The parent link of a nested transaction.
pub(crate) struct Nested<'p> {
    /// The parent (`mt_parent`), borrowed for the child's whole life.
    parent: &'p mut (dyn NestedParent + 'p),
    /// The parent's cursors as they were when the child began (`mc_backup`
    /// of every shadowed cursor).
    cursor_backup: Vec<(usize, Slot)>,
}

/// How a nested transaction ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// Successful `mdb_txn_commit`: merge into the parent.
    Commit,
    /// `mdb_txn_abort`: discard.
    Abort,
    /// `mdb_txn_commit` of a failed child: discard, and the parent fails
    /// too (`parent->mt_flags |= MDB_TXN_ERROR`).
    Failed,
}

/// The parts of a parent transaction a child takes over and gives back.
/// (A trait object, because the child's type cannot name the parent's
/// lifetime.)
pub(crate) trait NestedParent {
    fn parts(&mut self) -> ParentParts<'_>;
}

/// Mutable access to a parent transaction's state.
pub(crate) struct ParentParts<'a> {
    dirty: &'a mut DirtyPages,
    spilled: &'a mut SpillSet,
    parents: &'a mut Vec<Layer>,
    dbs: &'a mut Vec<TxnDb>,
    next_pgno: &'a mut u64,
    w: &'a mut WriteState,
}

impl NestedParent for RwTxn<'_> {
    fn parts(&mut self) -> ParentParts<'_> {
        ParentParts {
            dirty: &mut self.core.dirty,
            spilled: &mut self.core.spilled,
            parents: &mut self.core.parents,
            dbs: self.core.dbs.get_mut(),
            next_pgno: &mut self.core.next_pgno,
            w: &mut self.w,
        }
    }
}

impl<'env> RwTxn<'env> {
    /// `mdb_txn_begin(env, parent, 0)`: begin a transaction nested in this
    /// one. It sees all of this transaction's changes; its own become part
    /// of this transaction when it [commits](RwTxn::commit) and are
    /// discarded when it aborts or is dropped. The parent cannot be used
    /// while the child exists (it is borrowed), nor can the parent's
    /// [`RwCursor`](crate::core::RwCursor)s.
    ///
    /// Handle-based cursors ([`CursorId`](crate::core::CursorId)) behave
    /// as C's cursors do across `mdb_txn_begin` with a parent: the
    /// parent's (and further ancestors') handles can be used through the
    /// child with all of `cursor_get`, `cursor_put`, `cursor_del`,
    /// `cursor_count`, … and then work on the child's data; the child's
    /// changes move them along as they move the child's own cursors. When
    /// the child commits, they keep the position they have then; when it
    /// aborts, they are restored to the position they had when it began
    /// (`mc_backup`). [`RwTxn::close_cursor`] on such a handle in the child
    /// does nothing. Cursors opened in the child are closed when it ends
    /// (their handles become invalid, `EINVAL`).
    ///
    /// Children can be nested further. `MdbError::BadTxn` if this
    /// transaction has failed.
    pub fn begin_nested_txn(&mut self) -> Result<RwTxn<'_>> {
        self.check_usable()?;
        let depth = self.w.cursors.depth() + 1;

        // Copy parent's mt_dbflags, but clear DB_NEW.
        let dbs: Vec<TxnDb> = self
            .core
            .dbs
            .get_mut()
            .iter()
            .map(|e| TxnDb {
                state: e.state & !DB_NEW,
                ..*e
            })
            .collect();
        let mut core = TxnCore::new(self.core.env, dbs);
        core.map = Arc::clone(&self.core.map);
        core.txnid = self.core.txnid;
        core.next_pgno = self.core.next_pgno;
        let layer = Layer {
            dirty: std::mem::take(&mut self.core.dirty),
            spilled: std::mem::take(&mut self.core.spilled),
        };
        core.parents = std::iter::once(layer)
            .chain(std::mem::take(&mut self.core.parents))
            .collect();

        // mdb_cursor_shadow: the child tracks the parent's cursors.
        let mut cursors = std::mem::take(&mut self.w.cursors);
        let cursor_backup = cursors.backup();
        cursors.set_depth(depth);

        let w = WriteState {
            flags: self.w.flags,
            free_pgs: Idl::new(),
            loose: Vec::new(),
            dirty_room: self.w.dirty_room,
            // The child works on a copy of the reclaimed pages; the
            // parent's stay as they are in case the child aborts.
            pghead: self.w.pghead.clone(),
            pglast: self.w.pglast,
            pg_oldest: self.w.pg_oldest,
            cursors,
            base_main: None,
            dropped: Vec::new(),
        };
        self.w.flags |= TXN_HAS_CHILD;
        Ok(RwTxn::from_parts(
            core,
            w,
            Nested {
                parent: self,
                cursor_backup,
            },
        ))
    }

    /// `true` for a nested transaction (`mt_parent != NULL`).
    pub(crate) fn has_parent(&self) -> bool {
        self.nested.is_some()
    }

    /// Ends a nested transaction: merge into the parent or discard (see the
    /// module documentation).
    pub(crate) fn end_nested(&mut self, how: End) {
        let Some(nested) = self.nested.take() else {
            return;
        };
        let Nested {
            parent,
            cursor_backup,
        } = nested;
        let p = parent.parts();
        if how != End::Commit {
            // Reads now see the parent's state: drop our own pages first.
            self.core.dirty.clear();
            self.core.spilled.clear();
            let main = p.dbs[MAIN_DBI as usize].db;
            self.close_vanished_handles(main);
        }

        // The parent's pages come back.
        let mut layers = std::mem::take(&mut self.core.parents).into_iter();
        let mine = layers.next().unwrap_or_default();
        *p.parents = layers.collect();
        let mut p_dirty = mine.dirty;
        let mut p_spilled = mine.spilled;

        let mut cursors = std::mem::take(&mut self.w.cursors);
        let depth = cursors.depth();
        // Only the parent's cursors survive the child.
        cursors.release_owned(depth);

        if how == End::Commit {
            // Append our free list to parent's.
            p.w.free_pgs.append_list(&self.w.free_pgs);
            *p.next_pgno = self.core.next_pgno;
            p.w.flags = self.w.flags;

            // Update parent's DB table, preserving parent's DB_NEW status.
            let child_dbs = self.core.dbs.get_mut();
            for (i, (pe, ce)) in p.dbs.iter_mut().zip(child_dbs.iter()).enumerate() {
                let keep_new = if i < CORE_DBS as usize {
                    0
                } else {
                    pe.state & DB_NEW
                };
                *pe = *ce;
                pe.state |= keep_new;
            }

            let child_dirty = std::mem::take(&mut self.core.dirty);
            let child_spilled = std::mem::take(&mut self.core.spilled);
            // Remove anything in our dirty list from parent's spill list.
            if !p_spilled.is_empty() {
                for pgno in child_dirty.pgnos() {
                    p_spilled.remove(&pgno);
                }
            }
            // Remove anything in our spill list from parent's dirty list:
            // our newer version is on disk.
            if !child_spilled.is_empty() {
                p_dirty.retain(|pgno, _| !child_spilled.contains(&pgno));
            }
            // Merge our dirty list with parent's.
            p_dirty.merge(child_dirty);
            p.w.dirty_room = self.w.dirty_room;
            p_spilled.extend(child_spilled);

            // Append our loose page list to parent's. (C's list is popped
            // from its head, ours from the end of the vector: the child's
            // pages come after the parent's in C's order.)
            let mut loose = std::mem::take(&mut self.w.loose);
            loose.append(&mut p.w.loose);
            p.w.loose = loose;

            // The child's reclaimed-page state replaces the parent's.
            p.w.pghead = self.w.pghead.take();
            p.w.pglast = self.w.pglast;
            p.w.dropped.append(&mut self.w.dropped);
        } else {
            // Close the handles of databases created in this child.
            let registry = &self.core.env.dbis;
            let child_dbs = self.core.dbs.get_mut();
            for (i, e) in child_dbs.iter().enumerate().skip(CORE_DBS as usize) {
                if e.state & DB_NEW != 0 {
                    registry.finish_new(i, false, e.db.flags);
                }
            }
            // Reopen the handles the child closed with drop_db: the drop is
            // undone, and C would leave the parent unable to commit its
            // changes to that DB (KNOWN_C_BUGS.md #20).
            for (i, mut slot, _) in std::mem::take(&mut self.w.dropped) {
                let pe = &mut p.dbs[i];
                // Only a handle of the database the parent has a view of
                // (the first drop of that name: later ones are of databases
                // the child created again).
                let same = slot.name.as_deref().map(name_id) == Some(pe.name_id);
                if pe.state & DB_VALID == 0 || !same {
                    continue;
                }
                slot.valid = pe.state & DB_NEW == 0;
                if let Some(seq) = registry.reopen(i, slot) {
                    pe.seq = seq;
                }
            }
            for (i, (pe, ce)) in p.dbs.iter_mut().zip(child_dbs.iter()).enumerate() {
                if pe.state == 0 {
                    continue;
                }
                let seq = registry.seq_of(i);
                if pe.state & DB_VALID != 0
                    && pe.seq != seq
                    && registry
                        .name_of(i)
                        .is_some_and(|n| name_id(&n) == pe.name_id)
                {
                    // The child opened the parent's database again in a
                    // new generation of the slot (after dropping it, say);
                    // as in C, where it bumped the sequence number in its
                    // parent's `mt_dbiseqs`, the handle stays usable.
                    pe.seq = seq;
                } else if ce.state != 0 && ce.seq == pe.seq {
                    // Comparators set during the child are environment-wide
                    // in C (`mdb_set_compare` writes `me_dbxs`) and survive
                    // the abort.
                    pe.key_cmp = ce.key_cmp;
                    pe.dup_cmp = ce.dup_cmp;
                }
            }
            cursors.restore(cursor_backup);
            if how == End::Failed {
                p.w.flags |= TXN_ERROR;
            }
        }
        // me_pgoldest lives in the environment in C; the child's view of the
        // oldest reader stays.
        p.w.pg_oldest = self.w.pg_oldest;
        p.w.flags &= !TXN_HAS_CHILD;
        cursors.set_depth(depth - 1);
        p.w.cursors = cursors;
        *p.dirty = p_dirty;
        *p.spilled = p_spilled;

        self.core.dirty.clear();
        self.core.spilled.clear();
        self.w.free_pgs.clear();
        self.w.loose.clear();
        self.w.pghead = None;
        self.w.flags = TXN_FINISHED;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::core::audit::audit;
    use crate::core::{Environment, RwTxn, Transaction};
    use crate::flags::{DatabaseFlags, EnvironmentFlags, WriteFlags};

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

    fn step(
        txn: &mut RwTxn<'_>,
        rng: &mut Rng,
        m: &mut Model,
        depth: usize,
        log: &mut Vec<String>,
    ) {
        for _ in 0..1 + rng.below(6) {
            let s = txn
                .create_db(Some("scratch"), DatabaseFlags::CREATE)
                .unwrap();
            match rng.below(6) {
                0 => {
                    log.push(format!("{depth}: drop"));
                    txn.drop_db(s).unwrap();
                    m.clear();
                }
                1 if depth < 3 => {
                    log.push(format!("{depth}: nb"));
                    let mut c = txn.begin_nested_txn().unwrap();
                    let mut m2 = m.clone();
                    step(&mut c, rng, &mut m2, depth + 1, log);
                    if rng.below(2) == 0 {
                        log.push(format!("{depth}: na"));
                        c.abort();
                    } else {
                        log.push(format!("{depth}: nc"));
                        c.commit().unwrap();
                        *m = m2;
                    }
                }
                _ => {
                    let n = rng.below(40);
                    log.push(format!("{depth}: fill {n}"));
                    for _ in 0..n {
                        let k = format!("s{:03}", rng.below(300)).into_bytes();
                        let v = vec![rng.below(250) as u8; 1 + rng.below(3000)];
                        txn.put(s, &k, &v, WriteFlags::empty()).unwrap();
                        m.insert(k, v);
                    }
                }
            }
            // The view of the handle must match the model at every step.
            let s = txn
                .create_db(Some("scratch"), DatabaseFlags::CREATE)
                .unwrap();
            let got: Model = {
                let mut c = txn.cursor(s).unwrap();
                let mut out = Model::new();
                while let Some((k, v)) = c.next().unwrap() {
                    out.insert(k.to_vec(), v.to_vec());
                }
                out
            };
            assert!(
                got == *m,
                "scratch differs at depth {depth}:\n{}",
                log.join("\n")
            );
        }
    }

    /// Dropping, re-creating and filling a database across nested
    /// transactions that commit or abort keeps the handle usable, the data
    /// right and every page accounted for.
    #[test]
    fn drops_across_nested_transactions() {
        for seed in 1..200u64 {
            let dir = tempfile::tempdir().unwrap();
            let env = Environment::builder()
                .map_size(64 << 20)
                .max_dbs(4)
                .flags(EnvironmentFlags::NO_SYNC)
                .open(dir.path())
                .unwrap();
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut model = Model::new();
            let mut log = Vec::new();
            for t in 0..6 {
                log.push(format!("-- txn {t}"));
                let mut txn = env.begin_rw_txn().unwrap();
                let mut m2 = model.clone();
                step(&mut txn, &mut rng, &mut m2, 0, &mut log);
                if rng.below(4) == 0 {
                    log.push("abort".into());
                    txn.abort();
                } else {
                    log.push("commit".into());
                    txn.commit().unwrap();
                    model = m2;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| audit(&env)));
                assert!(
                    r.is_ok(),
                    "seed {seed}: audit failed after:\n{}",
                    log.join("\n")
                );
            }
        }
    }
}
