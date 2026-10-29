//! `check-freelist`: audit the page usage of a snapshot (a port of
//! `c/cpeer_pagewalk.c`).
//!
//! Every page `0..=last_pgno` of a committed snapshot is used exactly once
//! (see the comment above `MDB_page` in mdb.c, and `mdb_audit()`):
//!
//! * pages 0 and 1 are the meta pages;
//! * a page of a B-tree: branch, leaf or overflow page of the free DB, the
//!   main DB, a named DB, or a DUPSORT sub-DB;
//! * or it is listed in a free DB record (`txnid -> [count, pgno...]`).
//!
//! Anything else is a leak (used by nothing) or a double use, and a C writer
//! meeting such a file would sooner or later corrupt data.
//!
//! The walk reads the data file with `pread` (`FileExt::read_at`) and
//! decodes pages with `lmdb_core::format` only, independently of the library's
//! cursor and transaction code. The snapshot is pinned with a read
//! transaction, which keeps writers from reusing its pages while we read
//! them. Every B-tree descriptor (`MDB_db`) is checked against what the walk
//! finds: depth, page counts, entries. A DUPSORT DB's branch/leaf counts
//! include its sub-DBs' pages (ITS#9388).
//!
//! Output, error messages and their order are those of the C peer, so the
//! harness can compare the two reports line by line.

use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use lmdb_core::consts::{MDB_DUPFIXED, P_INVALID};
use lmdb_core::format::layout::{
    DB_SIZE, META_OFFSET_IN_PAGE, META_SIZE, NODE_SIZE, PAGE_HDR_SIZE,
};
use lmdb_core::format::node::{F_BIGDATA, F_DUPDATA, F_SUBDATA};
use lmdb_core::format::page::{P_DIRTY, P_LEAF, P_LEAF2, P_META, P_OVERFLOW, P_SUBP};
use lmdb_core::format::{Db, Meta, NodeRef, PageRef};
use lmdb_core::Transaction;

use crate::args::{Check, CmdResult, Fail, Opts, EXIT_LMDB, EXIT_MISMATCH};

/// At most this many problems are printed.
const MAX_ERRORS: u64 = 20;
/// At most this many named DBs are walked (`MAX_DBS` in C).
const MAX_DBS: usize = 1024;
/// A `last_pgno` beyond this is treated as corrupt instead of allocating a
/// page table for it (C would just fail to `calloc`).
const MAX_PAGES: u64 = 1 << 36;

/// What a page is used by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Not (yet) seen.
    None,
    /// Pages 0 and 1.
    Meta,
    /// Listed in a free DB record.
    FreeListed,
    /// A page of the free DB's own tree.
    FreeDb,
    /// A page of the tree of DB `k` (`k` indexes [`Walker::names`]; 0 is the
    /// main DB). DUPSORT sub-DB pages count for their DB.
    Db(usize),
}

/// The kind of tree being walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TreeKind {
    Free,
    Main,
    Named,
    DupSub,
}

/// Totals found by walking one tree.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    branch: u64,
    leaf: u64,
    overflow: u64,
    entries: u64,
    /// Pages of DUPSORT sub-DBs.
    sub_branch: u64,
    sub_leaf: u64,
    /// DUPSORT keys whose values are in a sub-DB / a sub-page.
    subdbs: u64,
    subpages: u64,
}

/// The audit state of one snapshot (`struct walker`).
struct Walker {
    file: File,
    psize: usize,
    last_pg: u64,
    /// Per page: what uses it.
    used: Vec<Use>,
    /// DB `k`'s name, for messages and output (raw bytes, as C prints them).
    names: Vec<Vec<u8>>,
    errors: u64,
    free_listed: u64,
}

/// One tree being walked (`struct tree`).
struct Tree {
    kind: TreeKind,
    used: Use,
    /// Expected leaf level (`md_depth`).
    depth: u32,
    /// The DB has `MDB_DUPFIXED`.
    dupfixed: bool,
    c: Counts,
    /// Named DBs found in the main DB, walked afterwards.
    subs: Vec<(Vec<u8>, Db)>,
}

