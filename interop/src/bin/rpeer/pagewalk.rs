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
//! decodes pages with `rlmdb::format` only, independently of the library's
//! cursor and transaction code. The snapshot is pinned with a read
//! transaction, which keeps writers from reusing its pages while we read
//! them. Every B-tree descriptor (`MDB_db`) is checked against what the walk
//! finds: depth, page counts, entries. A DUPSORT DB's branch/leaf counts
//! include its sub-DBs' pages (ITS#9388).
//!
//! The data-file format (LMDB 0.9 or 1.0) is taken from the meta pages
//! ([`sniff_layout`]) and every page is decoded with its [`Layout`]. A 1.0
//! snapshot is also checked for what only 1.0 records: every page reachable
//! from it carries the txnid of the transaction that wrote it (`mp_txnid` in
//! `1..=snapshot`; not meta pages, which carry 0, nor sub-pages, which C's
//! compacting copy leaves as they were while it stamps every page 1), and
//! the `MDB_ovpage` of an `F_BIGDATA` node agrees with its overflow run
//! (`op_pages == mp_pages`, `op_txnid` in `1..=mp_txnid`: an in-place
//! overwrite restamps the page but not the reference). The C peer decodes
//! the format of the library it is compiled against (`cpeer`: 0.9,
//! `cpeer10`: 1.0).
//!
//! Output, error messages and their order are those of the C peer, so the
//! harness can compare the two reports line by line.

use std::fmt::Display;
use std::fs::File;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use rlmdb::consts::{MDB_DUPFIXED, MDB_MAGIC, P_INVALID};
use rlmdb::format::layout::{Format, Layout, DB_SIZE, META_SIZE, NODE_SIZE, V09, V10};
use rlmdb::format::node::{F_BIGDATA, F_DUPDATA, F_SUBDATA};
use rlmdb::format::page::{P_LEAF, P_LEAF2, P_META, P_OVERFLOW, P_SUBP};
use rlmdb::format::{Db, Meta, NodeRef, OverflowRef, PageRef};
use rlmdb::Transaction;

use crate::args::{Check, CmdResult, Fail, Opts, EXIT_LMDB, EXIT_MISMATCH};

/// At most this many problems are printed.
const MAX_ERRORS: u64 = 20;
/// At most this many named DBs are walked (`MAX_DBS` in C).
const MAX_DBS: usize = 1024;
/// A `last_pgno` beyond this is treated as corrupt instead of allocating a
/// page table for it (C would just fail to `calloc`).
const MAX_PAGES: u64 = 1 << 36;
/// The longer page header of the two formats (`MAX_PAGEHDRSZ` in C): how
/// much of a meta page to read before its `MDB_meta`.
const MAX_PAGE_HDR: usize = 24;

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

/// The snapshot to audit: its meta page and how to decode the file.
struct Snapshot {
    /// The data-file format.
    layout: &'static Layout,
    /// The page size recorded in meta page 0.
    psize: usize,
    /// The snapshot's txnid (`mm_txnid` of `meta`).
    txnid: u64,
    /// The meta page of the snapshot.
    meta: Meta,
}

/// The audit state of one snapshot (`struct walker`).
struct Walker<'o> {
    file: File,
    layout: &'static Layout,
    psize: usize,
    /// The snapshot (1.0: the largest valid `mp_txnid`).
    txnid: u64,
    last_pg: u64,
    /// Per page: what uses it.
    used: Vec<Use>,
    /// DB `k`'s name, for messages and output (raw bytes, as C prints them).
    names: Vec<Vec<u8>>,
    errors: u64,
    free_listed: u64,
    /// The report (stdout in `rpeer`).
    out: &'o mut dyn Write,
    /// The problems (stderr in `rpeer`).
    err: &'o mut dyn Write,
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

/// `read_node`: node `i` of `page` (of format `l`), or `None` if it does
/// not fit.
fn read_node<'p>(l: &'static Layout, page: &'p [u8], i: usize) -> Option<Node<'p>> {
    let off = PageRef::with_layout(page, l)?.node_offset(i)?;
    if off < l.page_hdr || off + NODE_SIZE > page.len() {
        return None;
    }
    let raw = NodeRef::with_layout(&page[off..], l)?;
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

/// The C name of a DB: its bytes up to the first NUL (C 1.0 stores the
/// name with its terminating NUL as the key in the main DB, 0.9 without).
fn c_name(bytes: &[u8]) -> &[u8] {
    bytes.split(|&b| b == 0).next().unwrap_or_default()
}

/// The release that writes a format, for messages (`layout.name` in C).
fn format_name(l: &Layout) -> &'static str {
    match l.format {
        Format::V09 => "LMDB 0.9",
        Format::V10 => "LMDB 1.0",
    }
}

/// The `MDB_meta` of a meta page of format `l` (`read_meta`), from the
/// first bytes of the page.
fn meta_of(page: &[u8], l: &Layout) -> Option<Meta> {
    Meta::read(page.get(l.meta_offset()..)?)
}

