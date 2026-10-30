//! The interop test scenarios.
//!
//! Every scenario is a function `fn(Pair) -> Result<()>`: the
//! [`Pair::writer`] peer creates environments and writes, the
//! [`Pair::reader`] peer reads, checks and audits (as does the C peer, the
//! reference); the scenarios in [`bidir`] let the reader write too, those
//! in [`readers`](mod@readers) are about the reader table and the lock file.
//! [`scenario_tests!`](crate::scenario_tests) turns all of them into
//! `#[test]`s for one pair; `tests/` has one file per ordered pair of the
//! three peers of each format (`c_vs_c.rs`, `rust_writes_c_reads.rs`, ...,
//! and for LMDB 1.0 `c10_vs_c10.rs`, `rust_writes_c10_reads.rs`,
//! `rust_vs_rust10.rs`, ...). The scenarios in [`all_peers`] use all three
//! peers of a format at once and do not take a pair; those in [`mixed`]
//! have environments of both formats open at once.
//!
//! A pair has a format ([`Pair::format`]): its environments are created
//! in it, and the C peer of that release ([`Pair::reference`]: `cpeer` for
//! 0.9, `cpeer10` for 1.0) is the reference that verifies and audits
//! everything, as do the C tools of that release ([`crate::tools`]).
//!
//! To add a scenario: write the function here (or in [`bidir`] or
//! [`readers`](mod@readers)) and add its name to the list in
//! [`__scenario_list!`](crate::__scenario_list).
//!
//! Durations: loops run for [`loop_secs`] (`$INTEROP_LOOP_SECS`, default
//! 1.5 s); every scenario should finish in well under 10 s.
//!
//! Peers that do not implement a command yet (`rpeer` while `rlmdb`
//! lacks a feature, `COnRust` while `rlmdb-capi` returns `ENOSYS`) are
//! handled in two ways, both reported with a `[interop] SKIPPED` line on
//! the real stderr (visible without `--nocapture`): a step that some other
//! peer could do just as well runs on the pair's C peer instead
//! ([`Pair::capable`]); otherwise
//! the scenario stops and the test passes as skipped.

pub mod all_peers;
pub mod bidir;
pub mod mixed;
pub mod readers;

pub use bidir::*;
pub use readers::*;

use std::fmt;
use std::io::Write;

use crate::report::{
    CreateReport, FreelistReport, HoldReady, ReaderLoopReport, ReadersReport, StatReport,
    VerifyReport, WriteReport, WriterLoopReport,
};
use crate::{bail, bins, ensure, tools, Error, Format, Output, Peer, PeerProcess, Result, TestEnv};

/// The two peers of a scenario, and the format of its environments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    /// Creates environments, writes data, runs writer loops.
    pub writer: Peer,
    /// Reads and verifies, holds readers, runs reader loops.
    pub reader: Peer,
    /// The data-file format of the environments the scenario creates.
    pub format: Format,
    /// Their page size (LMDB 1.0 only); `None` is the OS page size.
    pub page_size: Option<u32>,
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "writer={} reader={} format={}",
            self.writer,
            self.reader,
            self.format.name()
        )?;
        if let Some(ps) = self.page_size {
            write!(f, " pagesize={ps}")?;
        }
        Ok(())
    }
}

impl Pair {
    /// A (writer, reader) pair on LMDB 0.9 environments.
    pub fn new(writer: Peer, reader: Peer) -> Pair {
        Pair::with_format(writer, reader, Format::V09)
    }

    /// A (writer, reader) pair on environments of `format`.
    ///
    /// # Panics
    ///
    /// If a peer cannot open such environments ([`Peer::handles`]: `C`
    /// is 0.9 only, `C10` 1.0 only).
    pub fn with_format(writer: Peer, reader: Peer, format: Format) -> Pair {
        for peer in [writer, reader] {
            assert!(
                peer.handles(format),
                "peer {peer} cannot open LMDB {} environments",
                format.name()
            );
        }
        Pair {
            writer,
            reader,
            format,
            page_size: None,
        }
    }

    /// The same pair on environments with pages of `page_size` bytes.
    ///
    /// # Panics
    ///
    /// For LMDB 0.9, whose page size is the OS page size.
    pub fn with_page_size(self, page_size: u32) -> Pair {
        assert_eq!(self.format, Format::V10, "only LMDB 1.0 sets the page size");
        Pair {
            page_size: Some(page_size),
            ..self
        }
    }

    /// The C peer of the pair's format: the reference implementation, which
    /// verifies and audits in every scenario ([`Peer::reference`]).
    pub fn reference(&self) -> Peer {
        Peer::reference(self.format)
    }

    /// A new, not yet created environment location in the pair's format
    /// and page size.
    pub fn env(&self) -> Result<TestEnv> {
        Ok(self.configure(TestEnv::new()?))
    }

    /// `env` in the pair's format and page size.
    pub fn configure(&self, env: TestEnv) -> TestEnv {
        env.with_format(self.format).with_page_size(self.page_size)
    }

    /// A value size of at least `n` bytes that needs overflow pages: `n`,
    /// which does at the OS page size, or more than half of a larger page
    /// size the pair asks for (64 KiB pages hold values of 32 KB inline).
    fn overflowing(&self, n: u32) -> u32 {
        match self.page_size {
            Some(ps) if n <= ps / 2 => ps / 2 + 1,
            _ => n,
        }
    }

    /// At least `n` duplicates of `valsize` bytes per key, enough for keys
    /// with many of them to need a sub-DB: `n` at the OS page size, more
    /// on a larger page size the pair asks for (a sub-page grows to half a
    /// page).
    fn subdb_dups(&self, n: u32, valsize: u32) -> u32 {
        match self.page_size {
            Some(ps) => n.max(ps / valsize),
            None => n,
        }
    }

