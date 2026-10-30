//! The lock-step differential harness of the write path (used by
//! `rlmdb/tests/write_differential.rs` against C LMDB 0.9, and included by
//! `interop/tests/write_differential10.rs` against C LMDB 1.0).
//!
//! The same script of puts, deletes, cursor moves, nested transactions and
//! drops on several cursors of one write transaction runs in C
//! (`tests/c/writeops.c`, built against the oracle's library) and in Rust
//! (the handle-based cursor API), each in a fresh environment of the
//! oracle's format and page size. After every step the position of every
//! cursor must agree, which checks the cursor fixups after splits, merges,
//! rebalances, sub-page growth and sub-DB promotion; the committed
//! databases must have the same contents and pass the C page audit, and the
//! oracle may check more (the 1.0 one compares the files page by page).
//!
//! This file is self-contained (no `common` module) so that another crate
//! can include it with `#[path]`.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use rlmdb::consts::*;
use rlmdb::{
    CursorId, CursorOp, DatabaseFlags, Environment, EnvironmentFlags, Error, Format, RwTxn,
    Transaction, WriteFlags,
};

/// The C side of a differential round: which `writeops` to run, the format
/// and page size of the environments, and the checks of the results.
pub trait Oracle {
    /// The C `writeops` program, built against the oracle's library.
    fn writeops(&self) -> &Path;
    /// The data-file format both sides create.
    fn format(&self) -> Format;
    /// The page size of the new environments (`None`: the default).
    fn page_size(&self) -> Option<usize>;
    /// A dirty-page limit for the Rust side (`Environment::set_dirty_limit`),
    /// so that its transactions spill where C's do not (`None`: C's limit).
    /// Spilling must not change what is written.
    fn dirty_limit(&self) -> Option<usize> {
        None
    }
    /// Whether the C tools can open an environment the Rust side still
    /// holds open. The 1.0 oracle cannot until rlmdb writes 1.0's lock
    /// file (`DUAL_FORMAT.md` stage 3): the Rust side closes first.
    fn shares_open_environments(&self) -> bool {
        true
    }
    /// The contents of database `db` in `env` as the C tools see them (for
    /// example the data part of `mdb_dump -s <db>`), to compare.
    fn dump(&self, env: &Path, db: &str) -> String;
    /// The C page audit (`cpeer check-freelist`) of `env`: exit code,
    /// stdout, stderr.
    fn check_freelist(&self, env: &Path) -> (i32, String, String);
    /// Page counts and free pages of `env` (`mdb_stat -a -f`), in a form
    /// that can be compared between the two environments.
    fn page_usage(&self, env: &Path) -> String;
    /// More checks of a round that agreed in full: `c_env` written by C,
    /// `rust_env` by Rust.
    fn after_round(&self, _c_env: &Path, _rust_env: &Path, _tag: &str) {}
}

/// Lower-case hex of `b` (the script's encoding of keys and data).
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A small xorshift generator (the scripts are reproducible from a seed).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn code<T>(r: &Result<T, Error>) -> i32 {
    match r {
        Ok(_) => 0,
        Err(e) => e.to_err_code(),
    }
}

/// One step of a script on the Rust side; output formatted like
/// `writeops.c`.
fn rust_step(
    txn: &mut RwTxn<'_>,
    db: rlmdb::Dbi,
    cur: &[CursorId],
    op: &str,
    dupsort: bool,
) -> String {
    let f: Vec<&str> = op.split(':').collect();
    let rc = match f[0] {
        "p" => code(&txn.put(
            db,
            &unhex(f[1]),
            &unhex(f[2]),
            WriteFlags::from_bits_retain(f[3].parse().unwrap()),
        )),
        "cp" => code(&txn.cursor_put(
            cur[f[1].parse::<usize>().unwrap()],
            &unhex(f[2]),
            &unhex(f[3]),
            WriteFlags::from_bits_retain(f[4].parse().unwrap()),
        )),
        "d" => {
            let data = f.get(2).map(|d| unhex(d));
            match txn.del(db, &unhex(f[1]), data.as_deref()) {
                Ok(true) => 0,
                Ok(false) => MDB_NOTFOUND,
                Err(e) => e.to_err_code(),
            }
        }
        "cd" => code(&txn.cursor_del(
            cur[f[1].parse::<usize>().unwrap()],
            WriteFlags::from_bits_retain(f[2].parse().unwrap()),
        )),
        "g" => {
            let key = f.get(3).map(|k| unhex(k));
            let data = f.get(4).map(|d| unhex(d));
            let op = CursorOp::from_raw(f[2].parse().unwrap()).unwrap();
            match txn.cursor_get(
                cur[f[1].parse::<usize>().unwrap()],
                key.as_deref(),
                data.as_deref(),
                op,
            ) {
                Ok(Some(_)) => 0,
                Ok(None) => MDB_NOTFOUND,
                Err(e) => e.to_err_code(),
            }
        }
        other => panic!("bad op {other}"),
    };
    let mut line = format!("{} rc={rc}", f[0]);
    for (i, &c) in cur.iter().enumerate() {
        line.push_str(&probe(txn, c, i, dupsort));
    }
    line
}

