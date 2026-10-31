//! Test support: a page audit of the newest snapshot, in the spirit of the
//! C peer's `check-freelist` (interop/c/cpeer_pagewalk.c), for unit
//! tests that cannot reach the C tools.
//!
//! Every page from 2 to `last_pgno` must be used exactly once: by a tree
//! (branch, leaf and overflow pages of the free DB, the main DB, the named
//! DBs and their `DUP_SORT` sub-DBs) or listed in a free DB record; and
//! each DB record's depth and page counts must match its tree.
//!
//! [`page_use`] reports who uses which page in any snapshot, for tests that
//! check which pages a writer may reuse while a reader pins one.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::consts::*;
use crate::core::dbi::Dbi;
use crate::core::env::Environment;
use crate::core::txn::{RoTxn, Transaction, TxnCore};
use crate::format::Db;
use crate::idl::Idl;

/// Who uses a page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Use {
    Tree(String),
    Free(u64),
}

struct Audit<'a, 't> {
    core: &'a TxnCore<'t>,
    used: HashMap<u64, Use>,
}

#[derive(Default)]
struct Counts {
    depth: u16,
    branch: u64,
    leaf: u64,
    overflow: u64,
    entries: u64,
}

impl Audit<'_, '_> {
    fn claim(&mut self, pgno: u64, by: Use) {
        if let Some(prev) = self.used.insert(pgno, by.clone()) {
            panic!("page {pgno} used by {prev:?} and {by:?}");
        }
    }

    /// Walks the tree of `db`, checks its record and returns nothing.
    fn tree(&mut self, name: &str, db: &Db, dupsort_sub: bool) {
        if db.root == P_INVALID {
            assert_eq!(db.depth, 0, "{name}: empty tree with depth");
            assert_eq!(db.entries, 0, "{name}: empty tree with entries");
            return;
        }
        let mut c = Counts::default();
        self.walk(name, db.root, 1, &mut c, dupsort_sub);
        assert_eq!(db.depth, c.depth, "{name}: depth");
        assert_eq!(db.branch_pages, c.branch, "{name}: branch pages");
        assert_eq!(db.leaf_pages, c.leaf, "{name}: leaf pages");
        assert_eq!(db.overflow_pages, c.overflow, "{name}: overflow pages");
        assert_eq!(db.entries, c.entries, "{name}: entries");
    }

    fn walk(&mut self, name: &str, pgno: u64, level: u16, c: &mut Counts, sub: bool) {
        self.claim(pgno, Use::Tree(name.to_owned()));
        let page = self.core.page(pgno).expect("page");
        c.depth = c.depth.max(level);
        if page.is_branch() {
            c.branch += 1;
            for i in 0..page.num_keys() {
                let child = page.node(i).unwrap().child_pgno();
                self.walk(name, child, level + 1, c, sub);
            }
            return;
        }
        assert!(
            page.is_leaf(),
            "{name}: page {pgno} is neither branch nor leaf"
        );
        c.leaf += 1;
        if page.is_leaf2() || sub {
            c.entries += page.num_keys() as u64;
            return;
        }
        for i in 0..page.num_keys() {
            let node = page.node(i).unwrap();
            if node.is_bigdata() {
                let ov = node.overflow_pgno().unwrap();
                let n = self.core.page(ov).unwrap().overflow_pages() as u64;
                for p in ov..ov + n {
                    self.claim(p, Use::Tree(name.to_owned()));
                }
                c.overflow += n;
                c.entries += 1;
            } else if node.is_dupdata() {
                let data = node.inline_data().unwrap();
                if node.is_subdata() {
                    // A sub-DB's pages count towards the main record's (ITS#9388).
                    let sdb = Db::read(data).unwrap();
                    let mut sc = Counts::default();
                    self.walk(name, sdb.root, 1, &mut sc, true);
                    assert_eq!(sdb.entries, sc.entries, "{name}: sub-DB entries");
                    c.branch += sc.branch;
                    c.leaf += sc.leaf;
                    c.entries += sc.entries;
                } else {
                    let sp = crate::format::PageRef::with_layout(data, node.layout()).unwrap();
                    c.entries += sp.num_keys() as u64;
                }
            } else if node.is_subdata() && name == "main" {
                let sdb = Db::read(node.inline_data().unwrap()).unwrap();
                let sub_name = String::from_utf8_lossy(node.key().unwrap()).into_owned();
                self.tree(&sub_name, &sdb, false);
                c.entries += 1;
            } else {
                c.entries += 1;
            }
        }
    }
}

/// Who uses the pages of a snapshot (see [`page_use`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageUse {
    /// The pages of every tree: the free DB, the main DB, the named DBs and
    /// their `DUP_SORT` sub-DBs, overflow runs.
    pub(crate) tree: BTreeSet<u64>,
    /// The pages listed in free DB records, with the record's txnid.
    pub(crate) free: BTreeMap<u64, u64>,
    /// `last_pgno` of the snapshot.
    pub(crate) last_pgno: u64,
}

/// Who uses which page in the snapshot of `txn`. Panics, like [`audit`],
/// on a page used twice or a DB record that does not match its tree.
pub(crate) fn page_use(txn: &RoTxn<'_>) -> PageUse {
    let core = txn.core();
    let mut a = Audit {
        core,
        used: HashMap::new(),
    };
    let (free, main) = {
        let dbs = core.dbs.borrow();
        (dbs[FREE_DBI as usize].db, dbs[MAIN_DBI as usize].db)
    };
    a.tree("free", &free, false);
    a.tree("main", &main, false);
    let mut cur = txn.cursor(Dbi::FREE).unwrap();
    while let Some((k, v)) = cur.next().unwrap() {
        let id = u64::from_ne_bytes(k.try_into().unwrap());
        let idl = Idl::from_bytes(v).unwrap();
        for &p in idl.as_slice() {
            a.claim(p, Use::Free(id));
        }
    }
    let mut uses = PageUse {
        tree: BTreeSet::new(),
        free: BTreeMap::new(),
        last_pgno: core.next_pgno - 1,
    };
    for (p, by) in a.used {
        match by {
            Use::Tree(_) => uses.tree.insert(p),
            Use::Free(id) => uses.free.insert(p, id).is_none(),
        };
    }
    uses
}

/// Audits the newest snapshot of `env` (see the module documentation).
pub(crate) fn audit(env: &Environment) {
    let txn = env.begin_ro_txn().unwrap();
    let uses = page_use(&txn);
    let last = uses.last_pgno;
    for p in NUM_METAS as u64..=last {
        assert!(
            uses.tree.contains(&p) || uses.free.contains_key(&p),
            "page {p} of {last} is unaccounted for"
        );
    }
    assert!(
        uses.tree.iter().chain(uses.free.keys()).all(|&p| p <= last),
        "page past last_pgno in use"
    );
}