    /// `peer` if it implements `command`, else the reference C peer (which
    /// implements everything), with a SKIPPED note. For steps whose point
    /// is not which peer runs them.
    pub fn capable(&self, peer: Peer, command: &str) -> Peer {
        if peer.supports(command) {
            peer
        } else {
            let reference = self.reference();
            skipped(format_args!(
                "{peer} {command}: not implemented by this peer yet, {reference} runs it instead"
            ));
            reference
        }
    }

    /// The distinct peers: writer, then reader if different.
    pub fn peers(&self) -> Vec<Peer> {
        let mut v = vec![self.writer];
        if self.reader != self.writer {
            v.push(self.reader);
        }
        v
    }

    /// The peers that audit page usage: the reader and always the
    /// reference C peer.
    pub fn auditors(&self) -> Vec<Peer> {
        let reference = self.reference();
        let mut v = vec![reference];
        if self.reader != reference {
            v.push(self.reader);
        }
        v
    }

    /// The peers that verify written data: writer, reader and always the
    /// reference C peer.
    pub fn verifiers(&self) -> Vec<Peer> {
        let mut v = self.peers();
        if !v.contains(&self.reference()) {
            v.push(self.reference());
        }
        v
    }

    /// The peers of the format not in the pair (they run extra reader
    /// loops).
    pub fn others(&self) -> Vec<Peer> {
        Peer::all(self.format)
            .into_iter()
            .filter(|p| *p != self.writer && *p != self.reader)
            .collect()
    }

    /// The other peer of the pair: the reader for the writer and the other
    /// way round.
    pub fn other(&self, peer: Peer) -> Peer {
        if peer == self.writer {
            self.reader
        } else {
            self.writer
        }
    }
}