/// Removes the duplicate counts (` n=..`) from a step's output: C's cached
/// counts go stale when another cursor changes the same key's duplicates
/// (KNOWN_C_BUGS.md #14), Rust's do not. The counts in the DB records are
/// checked by the page audit at the end of each round instead.
fn mask_counts(_op: &str, line: &str) -> String {
    line.split(" | ")
        .map(|part| part.split(" n=").next().unwrap_or(part))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The current key and data (hex) of cursor `n` in a step's output.
fn item_of(line: &str, n: usize) -> Option<(String, String)> {
    let part = line.split(" | ").nth(n + 1)?;
    let k = part.split(" k=").nth(1)?.split(' ').next()?;
    let d = part.split(" d=").nth(1)?.split(' ').next()?;
    (k != "-").then(|| (k.to_owned(), d.to_owned()))
}

/// The ` n=..` count field of cursor `n` in a step's output.
fn count_of(line: &str, n: usize) -> Option<&str> {
    line.split(" | ")
        .nth(n + 1)
        .and_then(|part| part.split(" n=").nth(1))
}

/// `MDB_GET_CURRENT` (+ count) on a cursor, formatted like writeops.c.
fn probe(txn: &mut RwTxn<'_>, c: CursorId, i: usize, dupsort: bool) -> String {
    let (rc, k, d) = match txn.cursor_get(c, None, None, CursorOp::GetCurrent) {
        Ok(Some((k, d))) => (0, hex(k), hex(d)),
        Ok(None) => (MDB_NOTFOUND, "-".into(), "-".into()),
        Err(e) => (e.to_err_code(), "-".into(), "-".into()),
    };
    let mut s = format!(" | c{i} rc={rc} k={k} d={d}");
    if rc == 0 && dupsort {
        match txn.cursor_count(c) {
            Ok(n) => s.push_str(&format!(" n=0:{n}")),
            Err(e) => s.push_str(&format!(" n={}:0", e.to_err_code())),
        }
    }
    s
}

/// `writeops` running as a child process, one op at a time.
struct CRun {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CRun {
    fn start(
        o: &dyn Oracle,
        path: &Path,
        name: &str,
        dbflags: u32,
        ncur: usize,
        map_size: usize,
    ) -> CRun {
        let mut child = Command::new(o.writeops())
            .arg(path)
            .args([
                name,
                &dbflags.to_string(),
                &ncur.to_string(),
                &map_size.to_string(),
                &o.page_size().unwrap_or(0).to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        CRun {
            child,
            stdin,
            stdout,
        }
    }

    /// Sends one op and returns its output line, or `None` if C died.
    fn step(&mut self, op: &str) -> Option<String> {
        let stdin = self.stdin.as_mut()?;
        if writeln!(stdin, "{op}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return None;
        }
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(n) if n > 0 => Some(line.trim_end().to_owned()),
            _ => None,
        }
    }

    /// Closes stdin (C commits) and returns the commit line and stderr.
    fn finish(mut self) -> (Option<String>, String) {
        drop(self.stdin.take());
        let mut rest = String::new();
        let _ = self.stdout.read_to_string(&mut rest);
        let out = self.child.wait_with_output().unwrap();
        (
            rest.lines().last().map(str::to_owned),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

/// What a script draws from.
pub struct Profile {
    pub dbflags: u32,
    pub keys: usize,
    pub dupfixed_size: Option<usize>,
    pub big: bool,
    /// Keys of 200..480 bytes: deep trees, branch and root splits, merges.
    pub long_keys: bool,
    /// Nested transactions: `nb`, `nc`, `na` ops.
    pub nested: bool,
    /// `mdb_drop`: `dr:0` / `dr:1` ops.
    pub drops: bool,
    /// With `Some(nodemax)`: no `MDB_APPENDDUP` put of a key and item whose
    /// leaf node (1.0's `LEAFSIZE`, both padded to even lengths) is larger
    /// than `nodemax`. For a new key that item goes straight to a new,
    /// empty sub-DB, where C reads a node out of bounds (`KNOWN_C_BUGS.md`
    /// #32) and goes on unpredictably; only reachable with LMDB 1.0's small
    /// pages.
    pub appenddup_nodemax: Option<usize>,
}

impl Profile {
    pub fn new(dbflags: u32, keys: usize) -> Profile {
        Profile {
            dbflags,
            keys,
            dupfixed_size: None,
            big: false,
            long_keys: false,
            nested: false,
            drops: false,
            appenddup_nodemax: None,
        }
    }
}

fn gen_key(rng: &mut Rng, p: &Profile) -> Vec<u8> {
    if p.dbflags & MDB_INTEGERKEY != 0 {
        return (rng.below(p.keys) as u64 * 7).to_ne_bytes().to_vec();
    }
    let n = rng.below(p.keys);
    let mut k = format!("key{n:04}").into_bytes();
    if p.long_keys {
        k.extend(std::iter::repeat_n(b'y', 200 + (n * 37) % 280));
    } else if n % 7 == 0 {
        k.extend(std::iter::repeat_n(b'x', 50 + n % 150));
    }
    k
}

fn gen_data(rng: &mut Rng, p: &Profile) -> Vec<u8> {
    if let Some(sz) = p.dupfixed_size {
        let v = rng.below(400) as u64;
        let mut d = v.to_be_bytes().to_vec();
        d.resize(sz, b'f');
        return d;
    }
    if p.dbflags & MDB_DUPSORT != 0 {
        let n = rng.below(300);
        let len = 4 + (n * 13) % 120;
        let mut d = format!("{n:05}").into_bytes();
        d.resize(len, b'd');
        return d;
    }
    let len = if p.big && rng.below(12) == 0 {
        1000 + rng.below(40000)
    } else {
        let max = if rng.below(4) == 0 { 900 } else { 40 };
        1 + rng.below(max)
    };
    let mut d = vec![b'v'; len];
    d[0] = rng.below(256) as u8;
    d
}

/// Produces the ops of a round: a prefill of puts, then random ops. A
/// cursor of a `DUP_SORT` DB left by a failed `MDB_SET`, `MDB_SET_KEY`,
/// `MDB_SET_RANGE` or `MDB_GET_BOTH` is repositioned with `MDB_FIRST` before
/// anything else uses it (a correct client would not move it; C would use
/// the stale sub-cursor, or read the node past the last one, `KNOWN_C_BUGS.md`
/// #4/#6/#14).
pub struct Gen<'a> {
    rng: &'a mut Rng,
    p: &'a Profile,
    ncur: usize,
    prefill: usize,
    heal: Option<usize>,
    /// Cursors to clear with `MDB_SET_RANGE` (see [`Gen::next`]).
    reset: std::collections::VecDeque<usize>,
    /// Which cursors were not initialised (`EINVAL`) after the last step.
    uninit: Vec<bool>,
}

impl Gen<'_> {
    /// The next op for a transaction at nesting depth `depth`.
    ///
    /// A cursor that a delete left uninitialised (the tree became empty)
    /// keeps its old root page in C (`mc_pg[0]`), which `mdb_page_search`
    /// trusts if the page number matches the root: once the root's page
    /// number is reused (1.0 reuses the emptied root at once) and its buffer
    /// replaced (a nested commit), C's `MDB_FIRST`/`MDB_LAST`/`MDB_PREV`
    /// read freed memory (`KNOWN_C_BUGS.md` #33). Such a cursor is cleared
    /// with `MDB_SET_RANGE`, which drops `mc_pg[0]` before searching.
    fn next(&mut self, prev: Option<(&str, &str)>, depth: usize) -> String {
        if let Some((op, line)) = prev {
            let now: Vec<bool> = line
                .split(" | ")
                .skip(1)
                .map(|c| c.contains(" rc=22 "))
                .collect();
            if op.starts_with("d:") || op.starts_with("cd:") {
                for (n, &u) in now.iter().enumerate() {
                    if u && !self.uninit.get(n).copied().unwrap_or(true) {
                        self.reset.push_back(n);
                    }
                }
            }
            self.uninit = now;
        }
        if let Some(n) = self.reset.pop_front() {
            return format!("g:{n}:{MDB_SET_RANGE}:{}", hex(&[0; 8]));
        }
        if let Some((op, line)) = prev {
            let f: Vec<&str> = op.split(':').collect();
            let failed = line.split_whitespace().nth(1) != Some("rc=0");
            if f[0] == "g"
                && failed
                && self.p.dbflags & MDB_DUPSORT != 0
                && matches!(f[2].parse::<u32>(), Ok(2 | 3 | 15 | 16 | 17))
            {
                self.heal = f[1].parse().ok();
            }
        }
        if let Some(n) = self.heal.take() {
            return format!("g:{n}:{MDB_FIRST}");
        }
        let (rng, p) = (&mut *self.rng, self.p);
        if self.prefill > 0 {
            self.prefill -= 1;
            return format!("p:{}:{}:0", hex(&gen_key(rng, p)), hex(&gen_data(rng, p)));
        }
        if p.nested {
            match rng.below(50) {
                0 if depth < 3 => return "nb".into(),
                1 if depth > 0 => return "nc".into(),
                2 if depth > 0 => return "na".into(),
                _ => {}
            }
        }
        if p.drops {
            match rng.below(300) {
                0 => return "dr:0".into(),
                1 if depth == 0 => return "dr:1".into(),
                _ => {}
            }
        }
        // In a nested transaction the ancestors' cursors can be used too.
        let op = gen_op(rng, p, self.ncur * (depth + 1));
        // MDB_CURRENT only on a cursor that is on an item: C asserts or
        // corrupts otherwise (KNOWN_C_BUGS.md #13).
        let f: Vec<&str> = op.split(':').collect();
        if f[0] == "cp" && f[4] == MDB_CURRENT.to_string() {
            let on_item = prev.is_some_and(|(_, l)| l.contains(&format!("| c{} rc=0 ", f[1])));
            if !on_item {
                return format!("{}:0", f[..4].join(":"));
            }
        }
        op
    }
}

/// Put flags `f` for `k`/`d`, without `MDB_APPENDDUP` where
/// [`Profile::appenddup_nodemax`] says so.
fn appenddup_ok(p: &Profile, f: u32, k: &[u8], d: &[u8]) -> u32 {
    let even = |n: usize| (n + 1) & !1;
    match p.appenddup_nodemax {
        Some(max) if f == MDB_APPENDDUP && 8 + even(k.len()) + even(d.len()) > max => 0,
        _ => f,
    }
}

fn gen_op(rng: &mut Rng, p: &Profile, ncur: usize) -> String {
    let get_ops: &[u32] = &[
        MDB_FIRST,
        MDB_LAST,
        MDB_NEXT,
        MDB_PREV,
        MDB_NEXT,
        MDB_PREV,
        MDB_SET,
        MDB_SET_KEY,
        MDB_SET_RANGE,
        MDB_NEXT_NODUP,
        MDB_PREV_NODUP,
        MDB_NEXT_DUP,
        MDB_PREV_DUP,
        MDB_FIRST_DUP,
        MDB_LAST_DUP,
        MDB_GET_BOTH,
        MDB_GET_BOTH_RANGE,
    ];
    let put_flags = [
        0,
        0,
        0,
        MDB_NOOVERWRITE,
        MDB_NODUPDATA,
        MDB_CURRENT,
        MDB_APPEND,
        MDB_APPENDDUP,
    ];
    let c = rng.below(ncur);
    match rng.below(20) {
        0..=4 => {
            let f = put_flags[rng.below(put_flags.len())];
            let f = if f == MDB_CURRENT { 0 } else { f };
            let (k, d) = (gen_key(rng, p), gen_data(rng, p));
            let f = appenddup_ok(p, f, &k, &d);
            format!("p:{}:{}:{f}", hex(&k), hex(&d))
        }
        5..=8 => {
            let mut f = put_flags[rng.below(put_flags.len())];
            if f == MDB_CURRENT && p.dbflags & MDB_DUPSORT != 0 {
                // The new item would have to sort like the old one.
                f = 0;
            }
            let (k, d) = (gen_key(rng, p), gen_data(rng, p));
            let f = appenddup_ok(p, f, &k, &d);
            format!("cp:{c}:{}:{}:{f}", hex(&k), hex(&d))
        }
        9..=10 => {
            if p.dbflags & MDB_DUPSORT != 0 && rng.below(2) == 0 {
                format!("d:{}:{}", hex(&gen_key(rng, p)), hex(&gen_data(rng, p)))
            } else {
                format!("d:{}", hex(&gen_key(rng, p)))
            }
        }
        11..=12 => {
            let f = if p.dbflags & MDB_DUPSORT != 0 && rng.below(3) == 0 {
                MDB_NODUPDATA
            } else {
                0
            };
            format!("cd:{c}:{f}")
        }
        _ => {
            let op = get_ops[rng.below(get_ops.len())];
            let dup_op = matches!(
                op,
                MDB_NEXT_DUP
                    | MDB_PREV_DUP
                    | MDB_FIRST_DUP
                    | MDB_LAST_DUP
                    | MDB_GET_BOTH
                    | MDB_GET_BOTH_RANGE
            );
            if p.dbflags & MDB_DUPSORT == 0 && dup_op {
                format!("g:{c}:{MDB_NEXT}")
            } else if matches!(op, MDB_SET | MDB_SET_KEY | MDB_SET_RANGE) {
                format!("g:{c}:{op}:{}", hex(&gen_key(rng, p)))
            } else if matches!(op, MDB_GET_BOTH | MDB_GET_BOTH_RANGE) {
                format!(
                    "g:{c}:{op}:{}:{}",
                    hex(&gen_key(rng, p)),
                    hex(&gen_data(rng, p))
                )
            } else {
                format!("g:{c}:{op}")
            }
        }
    }
}

/// Where the ops of a round come from.
pub enum Ops<'a> {
    Gen(Gen<'a>, usize),
    List(Vec<String>),
}

/// Outcome of a round.
#[derive(Debug, PartialEq, Eq)]
pub enum Round {
    /// Every step and the committed result agreed.
    Same,
    /// C died or took a known-buggy path at this step; all before agreed.
    CStopped(usize),
}

/// How a transaction level of a round ended.
enum LevelEnd {
    /// The ops ran out.
    Done,
    /// An `nc`/`na` op ended this nested transaction; C's output line.
    Commit(String),
    Abort(String),
    /// C stopped (see [`Round::CStopped`]).
    Stopped,
}

/// The state of a round: the C process, the op source and what the checks
/// need from previous steps.
struct Driver<'a> {
    o: &'a dyn Oracle,
    p: &'a Profile,
    ncur: usize,
    ops: Ops<'a>,
    tag: &'a str,
    dupsort: bool,
    /// `None` once C finished or died.
    crun: Option<CRun>,
    history: Vec<String>,
    prev: Option<(String, String)>,
    /// The unmasked outputs of the previous step, for spotting stale counts.
    prev_raw: Option<(String, String)>,
    heal: std::collections::VecDeque<String>,
    i: usize,
    /// C's output for an `nb` op, compared once Rust's child exists.
    pending_c_line: Option<String>,
}

impl Driver<'_> {
    fn next_op(&mut self, depth: usize) -> Option<String> {
        match &mut self.ops {
            Ops::Gen(g, n) => {
                if self.i >= *n {
                    // Unwind nested transactions before the final commit.
                    return (depth > 0).then(|| "nc".to_owned());
                }
                let prev = self.prev.as_ref().map(|(o, l)| (o.as_str(), l.as_str()));
                Some(match self.heal.pop_front() {
                    Some(op) => op,
                    None => g.next(prev, depth),
                })
            }
            Ops::List(l) => l.get(self.i).cloned(),
        }
    }

    fn record(&mut self, op: &str) {
        if let Ok(dir) = std::env::var("WRITEOPS_DUMP") {
            self.history.push(op.to_owned());
            let f = format!(
                "{dir}/script-{}-{}-{}.txt",
                self.tag, self.p.dbflags, self.ncur
            );
            std::fs::write(
                &f,
                self.history
                    .iter()
                    .map(|o| format!("{o}\n"))
                    .collect::<String>(),
            )
            .unwrap();
        }
    }

    /// Compares C's and Rust's output of a step. `false` if C took a
    /// known-buggy path and the round must stop.
    fn compare(&mut self, op: String, cl: String, rl_raw: String) -> bool {
        let i = self.i;
        let tag = self.tag;
        let rl = mask_counts(&op, &rl_raw);
        if std::env::var("WRITEOPS_DEBUG").is_ok() {
            for n in 0..cl.split(" | ").count().saturating_sub(1) {
                if count_of(&cl, n) != count_of(&rl_raw, n) {
                    eprintln!(
                        "DEBUG step {} {op}: cursor {n} count C {:?} Rust {:?} at {:?}",
                        self.i,
                        count_of(&cl, n),
                        count_of(&rl_raw, n),
                        item_of(&rl_raw, n)
                    );
                }
            }
        }
        if matches!(self.ops, Ops::Gen(..)) {
            // A cursor whose cached duplicate count C let go stale is put
            // back on its item with MDB_GET_BOTH, which makes C rebuild its
            // sub-cursor (KNOWN_C_BUGS.md #14).
            for n in 0..cl.split(" | ").count().saturating_sub(1) {
                if count_of(&cl, n) != count_of(&rl_raw, n) {
                    if let Some((k, d)) = item_of(&rl_raw, n) {
                        self.heal.push_back(format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}"));
                    }
                }
            }
        }
        self.prev_raw = Some((cl.clone(), rl_raw));
        let cl = mask_counts(&op, &cl);
        if cl != rl && self.dupsort {
            if let Some(stale) = stale_subcursors(&op, &cl, &rl) {
                // KNOWN_C_BUGS.md #18: C left these cursors' sub-cursors
                // stale; put them on Rust's (correct) item, which makes C
                // set the sub-cursor up again.
                for n in stale {
                    if let Some((k, d)) = item_of(&rl, n) {
                        self.heal.push_back(format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}"));
                    }
                }
                eprintln!("NOTE: {tag}: stale C sub-cursor after step {i} (known bug #18)");
                self.prev = Some((op, rl));
                self.i += 1;
                return true;
            }
        }
        if cl != rl {
            let prev = self.prev.as_ref().map(|(_, l)| l.as_str());
            if let Some(n) = stale_eof(&op, prev, &cl, &rl) {
                // KNOWN_C_BUGS.md #28: C's cursor kept a C_EOF from before
                // and stopped although items were added after it; put it
                // where Rust's (correctly) went.
                if let Some((k, d)) = item_of(&rl, n) {
                    self.heal.push_back(if self.dupsort {
                        format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}")
                    } else {
                        format!("g:{n}:{MDB_SET}:{k}")
                    });
                }
                eprintln!("NOTE: {tag}: stale C_EOF in C at step {i} (known bug #28)");
                self.prev = Some((op, rl));
                self.i += 1;
                return true;
            }
        }
        if cl != rl {
            if known_c_bug(&op, &cl, &rl) {
                // C's result came from an unchecked node read; the states
                // agree only if C changed nothing.
                if cl.split_once(" | ").map(|x| x.1) != rl.split_once(" | ").map(|x| x.1) {
                    eprintln!("NOTE: {tag}: C went its own way at step {i}");
                    return false;
                }
            } else {
                panic!(
                    "{tag}: step {i} {:?} differs\n  C:    {cl}\n  Rust: {rl}",
                    &op[..op.len().min(200)]
                );
            }
        }
        self.prev = Some((op, cl));
        self.i += 1;
        true
    }

    /// The output line of op `name` with return code `rc`, probing `cur`.
    fn line(&self, txn: &mut RwTxn<'_>, cur: &[CursorId], name: &str, rc: i32) -> String {
        let mut line = format!("{name} rc={rc}");
        for (i, &c) in cur.iter().enumerate() {
            line.push_str(&probe(txn, c, i, self.dupsort));
        }
        line
    }

    /// The `nb` op: begin a nested transaction of `txn` (the `nb` step is
    /// compared here), run it until its `nc`/`na` and end it. Returns the
    /// ending op, C's output for it and Rust's return code, `None` if C
    /// stopped, or the error of `begin_nested_txn`.
    #[allow(clippy::type_complexity)]
    fn run_child(
        &mut self,
        txn: &mut RwTxn<'_>,
        db: &mut rlmdb::Dbi,
        depth: usize,
        op: String,
        parent_cur: &[CursorId],
    ) -> Result<Option<(&'static str, String, i32)>, i32> {
        let mut child = txn.begin_nested_txn().map_err(|e| e.to_err_code())?;
        // The child's own cursors, then the ancestors' (numbered as in
        // writeops.c).
        let mut ccur: Vec<CursorId> = (0..self.ncur)
            .map(|_| child.open_cursor(*db).unwrap())
            .chain(parent_cur.iter().copied())
            .collect();
        let cl = self.pending_c_line.take().expect("C's nb line");
        let rl = self.line(&mut child, &ccur, "nb", 0);
        if !self.compare(op, cl, rl) {
            return Ok(None);
        }
        let end = self.level(&mut child, db, depth + 1, &mut ccur);
        let (name, cl, commit) = match end {
            LevelEnd::Commit(cl) => ("nc", cl, true),
            LevelEnd::Abort(cl) => ("na", cl, false),
            LevelEnd::Done => unreachable!(),
            LevelEnd::Stopped => return Ok(None),
        };
        for &id in &ccur[..self.ncur] {
            child.close_cursor(id);
        }
        let rc = if commit {
            code(&child.commit())
        } else {
            child.abort();
            0
        };
        Ok(Some((name, cl, rc)))
    }

    /// Runs ops on `txn` (nesting depth `depth`) until the ops run out, an
    /// `nc`/`na` ends it, or C stops. `cur` are the cursors usable in it:
    /// its own, then its ancestors'.
    fn level(
        &mut self,
        txn: &mut RwTxn<'_>,
        db: &mut rlmdb::Dbi,
        depth: usize,
        cur: &mut Vec<CursorId>,
    ) -> LevelEnd {
        loop {
            let Some(op) = self.next_op(depth) else {
                assert_eq!(depth, 0, "{}: script ends in a nested txn", self.tag);
                return LevelEnd::Done;
            };
            self.record(&op);
            if let (Some(n), Some((pc, pr)), "cd") = (
                cursor_of(&op),
                &self.prev_raw,
                op.split(':').next().unwrap_or(""),
            ) {
                if count_of(pc, n) != count_of(pr, n) {
                    // KNOWN_C_BUGS.md #14: C's cached count for this cursor
                    // is stale; its delete would act on it (and may lose
                    // data).
                    eprintln!("NOTE: {}: stale C count before step {}", self.tag, self.i);
                    return LevelEnd::Stopped;
                }
            }
            let Some(cl) = self.crun.as_mut().and_then(|c| c.step(&op)) else {
                let (_, err) = self.crun.take().expect("C running").finish();
                assert!(
                    err.contains("Assertion") || err.is_empty(),
                    "{}: C writeops failed at step {}: {err}",
                    self.tag,
                    self.i
                );
                eprintln!(
                    "NOTE: {}: C died at step {} {}: {}",
                    self.tag,
                    self.i,
                    &op[..op.len().min(60)],
                    err.trim()
                );
                return LevelEnd::Stopped;
            };
            if op == "nb" {
                self.pending_c_line = Some(cl.clone());
            }
            let rl_raw = match op.as_str() {
                "nc" if depth > 0 => return LevelEnd::Commit(cl),
                "na" if depth > 0 => return LevelEnd::Abort(cl),
                "nb" => match self.run_child(txn, db, depth, op.clone(), cur) {
                    Err(rc) => {
                        self.pending_c_line = None;
                        self.line(txn, cur, "nb", rc)
                    }
                    Ok(None) => return LevelEnd::Stopped,
                    Ok(Some((name, cl, rc))) => {
                        let rl = self.line(txn, cur, name, rc);
                        if !self.compare(name.to_owned(), cl, rl) {
                            return LevelEnd::Stopped;
                        }
                        continue;
                    }
                },
                "dr:0" => {
                    let rc = code(&txn.clear_db(*db));
                    self.line(txn, cur, "dr", rc)
                }
                "dr:1" => {
                    assert_eq!(depth, 0);
                    for id in cur.drain(..) {
                        txn.close_cursor(id);
                    }
                    let rc = code(&txn.drop_db(*db));
                    *db = txn
                        .create_db(
                            Some("t"),
                            DatabaseFlags::from_bits_retain(self.p.dbflags) | DatabaseFlags::CREATE,
                        )
                        .unwrap();
                    cur.extend((0..self.ncur).map(|_| txn.open_cursor(*db).unwrap()));
                    self.line(txn, cur, "dr", rc)
                }
                _ if op.starts_with("b:") => {
                    let f: Vec<&str> = op.split(':').collect();
                    let rc = code(&bulk_put(
                        txn,
                        *db,
                        f[1].parse().unwrap(),
                        f[2].parse().unwrap(),
                        f[3].parse().unwrap(),
                    ));
                    self.line(txn, cur, "b", rc)
                }
                _ => rust_step(txn, *db, cur, &op, self.dupsort),
            };
            if !self.compare(op, cl, rl_raw) {
                return LevelEnd::Stopped;
            }
        }
    }
}

pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `bulk_put` of writeops.c: `n` puts of SplitMix64 keys and values.
fn bulk_put(
    txn: &mut RwTxn<'_>,
    db: rlmdb::Dbi,
    n: u64,
    seed: u64,
    len: usize,
) -> Result<(), Error> {
    let mut st = seed;
    let mut value = vec![0u8; len];
    for _ in 0..n {
        let key = splitmix64(&mut st).to_le_bytes();
        for chunk in value.chunks_mut(8) {
            let x = splitmix64(&mut st).to_le_bytes();
            chunk.copy_from_slice(&x[..chunk.len()]);
        }
        txn.put(db, &key, &value, WriteFlags::empty())?;
    }
    Ok(())
}

/// Runs one round in lock step: each op goes to C, then to Rust, and the
/// two outputs must be equal.
pub fn run_round(o: &dyn Oracle, p: &Profile, ncur: usize, ops: Ops<'_>, tag: &str) -> Round {
    run_round_sized(o, p, ncur, ops, tag, 64 << 20)
}

/// [`run_round`] with environments of `map_size` bytes; above 64 MiB the
/// contents are compared with cursors instead of dumps, and the page usage
/// (`mdb_stat -a -f`) must agree too.
pub fn run_round_sized(
    o: &dyn Oracle,
    p: &Profile,
    ncur: usize,
    ops: Ops<'_>,
    tag: &str,
    map_size: usize,
) -> Round {
    let cdir = tempfile::tempdir().unwrap();
    let rdir = tempfile::tempdir().unwrap();
    let mut builder = Environment::builder();
    builder
        .format(o.format())
        .map_size(map_size)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = o.page_size() {
        builder.page_size(ps);
    }
    let env = builder.open(rdir.path()).unwrap();
    if let Some(n) = o.dirty_limit() {
        env.set_dirty_limit(n);
    }
    let mut txn = env.begin_rw_txn().unwrap();
    let mut db = txn
        .create_db(
            Some("t"),
            DatabaseFlags::from_bits_retain(p.dbflags) | DatabaseFlags::CREATE,
        )
        .unwrap();
    let mut cur: Vec<CursorId> = (0..ncur).map(|_| txn.open_cursor(db).unwrap()).collect();
    let crun = CRun::start(o, cdir.path(), "t", p.dbflags, ncur, map_size);
    let mut drv = Driver {
        o,
        p,
        ncur,
        ops,
        tag,
        dupsort: p.dbflags & MDB_DUPSORT != 0,
        crun: Some(crun),
        history: Vec::new(),
        prev: None,
        prev_raw: None,
        heal: Default::default(),
        i: 0,
        pending_c_line: None,
    };
    match drv.level(&mut txn, &mut db, 0, &mut cur) {
        LevelEnd::Done => {}
        LevelEnd::Stopped => return Round::CStopped(drv.i),
        LevelEnd::Commit(_) | LevelEnd::Abort(_) => unreachable!(),
    }
    let (c_commit, err) = drv.crun.take().expect("C running").finish();
    for id in cur {
        txn.close_cursor(id);
    }
    let r_commit = format!("commit rc={}", code(&txn.commit()));
    assert_eq!(
        c_commit.as_deref(),
        Some(r_commit.as_str()),
        "{tag}: commit; C stderr {err}"
    );
    // The Rust side of the comparisons below: the writer's environment,
    // or the same reopened read-only after closing it for an oracle that
    // cannot read an environment rlmdb holds open.
    let env = if o.shares_open_environments() {
        env
    } else {
        drop(env);
        Environment::builder()
            .max_dbs(8)
            .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
            .open(rdir.path())
            .unwrap()
    };
    // Same committed content, valid page usage on both.
    if map_size <= 64 << 20 {
        assert!(
            o.dump(cdir.path(), "t") == o.dump(rdir.path(), "t"),
            "{tag}: dumps differ"
        );
    } else {
        // Too big for mdb_dump text: walk both with Rust cursors.
        let cenv = Environment::builder()
            .max_dbs(8)
            .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
            .open(cdir.path())
            .unwrap();
        let (ct, rt) = (cenv.begin_ro_txn().unwrap(), env.begin_ro_txn().unwrap());
        let (cdb, rdb) = (
            ct.open_db(Some("t")).unwrap(),
            rt.open_db(Some("t")).unwrap(),
        );
        let (mut cc, mut rc) = (ct.cursor(cdb).unwrap(), rt.cursor(rdb).unwrap());
        let mut n = 0u64;
        loop {
            let (a, b) = (cc.next().unwrap(), rc.next().unwrap());
            assert!(a == b, "{tag}: contents differ at item {n}");
            if a.is_none() {
                break;
            }
            n += 1;
        }
        eprintln!("{tag}: {n} items agree");
    }
    let (code, stdout, stderr) = o.check_freelist(rdir.path());
    assert_eq!(
        code, 0,
        "{tag}: check-freelist of Rust:\n{stdout}\n{stderr}"
    );
    let (code, stdout, stderr) = o.check_freelist(cdir.path());
    if code != 0 {
        // KNOWN_C_BUGS.md #14: C's MDB_NODUPDATA cursor delete subtracts a
        // stale duplicate count, so only md_entries may be off.
        let only_entries = stderr.lines().all(|l| {
            !l.starts_with("cpeer:") || {
                let (a, b) = l.split_once(", walk found ").unwrap_or(("", "-"));
                let strip = |x: &str| {
                    x.split(" entries=")
                        .next()
                        .unwrap_or("")
                        .rsplit("says ")
                        .next()
                        .unwrap_or("")
                        .to_owned()
                };
                l.contains("descriptor says") && strip(a) == strip(b)
            }
        });
        assert!(
            only_entries,
            "{tag}: check-freelist of C:\n{stdout}\n{stderr}"
        );
        eprintln!("NOTE: {tag}: C miscounted entries (known bug #14)");
        return Round::Same;
    }
    if map_size > 64 << 20 {
        // Page usage is the same, page for page (sizes, free pages).
        assert_eq!(
            o.page_usage(cdir.path()),
            o.page_usage(rdir.path()),
            "{tag}: mdb_stat -a -f"
        );
    }
    o.after_round(cdir.path(), rdir.path(), tag);
    Round::Same
}

/// Runs `rounds` rounds of `n` random ops (half of them a prefill of puts)
/// drawn from `p` with `seed`; at least half of them must agree in full
/// (C may stop early on a known C bug).
pub fn run_profile(o: &dyn Oracle, p: Profile, seed: u64, rounds: usize, n: usize) {
    let mut rng = Rng(seed | 1);
    let mut full = 0;
    for round in 0..rounds {
        let ncur = 1 + rng.below(4);
        let gen = Gen {
            rng: &mut rng,
            p: &p,
            ncur,
            prefill: n / 2,
            heal: None,
            reset: Default::default(),
            uninit: Vec::new(),
        };
        let tag = format!("{seed:x}-{round}");
        if run_round(o, &p, ncur, Ops::Gen(gen, n + n / 2), &tag) == Round::Same {
            full += 1;
        }
    }
    eprintln!("{full} of {rounds} rounds compared in full");
    assert!(
        full * 2 >= rounds,
        "too few rounds compared in full: {full} of {rounds}"
    );
}

/// The cursor an op uses, if any. After a failed `MDB_SET`/`MDB_GET_BOTH`
/// on a `DUP_SORT` DB the cursor may sit on a key with duplicates while its
/// sub-cursor still describes another key (C clears only its flags); such
/// a cursor is "suspect" until it is positioned successfully.
fn cursor_of(op: &str) -> Option<usize> {
    let f: Vec<&str> = op.split(':').collect();
    match f[0] {
        "g" | "cp" | "cd" => f[1].parse().ok(),
        _ => None,
    }
}

/// `KNOWN_C_BUGS.md` #18: after a delete (`d`, `cd`) moved cursors onto
/// a key with duplicates, C's cursors whose sub-cursor was still marked
/// initialised from an earlier key report `MDB_NOTFOUND` or another
/// duplicate of that key. Returns the cursors that differ only that way,
/// if every difference is of that kind.
fn stale_subcursors(op: &str, cl: &str, rl: &str) -> Option<Vec<usize>> {
    if !(op.starts_with("cd:") || op.starts_with("d:")) {
        return None;
    }
    let (c_parts, r_parts): (Vec<&str>, Vec<&str>) =
        (cl.split(" | ").collect(), rl.split(" | ").collect());
    if c_parts.len() != r_parts.len() || c_parts[0] != r_parts[0] {
        return None;
    }
    let mut stale = Vec::new();
    for (n, (c, r)) in c_parts[1..].iter().zip(&r_parts[1..]).enumerate() {
        if c == r {
            continue;
        }
        let ok = r.contains(" rc=0 ")
            && (c.contains(" rc=-30798 ") || {
                let key = |x: &str| x.split(" d=").next().map(str::to_owned);
                c.contains(" rc=0 ") && key(c) == key(r)
            });
        if !ok {
            return None;
        }
        stale.push(n);
    }
    (!stale.is_empty()).then_some(stale)
}

/// `KNOWN_C_BUGS.md` #28: C's cursor kept a `C_EOF` from an earlier move
/// (`MDB_LAST`, a failed `MDB_NEXT`, an `MDB_SET` that did not clear it)
/// although items were added after it since: `MDB_NEXT` & co. report
/// `MDB_NOTFOUND`, and `MDB_GET_MULTIPLE` fetches nothing and stays put,
/// where Rust moves on. Returns the cursor if that is the only difference
/// (`prev`: the agreed output of the step before).
fn stale_eof(op: &str, prev: Option<&str>, cl: &str, rl: &str) -> Option<usize> {
    let f: Vec<&str> = op.split(':').collect();
    if f[0] != "g" {
        return None;
    }
    let n: usize = f[1].parse().ok()?;
    let opn: u32 = f[2].parse().ok()?;
    let (c_parts, r_parts): (Vec<&str>, Vec<&str>) =
        (cl.split(" | ").collect(), rl.split(" | ").collect());
    if c_parts.len() != r_parts.len() || c_parts.len() <= n + 1 {
        return None;
    }
    let others_agree = c_parts[1..]
        .iter()
        .zip(&r_parts[1..])
        .enumerate()
        .all(|(i, (c, r))| i == n || c == r);
    if !others_agree {
        return None;
    }
    if [MDB_NEXT, MDB_NEXT_DUP, MDB_NEXT_NODUP, MDB_NEXT_MULTIPLE].contains(&opn) {
        return (c_parts[0] == "g rc=-30798" && r_parts[0] == "g rc=0").then_some(n);
    }
    if opn == MDB_GET_MULTIPLE {
        let before = prev?.split(" | ").nth(n + 1)?;
        let key = |x: &str| x.split(" d=").next().map(str::to_owned);
        let ok = c_parts[0] == "g rc=0"
            && r_parts[0] == "g rc=0"
            && c_parts[n + 1] == before
            && key(c_parts[n + 1]) == key(r_parts[n + 1]);
        return ok.then_some(n);
    }
    None
}

/// C results that come from reading an unchecked node past the end of a
/// page (`KNOWN_C_BUGS.md` #13): `MDB_CURRENT` on a cursor that is not on
/// an item. Rust reports `MDB_NOTFOUND`.
fn known_c_bug(op: &str, cl: &str, rl: &str) -> bool {
    op.starts_with("cp:")
        && op.ends_with(&format!(":{MDB_CURRENT}"))
        && rl.starts_with("cp rc=-30798")
        && cl != rl
}