/// A node of a branch or leaf page that fits in its page (`struct node`).
struct Node<'p> {
    raw: NodeRef<'p>,
    flags: u16,
    key: &'p [u8],
    dsize: usize,
}

/// `read_node`: node `i` of `page`, or `None` if it does not fit.
fn read_node(page: &[u8], i: usize) -> Option<Node<'_>> {
    let off = PageRef::new(page)?.node_offset(i)?;
    if off < PAGE_HDR_SIZE || off + NODE_SIZE > page.len() {
        return None;
    }
    let raw = NodeRef::new(&page[off..])?;
    Some(Node {
        raw,
        flags: raw.flags(),
        key: raw.key()?,
        dsize: raw.data_size(),
    })
}

fn rd64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.get(..8)?.try_into().ok()?))
}

/// The C name of a DB: its bytes up to the first NUL.
fn c_name(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or_default()
}

impl Walker {
    /// `fail`: report a problem (the first [`MAX_ERRORS`] are printed).
    fn fail(&mut self, msg: impl std::fmt::Display) {
        let n = self.errors;
        self.errors += 1;
        if n < MAX_ERRORS {
            eprintln!("rpeer: check-freelist: {msg}");
        }
    }

    fn name(&self, k: usize) -> String {
        String::from_utf8_lossy(&self.names[k]).into_owned()
    }

    /// `use_name`: for messages.
    fn use_name(&self, used: Use) -> String {
        match used {
            Use::None => "nothing".to_owned(),
            Use::Meta => "meta".to_owned(),
            Use::FreeListed => "free list".to_owned(),
            Use::FreeDb => "free DB tree".to_owned(),
            Use::Db(k) => format!("tree of DB {}", self.name(k)),
        }
    }

    /// The DB name of a tree, for messages.
    fn tree_name(&self, t: &Tree) -> String {
        match t.used {
            Use::Db(k) => self.name(k),
            other => self.use_name(other),
        }
    }

    /// `mark`: pages `pgno..pgno+n` are used by `used`.
    fn mark(&mut self, pgno: u64, n: u64, used: Use) {
        for p in pgno..pgno.saturating_add(n) {
            if p > self.last_pg {
                self.fail(format!(
                    "page {p} ({}) is beyond last_pgno {}",
                    self.use_name(used),
                    self.last_pg
                ));
                return;
            }
            let prev = self.used[p as usize];
            if prev != Use::None {
                self.fail(format!(
                    "page {p} used twice: {} and {}",
                    self.use_name(prev),
                    self.use_name(used)
                ));
                continue;
            }
            self.used[p as usize] = used;
        }
    }