/// Seconds that `writer-loop`/`reader-loop` run in the scenarios:
/// `$INTEROP_LOOP_SECS`, default 1.5.
pub fn loop_secs() -> f64 {
    std::env::var("INTEROP_LOOP_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.5)
}

const MIB: u64 = 1 << 20;

fn log(msg: impl fmt::Display) {
    println!("[interop] {msg}");
}

/// Something worth seeing in a passing run, on the real stderr like
/// [`skipped`]: `[interop] NOTE ...`.
fn note(msg: impl fmt::Display) {
    let _ = writeln!(std::io::stderr().lock(), "[interop] NOTE {msg}");
}

/// Wait for a `reader-loop` of `peer` started in the background and check
/// its report: it checked more than one snapshot, and only C LMDB's reader
/// raced. That reader can copy the meta page of a later commit than the
/// txnid it pins, and its next snapshot is then older (`KNOWN_C_BUGS.md`
/// #31; `mdb_txn_renew0` of C 1.0 does the same); `cpeer` and `cpeer10`
/// tolerate a step back of `#commits` with that signature and count it in
/// `meta_races`, noted here. `rlmdb` re-checks the meta page, so on it
/// (`rpeer`, `cpeer_capi`) a race is an error.
fn finish_reader_loop(peer: Peer, p: PeerProcess) -> Result<ReaderLoopReport> {
    let out = p.finish()?.ok()?;
    let r = ReaderLoopReport::parse(&out.stdout)?;
    ensure!(r.reads > 1, "`{}` checked {} snapshots", out.label, r.reads);
    if r.meta_races > 0 {
        ensure!(
            !peer.is_rust(),
            "`{}` runs on rlmdb, which cannot race, but #commits went back: {r:?}",
            out.label
        );
        let races: Vec<&str> = out
            .stderr
            .lines()
            .filter(|l| l.contains("meta race"))
            .collect();
        note(format_args!(
            "`{}`: #commits went back {} time(s) and came back at once: C's reader meta race \
             (KNOWN_C_BUGS.md #31): {}",
            out.label,
            r.meta_races,
            races.join("; ")
        ));
    }
    log(format_args!("{}: {r:?}", out.label));
    Ok(r)
}

pub use crate::peer::skipped;

// ---- Data sets ---------------------------------------------------------------

/// A data set of the deterministic generator (README: "Deterministic data
/// set"): DB name (`-` = main DB), number of keys, seed, and data options
/// (`--dupsort`, `--dups N`, `--big N`, …). Fill-only options
/// (`--per-txn`, `--append`, `--reserve`) are passed to [`DataSet::fill`].
#[derive(Clone, Debug)]
pub struct DataSet {
    /// DB name, `-` for the main DB.
    pub db: String,
    /// Number of keys.
    pub count: u32,
    /// Seed.
    pub seed: u32,
    /// Data options, as peer arguments.
    pub options: Vec<String>,
}

impl DataSet {
    /// `count` keys with `seed` in DB `db`.
    pub fn new(db: &str, count: u32, seed: u32) -> DataSet {
        DataSet {
            db: db.to_owned(),
            count,
            seed,
            options: Vec::new(),
        }
    }

    /// Add data options, e.g. `.with(&["--dupsort", "--dups", "8"])`.
    pub fn with(mut self, options: &[&str]) -> DataSet {
        self.options.extend(options.iter().map(|s| s.to_string()));
        self
    }

    /// The same data set after `delete --every every`.
    pub fn after_delete(&self, every: u32) -> DataSet {
        self.clone().with(&["--deleted-every", &every.to_string()])
    }

    /// The same DB and options with `count` keys (`0`: an empty DB with
    /// this data set's flags).
    pub fn with_count(&self, count: u32) -> DataSet {
        DataSet {
            count,
            ..self.clone()
        }
    }

    /// `<db> <count> <seed> <options...>`.
    pub fn args(&self) -> Vec<String> {
        let mut v = vec![
            self.db.clone(),
            self.count.to_string(),
            self.seed.to_string(),
        ];
        v.extend(self.options.iter().cloned());
        v
    }

    /// `fill` with `peer`, plus fill-only options.
    pub fn fill(&self, peer: Peer, env: &TestEnv, extra: &[&str]) -> Result<WriteReport> {
        let out = peer.on("fill", env).args(self.args()).args(extra).run()?;
        WriteReport::parse(&out.stdout)
    }

    /// `verify` with `peer`.
    pub fn verify(&self, peer: Peer, env: &TestEnv) -> Result<VerifyReport> {
        let out = peer.on("verify", env).args(self.args()).run()?;
        VerifyReport::parse(&out.stdout)
    }

    /// `delete --every every` with `peer`, plus options.
    pub fn delete(
        &self,
        peer: Peer,
        env: &TestEnv,
        every: u32,
        extra: &[&str],
    ) -> Result<WriteReport> {
        let out = peer
            .on("delete", env)
            .args(self.args())
            .args(["--every", &every.to_string()])
            .args(extra)
            .run()?;
        WriteReport::parse(&out.stdout)
    }
}

// ---- Helpers -------------------------------------------------------------------

/// `create` with `peer`; logs the page size.
pub fn create(peer: Peer, env: &TestEnv, mapsize: u64) -> Result<CreateReport> {
    let out = peer
        .on("create", env)
        .args([mapsize.to_string(), "64".to_owned()])
        .run()?;
    let r = CreateReport::parse(&out.stdout)?;
    log(format_args!(
        "{peer} created {} with page size {} (map size {})",
        env.path().display(),
        r.psize,
        r.mapsize
    ));
    Ok(r)
}

/// A new environment in the pair's format, created by the pair's writer.
fn new_env(pair: &Pair, mapsize: u64) -> Result<TestEnv> {
    let env = pair.env()?;
    create(pair.writer, &env, mapsize)?;
    Ok(env)
}

/// `stat` with `peer`.
pub fn stat(peer: Peer, env: &TestEnv) -> Result<StatReport> {
    StatReport::parse(&peer.on("stat", env).run()?.stdout)
}

/// `readers` with `peer`.
pub fn readers(peer: Peer, env: &TestEnv) -> Result<ReadersReport> {
    ReadersReport::parse(&peer.on("readers", env).run()?.stdout)
}

/// `dump --all` with `peer`.
pub fn dump_all(peer: Peer, env: &TestEnv) -> Result<String> {
    Ok(peer.on("dump", env).arg("--all").run()?.stdout)
}

/// `reader-check` with `peer`: how many stale reader slots it cleared.
pub fn reader_check(peer: Peer, env: &TestEnv) -> Result<u64> {
    let out = peer.on("reader-check", env).run()?;
    crate::report::Kv::parse(&out.stdout).num("cleared")
}

/// `check-freelist` with `peer`.
pub fn check_freelist(peer: Peer, env: &TestEnv) -> Result<FreelistReport> {
    let out = peer.on("check-freelist", env).run()?;
    FreelistReport::parse(&out.stdout).map_err(|e| e.context(format!("`{}`", out.label)))
}

/// `check-freelist` with every auditor of the pair; they must agree. Also
/// checks that `stat` (LMDB's own counters) matches the page walk. Returns
/// the C report.
pub fn audit(pair: &Pair, env: &TestEnv) -> Result<FreelistReport> {
    let mut reports = Vec::new();
    for peer in pair.auditors() {
        reports.push((peer, check_freelist(peer, env)?));
    }
    let (_, c) = &reports[0];
    for (peer, r) in &reports[1..] {
        ensure!(
            (
                r.txnid,
                r.last_pgno,
                r.pages_free,
                r.pages_freedb,
                r.pages_tree
            ) == (
                c.txnid,
                c.last_pgno,
                c.pages_free,
                c.pages_freedb,
                c.pages_tree
            ) && r.dbs == c.dbs,
            "check-freelist of {peer} disagrees with C:\n{r:#?}\nC: {c:#?}"
        );
    }
    ensure!(c.pages_leaked == 0, "leaked pages: {c:#?}");
    let st = stat(pair.reader, env)?;
    ensure!(
        st.free_pages == c.pages_free,
        "stat free_pages {} != check-freelist {}",
        st.free_pages,
        c.pages_free
    );
    for walked in c.dbs.iter() {
        let s = st.db(&walked.name)?;
        ensure!(
            (
                s.depth,
                s.branch_pages,
                s.leaf_pages,
                s.overflow_pages,
                s.entries
            ) == (
                walked.depth,
                walked.branch_pages,
                walked.leaf_pages,
                walked.overflow_pages,
                walked.entries
            ),
            "stat of {} {s:?} differs from the page walk {walked:?}",
            walked.name
        );
    }
    log(format_args!(
        "audit {}: txnid {} pages {} = 2 meta + {} free DB + {} free + {} tree",
        env.path().display(),
        c.txnid,
        c.pages_total,
        c.pages_freedb,
        c.pages_free,
        c.pages_tree
    ));
    Ok(reports.swap_remove(0).1)
}

/// `drop <db>` (`--delete` if `delete`) with `peer`: `dropped db=..
/// delete=.. entries=.. pages=..`.
pub fn drop_db(peer: Peer, env: &TestEnv, db: &str, delete: bool) -> Result<crate::report::Kv> {
    let mut cmd = peer.on("drop", env).arg(db);
    if delete {
        cmd = cmd.arg("--delete");
    }
    Ok(crate::report::Kv::parse(&cmd.run()?.stdout))
}

/// `ds` verifies with every peer of `peers`.
fn verify_by(peers: &[Peer], env: &TestEnv, ds: &DataSet) -> Result<()> {
    for peer in peers {
        ds.verify(*peer, env)?;
    }
    Ok(())
}

/// Writer fills `ds`; the writer, the reader and C verify it.
fn fill_verify(
    pair: &Pair,
    env: &TestEnv,
    ds: &DataSet,
    fill_extra: &[&str],
) -> Result<WriteReport> {
    let filled = ds.fill(pair.writer, env, fill_extra)?;
    for peer in pair.verifiers() {
        let v = ds.verify(peer, env)?;
        ensure!(
            v.entries == filled.entries,
            "{peer} verified {} entries of {}, fill reported {}",
            v.entries,
            ds.db,
            filled.entries
        );
    }
    Ok(filled)
}

/// Both peers print the same `dump --all`; returns it.
fn same_dump(pair: &Pair, env: &TestEnv) -> Result<String> {
    let w = dump_all(pair.writer, env)?;
    let r = dump_all(pair.reader, env)?;
    ensure!(
        w == r,
        "dump --all differs between {} and {}",
        pair.writer,
        pair.reader
    );
    Ok(w)
}

fn subdbs(report: &FreelistReport, db: &str) -> Result<u64> {
    report
        .db(db)?
        .subdbs
        .ok_or_else(|| Error::new("check-freelist line without subdbs="))
}

// ---- Scenarios ---------------------------------------------------------------------

/// A new environment: page size, meta pages, lock file size, empty stats,
/// as seen by the reader and by `mdb_stat -e`.
pub fn create_stat(pair: Pair) -> Result<()> {
    let env = pair.env()?;
    let c = create(pair.writer, &env, 64 * MIB)?;
    let layout = tools::c_layout_for(pair.format)?;
    ensure!(c.last_txnid == 0, "new env has txnid {}", c.last_txnid);
    if let Some(ps) = pair.page_size {
        ensure!(c.psize == ps, "page size {}, asked for {ps}", c.psize);
    }
    // 511 in 0.9; in 1.0 it depends on the page size, as `layout_dump10`
    // measures for some (`me_nodemax - 56`, DUAL_FORMAT.md §1, for others).
    let maxkeysize = match (pair.format, pair.page_size) {
        (Format::V09, _) => 511,
        (Format::V10, None) => layout.get("mdb_env_get_maxkeysize(os_page_size)")? as u32,
        (Format::V10, Some(ps)) => layout
            .get(&format!("mdb_env_get_maxkeysize({ps})"))
            .map_or((((ps - 24) / 2) & !1) - 2 - 56, |k| k as u32),
    };
    ensure!(c.maxkeysize == maxkeysize, "maxkeysize {}", c.maxkeysize);
    ensure!(c.mapsize == 64 * MIB, "mapsize {}", c.mapsize);

    // A new data file is exactly the two meta pages.
    let data_len = std::fs::metadata(env.data_file())?.len();
    ensure!(
        data_len == 2 * u64::from(c.psize),
        "new data file is {data_len} bytes, psize {}",
        c.psize
    );
    // The lock file is the reader table: header + maxreaders slots.
    let lock_len = std::fs::metadata(env.lock_file())?.len();
    let want = layout.get("offsetof(MDB_txninfo.mti_readers)")?
        + 126 * layout.get("sizeof(MDB_reader)")?;
    ensure!(
        lock_len == want,
        "lock file is {lock_len} bytes, expected {want}"
    );

    for peer in pair.peers() {
        let st = stat(peer, &env)?;
        ensure!(st.psize == c.psize, "{peer}: psize {}", st.psize);
        ensure!(
            (st.last_pgno, st.last_txnid, st.maxreaders, st.free_pages) == (1, 0, 126, 0),
            "{peer}: unexpected stat of an empty env: {st:?}"
        );
        ensure!(
            st.maxkeysize == maxkeysize,
            "{peer}: maxkeysize {}",
            st.maxkeysize
        );
        let main = st.db("-")?;
        ensure!(
            main.entries == 0 && main.depth == 0 && main.flags == Some(0),
            "{peer}: main DB {main:?}"
        );
    }
    let ms = tools::mdb_stat(&env, &["-e"])?;
    let e = ms.env.as_ref().ok_or_else(|| Error::new("no env info"))?;
    ensure!(
        (
            e.page_size,
            e.pages_used,
            e.last_txnid,
            e.map_size,
            e.max_readers
        ) == (c.psize, 2, 0, 64 * MIB, 126),
        "mdb_stat -e: {e:?}"
    );
    ensure!(
        ms.db(None).map(|d| d.entries) == Some(0),
        "mdb_stat main DB: {:?}",
        ms.dbs
    );
    Ok(())
}

/// The main DB, filled in several transactions.
pub fn fill_verify_plain(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let ds = DataSet::new("-", 3000, 11);
    let filled = fill_verify(&pair, &env, &ds, &["--per-txn", "700"])?;
    ensure!(filled.txns == 5, "fill used {} txns", filled.txns);
    audit(&pair, &env)?;
    Ok(())
}

/// Several named DBs of different kinds in one environment; both peers
/// print the same dump and stats.
pub fn fill_verify_named(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        DataSet::new("plain", 1500, 1),
        DataSet::new("dups", 800, 2).with(&["--dupsort", "--dups", "6"]),
        DataSet::new("big", 60, 3).with(&["--big", "40000"]),
        DataSet::new("ik", 500, 4).with(&["--intkey"]),
    ];
    for ds in &sets {
        fill_verify(&pair, &env, ds, &[])?;
    }
    same_dump(&pair, &env)?;
    let w = stat(pair.writer, &env)?;
    let r = stat(pair.reader, &env)?;
    ensure!(w.dbs == r.dbs, "stat differs:\n{w:#?}\n{r:#?}");
    ensure!(
        r.db("-")?.entries == 4,
        "main DB holds {} records",
        r.db("-")?.entries
    );
    audit(&pair, &env)?;
    Ok(())
}