/// Does meta page `page` hold an `MDB_meta` of format `l` (magic and
/// `mm_version` after `l`'s page header)?
fn is_meta_of(page: &[u8], l: &Layout) -> bool {
    meta_of(page, l).is_some_and(|m| m.magic == MDB_MAGIC && m.version == l.data_version)
}

/// The format of a data file, from the first bytes of a meta page: LMDB 0.9
/// keeps the `MDB_meta` after a 16-byte page header with `mm_version` 1,
/// LMDB 1.0 after a 24-byte one with `mm_version` 3. `None` if neither.
/// (The two cannot be confused: where 1.0 has the magic, 0.9 has
/// `mm_address`; where 0.9 has it, 1.0 has `mp_pad` and `mp_flags`.)
fn sniff_layout(page: &[u8]) -> Option<&'static Layout> {
    [&V09, &V10].into_iter().find(|l| is_meta_of(page, l))
}

/// The check of `mp_lower`/`mp_upper` (as stored, `PAGEBASE`-relative) of a
/// branch or leaf page of `psize` bytes: pointers start after the header,
/// end before the nodes, which end within the page.
fn lower_upper_ok(l: &Layout, lower: usize, upper: usize, psize: usize) -> bool {
    lower >= l.empty_lower() && lower <= upper && upper + l.ptr_base <= psize
}

/// The check of the header of a `P_LEAF2` page or sub-page of `size`
/// bytes: packed items of `mp_pad` bytes, no pointers, no nodes.
/// `mp_lower` and `mp_upper` only count (`mdb_node_add`: `mp_lower +=
/// sizeof(indx_t)`, `mp_upper -= ksize - sizeof(indx_t)`, unsigned and
/// stored modulo 2^16), so `mp_upper` lies past the end of a page of 1-byte
/// items. What holds: the items fit, and `SIZELEFT` is exactly the space
/// after them.
fn leaf2_ok(l: &Layout, p: &PageRef<'_>, size: usize) -> bool {
    if usize::from(p.lower()) < l.empty_lower() {
        return false;
    }
    let used = l.page_hdr + p.num_keys() * usize::from(p.pad());
    used <= size && usize::from(p.upper().wrapping_sub(p.lower())) == size - used
}

/// The check of a `P_SUBP` sub-page header in a node with `dsize` data
/// bytes: flags `P_LEAF|P_SUBP` (`|P_LEAF2` in a `MDB_DUPFIXED` DB), which
/// 0.9 may combine with `P_DIRTY` (C 0.9 writes 0x52, 1.0 0x42), and its
/// node pointers within the node (its items, [`leaf2_ok`]).
fn subpage_ok(l: &Layout, sp: &PageRef<'_>, dsize: usize, dupfixed: bool) -> bool {
    let want = P_LEAF | P_SUBP | if dupfixed { P_LEAF2 } else { 0 };
    let lower = usize::from(sp.lower());
    sp.flags() & !l.subpage_dirty() == want
        && if dupfixed {
            leaf2_ok(l, sp, dsize)
        } else {
            lower >= l.empty_lower() && lower + l.ptr_base <= dsize
        }
}