    /// `load`: read `n` pages starting at `pgno`; `None` (after reporting)
    /// on error.
    fn load(&mut self, pgno: u64, n: u64) -> Option<Vec<u8>> {
        let end = pgno.checked_add(n).and_then(|e| e.checked_sub(1));
        if pgno < 2 || end.is_none_or(|e| e > self.last_pg) {
            self.fail(format!(
                "page reference {pgno} (+{n}) outside 2..{}",
                self.last_pg
            ));
            return None;
        }
        let len = n as usize * self.psize;
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            match self
                .file
                .read_at(&mut buf[got..], pgno * self.psize as u64 + got as u64)
            {
                Ok(0) | Err(_) => break,
                Ok(k) => got += k,
            }
        }
        if got != len {
            self.fail(format!("short read of page {pgno}: {got} of {len} bytes"));
            return None;
        }
        Some(buf)
    }

    /// `node_data`: the data of a leaf node, following `F_BIGDATA` to its
    /// overflow pages (which are marked as used by the tree). Returns the
    /// data, or `None` on error, and the number of overflow pages.
    fn node_data(&mut self, t: &Tree, n: &Node<'_>) -> (Option<Vec<u8>>, u64) {
        if n.flags & F_BIGDATA == 0 {
            return (n.raw.inline_data().map(<[u8]>::to_vec), 0);
        }
        let Some(pg) = n.raw.overflow_pgno() else {
            self.fail("overflow node without a page number");
            return (None, 0);
        };
        let Some(first) = self.load(pg, 1) else {
            return (None, 0);
        };
        let ov = PageRef::new(&first).expect("a whole page");
        let npages = u64::from(ov.overflow_pages());
        if ov.pgno() != pg || ov.flags() & P_OVERFLOW == 0 {
            self.fail(format!(
                "page {pg} should be an overflow page (flags {:#x})",
                ov.flags()
            ));
            return (None, 0);
        }
        if npages == 0 || (n.dsize + PAGE_HDR_SIZE) as u64 > npages * self.psize as u64 {
            self.fail(format!(
                "overflow page {pg}: {npages} pages for {} bytes",
                n.dsize
            ));
            return (None, 0);
        }
        self.mark(pg, npages, t.used);
        let Some(run) = self.load(pg, npages) else {
            return (None, npages);
        };
        (
            Some(run[PAGE_HDR_SIZE..PAGE_HDR_SIZE + n.dsize].to_vec()),
            npages,
        )
    }

    /// `free_record`: a free DB record, key = txnid, data = `[count,
    /// pgno...]` sorted descending; the pages are marked as free.
    fn free_record(&mut self, n: &Node<'_>, data: &[u8]) {
        if n.key.len() != 8 {
            self.fail(format!("free DB key of {} bytes", n.key.len()));
            return;
        }
        let txnid = rd64(n.key).expect("8 bytes");
        let count = rd64(data).unwrap_or(0);
        let fits = count
            .checked_add(1)
            .and_then(|c| c.checked_mul(8))
            .is_some_and(|need| need <= n.dsize as u64);
        if n.dsize < 8 || !fits {
            self.fail(format!(
                "free DB record {txnid}: {} bytes do not hold its page list",
                n.dsize
            ));
            return;
        }
        let mut prev = P_INVALID;
        for k in 0..count as usize {
            let pg = rd64(&data[8 * (k + 1)..]).expect("checked size");
            if pg >= prev {
                self.fail(format!(
                    "free DB record {txnid} is not sorted descending at {pg}"
                ));
            }
            prev = pg;
            self.mark(pg, 1, Use::FreeListed);
        }
        self.free_listed += count;
    }

    /// `leaf_node`: a leaf node of a main, named or sub-DB.
    fn leaf_node(&mut self, t: &mut Tree, n: &Node<'_>) {
        let f = n.flags & (F_BIGDATA | F_SUBDATA | F_DUPDATA);
        if f == F_BIGDATA {
            let (_, ovpages) = self.node_data(t, n);
            t.c.overflow += ovpages;
            t.c.entries += 1;
        } else if f == F_SUBDATA | F_DUPDATA {
            // DUPSORT values in a sub-DB of their own.
            let sub = n
                .raw
                .inline_data()
                .filter(|_| n.dsize == DB_SIZE)
                .and_then(Db::read);
            let Some(sub) = sub else {
                self.fail(format!("sub-DB record of {} bytes", n.dsize));
                return;
            };
            let label = match t.used {
                Use::Db(k) => self.names[k].clone(),
                _ => Vec::new(),
            };
            let sc = self.walk_db(&label, &sub, TreeKind::DupSub, t.used);
            t.c.sub_branch += sc.branch;
            t.c.sub_leaf += sc.leaf;
            t.c.entries += sc.entries;
            t.c.subdbs += 1;
        } else if f == F_DUPDATA {
            // DUPSORT values in a sub-page inside the node.
            let want = P_LEAF | P_SUBP | if t.dupfixed { P_LEAF2 } else { 0 };
            let sp = n
                .raw
                .inline_data()
                .filter(|d| d.len() >= PAGE_HDR_SIZE)
                .and_then(PageRef::new);
            let ok = sp.is_some_and(|sp| {
                sp.flags() & !P_DIRTY == want
                    && usize::from(sp.lower()) >= PAGE_HDR_SIZE
                    && usize::from(sp.lower()) <= n.dsize
            });
            match sp {
                Some(sp) if ok => {
                    t.c.entries += sp.num_keys() as u64;
                    t.c.subpages += 1;
                }
                _ => {
                    let flags = sp.map_or(0, |sp| sp.flags());
                    let name = self.tree_name(t);
                    self.fail(format!(
                        "bad sub-page in DB {name} (flags {flags:#x}, {} bytes)",
                        n.dsize
                    ));
                }
            }
        } else if f == F_SUBDATA {
            // A named DB, only valid in the main DB.
            let db = n
                .raw
                .inline_data()
                .filter(|_| n.dsize == DB_SIZE)
                .and_then(Db::read);
            match db {
                Some(db) if t.kind == TreeKind::Main => {
                    t.subs.push((c_name(n.key).to_vec(), db));
                    t.c.entries += 1;
                }
                _ => {
                    let name = self.tree_name(t);
                    self.fail(format!("unexpected named-DB record in DB {name}"));
                }
            }
        } else if f == 0 {
            if t.kind == TreeKind::DupSub && n.dsize != 0 {
                self.fail(format!("sub-DB node with {} data bytes", n.dsize));
            }
            t.c.entries += 1;
        } else {
            let name = self.tree_name(t);
            self.fail(format!("bad node flags {:#x} in DB {name}", n.flags));
        }
    }

    /// `walk_page`: page `pgno` at `level` (the root is level 1) and,
    /// recursively, its children.
    fn walk_page(&mut self, t: &mut Tree, pgno: u64, level: u32) {
        let Some(buf) = self.load(pgno, 1) else {
            return;
        };
        let page = PageRef::new(&buf).expect("a whole page");
        let flags = page.flags();
        if page.pgno() != pgno {
            self.fail(format!("page {pgno} says it is page {}", page.pgno()));
            return;
        }
        let (lower, upper) = (usize::from(page.lower()), usize::from(page.upper()));
        if lower < PAGE_HDR_SIZE || lower > upper || upper > self.psize {
            self.fail(format!("page {pgno}: bad lower/upper {lower}/{upper}"));
            return;
        }
        self.mark(pgno, 1, t.used);
        let nkeys = page.num_keys();

        if page.is_branch() {
            t.c.branch += 1;
            if level >= t.depth {
                self.fail(format!(
                    "branch page {pgno} at level {level} of a tree of depth {}",
                    t.depth
                ));
            }
            for i in 0..nkeys {
                let Some(n) = read_node(&buf, i) else {
                    self.fail(format!("branch page {pgno}: node {i} out of bounds"));
                    break;
                };
                self.walk_page(t, n.raw.child_pgno(), level + 1);
            }
        } else if page.is_leaf() {
            t.c.leaf += 1;
            if level != t.depth {
                self.fail(format!(
                    "leaf page {pgno} at level {level} of a tree of depth {}",
                    t.depth
                ));
            }
            if page.is_leaf2() {
                // DUPFIXED sub-DB: packed values, their size in mp_pad.
                if t.kind != TreeKind::DupSub || page.pad() == 0 {
                    self.fail(format!("unexpected LEAF2 page {pgno}"));
                }
                t.c.entries += nkeys as u64;
                return;
            }
            for i in 0..nkeys {
                let n = read_node(&buf, i).filter(|n| {
                    if n.flags & F_BIGDATA != 0 {
                        n.raw.overflow_pgno().is_some()
                    } else {
                        n.raw.inline_data().is_some()
                    }
                });
                let Some(n) = n else {
                    self.fail(format!("leaf page {pgno}: node {i} out of bounds"));
                    break;
                };
                if t.kind == TreeKind::Free {
                    let (data, ovpages) = self.node_data(t, &n);
                    t.c.overflow += ovpages;
                    t.c.entries += 1;
                    if let Some(data) = data {
                        self.free_record(&n, &data);
                    }
                } else {
                    self.leaf_node(t, &n);
                }
            }
        } else {
            self.fail(format!("page {pgno} has unexpected flags {flags:#x}"));
        }
    }

    /// `walk_db`: walk a whole tree, compare the totals with its `MDB_db`,
    /// print its line (except for sub-DBs), then walk the named DBs it
    /// records. `label` is the DB name for messages and output.
    fn walk_db(&mut self, label: &[u8], db: &Db, kind: TreeKind, used: Use) -> Counts {
        let shown = String::from_utf8_lossy(label).into_owned();
        let mut t = Tree {
            kind,
            used,
            depth: u32::from(db.depth),
            dupfixed: u32::from(db.flags) & MDB_DUPFIXED != 0,
            c: Counts::default(),
            subs: Vec::new(),
        };
        if db.root == P_INVALID {
            if db.depth != 0
                || db.branch_pages != 0
                || db.leaf_pages != 0
                || db.overflow_pages != 0
                || db.entries != 0
            {
                self.fail(format!(
                    "DB {shown} is empty but its descriptor has depth {}, entries {}",
                    db.depth, db.entries
                ));
            }
        } else {
            self.walk_page(&mut t, db.root, 1);
        }

        // Since ITS#9388 a DUPSORT DB's branch/leaf counts include the pages
        // of its sub-DBs; overflow pages never occur in sub-DBs.
        let want_branch = t.c.branch + t.c.sub_branch;
        let want_leaf = t.c.leaf + t.c.sub_leaf;
        if db.branch_pages != want_branch
            || db.leaf_pages != want_leaf
            || db.overflow_pages != t.c.overflow
            || db.entries != t.c.entries
        {
            self.fail(format!(
                "DB {shown}{}: descriptor says branch={} leaf={} overflow={} entries={}, \
                 walk found branch={want_branch} leaf={want_leaf} overflow={} entries={}",
                if kind == TreeKind::DupSub {
                    " (a sub-DB)"
                } else {
                    ""
                },
                db.branch_pages,
                db.leaf_pages,
                db.overflow_pages,
                db.entries,
                t.c.overflow,
                t.c.entries
            ));
        }
        if kind != TreeKind::DupSub {
            let mut out = io::stdout().lock();
            let _ = out.write_all(b"db=");
            let _ = out.write_all(label);
            let _ = writeln!(
                out,
                " flags={:#x} depth={} branch_pages={want_branch} leaf_pages={want_leaf} \
                 overflow_pages={} entries={} subdbs={} subpages={}",
                db.flags, db.depth, t.c.overflow, t.c.entries, t.c.subdbs, t.c.subpages
            );
        }

        // Named DBs recorded in the main DB are trees of their own.
        for (name, sub) in std::mem::take(&mut t.subs) {
            if self.names.len() == MAX_DBS {
                self.fail("too many named DBs");
                break;
            }
            self.names.push(name.clone());
            let k = self.names.len() - 1;
            self.walk_db(&name, &sub, TreeKind::Named, Use::Db(k));
        }
        t.c
    }
}