/// `MDB_INTEGERKEY` (8-byte keys), with and without overflow values.
pub fn fill_verify_intkey(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ik", 4000, 5).with(&["--intkey"]),
        &["--per-txn", "1000"],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ik_big", 90, 6).with(&["--intkey", "--big", "30000"]),
        &[],
    )?;
    audit(&pair, &env)?;
    Ok(())
}

/// `MDB_REVERSEKEY`, also written with `MDB_RESERVE`.
pub fn fill_verify_revkey(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("rk", 3000, 7).with(&["--revkey"]),
        &[],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("rk_res", 500, 8).with(&["--revkey", "--valsize", "200"]),
        &["--reserve"],
    )?;
    audit(&pair, &env)?;
    Ok(())
}

/// `MDB_DUPSORT`: values in sub-pages, values in sub-DBs, `MDB_REVERSEDUP`.
pub fn fill_verify_dupsort(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ds", 1500, 9).with(&["--dupsort", "--dups", "8"]),
        &["--per-txn", "400"],
    )?;
    // Up to 1500 values of up to 40 bytes per key: sub-DBs.
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ds_sub", 30, 10).with(&["--dupsort", "--dups", "1500", "--valsize", "40"]),
        &[],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ds_rev", 300, 11).with(&["--revdup", "--dups", "10"]),
        &[],
    )?;
    let a = audit(&pair, &env)?;
    ensure!(subdbs(&a, "ds_sub")? > 0, "ds_sub has no sub-DBs");
    ensure!(a.db("ds")?.subpages.unwrap_or(0) > 0, "ds has no sub-pages");
    Ok(())
}

