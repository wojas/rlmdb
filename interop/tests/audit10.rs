//! The checkers on LMDB 1.0 data files: the page audit of `cpeer10
//! check-freelist` (`c/cpeer_pagewalk.c` built against the 1.0 library)
//! and the structural comparison of `interop::pagecmp`.
//!
//! The files are written by the C 1.0 programs alone: `mdb_load10` creates
//! an environment with the page size its input names (`db_pagesize=`, any
//! power of two from 256 to 65536), `cpeer10` fills and deletes through the
//! public API (at the OS page size when it creates the environment itself),
//! `mdb_copy10` copies, and the upstream `mtest*` programs write theirs. A
//! broken file is a copy of a good one with a few bytes changed, as
//! `tests/c_vs_c.rs` does for 0.9. The bytes to change are found with the
//! 1.0 offsets of `DUAL_FORMAT.md` §1, decoded here independently of
//! `rlmdb`.

use std::ffi::OsString;
use std::path::Path;

use interop::pagecmp::{compare_data_files, data_format, reachable_pages};
use interop::report::FreelistReport;
use interop::tools::run_tool_any_status;
use interop::{bins, scenarios, Output, Peer, TestEnv};

// ---- running the C 1.0 programs ----------------------------------------------

/// Run a C program with `args` (and `stdin`), whatever its exit status.
fn run_any(bin: &Path, args: &[&str], cwd: Option<&Path>, stdin: Option<&[u8]>) -> Output {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    run_tool_any_status(bin, &args, cwd, stdin).expect("run a C program")
}

/// Run a C program; it must succeed.
fn run(bin: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let out = run_any(bin, args, None, stdin);
    assert!(out.success(), "{out}");
    out
}

fn path(env: &TestEnv) -> &str {
    env.path().to_str().expect("a UTF-8 temp path")
}

/// `cpeer10 check-freelist` on `env`, whatever its outcome.
fn audit(env: &TestEnv) -> Output {
    run_any(bins::cpeer10(), &["check-freelist", path(env)], None, None)
}

/// `cpeer10 check-freelist` on `env`, which must pass.
fn audit_ok(env: &TestEnv) -> FreelistReport {
    let out = audit(env);
    assert!(out.success(), "{out}");
    FreelistReport::parse(&out.stdout).expect("a check-freelist report")
}

/// `cpeer10 check-freelist` on `env` must fail and say `expect`.
fn assert_audit_fails(env: &TestEnv, expect: &str) {
    let out = audit(env);
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stdout.contains("\nerrors="), "{out}");
    assert!(out.stderr.contains(expect), "expected `{expect}`: {out}");
}

/// A new, empty 1.0 environment: with `psize`-byte pages, created by
/// `mdb_load10` from an empty dump; with `None`, created by `cpeer10
/// create` at the OS page size.
fn new_env10(psize: Option<usize>) -> TestEnv {
    let env = TestEnv::new().expect("temp env");
    match psize {
        Some(psize) => {
            let header = format!(
                "VERSION=3\nformat=bytevalue\nmapsize=268435456\ndb_pagesize={psize}\n\
                 HEADER=END\nDATA=END\n"
            );
            run(bins::mdb_load10(), &[path(&env)], Some(header.as_bytes()));
        }
        None => {
            run(
                bins::cpeer10(),
                &["create", path(&env), "268435456", "16"],
                None,
            );
        }
    }
    env
}

/// The data sets [`fill10`] writes into an environment of `psize`-byte
/// pages, as `cpeer10` arguments `<db> <count> <seed> [options]`, and the
/// `delete --every` applied to some. `plain` has overflow values of two
/// pages and more, `dups` and `fixed` (DUPSORT, DUPFIXED) have up to
/// `psize / 16` values per key, so some keys keep them in a sub-page and
/// others in a sub-DB.
fn data_sets(psize: usize) -> Vec<(Vec<String>, Option<u32>)> {
    let dups = (psize / 16).max(40).to_string();
    let big = (2 * psize + 100).to_string();
    let set = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    vec![
        (set(&["plain", "200", "1", "--big", &big]), Some(2)),
        (
            set(&["dups", "100", "2", "--dupsort", "--dups", &dups]),
            Some(3),
        ),
        (
            set(&["fixed", "100", "3", "--dupfixed", "--dups", &dups]),
            None,
        ),
        (set(&["ints", "100", "4", "--intkey"]), None),
    ]
}