/// `pick_meta`: read both meta pages and return the one of snapshot
/// `txnid`. `psize` is updated to the page size recorded in meta page 0,
/// which also locates meta page 1.
fn pick_meta(file: &File, txnid: u64, psize: &mut usize) -> CmdResult<Option<Meta>> {
    for k in 0..2u64 {
        let mut buf = [0u8; META_OFFSET_IN_PAGE + META_SIZE];
        let off = if k == 0 { 0 } else { *psize as u64 };
        file.read_exact_at(&mut buf, off).map_err(|_| {
            Fail::new(
                EXIT_LMDB,
                format!("check-freelist: cannot read meta page {k}"),
            )
        })?;
        let page = PageRef::new(&buf).expect("header");
        let m = Meta::read(&buf[META_OFFSET_IN_PAGE..]).expect("meta size");
        if m.magic != lmdb_core::consts::MDB_MAGIC || m.version != 1 {
            return Err(Fail::mismatch(format!(
                "check-freelist: meta page {k}: magic {:#x} version {}",
                m.magic, m.version
            )));
        }
        if page.pgno() != k || page.flags() & P_META == 0 {
            return Err(Fail::mismatch(format!(
                "check-freelist: page {k} is not a meta page"
            )));
        }
        if k == 0 {
            *psize = m.page_size() as usize;
        }
        if m.txnid == txnid {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// `check-freelist <env>`: see the module documentation.
pub fn cmd_check_freelist(o: &mut Opts) -> CmdResult {
    let env = o.open_env(true)?;
    let fname = if o.nosubdir {
        env.path().to_path_buf()
    } else {
        PathBuf::from(env.path()).join("data.mdb")
    };
    let file = File::open(&fname)
        .map_err(|e| Fail::new(EXIT_LMDB, format!("open {}: {e}", fname.display())))?;
    let env_psize = env.stat().check("mdb_env_stat")?.page_size as usize;
    let mut psize = env_psize;

    // Pin a snapshot, then find its meta page. A concurrent writer may
    // already have overwritten it (two commits later); then try again.
    let mut tries = 0;
    let (txn, meta) = loop {
        let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
        if let Some(m) = pick_meta(&file, txn.id(), &mut psize)? {
            break (txn, m);
        }
        let txnid = txn.id();
        drop(txn);
        if tries == 100 {
            return Err(Fail::mismatch(format!(
                "check-freelist: no meta page has txnid {txnid}"
            )));
        }
        tries += 1;
    };
    if meta.last_pgno >= MAX_PAGES {
        return Err(Fail::mismatch(format!(
            "check-freelist: implausible last_pgno {}",
            meta.last_pgno
        )));
    }
    let mut w = Walker {
        file,
        psize,
        last_pg: meta.last_pgno,
        used: vec![Use::None; meta.last_pgno as usize + 1],
        names: vec![b"-".to_vec()],
        errors: 0,
        free_listed: 0,
    };
    if w.psize != env_psize {
        w.fail(format!(
            "meta psize {}, mdb_env_stat says {env_psize}",
            w.psize
        ));
    }
    w.used[0] = Use::Meta;
    if let Some(u) = w.used.get_mut(1) {
        *u = Use::Meta;
    }
    println!(
        "psize={}\ntxnid={}\nlast_pgno={}",
        w.psize,
        txn.id(),
        meta.last_pgno
    );

    let fc = w.walk_db(b"@free", &meta.dbs[0], TreeKind::Free, Use::FreeDb);
    w.walk_db(b"-", &meta.dbs[1], TreeKind::Main, Use::Db(0));
    drop(txn);

    let (mut leaks, mut tree) = (0u64, 0u64);
    for p in 0..=meta.last_pgno {
        match w.used[p as usize] {
            Use::None => {
                if leaks < MAX_ERRORS {
                    w.fail(format!("page {p} is leaked (neither used nor free)"));
                }
                leaks += 1;
            }
            Use::Db(_) => tree += 1,
            _ => {}
        }
    }
    println!(
        "pages_meta=2\npages_freedb={}\npages_free={}\npages_tree={tree}\npages_leaked={leaks}\npages_total={}",
        fc.branch + fc.leaf + fc.overflow,
        w.free_listed,
        meta.last_pgno + 1
    );
    if w.errors > 0 {
        println!("errors={}", w.errors);
        return Err(Fail::quiet(EXIT_MISMATCH));
    }
    println!("ok");
    Ok(())
}