impl Walker<'_> {
    /// `fail`: report a problem (the first [`MAX_ERRORS`] are printed).
    fn fail(&mut self, msg: impl Display) {
        let n = self.errors;
        self.errors += 1;
        if n < MAX_ERRORS {
            let _ = writeln!(self.err, "rpeer: check-freelist: {msg}");
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
            match read_at(
                &self.file,
                &mut buf[got..],
                pgno * self.psize as u64 + got as u64,
            ) {
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

    /// `check_txnid` (1.0): a page reachable from the snapshot was written
    /// by a transaction of it, so `1 <= mp_txnid <= snapshot txnid`
    /// (`mdb_page_flush` stamps the writer's txnid, the compacting copy 1).
    /// C 1.0 writers rely on it: a page with a too large `mp_txnid` looks
    /// like their own and is changed in place. 0.9 pages have no txnid.
    fn check_txnid(&mut self, pgno: u64, page: &PageRef<'_>) {
        let Some(txnid) = page.txnid() else {
            return;
        };
        if txnid == 0 || txnid > self.txnid {
            self.fail(format!(
                "page {pgno}: mp_txnid {txnid} is not in 1..{} (the snapshot)",
                self.txnid
            ));
        }
    }

    /// `check_ovpage` (1.0): the `MDB_ovpage` of an `F_BIGDATA` node
    /// against the first page `ov` of its run: the same length, and written
    /// by a transaction no later than the page (an in-place overwrite
    /// restamps the page, not the reference). 0.9 stores only the pgno.
    fn check_ovpage(&mut self, r: &OverflowRef, ov: &PageRef<'_>) {
        let Some(mp_txnid) = ov.txnid() else {
            return;
        };
        let mp_pages = ov.overflow_pages();
        if r.pages != u64::from(mp_pages) {
            self.fail(format!(
                "overflow page {}: op_pages {} in the node, mp_pages {mp_pages} in the page",
                r.pgno, r.pages
            ));
        }
        if r.txnid == 0 || r.txnid > mp_txnid {
            self.fail(format!(
                "overflow page {}: op_txnid {} is not in 1..{mp_txnid} (its mp_txnid)",
                r.pgno, r.txnid
            ));
        }
    }

    /// `node_data`: the data of a leaf node, following `F_BIGDATA` to its
    /// overflow pages (which are marked as used by the tree). Returns the
    /// data, or `None` on error, and the number of overflow pages.
    fn node_data(&mut self, t: &Tree, n: &Node<'_>) -> (Option<Vec<u8>>, u64) {
        if n.flags & F_BIGDATA == 0 {
            return (n.raw.inline_data().map(<[u8]>::to_vec), 0);
        }
        let Some(r) = n.raw.overflow_ref() else {
            self.fail("overflow node without a page number");
            return (None, 0);
        };
        let pg = r.pgno;
        let Some(first) = self.load(pg, 1) else {
            return (None, 0);
        };
        let l = self.layout;
        let ov = PageRef::with_layout(&first, l).expect("a whole page");
        let npages = u64::from(ov.overflow_pages());
        if ov.pgno() != pg || ov.flags() & P_OVERFLOW == 0 {
            self.fail(format!(
                "page {pg} should be an overflow page (flags {:#x})",
                ov.flags()
            ));
            return (None, 0);
        }
        self.check_txnid(pg, &ov);
        self.check_ovpage(&r, &ov);
        if npages == 0 || (n.dsize + l.page_hdr) as u64 > npages * self.psize as u64 {
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
        (Some(run[l.page_hdr..l.page_hdr + n.dsize].to_vec()), npages)
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
            let l = self.layout;
            let sp = n
                .raw
                .inline_data()
                .filter(|d| d.len() >= l.page_hdr)
                .and_then(|d| PageRef::with_layout(d, l));
            let ok = sp.is_some_and(|sp| subpage_ok(l, &sp, n.dsize, t.dupfixed));
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
        let l = self.layout;
        let page = PageRef::with_layout(&buf, l).expect("a whole page");
        let flags = page.flags();
        if page.pgno() != pgno {
            self.fail(format!("page {pgno} says it is page {}", page.pgno()));
            return;
        }
        self.check_txnid(pgno, &page);
        let (lower, upper) = (usize::from(page.lower()), usize::from(page.upper()));
        let header_ok = if page.is_leaf2() {
            leaf2_ok(l, &page, self.psize)
        } else {
            lower_upper_ok(l, lower, upper, self.psize)
        };
        if !header_ok {
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
                let Some(n) = read_node(l, &buf, i) else {
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
                // The data part must fit too: the overflow reference of an
                // F_BIGDATA node, else the inline data.
                let n = read_node(l, &buf, i).filter(|n| {
                    if n.flags & F_BIGDATA != 0 {
                        n.raw.overflow_ref().is_some()
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
            let _ = self.out.write_all(b"db=");
            let _ = self.out.write_all(label);
            let _ = writeln!(
                self.out,
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

/// `wrong_format`: meta page `k` (its first bytes in `page`) is not a meta
/// page of format `l`.
fn wrong_format(k: u64, page: &[u8], l: &Layout) -> Fail {
    let other = if l.format == Format::V09 { &V10 } else { &V09 };
    let m = meta_of(page, l).unwrap_or_default();
    if is_meta_of(page, other) {
        return Fail::mismatch(format!(
            "check-freelist: meta page {k} is in the {} format (data version {}), \
             but this program decodes {} files (data version {})",
            format_name(other),
            other.data_version,
            format_name(l),
            l.data_version
        ));
    }
    Fail::mismatch(format!(
        "check-freelist: meta page {k}: magic {:#x} version {} at offset {}, \
         not an {} meta page (magic {MDB_MAGIC:#x} version {})",
        m.magic,
        m.version,
        l.meta_offset(),
        format_name(l),
        l.data_version
    ))
}

/// `pick_meta`: read both meta pages and return the one of snapshot
/// `txnid`, and the format of the file, which meta page 0 tells
/// ([`sniff_layout`]); meta page 1 must be in the same format. `psize` is
/// updated to the page size recorded in meta page 0, which also locates
/// meta page 1.
fn pick_meta(
    file: &File,
    txnid: u64,
    psize: &mut usize,
) -> CmdResult<Option<(Meta, &'static Layout)>> {
    let mut layout = None;
    for k in 0..2u64 {
        let mut buf = [0u8; MAX_PAGE_HDR + META_SIZE];
        let off = if k == 0 { 0 } else { *psize as u64 };
        read_exact_at(file, &mut buf, off).map_err(|_| {
            Fail::new(
                EXIT_LMDB,
                format!("check-freelist: cannot read meta page {k}"),
            )
        })?;
        let l = match layout {
            Some(l) => l,
            None => sniff_layout(&buf).ok_or_else(|| {
                let (a, b) = (meta_of(&buf, &V09), meta_of(&buf, &V10));
                let (a, b) = (a.unwrap_or_default(), b.unwrap_or_default());
                Fail::mismatch(format!(
                    "check-freelist: meta page {k} is neither an LMDB 0.9 nor an LMDB 1.0 \
                     meta page (magic {:#x} version {} at offset {}, magic {:#x} version {} \
                     at offset {})",
                    a.magic,
                    a.version,
                    V09.meta_offset(),
                    b.magic,
                    b.version,
                    V10.meta_offset()
                ))
            })?,
        };
        layout = Some(l);
        if !is_meta_of(&buf, l) {
            return Err(wrong_format(k, &buf, l));
        }
        let page = PageRef::with_layout(&buf, l).expect("header");
        let m = meta_of(&buf, l).expect("meta size");
        if page.pgno() != k || page.flags() & P_META == 0 {
            return Err(Fail::mismatch(format!(
                "check-freelist: page {k} is not a meta page"
            )));
        }
        if k == 0 {
            *psize = m.page_size() as usize;
        }
        if m.txnid == txnid {
            return Ok(Some((m, l)));
        }
    }
    Ok(None)
}

/// The audit of snapshot `snap` of the data file `file`, given what
/// `mdb_env_stat` says the page size is (`env_psize`): the walk of every
/// tree, then the page accounting. Writes the report to `out` and the
/// problems to `err`; `Err` if there were any (after `errors=<n>`).
fn audit(
    file: File,
    snap: &Snapshot,
    env_psize: usize,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> CmdResult {
    let meta = &snap.meta;
    if meta.last_pgno >= MAX_PAGES {
        return Err(Fail::mismatch(format!(
            "check-freelist: implausible last_pgno {}",
            meta.last_pgno
        )));
    }
    let mut w = Walker {
        file,
        layout: snap.layout,
        psize: snap.psize,
        txnid: snap.txnid,
        last_pg: meta.last_pgno,
        used: vec![Use::None; meta.last_pgno as usize + 1],
        names: vec![b"-".to_vec()],
        errors: 0,
        free_listed: 0,
        out,
        err,
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
    writeln!(
        w.out,
        "psize={}\ntxnid={}\nlast_pgno={}",
        w.psize, snap.txnid, meta.last_pgno
    )?;

    let fc = w.walk_db(b"@free", &meta.dbs[0], TreeKind::Free, Use::FreeDb);
    w.walk_db(b"-", &meta.dbs[1], TreeKind::Main, Use::Db(0));

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
    writeln!(
        w.out,
        "pages_meta=2\npages_freedb={}\npages_free={}\npages_tree={tree}\npages_leaked={leaks}\npages_total={}",
        fc.branch + fc.leaf + fc.overflow,
        w.free_listed,
        meta.last_pgno + 1
    )?;
    if w.errors > 0 {
        writeln!(w.out, "errors={}", w.errors)?;
        return Err(Fail::quiet(EXIT_MISMATCH));
    }
    writeln!(w.out, "ok")?;
    Ok(())
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
    let (txn, meta, layout) = loop {
        let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
        if let Some((m, l)) = pick_meta(&file, txn.id(), &mut psize)? {
            break (txn, m, l);
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
    let snap = Snapshot {
        layout,
        psize,
        txnid: txn.id(),
        meta,
    };
    let result = audit(
        file,
        &snap,
        env_psize,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    drop(txn);
    result
}

/// `pread` (Unix `FileExt::read_at`, Windows `seek_read`).
fn read_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    #[cfg(unix)]
    return file.read_at(buf, off);
    #[cfg(windows)]
    return std::os::windows::fs::FileExt::seek_read(file, buf, off);
}

/// `pread` of all of `buf`.
fn read_exact_at(file: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match read_at(file, buf, off)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                off += n as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The format-dependent parts on hand-built pages of both formats, and
    //! the whole audit of LMDB 1.0 files written by the C 1.0 tools against
    //! `cpeer10 check-freelist`. (`rlmdb` cannot open a 1.0 environment
    //! yet, so these tests run the walk on the file without pinning a
    //! snapshot; nothing else uses the files meanwhile.)

    use std::ffi::OsString;
    use std::path::Path;

    use interop::tools::run_tool_any_status;
    use interop::{bins, Output, TestEnv};
    use rlmdb::format::layout::{META_LAST_PGNO, NODE_FLAGS};
    use rlmdb::format::meta::pick_newest;
    use rlmdb::format::page::{NodeData, PageMut};

    use super::*;

    #[test]
    fn max_page_hdr_is_the_longer_header() {
        assert_eq!(MAX_PAGE_HDR, V09.page_hdr.max(V10.page_hdr));
    }

    /// The two meta pages of a new environment of format `l`.
    fn meta_pages(l: &'static Layout, psize: usize) -> Vec<u8> {
        let mut buf = vec![0u8; 2 * psize];
        Meta::new_env_with_layout(l, psize as u32, 1 << 20, 0)
            .write_initial_pages_with_layout(&mut buf, psize, l);
        buf
    }

    #[test]
    fn sniffs_the_format_of_meta_pages() {
        for (l, other) in [(&V09, &V10), (&V10, &V09)] {
            let buf = meta_pages(l, 4096);
            assert_eq!(sniff_layout(&buf).map(|s| s.format), Some(l.format));
            assert!(is_meta_of(&buf, l) && !is_meta_of(&buf, other));
            let msg = wrong_format(1, &buf, other).message.unwrap();
            let want = format!(
                "check-freelist: meta page 1 is in the {} format (data version {}), but this \
                 program decodes {} files (data version {})",
                format_name(l),
                l.data_version,
                format_name(other),
                other.data_version
            );
            assert_eq!(msg, want);
        }
        // The magic after a 1.0 header, with 0.9's version: neither format.
        let mut buf = meta_pages(&V10, 4096);
        buf[V10.meta_offset() + 4..V10.meta_offset() + 8].copy_from_slice(&1u32.to_ne_bytes());
        assert!(sniff_layout(&buf).is_none());
        assert_eq!(
            wrong_format(0, &buf, &V10).message.unwrap(),
            "check-freelist: meta page 0: magic 0xbeefc0de version 1 at offset 24, not an \
             LMDB 1.0 meta page (magic 0xbeefc0de version 3)"
        );
        assert!(sniff_layout(&[0u8; 100]).is_none());
    }

    #[test]
    fn lower_and_upper() {
        // Empty pages as mdb_page_new leaves them: 0.9 lower 16, upper
        // psize; 1.0 lower 0, upper psize - 24.
        for l in [&V09, &V10] {
            for psize in [256, 4096, 32768, 65536] {
                if psize > l.max_pagesize {
                    continue;
                }
                let mut buf = vec![0u8; psize];
                PageMut::with_layout(&mut buf, l).init(2, P_LEAF);
                let p = PageRef::with_layout(&buf, l).unwrap();
                let (lower, upper) = (usize::from(p.lower()), usize::from(p.upper()));
                assert_eq!((lower, upper + l.ptr_base), (l.empty_lower(), psize));
                assert!(lower_upper_ok(l, lower, upper, psize));
                assert!(!lower_upper_ok(l, lower, upper + 1, psize));
                assert!(!lower_upper_ok(l, upper + 2, upper, psize));
            }
        }
        assert!(!lower_upper_ok(&V09, 14, 100, 4096));
        assert!(lower_upper_ok(&V10, 0, 4072, 4096));
        assert!(!lower_upper_ok(&V10, 0, 4073, 4096));
    }

    /// A sub-page of format `l` with flags `flags` holding two values, as
    /// the data of a leaf node: `P_LEAF2` keys if `dupfixed`, else nodes.
    fn subpage(l: &'static Layout, flags: u16, dupfixed: bool) -> Vec<u8> {
        let mut sp = vec![0u8; l.page_hdr + 32];
        let mut p = PageMut::with_layout(&mut sp, l);
        p.init(9, flags);
        if dupfixed {
            p.set_pad(4);
            p.add_leaf2_key(0, b"aaaa").unwrap();
            p.add_leaf2_key(1, b"bbbb").unwrap();
        } else {
            p.add_node(0, b"a", NodeData::Inline(&[]), 0).unwrap();
            p.add_node(1, b"bbb", NodeData::Inline(&[]), 0).unwrap();
        }
        sp
    }

    #[test]
    fn subpage_headers() {
        let ok = |l: &'static Layout, flags: u16, dupfixed: bool| {
            let sp = subpage(l, flags, dupfixed);
            let p = PageRef::with_layout(&sp, l).unwrap();
            assert_eq!(p.num_keys(), 2);
            subpage_ok(l, &p, sp.len(), dupfixed)
        };
        // 1.0 writes P_LEAF|P_SUBP (0x42), with P_LEAF2 in DUPFIXED DBs; it
        // has no P_DIRTY.
        assert!(ok(&V10, 0x42, false));
        assert!(ok(&V10, 0x62, true));
        assert!(!ok(&V10, 0x52, false));
        assert!(!ok(&V10, 0x72, true));
        assert!(!ok(&V10, 0x62, false));
        assert!(!ok(&V10, 0x42, true));
        // 0.9 writes P_DIRTY too (0x52), and 0x42 is fine as well.
        assert!(ok(&V09, 0x52, false) && ok(&V09, 0x42, false));
        assert!(ok(&V09, 0x72, true) && ok(&V09, 0x62, true));
        assert!(!ok(&V09, 0x43, false));
        // mp_lower must stay within the node's data (stored lower 4 +
        // PAGEBASE 24 = 28 bytes).
        let sp = subpage(&V10, 0x42, false);
        let p = PageRef::with_layout(&sp, &V10).unwrap();
        assert_eq!(p.lower(), 4);
        assert!(subpage_ok(&V10, &p, 28, false));
        assert!(!subpage_ok(&V10, &p, 27, false));
    }

    /// `P_LEAF2` headers: with 1-byte items `mp_upper` grows past the end
    /// of the (sub-)page, as C writes it; `SIZELEFT` must be the space
    /// after the items.
    #[test]
    fn leaf2_headers() {
        for l in [&V09, &V10] {
            for ksize in [1usize, 2, 3, 4] {
                let len = l.page_hdr + 8 * ksize;
                let mut sp = vec![0u8; len];
                let mut p = PageMut::with_layout(&mut sp, l);
                p.init(9, P_LEAF | P_LEAF2 | P_SUBP);
                p.set_pad(ksize as u16);
                for i in 0..6u8 {
                    p.add_leaf2_key(usize::from(i), &vec![i; ksize]).unwrap();
                }
                let p = PageRef::with_layout(&sp, l).unwrap();
                assert_eq!(p.num_keys(), 6);
                assert_eq!(p.size_left(), 2 * ksize);
                if ksize == 1 {
                    // 6 items moved mp_upper up by 6, past the end.
                    assert_eq!(usize::from(p.upper()) + l.ptr_base, len + 6);
                }
                assert!(subpage_ok(l, &p, len, true));
                assert!(leaf2_ok(l, &p, len));
                // A size that disagrees with SIZELEFT, either way.
                assert!(!leaf2_ok(l, &p, len - 1));
                assert!(!leaf2_ok(l, &p, len + 1));
            }
        }
    }

    /// Nodes of a 1.0 leaf: the data of a node with an odd key size starts
    /// after a pad byte, an `F_BIGDATA` node holds a 24-byte `MDB_ovpage`.
    #[test]
    fn nodes_of_1_0_pages() {
        let mut buf = vec![0u8; 1024];
        let mut p = PageMut::with_layout(&mut buf, &V10);
        p.init(5, P_LEAF);
        p.add_node(0, b"abc", NodeData::Inline(b"data"), 0).unwrap();
        let big = NodeData::Overflow {
            pgno: 9,
            data_len: 3000,
        };
        p.add_node(1, b"big", big, 0).unwrap();
        let n = read_node(&V10, &buf, 0).unwrap();
        assert_eq!((n.key, n.dsize, n.flags), (&b"abc"[..], 4, 0));
        assert_eq!(n.raw.inline_data(), Some(&b"data"[..]));
        let off = PageRef::with_layout(&buf, &V10)
            .unwrap()
            .node_offset(0)
            .unwrap();
        assert_eq!(&buf[off + 12..off + 16], b"data", "after EVEN(3) key bytes");
        let n = read_node(&V10, &buf, 1).unwrap();
        assert_eq!(n.flags, F_BIGDATA);
        let r = n.raw.overflow_ref().unwrap();
        assert_eq!((r.pgno, r.txnid, r.pages), (9, 0, 3));
        // Read as 0.9, the header has no nodes (mp_lower would be 0..16).
        assert!(read_node(&V09, &buf, 0).is_none());
    }

    /// A walker over a scratch file that writes its report and problems
    /// into `out` and `err`.
    fn walker<'o>(
        l: &'static Layout,
        txnid: u64,
        out: &'o mut Vec<u8>,
        err: &'o mut Vec<u8>,
    ) -> Walker<'o> {
        Walker {
            file: tempfile::tempfile().unwrap(),
            layout: l,
            psize: 1024,
            txnid,
            last_pg: 10,
            used: vec![Use::None; 11],
            names: vec![b"-".to_vec()],
            errors: 0,
            free_listed: 0,
            out,
            err,
        }
    }

    #[test]
    fn mp_txnid_and_ovpage_checks() {
        // An overflow run of 3 pages written by txn 7.
        let mut buf = vec![0u8; 1024];
        PageMut::with_layout(&mut buf, &V10).init_overflow(4, 3);
        V10.stamp_clean_page(&mut buf, 7);
        let ov = PageRef::with_layout(&buf, &V10).unwrap();
        let r = |txnid, pages| OverflowRef {
            pgno: 4,
            txnid,
            pages,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut w = walker(&V10, 9, &mut out, &mut err);
        w.check_txnid(4, &ov);
        w.check_ovpage(&r(7, 3), &ov);
        w.check_ovpage(&r(1, 3), &ov);
        assert_eq!(w.errors, 0);
        w.txnid = 6;
        w.check_txnid(4, &ov);
        w.check_ovpage(&r(8, 3), &ov);
        w.check_ovpage(&r(0, 2), &ov);
        let mut zero = buf.clone();
        V10.stamp_clean_page(&mut zero, 0);
        w.check_txnid(5, &PageRef::with_layout(&zero, &V10).unwrap());
        assert_eq!(w.errors, 5);
        drop(w);
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "rpeer: check-freelist: page 4: mp_txnid 7 is not in 1..6 (the snapshot)\n\
             rpeer: check-freelist: overflow page 4: op_txnid 8 is not in 1..7 (its mp_txnid)\n\
             rpeer: check-freelist: overflow page 4: op_pages 2 in the node, mp_pages 3 in the \
             page\n\
             rpeer: check-freelist: overflow page 4: op_txnid 0 is not in 1..7 (its mp_txnid)\n\
             rpeer: check-freelist: page 5: mp_txnid 0 is not in 1..6 (the snapshot)\n"
        );

        // 0.9 pages have neither: nothing to check.
        let mut buf = vec![0u8; 1024];
        PageMut::with_layout(&mut buf, &V09).init_overflow(4, 3);
        let ov = PageRef::with_layout(&buf, &V09).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut w = walker(&V09, 9, &mut out, &mut err);
        w.check_txnid(4, &ov);
        w.check_ovpage(&r(0, 0), &ov);
        assert_eq!(w.errors, 0);
    }

    // ---- LMDB 1.0 files written by C, audited by both ---------------------

    /// Run C program `bin`; it must succeed.
    fn run_c(bin: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        run_tool_any_status(bin, &args, None, stdin)
            .and_then(Output::ok)
            .unwrap()
    }

    /// An LMDB 1.0 environment of `psize`-byte pages written by the C 1.0
    /// tools alone: `mdb_load10` creates it, `cpeer10` fills named DBs
    /// (overflow values; DUPSORT and DUPFIXED values in sub-pages and
    /// sub-DBs) and deletes some, leaving free pages.
    fn c10_env(psize: usize) -> TestEnv {
        let env = TestEnv::new().unwrap();
        let path = env.path().to_str().unwrap();
        let header = format!(
            "VERSION=3\nformat=bytevalue\nmapsize=268435456\ndb_pagesize={psize}\n\
             HEADER=END\nDATA=END\n"
        );
        run_c(bins::mdb_load10(), &[path], Some(header.as_bytes()));
        let dups = (psize / 16).max(40).to_string();
        let big = (2 * psize + 100).to_string();
        for args in [
            &["fill", path, "plain", "200", "1", "--big", &big][..],
            &[
                "fill",
                path,
                "dups",
                "100",
                "2",
                "--dupsort",
                "--dups",
                &dups,
            ],
            &[
                "fill",
                path,
                "fixed",
                "100",
                "3",
                "--dupfixed",
                "--dups",
                &dups,
            ],
            &[
                "delete", path, "plain", "200", "1", "--every", "2", "--big", &big,
            ],
            &[
                "delete",
                path,
                "dups",
                "100",
                "2",
                "--every",
                "3",
                "--dupsort",
                "--dups",
                &dups,
            ],
        ] {
            run_c(bins::cpeer10(), args, None);
        }
        env
    }

    /// The newest meta page of a 1.0 data file: its file offset, the meta.
    fn newest_meta(data: &[u8]) -> (usize, Meta) {
        let psize = meta_of(data, &V10).unwrap().page_size() as usize;
        let metas = [0, psize].map(|off| meta_of(&data[off..], &V10).unwrap());
        let k = pick_newest(&metas);
        (k * psize, metas[k])
    }

    /// rpeer's audit of the newest snapshot of the data file `path`, as
    /// `rpeer check-freelist` would print it: exit status, stdout, stderr.
    fn rust_audit(path: &Path) -> (i32, String, String) {
        let (_, newest) = newest_meta(&std::fs::read(path).unwrap());
        let file = File::open(path).unwrap();
        let mut psize = newest.page_size() as usize;
        let (meta, layout) = pick_meta(&file, newest.txnid, &mut psize)
            .unwrap()
            .expect("the newest meta page");
        let snap = Snapshot {
            layout,
            psize,
            txnid: meta.txnid,
            meta,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = match audit(file, &snap, psize, &mut out, &mut err) {
            Ok(()) => 0,
            Err(f) => {
                if let Some(m) = f.message {
                    writeln!(err, "rpeer: {m}").unwrap();
                }
                i32::from(f.code)
            }
        };
        let text = |b| String::from_utf8(b).unwrap();
        (code, text(out), text(err))
    }

    /// `cpeer10 check-freelist` and rpeer's walk of the file report the
    /// same, line by line. Returns C's output.
    fn assert_same_audit(env: &TestEnv) -> Output {
        let args = [OsString::from("check-freelist"), env.path().into()];
        let c = run_tool_any_status(bins::cpeer10(), &args, None, None).unwrap();
        let (code, out, err) = rust_audit(&env.data_file());
        assert_eq!(c.code(), Some(code), "C: {c}\nRust:\n{out}{err}");
        assert_eq!(c.stdout, out, "report differs from C's");
        assert_eq!(
            c.stderr.replace("cpeer: ", ""),
            err.replace("rpeer: ", ""),
            "problems differ from C's"
        );
        c
    }

    #[test]
    fn walk_of_c10_files_matches_cpeer10() {
        for psize in [256, 1024, 16384, 65536] {
            let env = c10_env(psize);
            let c = assert_same_audit(&env);
            assert!(c.stdout.starts_with(&format!("psize={psize}\n")), "{c}");
            assert!(c.success() && c.stdout.ends_with("\nok\n"), "{c}");
        }
    }

    /// The pages of the tree under `root` of a 1.0 file, depth first.
    fn tree_pages(data: &[u8], psize: usize, root: u64) -> Vec<u64> {
        let (mut pages, mut stack) = (Vec::new(), vec![root]);
        while let Some(pgno) = stack.pop() {
            pages.push(pgno);
            let page = PageRef::with_layout(&data[pgno as usize * psize..], &V10).unwrap();
            if page.is_branch() {
                for i in (0..page.num_keys()).rev() {
                    stack.push(page.node(i).unwrap().child_pgno());
                }
            }
        }
        pages
    }

    /// The file offset of the first leaf node under `root` with exactly the
    /// node flags `flags`.
    fn first_node(data: &[u8], psize: usize, root: u64, flags: u16) -> usize {
        for pgno in tree_pages(data, psize, root) {
            let start = pgno as usize * psize;
            let page = PageRef::with_layout(&data[start..start + psize], &V10).unwrap();
            if !page.is_leaf() || page.is_leaf2() {
                continue;
            }
            for i in 0..page.num_keys() {
                if page.node(i).unwrap().flags() == flags {
                    return start + page.node_offset(i).unwrap();
                }
            }
        }
        panic!("no node with flags {flags:#x} under page {root}");
    }

    /// The `MDB_db` of named DB `name`. (C 1.0 stores the name with its
    /// terminating NUL as the key, 0.9 without.)
    fn named_db(data: &[u8], psize: usize, name: &[u8]) -> Db {
        let (_, meta) = newest_meta(data);
        for pgno in tree_pages(data, psize, meta.dbs[1].root) {
            let page = PageRef::with_layout(&data[pgno as usize * psize..], &V10).unwrap();
            for i in 0..page.num_keys() {
                let n = page.node(i).unwrap();
                if page.is_leaf()
                    && n.flags() == F_SUBDATA
                    && n.key() == Some(&[name, b"\0"].concat())
                {
                    return Db::read(n.inline_data().unwrap()).unwrap();
                }
            }
        }
        panic!("no DB {name:?}");
    }

    /// A copy of `env` (its data file) to mutate.
    fn copy_env(env: &TestEnv) -> TestEnv {
        let copy = TestEnv::new().unwrap();
        std::fs::copy(env.data_file(), copy.data_file()).unwrap();
        copy
    }

    fn patch(env: &TestEnv, offset: usize, bytes: &[u8]) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(env.data_file())
            .unwrap();
        interop::os::write_all_at(&f, bytes, offset as u64).unwrap();
    }

    /// Broken 1.0 files: both audits find the same problems, and say so in
    /// the same words.
    #[test]
    fn problems_in_c10_files_match_cpeer10() {
        let psize = 1024;
        let env = c10_env(psize);
        let data = std::fs::read(env.data_file()).unwrap();
        let (meta_off, meta) = newest_meta(&data);
        let snapshot = meta.txnid;
        let main = meta.dbs[1].root as usize;
        let ksize = |node: usize| usize::from(u16::from_ne_bytes([data[node + 6], data[node + 7]]));
        let big = first_node(
            &data,
            psize,
            named_db(&data, psize, b"plain").root,
            F_BIGDATA,
        );
        let ovpage = big + V10.node_data_offset(ksize(big));
        let sub = first_node(
            &data,
            psize,
            named_db(&data, psize, b"dups").root,
            F_DUPDATA,
        );
        let sub_flags = sub + V10.node_data_offset(ksize(sub)) + V10.flags_offset();
        let last_pgno = meta_off + V10.meta_offset() + META_LAST_PGNO;
        let u64b = |v: u64| v.to_ne_bytes().to_vec();
        let mutations = [
            (
                format!("page {main}: mp_txnid 0 is not in 1..{snapshot}"),
                main * psize + 8,
                u64b(0),
            ),
            (
                format!("mp_txnid {} is not in 1..{snapshot}", snapshot + 1),
                main * psize + 8,
                u64b(snapshot + 1),
            ),
            ("op_pages 9 in the node".into(), ovpage + 16, u64b(9)),
            ("op_txnid 0 is not in".into(), ovpage + 8, u64b(0)),
            (
                format!("op_txnid {} is not in", snapshot + 1),
                ovpage + 8,
                u64b(snapshot + 1),
            ),
            (
                format!("page {} is leaked", meta.last_pgno + 1),
                last_pgno,
                u64b(meta.last_pgno + 1),
            ),
            (
                "bad sub-page in DB dups (flags 0x52".into(),
                sub_flags,
                0x52u16.to_ne_bytes().to_vec(),
            ),
            (
                "bad node flags 0x3 in DB plain".into(),
                big + NODE_FLAGS,
                3u16.to_ne_bytes().to_vec(),
            ),
        ];
        for (expect, offset, bytes) in mutations {
            let copy = copy_env(&env);
            patch(&copy, offset, &bytes);
            let c = assert_same_audit(&copy);
            assert_eq!(c.code(), Some(1), "{expect}: {c}");
            assert!(c.stderr.contains(&expect), "{expect}: {c}");
        }
    }
}