/// `MDB_DUPFIXED`: `P_LEAF2` sub-pages and sub-DBs, `MDB_GET_MULTIPLE`.
pub fn fill_verify_dupfixed(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let dups = pair.subdb_dups(2000, 16).to_string();
    fill_verify(
        &pair,
        &env,
        &DataSet::new("df", 40, 12).with(&["--dupfixed", "--dups", &dups, "--valsize", "16"]),
        &[],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("df_small", 500, 13).with(&["--dupfixed", "--dups", "5", "--valsize", "8"]),
        &[],
    )?;
    let a = audit(&pair, &env)?;
    ensure!(subdbs(&a, "df")? > 0, "df has no sub-DBs");
    Ok(())
}

/// `MDB_INTEGERDUP` with 8-byte and 4-byte values.
pub fn fill_verify_intdup(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("id8", 60, 14).with(&["--intdup", "--dups", "1500"]),
        &[],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("id4", 300, 15).with(&["--intdup", "--valsize", "4", "--dups", "20"]),
        &[],
    )?;
    audit(&pair, &env)?;
    Ok(())
}

/// Values on overflow pages: one-page, many-page, written with `MDB_RESERVE`.
pub fn fill_verify_overflow(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 512 * MIB)?;
    let big = pair.overflowing(20000).to_string();
    let f = fill_verify(
        &pair,
        &env,
        &DataSet::new("ov", 300, 16).with(&["--big", &big]),
        &["--per-txn", "50"],
    )?;
    ensure!(
        f.kv.num::<u64>("overflow_pages")? > 0,
        "no overflow pages: {:?}",
        f.kv
    );
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ov_huge", 12, 17).with(&["--big", "300000"]),
        &[],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ov_res", 100, 18).with(&["--big", "9000"]),
        &["--reserve"],
    )?;
    audit(&pair, &env)?;
    Ok(())
}

/// Sorted bulk loads with `MDB_APPEND` / `MDB_APPENDDUP`.
pub fn fill_verify_append(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ap", 6000, 19),
        &["--append", "--per-txn", "2000"],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ap_dups", 400, 20).with(&["--dupsort", "--dups", "20"]),
        &["--append"],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ap_ik", 3000, 21).with(&["--intkey"]),
        &["--append"],
    )?;
    fill_verify(
        &pair,
        &env,
        &DataSet::new("ap_df", 50, 22).with(&["--dupfixed", "--dups", "800"]),
        &["--append"],
    )?;
    audit(&pair, &env)?;
    Ok(())
}

/// Deleting keys, single duplicates and overflow values, then everything.
pub fn delete_verify(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        (DataSet::new("del", 3000, 23), 3),
        (
            DataSet::new("deld", 600, 24).with(&["--dupsort", "--dups", "30"]),
            4,
        ),
        (DataSet::new("delbig", 200, 25).with(&["--big", "20000"]), 2),
        (
            DataSet::new("deldf", 40, 26).with(&["--dupfixed", "--dups", "1500"]),
            3,
        ),
    ];
    for (ds, every) in &sets {
        ds.fill(pair.writer, &env, &[])?;
        ds.delete(pair.writer, &env, *every, &["--per-txn", "100"])?;
        for peer in pair.peers() {
            ds.after_delete(*every).verify(peer, &env)?;
        }
    }
    audit(&pair, &env)?;

    // Delete what is left of "del": an empty but existing DB.
    let (del, every) = &sets[0];
    del.after_delete(*every).delete(pair.writer, &env, 1, &[])?;
    for peer in pair.peers() {
        let v = del.after_delete(1).verify(peer, &env)?;
        ensure!(v.entries == 0 && v.absent == 3000, "{peer}: {v:?}");
    }
    let a = audit(&pair, &env)?;
    ensure!(a.pages_free > 0, "deleting freed no pages");
    Ok(())
}