/// Fill `env` (pages of `psize` bytes) with [`data_sets`] and delete from
/// some, which leaves pages in the free list. Every write is a `cpeer10`
/// command.
fn fill10(env: &TestEnv, psize: usize) {
    let sets = data_sets(psize);
    for (set, _) in &sets {
        let mut args = vec!["fill", path(env)];
        args.extend(set.iter().map(String::as_str));
        run(bins::cpeer10(), &args, None);
    }
    for (set, every) in &sets {
        let Some(every) = every else { continue };
        let every = every.to_string();
        let mut args = vec!["delete", path(env)];
        args.extend(set.iter().map(String::as_str));
        args.extend(["--every", &every]);
        run(bins::cpeer10(), &args, None);
    }
}

/// `cpeer10 verify` of every data set of [`fill10`].
fn verify10(env: &TestEnv, psize: usize) {
    for (set, every) in data_sets(psize) {
        let mut args = vec!["verify".to_owned(), path(env).to_owned()];
        args.extend(set);
        if let Some(every) = every {
            args.extend(["--deleted-every".to_owned(), every.to_string()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run(bins::cpeer10(), &args, None);
    }
}

/// An environment of `psize`-byte pages (`None`: the OS page size) with
/// [`fill10`]'s data, and its page size.
fn filled_env10(psize: Option<usize>) -> (TestEnv, usize) {
    let env = new_env10(psize);
    let psize = psize.unwrap_or_else(|| File10::read(&env).psize);
    fill10(&env, psize);
    (env, psize)
}

// ---- the 1.0 data file, decoded just enough to find bytes to change ----------

/// `PAGEHDRSZ` of LMDB 1.0, which is also `PAGEBASE` and where the
/// `MDB_meta` starts in pages 0 and 1.
const HDR: usize = 24;
const P_BRANCH: usize = 0x01;
const P_LEAF: usize = 0x02;
const P_LEAF2: usize = 0x20;
const F_BIGDATA: usize = 0x01;
const F_SUBDATA: usize = 0x02;
const F_DUPDATA: usize = 0x04;

/// Offsets of `MDB_node`'s `mn_lo` and `mn_hi`, which C declares in the
/// other order on big-endian hosts.
const NODE_LO: usize = if cfg!(target_endian = "little") { 0 } else { 2 };
const NODE_HI: usize = 2 - NODE_LO;

fn rd16(data: &[u8], off: usize) -> usize {
    u16::from_ne_bytes(data[off..off + 2].try_into().unwrap()).into()
}

fn rd32(data: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(data[off..off + 4].try_into().unwrap())
}

fn rd64(data: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(data[off..off + 8].try_into().unwrap())
}

/// A 1.0 data file in memory.
struct File10 {
    data: Vec<u8>,
    psize: usize,
    /// File offset of the newest `MDB_meta`.
    meta: usize,
}

impl File10 {
    fn read(env: &TestEnv) -> File10 {
        let data = std::fs::read(env.data_file()).expect("read data file");
        assert_eq!(rd32(&data, HDR), 0xBEEF_C0DE, "a 1.0 meta page");
        assert_eq!(rd32(&data, HDR + 4), 3, "MDB_DATA_VERSION 3");
        let psize = rd32(&data, HDR + 24) as usize; // mm_dbs[0].md_pad
        let txnid = |m: usize| rd64(&data, m * psize + HDR + 128);
        let meta = usize::from(txnid(1) > txnid(0)) * psize + HDR;
        File10 { data, psize, meta }
    }

    /// `mm_txnid` of the newest meta page: the snapshot an audit sees.
    fn txnid(&self) -> u64 {
        rd64(&self.data, self.meta + 128)
    }

    /// File offset of `mm_last_pg` of the newest meta page.
    fn last_pgno_at(&self) -> usize {
        self.meta + 120
    }

    /// `md_root` of `mm_dbs[dbi]` (0: free DB, 1: main DB).
    fn root(&self, dbi: usize) -> u64 {
        rd64(&self.data, self.meta + 24 + 48 * dbi + 40)
    }

    /// File offset of page `pgno`.
    fn page(&self, pgno: u64) -> usize {
        pgno as usize * self.psize
    }

    fn flags(&self, pgno: u64) -> usize {
        rd16(&self.data, self.page(pgno) + 18)
    }

    /// File offsets of the nodes of branch or leaf page `pgno`:
    /// `mp_ptrs[i] + PAGEBASE`, `NUMKEYS = mp_lower >> 1`.
    fn nodes(&self, pgno: u64) -> Vec<usize> {
        let p = self.page(pgno);
        let nkeys = rd16(&self.data, p + 20) >> 1;
        (0..nkeys)
            .map(|i| p + rd16(&self.data, p + HDR + 2 * i) + HDR)
            .collect()
    }

    fn node_flags(&self, node: usize) -> usize {
        rd16(&self.data, node + 4)
    }

    fn node_key(&self, node: usize) -> &[u8] {
        let ksize = rd16(&self.data, node + 6);
        &self.data[node + 8..node + 8 + ksize]
    }

    /// File offset of a node's data: `NODEDATA`, after the key padded to an
    /// even length.
    fn node_data(&self, node: usize) -> usize {
        node + 8 + ((rd16(&self.data, node + 6) + 1) & !1)
    }

    /// The pages of the tree under `root`, depth first.
    fn tree(&self, root: u64) -> Vec<u64> {
        let (mut pages, mut stack) = (Vec::new(), vec![root]);
        while let Some(pgno) = stack.pop() {
            pages.push(pgno);
            if self.flags(pgno) & P_BRANCH != 0 {
                for node in self.nodes(pgno).into_iter().rev() {
                    // A branch node's child: mn_lo | mn_hi << 16 | mn_flags << 32.
                    let lo = rd16(&self.data, node + NODE_LO);
                    let hi = rd16(&self.data, node + NODE_HI);
                    stack.push((lo | hi << 16 | self.node_flags(node) << 32) as u64);
                }
            }
        }
        pages
    }

    /// The leaf nodes of the tree under `root` (not `P_LEAF2` pages).
    fn leaf_nodes(&self, root: u64) -> Vec<usize> {
        self.tree(root)
            .into_iter()
            .filter(|&pg| self.flags(pg) & (P_LEAF | P_LEAF2) == P_LEAF)
            .flat_map(|pg| self.nodes(pg))
            .collect()
    }

    /// The root page of named DB `name` (1.0 stores the name with its
    /// terminating NUL as the key in the main DB).
    fn named_root(&self, name: &str) -> u64 {
        let key = [name.as_bytes(), b"\0"].concat();
        let node = self
            .leaf_nodes(self.root(1))
            .into_iter()
            .find(|&n| self.node_flags(n) == F_SUBDATA && self.node_key(n) == key)
            .unwrap_or_else(|| panic!("no DB {name}"));
        rd64(&self.data, self.node_data(node) + 40)
    }

    /// The first leaf node under `root` with node flags exactly `flags`.
    fn first_node(&self, root: u64, flags: usize) -> usize {
        self.leaf_nodes(root)
            .into_iter()
            .find(|&n| self.node_flags(n) == flags)
            .unwrap_or_else(|| panic!("no node with flags {flags:#x} under page {root}"))
    }

    /// The first page of the tree under `root` with page flags `flags`.
    fn first_page(&self, root: u64, flags: usize) -> u64 {
        self.tree(root)
            .into_iter()
            .find(|&pg| self.flags(pg) & flags != 0)
            .unwrap_or_else(|| panic!("no page with flags {flags:#x} under page {root}"))
    }
}

/// Overwrite `bytes` at `offset` of the data file of `env`.
fn patch(env: &TestEnv, offset: usize, bytes: &[u8]) {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(env.data_file())
        .expect("open data file");
    interop::os::write_all_at(&f, bytes, offset as u64).expect("patch");
}

/// A copy of `env` (its data file) to break.
fn copy_env(env: &TestEnv) -> TestEnv {
    let copy = TestEnv::new().expect("temp env");
    std::fs::copy(env.data_file(), copy.data_file()).expect("copy data file");
    copy
}

// ---- check-freelist on good files ---------------------------------------------

/// `cpeer10 check-freelist` passes on C 1.0 files of every page size:
/// named DBs, DUPSORT values in sub-pages and sub-DBs, DUPFIXED (`P_LEAF2`
/// sub-pages and pages), `MDB_INTEGERKEY`, overflow values (also in the
/// free DB at small page sizes) and free pages; its counts agree with what
/// was written.
#[test]
fn audit_passes_on_c10_files() {
    for psize in [Some(256), Some(1024), Some(4096), None, Some(65536)] {
        let (env, psize) = filled_env10(psize);
        verify10(&env, psize);
        let r = audit_ok(&env);
        println!(
            "psize={psize} last_pgno={} pages_free={}",
            r.last_pgno, r.pages_free
        );
        assert_eq!(r.psize as usize, psize);
        assert_eq!(r.pages_leaked, 0);
        assert!(r.pages_free > 0, "the deletes freed pages");
        // Items 3, 9, 15, … keep their big value: 33 runs of 3 pages.
        assert_eq!(r.db("plain").unwrap().entries, 100);
        assert_eq!(r.db("plain").unwrap().overflow_pages, 99);
        for db in ["dups", "fixed"] {
            let line = r.db(db).unwrap();
            assert!(line.subdbs.unwrap() > 0, "{db}: {line:?}");
            assert!(line.subpages.unwrap() > 0, "{db}: {line:?}");
        }
        assert_eq!(r.db("ints").unwrap().entries, 100);
        if psize == 256 {
            assert!(r.db("@free").unwrap().overflow_pages > 0, "{r:?}");
        }
    }
}

/// Plain and compacting copies by `mdb_copy10` and `cpeer10 copy` pass:
/// the compacting copy is snapshot 1, all of its pages stamped 1. Its
/// sub-pages keep the `mp_txnid` they had (C copies leaf pages as they
/// are), which can be above the snapshot: the audit checks the txnid of
/// pages, not of sub-pages.
#[test]
fn audit_passes_on_copies_of_c10_files() {
    let (env, _) = filled_env10(Some(1024));
    let txnid = File10::read(&env).txnid();
    for (compact, tool) in [(false, true), (true, true), (true, false)] {
        let copy = env.derived(&format!("copy-{compact}-{tool}"));
        let mut args = if tool { vec![] } else { vec!["copy"] };
        if compact {
            args.push(if tool { "-c" } else { "--compact" });
        }
        args.extend([path(&env), path(&copy)]);
        if tool {
            std::fs::create_dir(copy.path()).expect("create the copy's directory");
            run(bins::mdb_copy10(), &args, None);
        } else {
            run(bins::cpeer10(), &args, None);
        }
        let r = audit_ok(&copy);
        if compact {
            assert_eq!((r.txnid, r.pages_free), (1, 0), "{r:?}");
            let f = File10::read(&copy);
            let sub = f.first_node(f.named_root("dups"), F_DUPDATA);
            assert!(rd64(&f.data, f.node_data(sub) + 8) > 1, "sub-page txnid");
        } else {
            assert_eq!(r.txnid, txnid);
        }
    }
}

/// The environments of the upstream 1.0 test programs pass (`mtest10`
/// uses 1 KiB pages; the encrypted ones cannot be opened without their
/// key and are left out).
#[test]
fn audit_passes_on_mtest10_files() {
    for name in bins::MTESTS10
        .iter()
        .filter(|n| !n.starts_with("mtest_enc"))
    {
        let env = TestEnv::with_name("testdb").expect("temp env");
        let bin = bins::mtest10(name).expect("built");
        let out = run_any(bin, &[], Some(env.root()), None);
        assert!(out.success(), "{out}");
        let r = audit_ok(&env);
        assert_eq!(r.pages_leaked, 0, "{name}");
        if *name == "mtest" {
            assert_eq!(r.psize, 1024);
        }
    }
}

// ---- check-freelist on broken files -------------------------------------------

#[test]
fn audit_reports_a_leaked_page() {
    let (env, _) = filled_env10(Some(1024));
    let f = File10::read(&env);
    let last = rd64(&f.data, f.last_pgno_at());
    patch(&env, f.last_pgno_at(), &(last + 1).to_ne_bytes());
    assert_audit_fails(
        &env,
        &format!("page {} is leaked (neither used nor free)", last + 1),
    );
}

/// The first page number of the first free DB record becomes the main DB's
/// root. The free DB of a 16 KiB-page environment is one leaf whose records
/// are inline; at small page sizes the record would be on an overflow page.
#[test]
fn audit_reports_a_page_used_twice() {
    let (env, _) = filled_env10(Some(16384));
    let f = File10::read(&env);
    let free_root = f.root(0);
    assert_eq!(f.flags(free_root) & P_LEAF, P_LEAF, "a one-page free DB");
    let record = f.nodes(free_root)[0];
    assert_eq!(f.node_flags(record) & F_BIGDATA, 0, "an inline record");
    let main = f.root(1);
    // Data: [count, pgno...].
    patch(&env, f.node_data(record) + 8, &main.to_ne_bytes());
    assert_audit_fails(
        &env,
        &format!("page {main} used twice: free list and tree of DB -"),
    );
}

/// Every reachable page's `mp_txnid` must be in `1..=snapshot`: a leaf, a
/// branch page and the first page of an overflow run, set to 0 and to the
/// next txnid.
#[test]
fn audit_reports_a_bad_mp_txnid() {
    let (env, psize) = filled_env10(Some(1024));
    let f = File10::read(&env);
    let snapshot = f.txnid();
    let plain = f.named_root("plain");
    let big = f.first_node(plain, F_BIGDATA);
    let overflow = rd64(&f.data, f.node_data(big));
    let pages = [
        f.root(1),
        f.first_page(f.named_root("dups"), P_BRANCH),
        f.first_page(plain, P_LEAF),
        overflow,
    ];
    for pgno in pages {
        for bad in [0, snapshot + 1] {
            let copy = copy_env(&env);
            patch(&copy, pgno as usize * psize + 8, &u64::to_ne_bytes(bad));
            assert_audit_fails(
                &copy,
                &format!("page {pgno}: mp_txnid {bad} is not in 1..{snapshot} (the snapshot)"),
            );
        }
    }
}

/// The `MDB_ovpage` of an `F_BIGDATA` node must match its run: `op_pages`
/// = `mp_pages`, `op_txnid` in `1..=mp_txnid`.
#[test]
fn audit_reports_a_bad_ovpage() {
    let (env, psize) = filled_env10(Some(1024));
    let f = File10::read(&env);
    let big = f.first_node(f.named_root("plain"), F_BIGDATA);
    let ovpage = f.node_data(big);
    let (pgno, op_txnid, op_pages) = (
        rd64(&f.data, ovpage),
        rd64(&f.data, ovpage + 8),
        rd64(&f.data, ovpage + 16),
    );
    let mp_txnid = rd64(&f.data, pgno as usize * psize + 8);
    let mp_pages = u64::from(rd32(&f.data, pgno as usize * psize + 20));
    assert_eq!(op_pages, mp_pages);
    assert!((1..=mp_txnid).contains(&op_txnid));
    let cases = [
        (
            ovpage + 16,
            mp_pages + 1,
            format!(
                "overflow page {pgno}: op_pages {} in the node, mp_pages {mp_pages} in the page",
                mp_pages + 1
            ),
        ),
        (
            ovpage + 16,
            mp_pages - 1,
            format!("op_pages {} in the node", mp_pages - 1),
        ),
        (
            ovpage + 8,
            mp_txnid + 1,
            format!(
                "overflow page {pgno}: op_txnid {} is not in 1..{mp_txnid} (its mp_txnid)",
                mp_txnid + 1
            ),
        ),
        (
            ovpage + 8,
            0,
            format!("overflow page {pgno}: op_txnid 0 is not in 1..{mp_txnid}"),
        ),
    ];
    for (offset, value, expect) in cases {
        let copy = copy_env(&env);
        patch(&copy, offset, &value.to_ne_bytes());
        assert_audit_fails(&copy, &expect);
    }
}

/// 1.0 sub-pages are `P_LEAF|P_SUBP` (0x42); 0.9's `P_DIRTY` bit (0x52)
/// has no place in a 1.0 file.
#[test]
fn audit_reports_0_9_sub_page_flags() {
    let (env, _) = filled_env10(Some(1024));
    let f = File10::read(&env);
    let sub = f.first_node(f.named_root("dups"), F_DUPDATA);
    let flags_at = f.node_data(sub) + 18;
    assert_eq!(rd16(&f.data, flags_at), 0x42);
    patch(&env, flags_at, &0x52u16.to_ne_bytes());
    assert_audit_fails(&env, "bad sub-page in DB dups (flags 0x52");
}

// ---- pagecmp ----------------------------------------------------------------

/// Two identical runs of the C 1.0 programs leave the same pages; a node's
/// pad byte (after a key of odd length) is not compared, its key is.
#[test]
fn pagecmp_finds_identical_c10_runs_equal() {
    for psize in [Some(1024), None] {
        let (a, _) = filled_env10(psize);
        let (b, _) = filled_env10(psize);
        compare_data_files(&a.data_file(), &b.data_file()).unwrap();
        assert!(reachable_pages(&a.data_file()).unwrap() > 20);
        assert_eq!(data_format(&a.data_file()).unwrap(), 3);

        let f = File10::read(&b);
        let node = f
            .leaf_nodes(f.named_root("plain"))
            .into_iter()
            .find(|&n| f.node_flags(n) == 0 && f.node_key(n).len() % 2 == 1)
            .expect("a key of odd length");
        let pad = node + 8 + f.node_key(node).len();
        patch(&b, pad, &[f.data[pad] ^ 0xa5]);
        compare_data_files(&a.data_file(), &b.data_file()).unwrap();
        patch(&b, pad - 1, &[f.data[pad - 1] ^ 0x5a]);
        let err = compare_data_files(&a.data_file(), &b.data_file()).unwrap_err();
        assert!(err.message().contains("differs"), "{err}");
    }
}

/// What 1.0 adds to the canonical form: `mp_txnid` of pages and sub-pages,
/// `op_txnid` and `op_pages` of overflow references. Changing any of them
/// makes the page that holds it differ.
#[test]
fn pagecmp_detects_1_0_fields() {
    let (a, psize) = filled_env10(Some(1024));
    let f = File10::read(&a);
    let plain = f.named_root("plain");
    let leaf = f.first_page(plain, P_LEAF);
    let big = f.first_node(plain, F_BIGDATA);
    let big_leaf = (big / psize) as u64;
    let overflow = rd64(&f.data, f.node_data(big));
    let sub = f.first_node(f.named_root("dups"), F_DUPDATA);
    let sub_leaf = (sub / psize) as u64;
    let cases = [
        ("mp_txnid of a leaf", leaf, leaf as usize * psize + 8),
        (
            "mp_txnid of an overflow page",
            overflow,
            overflow as usize * psize + 8,
        ),
        ("mp_txnid of a sub-page", sub_leaf, f.node_data(sub) + 8),
        ("op_txnid", big_leaf, f.node_data(big) + 8),
        ("op_pages", big_leaf, f.node_data(big) + 16),
    ];
    for (what, pgno, offset) in cases {
        let b = copy_env(&a);
        compare_data_files(&a.data_file(), &b.data_file()).unwrap();
        patch(&b, offset, &[f.data[offset] ^ 0x40]);
        let err = compare_data_files(&a.data_file(), &b.data_file())
            .expect_err(what)
            .to_string();
        assert!(
            err.contains(&format!("page {pgno} differs")),
            "{what}: {err}"
        );
    }
}

/// A 0.9 and a 1.0 file are never compared: whatever they hold, the
/// formats differ.
#[test]
fn pagecmp_refuses_files_of_different_formats() {
    let env09 = TestEnv::new().unwrap();
    scenarios::create(Peer::C, &env09, 64 << 20).unwrap();
    scenarios::DataSet::new("d", 100, 1)
        .fill(Peer::C, &env09, &[])
        .unwrap();
    let env10 = new_env10(None);
    run(
        bins::cpeer10(),
        &["fill", path(&env10), "d", "100", "1"],
        None,
    );
    let (f09, f10) = (env09.data_file(), env10.data_file());
    assert_eq!(data_format(&f09).unwrap(), 1);
    assert_eq!(data_format(&f10).unwrap(), 3);
    compare_data_files(&f09, &f09).unwrap();
    compare_data_files(&f10, &f10).unwrap();
    for (a, b) in [(&f09, &f10), (&f10, &f09)] {
        let err = compare_data_files(a, b).unwrap_err().to_string();
        assert!(
            err.contains("files of different formats cannot be compared"),
            "{err}"
        );
        assert!(
            err.contains("LMDB 0.9") && err.contains("LMDB 1.0"),
            "{err}"
        );
    }
    // Not an LMDB file at all.
    let lock = env10.lock_file();
    let err = data_format(&lock).unwrap_err().to_string();
    assert!(err.contains("not an LMDB 0.9 or 1.0 data file"), "{err}");
}