/// `copy` and `copy --compact` by the peers and by `mdb_copy [-c]`: same
/// content, valid page usage, compacted copies without free pages.
pub fn copy_dump_equal(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        DataSet::new("plain", 2000, 27),
        DataSet::new("dups", 300, 28).with(&["--dupsort", "--dups", "40"]),
        DataSet::new("big", 80, 29).with(&["--big", "25000"]),
    ];
    for ds in &sets {
        ds.fill(pair.writer, &env, &[])?;
        ds.delete(pair.writer, &env, 5, &[])?; // leave free pages behind
    }
    let original = dump_all(pair.reader, &env)?;
    let orig_audit = audit(&pair, &env)?;
    let orig_tool_dump = tools::mdb_dump(&env, &["-a"])?;

    let copy = env.derived("copy");
    pair.writer.on("copy", &env).arg(copy.path()).run()?;
    let compact = env.derived("compact");
    pair.capable(pair.reader, "copy")
        .on("copy", &env)
        .arg(compact.path())
        .arg("--compact")
        .run()?;
    let tcopy = env.derived("tcopy");
    tools::mdb_copy(&env, &tcopy, false)?;
    let tcompact = env.derived("tcompact");
    tools::mdb_copy(&env, &tcompact, true)?;

    for (target, compacted) in [
        (&copy, false),
        (&compact, true),
        (&tcopy, false),
        (&tcompact, true),
    ] {
        let what = target.path().display();
        ensure!(
            dump_all(pair.reader, target)? == original,
            "dump of {what} differs"
        );
        let tool_dump = tools::mdb_dump(target, &["-a"])?;
        ensure!(
            tool_dump
                .iter()
                .map(|d| (&d.name, &d.records))
                .eq(orig_tool_dump.iter().map(|d| (&d.name, &d.records))),
            "mdb_dump -a of {what} differs"
        );
        let a = audit(&pair, target)?;
        if compacted {
            ensure!(
                a.pages_free == 0 && a.pages_freedb == 0 && a.txnid == 1,
                "compacted copy {what}: {a:?}"
            );
            ensure!(
                a.last_pgno < orig_audit.last_pgno,
                "compaction did not shrink {what}"
            );
        } else {
            ensure!(
                (a.txnid, a.last_pgno, a.pages_free)
                    == (
                        orig_audit.txnid,
                        orig_audit.last_pgno,
                        orig_audit.pages_free
                    ),
                "plain copy {what}: {a:?}"
            );
        }
        for ds in &sets {
            ds.after_delete(5).verify(pair.reader, target)?;
        }
    }
    Ok(())
}

/// MVCC across processes: a reader pins a snapshot, the writer deletes and
/// rewrites the data and commits many transactions; the pinned snapshot
/// still shows the old data, and its pages were not reused.
pub fn mvcc_hold_reader(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 1024 * MIB)?;
    let old = DataSet::new("data", 2000, 1);
    old.fill(pair.writer, &env, &[])?;

    let mut holder = pair
        .reader
        .on("hold-reader", &env)
        .args(["60", "--until-stdin"])
        .args(old.args())
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;

    old.delete(pair.writer, &env, 1, &[])?;
    let new = DataSet::new("data", 2500, 2);
    new.fill(pair.writer, &env, &["--per-txn", "500"])?;
    let wl = pair
        .writer
        .on("writer-loop", &env)
        .args([
            "loop",
            &loop_secs().to_string(),
            "3",
            "--nosync",
            "--max-commits",
            "2000",
        ])
        .run()?;
    let wl = WriterLoopReport::parse(&wl.stdout)?;
    log(format_args!(
        "writer committed {} txns past the held snapshot {}",
        wl.commits, ready.txnid
    ));

    let table = readers(pair.writer, &env)?;
    let slots = table.of_pid(holder.pid());
    ensure!(
        slots.iter().any(|s| s.txnid == Some(ready.txnid)),
        "reader table does not show holder pid {} at txnid {}: {table:?}",
        holder.pid(),
        ready.txnid
    );
    new.verify(pair.writer, &env)?;
    // The free list is consistent while the snapshot is pinned, too.
    audit(&pair, &env)?;

    holder.release()?;
    let out = holder.finish()?.ok()?;
    ensure!(
        out.stdout.contains("verified db=data keys=2000"),
        "held snapshot did not verify the old data:\n{out}"
    );
    new.verify(pair.reader, &env)?;
    audit(&pair, &env)?;
    Ok(())
}

/// Two writer loops (one per peer of the pair, one synced and one
/// `--nosync`) and reader loops of all three peers run concurrently on one
/// environment. Writers must exclude each other (no lost updates of
/// `#commits`), readers must always see consistent snapshots.
pub fn concurrent_writers_readers(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 512 * MIB)?;
    let secs = loop_secs();
    pair.writer
        .on("writer-loop", &env)
        .args(["loop", "0", "1"])
        .run()?;

    let reader_secs = (secs + 0.5).to_string();
    let mut rl = Vec::new();
    // Reader loops of all three peers: the pair's two and the other one.
    let mut loopers = pair.peers();
    loopers.extend(pair.others());
    for peer in loopers {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["loop", reader_secs.as_str()])
            .spawn()?;
        p.wait_ready()?;
        rl.push((peer, p));
    }
    let w1 = pair
        .writer
        .on("writer-loop", &env)
        .args(["loop", &secs.to_string(), "11", "--nosync"])
        .spawn()?;
    let w2 = pair
        .capable(pair.reader, "writer-loop")
        .on("writer-loop", &env)
        .args(["loop", &secs.to_string(), "12"])
        .spawn()?;
    let w1 = WriterLoopReport::parse(&w1.finish()?.ok()?.stdout)?;
    let w2 = WriterLoopReport::parse(&w2.finish()?.ok()?.stdout)?;
    for (peer, p) in rl {
        finish_reader_loop(peer, p)?;
    }
    log(format_args!("writers: {w1:?} {w2:?}"));
    ensure!(w1.commits > 0 && w2.commits > 0, "a writer did not commit");
    // 1 commit by the initial loop, then every commit of both writers.
    let last = w1.last_commit.max(w2.last_commit);
    ensure!(
        last == 1 + w1.commits + w2.commits,
        "#commits is {last}, but the writers committed 1 + {} + {}",
        w1.commits,
        w2.commits
    );
    for peer in pair.peers() {
        peer.on("reader-loop", &env).args(["loop", "0"]).run()?;
    }
    audit(&pair, &env)?;
    Ok(())
}

/// After many commits (with nested transactions and aborts), the free list
/// accounts for every page, and `mdb_stat -ff` agrees with the walk.
pub fn freelist_after_writer_loop(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 512 * MIB)?;
    let secs = loop_secs().to_string();
    pair.writer
        .on("writer-loop", &env)
        .args(["fl", secs.as_str(), "7", "--nosync", "--keys", "3000"])
        .run()?;
    pair.writer
        .on("writer-loop", &env)
        .args(["fl", "0.3", "8"])
        .run()?;
    let a = audit(&pair, &env)?;
    ensure!(a.pages_free > 0, "no free pages after a writer loop: {a:?}");
    let ms = tools::mdb_stat(&env, &["-ff"])?;
    let fl = ms
        .freelist
        .ok_or_else(|| Error::new("mdb_stat -ff printed no freelist"))?;
    ensure!(
        fl.free_pages == a.pages_free,
        "mdb_stat free pages {} != walk {}",
        fl.free_pages,
        a.pages_free
    );
    ensure!(
        fl.transactions.iter().map(|t| t.1).sum::<u64>() == fl.free_pages,
        "mdb_stat -ff transactions do not add up: {fl:?}"
    );
    ensure!(
        fl.stat.entries == a.db("@free")?.entries,
        "mdb_stat free DB entries {} != walk {}",
        fl.stat.entries,
        a.db("@free")?.entries
    );
    for peer in pair.peers() {
        peer.on("reader-loop", &env).args(["fl", "0"]).run()?;
    }
    Ok(())
}

/// Nested transactions (see `nested-test` in README.md), twice.
pub fn nested_txns(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    for _ in 0..2 {
        pair.writer.on("nested-test", &env).run()?;
        for peer in pair.peers() {
            peer.on("nested-test", &env).arg("--check").run()?;
        }
        audit(&pair, &env)?;
    }
    Ok(())
}

/// `MDB_NOSUBDIR`: data file and `-lock` file side by side; copies too.
pub fn nosubdir(pair: Pair) -> Result<()> {
    let env = pair.configure(TestEnv::new_nosubdir()?);
    create(pair.writer, &env, 64 * MIB)?;
    ensure!(
        env.data_file().is_file(),
        "no data file {}",
        env.data_file().display()
    );
    ensure!(
        env.lock_file().is_file(),
        "no lock file {}",
        env.lock_file().display()
    );
    // (`verify -` would count the named DB's record in the main DB.)
    let main = DataSet::new("plain", 1000, 31);
    let named = DataSet::new("named", 200, 32).with(&["--dupsort"]);
    fill_verify(&pair, &env, &main, &[])?;
    fill_verify(&pair, &env, &named, &[])?;
    audit(&pair, &env)?;
    let copy = env.derived("copy.mdb");
    pair.writer
        .on("copy", &env)
        .arg(copy.path())
        .arg("--compact")
        .run()?;
    ensure!(copy.data_file().is_file(), "copy is not a file");
    main.verify(pair.reader, &copy)?;
    named.verify(pair.reader, &copy)?;
    audit(&pair, &copy)?;
    let ms = tools::mdb_stat(&env, &["-e", "-a"])?;
    ensure!(
        ms.db(Some("named")).map(|d| d.entries)
            == Some(stat(pair.reader, &env)?.db("named")?.entries),
        "mdb_stat -n -a: {:?}",
        ms.dbs
    );
    Ok(())
}

/// `mdb_dump -a` of the writer's data, `mdb_load` into a new environment:
/// the reader verifies the loaded data and sees the same dump.
///
/// Upstream bug: `mdb_load` loses the flags (`dupsort=1`, …) of the *first*
/// database in its input: `readhdr()` parses them before the load loop,
/// whose first statement is `flags = 0;` (since ITS#8662, 2017; C 1.0's
/// `mdb_load` too, `KNOWN_C_BUGS.md` #1). So the DB that sorts first here,
/// `a_plain`, has no flags. The tools are those of the pair's release.
pub fn dump_load_roundtrip(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        DataSet::new("a_plain", 400, 33),
        DataSet::new("dups", 150, 34).with(&["--dupsort", "--dups", "7"]),
        DataSet::new("ik", 300, 35).with(&["--intkey"]),
        DataSet::new("fx", 40, 36).with(&["--dupfixed", "--dups", "50", "--valsize", "8"]),
    ];
    for ds in &sets {
        ds.fill(pair.writer, &env, &[])?;
    }
    let text = tools::mdb_dump_text(&env, &["-a"])?;
    let loaded = env.derived("loaded");
    tools::mdb_load(&loaded, &[], &text)?;
    for ds in &sets {
        ds.verify(pair.reader, &loaded)?;
    }
    ensure!(
        dump_all(pair.reader, &loaded)? == dump_all(pair.reader, &env)?,
        "loaded environment dumps differently"
    );
    audit(&pair, &loaded)?;
    Ok(())
}

/// The databases written by the upstream `mtest*` programs (random data,
/// `MDB_FIXEDMAP`, named and DUPSORT DBs, cursor deletes) are valid for the
/// reader, and it dumps them like C does. For 1.0 these are C 1.0's
/// `mtest`…`mtest5` (1 KiB pages; not `mtest_enc*`, whose files are
/// encrypted, nor `mtest_remap`).
pub fn mtest_databases(pair: Pair) -> Result<()> {
    for name in bins::MTESTS {
        let env = pair.configure(TestEnv::with_name("testdb")?);
        tools::run_mtest_in(pair.format, name, env.root())?;
        let a = audit(&pair, &env)?;
        ensure!(
            dump_all(pair.reader, &env)? == dump_all(pair.reference(), &env)?,
            "{name}: dump differs between {} and {}",
            pair.reader,
            pair.reference()
        );
        log(format_args!(
            "{name}: {} pages, {} trees",
            a.pages_total,
            a.dbs.len()
        ));
    }
    Ok(())
}

/// The data generator is the same in both peers (`gen`), and matches the
/// pinned vectors of README.md.
pub fn gen_vectors(pair: Pair) -> Result<()> {
    for (args, pinned) in GEN_VECTORS {
        let mut outs: Vec<Output> = Vec::new();
        for peer in pair.peers() {
            outs.push(peer.cmd("gen").args(args.split_whitespace()).run()?);
        }
        for out in &outs {
            ensure!(
                out.stdout == *pinned,
                "`{}` printed\n{}expected\n{pinned}",
                out.label,
                out.stdout
            );
        }
    }
    for flags in [
        "",
        "--intkey",
        "--revkey --valsize 7",
        "--dupsort --dups 9 --valsize 30",
        "--dupfixed --dups 4",
        "--intdup --dups 6",
        "--intdup --dups 6 --valsize 4",
        "--big 70",
    ] {
        for i in [0u32, 1, 2, 3, 99, 4_000_000_000] {
            let args = format!("123456 {i} {flags}");
            let outs = pair
                .peers()
                .into_iter()
                .map(|p| {
                    p.cmd("gen")
                        .args(args.split_whitespace())
                        .run()
                        .map(|o| o.stdout)
                })
                .collect::<Result<Vec<_>>>()?;
            if outs.iter().any(|o| *o != outs[0]) {
                bail!("gen {args} differs between peers: {outs:?}");
            }
        }
    }
    Ok(())
}

/// `gen` arguments and the exact output (README.md, "Test vectors").
pub const GEN_VECTORS: &[(&str, &str)] = &[
    (
        "1 0",
        "key=39326361326630652e30\nndups=1\nvalue0=514e28b7f465b9a16a9e786e4f450980185dc406ec814c72a8b88bf89b74a851\n",
    ),
    (
        "7 42",
        "key=32316132376264622e3432\nndups=1\nvalue0=d2b5b8bf5eaa37a60e00deee47a807cd8b85a4e27eb43f6365c6214b7b7f5b3c\n",
    ),
    (
        "1 3 --intkey",
        "key=03000000d3de81ed\nndups=1\nvalue0=971e996436a3a7374ab28f4c85fc2539b91560a4940e827d6e0ac4326c88da86\n",
    ),
    (
        "9 7 --dupfixed --dups 3 --valsize 12",
        "key=34633335623464652e37\nndups=3\nvalue0=533a22b44701441bf41bcea5\nvalue1=533a22b5970b438dd36bef5d\nvalue2=533a22b65d1ee65a0c759f65\n",
    ),
    (
        "3 4 --intdup --dups 3",
        "key=31336463313039642e34\nndups=3\nvalue0=000000005e89d359\nvalue1=010000005f89d359\nvalue2=020000005c89d359\n",
    ),
    (
        "2 6 --big 40 --valsize 10",
        "key=66316536393130372e36\nndups=1\nvalue0=b74b1b2a890bc28ddb6257896e9c479884131aa5a2794e188c9ac940122d151471c04c2b03245b8b\n",
    ),
];

// ---- The scenario list -------------------------------------------------------------

/// Calls `$callback!` with `$args` and the names of all scenarios. The one
/// list of scenarios: add new ones here.
#[doc(hidden)]
#[macro_export]
macro_rules! __scenario_list {
    ($callback:ident ! ( $($args:tt)* )) => {
        $crate::$callback! { $($args)*
            create_stat
            fill_verify_plain
            fill_verify_named
            fill_verify_intkey
            fill_verify_revkey
            fill_verify_dupsort
            fill_verify_dupfixed
            fill_verify_intdup
            fill_verify_overflow
            fill_verify_append
            delete_verify
            alternating_writers
            delete_heavy
            drop_clear_dbs
            copy_cross
            map_full
            killed_writer
            same_writes_same_pages
            copy_dump_equal
            mvcc_hold_reader
            concurrent_writers_readers
            freelist_after_writer_loop
            readers_list
            nested_txns
            nosubdir
            dump_load_roundtrip
            mtest_databases
            gen_vectors
            stale_reader_cleared_by_reader
            stale_reader_cleared_by_mdb_stat
            reader_creates_lock_file
            reader_loop_during_nested
            notls_readers
        }
    };
}

/// Instantiate every scenario as a `#[test]` for one (writer, reader) pair,
/// on LMDB 0.9 environments unless a format is given, and for LMDB 1.0
/// optionally with a page size:
///
/// ```ignore
/// interop::scenario_tests!(interop::Peer::C, interop::Peer::Rust);
/// interop::scenario_tests!(interop::Peer::C10, interop::Peer::Rust, interop::Format::V10);
/// interop::scenario_tests!(interop::Peer::C10, interop::Peer::Rust, interop::Format::V10, 1024);
/// ```
#[macro_export]
macro_rules! scenario_tests {
    ($writer:expr, $reader:expr $(,)?) => {
        $crate::__scenario_list!(__scenario_tests_impl!(
            ($crate::scenarios::Pair::new($writer, $reader))
        ));
    };
    ($writer:expr, $reader:expr, $format:expr $(,)?) => {
        $crate::__scenario_list!(__scenario_tests_impl!(
            ($crate::scenarios::Pair::with_format($writer, $reader, $format))
        ));
    };
    ($writer:expr, $reader:expr, $format:expr, $page_size:expr $(,)?) => {
        $crate::__scenario_list!(__scenario_tests_impl!(
            ($crate::scenarios::Pair::with_format($writer, $reader, $format)
                .with_page_size($page_size))
        ));
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __scenario_tests_impl {
    (($pair:expr) $($name:ident)*) => {
        $(
            #[test]
            fn $name() {
                $crate::register_rpeer(option_env!("CARGO_BIN_EXE_rpeer"));
                let pair = $pair;
                match $crate::scenarios::$name(pair) {
                    Ok(()) => {}
                    // A peer lacks a command the scenario cannot do without.
                    Err(e) if e.is_not_implemented() => $crate::scenarios::skipped(format_args!(
                        "scenario {} ({}): {}",
                        stringify!($name),
                        pair,
                        e.message().lines().next().unwrap_or_default()
                    )),
                    Err(e) => panic!("scenario {} ({}) failed: {}", stringify!($name), pair, e),
                }
            }
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __scenario_table {
    (() $($name:ident)*) => {
        /// Every scenario by name, for tools that want to run them by hand.
        pub const ALL: &[(&str, fn(Pair) -> Result<()>)] = &[
            $((stringify!($name), $name as fn(Pair) -> Result<()>)),*
        ];
    };
}

crate::__scenario_list!(__scenario_table!(()));
