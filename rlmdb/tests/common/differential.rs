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
    CursorId, CursorOp, DatabaseFlags, Environment, EnvironmentFlags, Error, Format, MdbError,
    RwTxn, Transaction, WriteFlags,
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
    /// A data file both sides start from instead of a new environment (for
    /// example a 0.9 file with a page size C 0.9 cannot create but opens);
    /// both open it as it is, without a page size request.
    fn data_file(&self) -> Option<Vec<u8>> {
        None
    }
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
        // C deletes and puts (writeops.c), which is what rlmdb's MDB_CURRENT
        // does where C's in-place replacement goes wrong.
        "cr" => code(&txn.cursor_put(
            cur[f[1].parse::<usize>().unwrap()],
            &unhex(f[2]),
            &unhex(f[3]),
            WriteFlags::CURRENT,
        )),
        // C puts with the cursor's key, Rust with another one, which rlmdb's
        // MDB_CURRENT replaces by the cursor's (KNOWN_C_BUGS.md row 58).
        "cw" => code(&txn.cursor_put(
            cur[f[1].parse::<usize>().unwrap()],
            &unhex(f[3]),
            &unhex(f[4]),
            WriteFlags::CURRENT,
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
/// (KNOWN_C_BUGS.md row 14), Rust's do not. The counts in the DB records are
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
    /// row 32) and goes on unpredictably; only reachable with LMDB 1.0's small
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
        let be = v.to_be_bytes();
        // Items shorter than 8 bytes keep the low bytes (a 1-byte item
        // takes all 256 values), longer ones are padded.
        let mut d = be[be.len().saturating_sub(sz)..].to_vec();
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
/// rows 4, 6 and 14).
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
    /// read freed memory (`KNOWN_C_BUGS.md` row 33). Such a cursor is cleared
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
        // corrupts otherwise (KNOWN_C_BUGS.md row 13).
        let f: Vec<&str> = op.split(':').collect();
        if f[0] == "cp" && f[4] == MDB_CURRENT.to_string() {
            let current = prev
                .filter(|(_, l)| l.contains(&format!("| c{} rc=0 ", f[1])))
                .and_then(|(_, l)| item_of(l, f[1].parse().ok()?));
            let Some((ck, _)) = current else {
                return format!("{}:0", f[..4].join(":"));
            };
            // The generated key, of the length of the cursor's: where it is
            // another key, C puts the cursor's (C would write the other over
            // the node where it rewrites it), Rust the other one, which
            // rlmdb's MDB_CURRENT replaces by the cursor's (KNOWN_C_BUGS.md
            // row 58). Of that length, the spill estimates (C counts the
            // caller's key) are the same.
            let mut w = unhex(f[2]);
            w.resize(ck.len() / 2, b'w');
            let w = hex(&w);
            if w != ck {
                return format!("cw:{}:{ck}:{w}:{}", f[1], f[3]);
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

/// A script (for [`Ops::List`], two cursors) on an `MDB_DUPSORT |
/// MDB_DUPFIXED` DB with 1-byte items: all 256 values of one key in a
/// scrambled order, read back with `MDB_GET_MULTIPLE`/`MDB_NEXT_MULTIPLE`,
/// some deleted in an aborted nested transaction, then all but 16 deleted
/// one by one and a few with the cursor. On sub-pages that grows and
/// shrinks them; with 1.0's 256-byte pages the key's sub-DB splits its
/// `P_LEAF2` page and then merges and rebalances `P_LEAF2` pages.
pub fn dupfixed_1byte_script() -> Vec<String> {
    dupfixed_1byte_script_for(b"k")
}

/// [`dupfixed_1byte_script`] with the key `key`.
pub fn dupfixed_1byte_script_for(key: &[u8]) -> Vec<String> {
    let k = hex(key);
    let v = |i: usize| hex(&[(i * 167 % 256) as u8]);
    let mut ops: Vec<String> = (0..256).map(|i| format!("p:{k}:{}:0", v(i))).collect();
    ops.push(format!("g:0:{MDB_FIRST}"));
    ops.push(format!("g:0:{MDB_GET_MULTIPLE}"));
    for _ in 0..4 {
        ops.push(format!("g:0:{MDB_NEXT_MULTIPLE}"));
    }
    ops.push(format!("g:1:{MDB_LAST}"));
    ops.push("nb".into());
    ops.extend((0..40).map(|i| format!("d:{k}:{}", v(i * 3))));
    ops.push("na".into());
    for i in 0..240 {
        ops.push(format!("d:{k}:{}", v(i * 91)));
        if i % 16 == 0 {
            ops.push(format!("g:1:{MDB_PREV_DUP}"));
        }
    }
    ops.push(format!("g:0:{MDB_FIRST}"));
    ops.extend(std::iter::repeat_n("cd:0:0".to_string(), 3));
    ops.push(format!("g:0:{MDB_GET_MULTIPLE}"));
    ops
}

/// A script (for [`Ops::List`]) of puts of the 1-byte items `0..n`, in
/// that order, as duplicates of `key` (an `MDB_DUPSORT | MDB_DUPFIXED`
/// DB).
pub fn dupfixed_1byte_puts(key: &[u8], n: u8) -> Vec<String> {
    let k = hex(key);
    (0..n).map(|v| format!("p:{k}:{}:0", hex(&[v]))).collect()
}

/// A script (for [`Ops::List`], two cursors, an `MDB_DUPSORT` DB) of
/// `MDB_CURRENT` puts that replace a key's only item with items around the
/// largest that fits a node with the key, for an oracle with `me_nodemax`
/// `nodemax` and maximum key (and item) size `maxkey`, keys of `klen`
/// bytes, padded to an even length in nodes if `even_keys` (1.0).
///
/// Where the new item does not fit a node, the op is `cr`: C deletes the
/// item and puts the new one, which is what rlmdb's `MDB_CURRENT` must
/// write there (C's would store the item on overflow pages,
/// `KNOWN_C_BUGS.md` row 38). Elsewhere both sides put with `MDB_CURRENT`:
/// items that fit, and one byte over `maxkey` (`MDB_BAD_VALSIZE`, nothing
/// changed). The other cursor sits on the same item (where the next key
/// has a single item too) or on the key before it. The replaced keys then
/// get a second duplicate, a refused `MDB_NODUPDATA` put of the item, a
/// lookup and some deletes; two replacements run in nested transactions,
/// one committed, one aborted.
pub fn current_single_item_script(
    nodemax: usize,
    maxkey: usize,
    even_keys: bool,
    klen: usize,
) -> Vec<String> {
    const N: usize = 40;
    let kpad = if even_keys { (klen + 1) & !1 } else { klen };
    let key = |i: usize| {
        let mut k = format!("k{i:03}").into_bytes();
        k.resize(klen, b'q');
        hex(&k)
    };
    let item = |len: usize, seed: usize| {
        hex(&(0..len)
            .map(|b| (b * 7 + seed * 13 + 1) as u8)
            .collect::<Vec<u8>>())
    };
    let mut ops = Vec::new();
    // Every fifth key has duplicates on a sub-page, the others one item.
    let mut dups: std::collections::BTreeSet<usize> = (0..N).step_by(5).collect();
    for i in 0..N {
        ops.push(format!("p:{}:{}:0", key(i), item(3 + i % 7, i)));
        if dups.contains(&i) {
            for j in 1..4 {
                ops.push(format!("p:{}:{}:0", key(i), item(3 + j, 100 + i + j)));
            }
        }
    }
    // Largest item that fits a node with the key, and around it.
    let fit = (nodemax - 8).saturating_sub(kpad);
    let mut lens: Vec<usize> = [1, fit, fit + 1, fit + 2, maxkey - 1, maxkey, maxkey + 1]
        .into_iter()
        .filter(|&n| (1..=maxkey + 1).contains(&n))
        .collect();
    lens.sort_unstable();
    lens.dedup();
    let mut single = (0..N).filter(|i| i % 5 != 0);
    // Replaces key i's item by one of `len` bytes, the other cursor on the
    // same item or the key before; true if C must delete and put.
    let replace = |ops: &mut Vec<String>, i: usize, len: usize, same: bool| {
        let d = item(len, i);
        ops.push(format!("g:0:{MDB_SET}:{}", key(i)));
        let other = if same { i } else { i - 1 };
        ops.push(format!("g:1:{MDB_SET}:{}", key(other)));
        let routed = len <= maxkey && 8 + kpad + len > nodemax;
        if routed {
            ops.push(format!("cr:0:{}:{d}", key(i)));
        } else {
            ops.push(format!("cp:0:{}:{d}:{MDB_CURRENT}", key(i)));
        }
        ops.push(format!("g:0:{MDB_GET_CURRENT}"));
        routed
    };
    let mut routed_keys = Vec::new();
    for (n, &len) in lens.iter().enumerate() {
        for same in [false, true] {
            let i = single.next().expect("enough keys");
            // After the delete, the other cursor is on the next key: keep
            // it off duplicates there (C's sub-cursor fixups after a
            // delete are another matter, KNOWN_C_BUGS.md row 18).
            let same = same && i + 1 < N && !dups.contains(&(i + 1));
            if replace(&mut ops, i, len, same) {
                routed_keys.push((i, len));
                dups.insert(i);
            }
            if n % 2 == 1 {
                ops.push(format!("g:1:{MDB_NEXT}"));
            }
        }
    }
    for &(i, len) in &routed_keys {
        let d = item(len, i);
        ops.push(format!("p:{}:{}:0", key(i), item(5, 900 + i)));
        ops.push(format!("p:{}:{d}:{MDB_NODUPDATA}", key(i)));
        ops.push(format!("g:1:{MDB_GET_BOTH}:{}:{d}", key(i)));
        ops.push(format!("g:1:{MDB_NEXT_DUP}"));
    }
    if let Some(&(i, len)) = routed_keys.first() {
        ops.push(format!("d:{}:{}", key(i), item(len, i)));
    }
    if let Some(&(i, _)) = routed_keys.get(1) {
        ops.push(format!("d:{}", key(i)));
    }
    let big = *lens.iter().rfind(|&&n| n <= maxkey).expect("an item");
    for (end, keep) in [("nc", true), ("na", false)] {
        let i = single.next().expect("enough keys");
        ops.push("nb".into());
        replace(&mut ops, i, big, false);
        if keep {
            ops.push(format!("p:{}:{}:0", key(i), item(4, 700 + i)));
        }
        ops.push(end.into());
        ops.push(format!("g:0:{MDB_GET_CURRENT}"));
    }
    ops.push(format!("g:0:{MDB_FIRST}"));
    ops.extend(std::iter::repeat_n(format!("g:0:{MDB_NEXT}"), 2 * N));
    ops
}

/// Puts `items` under `key` in a DB `t` with `dbflags` of a fresh
/// environment of `o`'s shape (written by rlmdb, which writes C's pages),
/// commits, and returns the index in `items` of the first item of every
/// leaf of `key`'s sub-DB but the leftmost whose leaf and the one before it
/// hold as many items as the leftmost (not the rightmost, which is fuller
/// or emptier: a delete there may rebalance), read from the data file.
fn sub_db_leaf_firsts(o: &dyn Oracle, dbflags: u32, key: &[u8], items: &[Vec<u8>]) -> Vec<usize> {
    use rlmdb::format::meta::pick_newest;
    use rlmdb::format::{Db, Meta, NodeRef, PageRef};
    let dir = tempfile::tempdir().unwrap();
    if let Some(file) = o.data_file() {
        std::fs::write(dir.path().join("data.mdb"), &file).unwrap();
    }
    let mut builder = Environment::builder();
    builder
        .format(o.format())
        .map_size(64 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC);
    if let Some(ps) = o.page_size() {
        builder.page_size(ps);
    }
    let env = builder.open(dir.path()).unwrap();
    let ps = env.page_size();
    let mut txn = env.begin_rw_txn().unwrap();
    let flags = DatabaseFlags::from_bits_retain(dbflags) | DatabaseFlags::CREATE;
    let db = txn.create_db(Some("t"), flags).unwrap();
    for d in items {
        txn.put(db, key, d, WriteFlags::empty()).unwrap();
    }
    txn.commit().unwrap();
    drop(env);
    let file = std::fs::read(dir.path().join("data.mdb")).unwrap();
    let l = o.format().layout();
    let page = |pgno: u64| {
        let start = usize::try_from(pgno).unwrap() * ps;
        PageRef::with_layout(file.get(start..start + ps).expect("page in the file"), l).unwrap()
    };
    let metas = [0, 1].map(|i| Meta::read_page_with_layout(page(i).bytes(), l).unwrap());
    // The node of `key` in the tree at `root` (keys in byte order).
    let find = |root: u64, key: &[u8]| -> NodeRef<'_> {
        let mut p = page(root);
        while p.is_branch() {
            let mut next = None;
            for i in 0..p.num_keys() {
                let n = p.node(i).unwrap();
                if i > 0 && n.key().unwrap() > key {
                    break;
                }
                next = Some(n.child_pgno());
            }
            p = page(next.unwrap());
        }
        (0..p.num_keys())
            .map(|i| p.node(i).unwrap())
            .find(|n| n.key() == Some(key))
            .expect("key in the tree")
    };
    let sub_db = |n: NodeRef<'_>| {
        assert!(n.is_subdata(), "not a DB record");
        Db::read(n.inline_data().unwrap()).unwrap()
    };
    let name: &[u8] = if l.db_name_nul { b"t\0" } else { b"t" };
    let named = sub_db(find(metas[pick_newest(&metas)].dbs[1].root, name));
    let sub = sub_db(find(named.root, key));
    // The first item and the item count of every leaf, left to right.
    let mut leaves = Vec::new();
    let mut stack = vec![sub.root];
    while let Some(pgno) = stack.pop() {
        let p = page(pgno);
        if p.is_branch() {
            stack.extend(
                (0..p.num_keys())
                    .rev()
                    .map(|i| p.node(i).unwrap().child_pgno()),
            );
            continue;
        }
        let first = if p.is_leaf2() {
            p.leaf2_key(0, usize::from(p.pad())).unwrap()
        } else {
            p.node(0).unwrap().key().unwrap()
        };
        let index = items.iter().position(|d| d == first).expect("an item");
        leaves.push((index, p.num_keys()));
    }
    let firsts: Vec<usize> = (1..leaves.len())
        .filter(|&i| leaves[i].1 == leaves[0].1 && leaves[i - 1].1 == leaves[0].1)
        .map(|i| leaves[i].0)
        .collect();
    assert!(firsts.len() >= 4, "the sub-DB's leaves: {leaves:?}");
    firsts
}

/// A script (for [`Ops::List`], two cursors, an `MDB_DUPSORT` DB with
/// `dbflags`, `KNOWN_C_BUGS.md` row 56) that puts `n` items of `item_len`
/// bytes under one key, then, from the right, at one boundary between two
/// leaves of the sub-DB at a time ([`sub_db_leaf_firsts`]), replaces an
/// item with `MDB_CURRENT`, the other cursor on the item after the next
/// one, or on the item itself where it is replaced in place (the two
/// rightmost boundaries in a committed and an aborted nested
/// transaction): the leaf's first item by a shorter and a longer item that
/// sort before it, below the branch key above the leaf (`cr`), by one of
/// the same size before it and a shorter one after it, or the leaf's
/// second item (`cp`); or, after deleting the leaf's first item, the
/// previous leaf's last item by the deleted item itself, by items after it
/// of the same size and shorter, at or after the branch key the delete
/// left (`cr`), or by one of the same size before it (`cp`). With
/// `MDB_DUPFIXED` only items of the same size. Then the leftmost leaf's
/// first item, shorter (`cp`). Then every new item is looked up, items
/// between new ones and the old ones are put, and the database is walked.
///
/// Where C's `MDB_CURRENT` leaves the new item in a leaf searches do not
/// look in, the op is `cr`: C deletes the item and puts the new one, which
/// is what rlmdb's `MDB_CURRENT` must write there. Elsewhere both sides
/// put with `MDB_CURRENT` in place. `in_place_other_size`: keep the `cp`
/// ops with items of another size (C 1.0 lacks ITS#8447 and writes those
/// over the old key, see the 1.0 section of `KNOWN_C_BUGS.md`).
pub fn current_dup_outside_leaf_script(
    o: &dyn Oracle,
    dbflags: u32,
    item_len: usize,
    n: usize,
    in_place_other_size: bool,
) -> Vec<String> {
    let fixed = dbflags & MDB_DUPFIXED != 0;
    let key = b"k";
    let item = |j: usize| {
        let mut d = format!("b{j:04}").into_bytes();
        d.resize(item_len.max(d.len()), b'p');
        d
    };
    let items: Vec<Vec<u8>> = (0..n).map(item).collect();
    let cat = |b: &[u8], s: &str| [b, s.as_bytes()].concat();
    let last = |b: &[u8], c: u8| {
        let mut d = b.to_vec();
        *d.last_mut().unwrap() = c;
        d
    };
    let firsts = sub_db_leaf_firsts(o, dbflags, key, &items);
    let k = hex(key);
    let mut ops: Vec<String> = items
        .iter()
        .map(|d| format!("p:{k}:{}:0", hex(d)))
        .collect();
    let mut added = Vec::new();
    let mut between = Vec::new();
    let mut replace = |ops: &mut Vec<String>, j: usize, d: Vec<u8>, routed: bool| {
        if !routed && !in_place_other_size && d.len() != item_len {
            return;
        }
        ops.push(format!("g:0:{MDB_GET_BOTH}:{k}:{}", hex(&items[j])));
        // The other cursor: on the item itself for a replacement in place,
        // which a delete and insert would move it off.
        let other = if routed { j + 2 } else { j };
        if other < n {
            ops.push(format!("g:1:{MDB_GET_BOTH}:{k}:{}", hex(&items[other])));
        }
        if routed {
            ops.push(format!("cr:0:{k}:{}", hex(&d)));
        } else {
            ops.push(format!("cp:0:{k}:{}:{MDB_CURRENT}", hex(&d)));
        }
        ops.push(format!("g:0:{MDB_GET_CURRENT}"));
        added.push(d);
    };
    let del = |ops: &mut Vec<String>, j: usize| {
        ops.push(format!("g:0:{MDB_GET_BOTH}:{k}:{}", hex(&items[j])));
        ops.push("cd:0:0".into());
    };
    let cases: &[usize] = if fixed {
        &[2, 5, 6, 8]
    } else {
        &[0, 1, 2, 3, 4, 5, 6, 7, 8]
    };
    for c in (0..firsts.len()).rev() {
        let f = firsts[c];
        let prev = &items[f - 1];
        let end = match firsts.len() - 1 - c {
            0 => Some("nc"),
            1 => Some("na"),
            _ => None,
        };
        if end.is_some() {
            ops.push("nb".into());
        }
        match cases[c % cases.len()] {
            // The first item, by a shorter one before it: below the key.
            0 => {
                replace(&mut ops, f, cat(&prev[..5], "q"), true);
                between.push(cat(&prev[..5], "r"));
            }
            // The first item, by a longer one before it: below the key.
            1 => replace(&mut ops, f, cat(prev, "q"), true),
            // The first item, by one of the same size before it.
            2 => replace(&mut ops, f, last(&items[f], b'o'), false),
            // The first item, by a shorter one after it.
            3 => replace(&mut ops, f, cat(&items[f][..5], "q"), false),
            // The second item, shorter.
            4 => {
                if f + 2 < n {
                    replace(&mut ops, f + 1, cat(&items[f][..5], "q"), false);
                }
            }
            // The previous leaf's last item, by the deleted first item.
            5 => {
                del(&mut ops, f);
                replace(&mut ops, f - 1, items[f].clone(), true);
            }
            // ... by one of the same size after it.
            6 => {
                del(&mut ops, f);
                replace(&mut ops, f - 1, last(&items[f], b'q'), true);
                between.push(items[f].clone());
            }
            // ... by a shorter one after it.
            7 => {
                del(&mut ops, f);
                replace(&mut ops, f - 1, cat(&items[f][..5], "q"), true);
            }
            // ... by one of the same size before it.
            _ => {
                del(&mut ops, f);
                replace(&mut ops, f - 1, last(prev, b'q'), false);
            }
        }
        if let Some(end) = end {
            ops.push(end.into());
            ops.push(format!("g:0:{MDB_GET_CURRENT}"));
        }
    }
    if !fixed {
        // The leftmost leaf's first item, shorter.
        replace(&mut ops, 0, items[0][..4].to_vec(), false);
    }
    for d in &added {
        ops.push(format!("g:1:{MDB_GET_BOTH}:{k}:{}", hex(d)));
    }
    for d in &between {
        ops.push(format!("p:{k}:{}:0", hex(d)));
    }
    ops.push(format!("g:0:{MDB_FIRST}"));
    ops.extend(std::iter::repeat_n(
        format!("g:0:{MDB_NEXT}"),
        n + between.len(),
    ));
    ops
}

/// A script (for [`Ops::List`], two cursors, a DB with `dbflags`,
/// `KNOWN_C_BUGS.md` row 58) of `MDB_CURRENT` puts with a key that is not
/// the cursor's, for an oracle of `ps`-byte pages with `me_nodemax`
/// `nodemax` and maximum key (and duplicate) size `maxkey`, keys padded
/// to an even length in nodes if `even_keys` (1.0).
///
/// Where C keeps the old key, both sides put with the other key (`cp`):
/// data of the same size in the node; data on overflow pages that fits the
/// pages it replaces (overwritten in place, as they are this
/// transaction's); a duplicate of the same size on a sub-page or in a
/// sub-DB of one leaf (C does not touch the key there). Where C would write
/// the other key over the node (data of another size, to or from overflow
/// pages, onto more overflow pages; another size for a key's only
/// `MDB_DUPSORT` item), C puts with the cursor's key and Rust with the
/// other one (`cw`): rlmdb must write what C writes for a caller that
/// passes the right key. The other keys sort before and after every key,
/// right after the cursor's, are a neighbour's, or as long as keys may be;
/// the other cursor is on the same item or the next key, and looks the
/// other key up afterwards. Two keys are replaced in nested transactions,
/// one committed, one aborted; then keys between the old ones are put and
/// the database is walked.
pub fn current_other_key_script(
    dbflags: u32,
    ps: usize,
    nodemax: usize,
    maxkey: usize,
    even_keys: bool,
) -> Vec<String> {
    const N: usize = 120;
    const KLEN: usize = 12;
    let dupsort = dbflags & MDB_DUPSORT != 0;
    let fixed = dbflags & MDB_DUPFIXED != 0;
    let knode = if even_keys { (KLEN + 1) & !1 } else { KLEN };
    let key = |i: usize| {
        let mut k = format!("k{i:04}").into_bytes();
        k.resize(KLEN, b'q');
        k
    };
    let val = |len: usize, seed: usize| -> Vec<u8> {
        (0..len).map(|b| (b * 7 + seed * 13 + 1) as u8).collect()
    };
    // Items of a 40th of the page: a few dozen in a leaf.
    let vs = (ps / 40).clamp(4, maxkey - 2);
    // A larger item for a key's only MDB_DUPSORT item: within the maximum
    // item size and a node with the key (KNOWN_C_BUGS.md row 38 otherwise).
    let larger_dup = (2 * vs).min(maxkey).min(nodemax - 8 - knode);
    // Duplicates: `tag` and `j` (2 bytes) first, so that other filler bytes
    // keep their place.
    let dup = |tag: u8, j: usize, seed: usize| -> Vec<u8> {
        [&[tag, (j >> 8) as u8, j as u8][..], &val(vs - 3, seed)].concat()
    };
    let subpage_key = |i: usize| dupsort && i % 5 == 2;
    // A sub-DB of one leaf: past the sub-page's limit (about half a page),
    // three quarters of a leaf.
    let hdr = if even_keys { 24 } else { 16 };
    let item_space = if fixed { vs } else { ((8 + vs + 1) & !1) + 2 };
    let n_sub = 3 * (ps - hdr) / 4 / item_space;
    let sub_key = |i: usize| dupsort && i == N / 2 + 1;
    let k = |i: usize| hex(&key(i));
    let mut ops = Vec::new();
    for i in 0..N {
        if sub_key(i) {
            for j in 0..n_sub {
                ops.push(format!("p:{}:{}:0", k(i), hex(&dup(b'e', j, j))));
            }
        } else if subpage_key(i) {
            for j in [10, 20, 30] {
                ops.push(format!("p:{}:{}:0", k(i), hex(&dup(b'd', j, i))));
            }
        } else {
            ops.push(format!("p:{}:{}:0", k(i), hex(&val(vs, i))));
        }
    }
    let mut wrong_n = 0;
    let mut wrong = |i: usize| -> String {
        wrong_n += 1;
        let mut between = key(i);
        between.push(b'x');
        let neighbour = if i + 1 < N { key(i + 1) } else { key(i - 1) };
        hex(&match wrong_n % 5 {
            0 => b"a".to_vec(),
            1 => b"zz".to_vec(),
            2 => between,
            3 => neighbour,
            _ => vec![b'w'; maxkey],
        })
    };
    // Replaces the item of key i the cursors are positioned on (`pos`: the
    // `g` op's tail) by `d`, with the other key in both (`cp`) or on the
    // Rust side only (`cw`); the other cursor on the same item or the next
    // key.
    let mut n = 0;
    let mut replace = |ops: &mut Vec<String>, i: usize, pos: &str, d: &[u8], in_place: bool| {
        n += 1;
        ops.push(format!("g:0:{pos}"));
        if n % 2 == 0 && i + 1 < N {
            ops.push(format!("g:1:{MDB_SET}:{}", k(i + 1)));
        } else {
            ops.push(format!("g:1:{pos}"));
        }
        let w = wrong(i);
        if in_place {
            ops.push(format!("cp:0:{w}:{}:{MDB_CURRENT}", hex(d)));
        } else {
            ops.push(format!("cw:0:{}:{w}:{}", k(i), hex(d)));
        }
        ops.push(format!("g:0:{MDB_GET_CURRENT}"));
        ops.push(format!("g:1:{MDB_SET}:{w}"));
    };
    // Keys across the DB (in a DUPSORT DB a third of them with a sub-page),
    // the last one, the sub-DB's.
    let picks: Vec<usize> = (0..12)
        .map(|c| c * N / 12 + c % 3)
        .chain([N - 1, N / 2 + 1])
        .collect();
    for (c, &i) in picks.iter().enumerate() {
        let end = match picks.len() - c {
            3 => Some("nc"),
            4 => Some("na"),
            _ => None,
        };
        if end.is_some() {
            ops.push("nb".into());
        }
        let set = format!("{MDB_SET}:{}", k(i));
        if sub_key(i) {
            for j in [0, n_sub / 2, n_sub - 1] {
                let pos = format!("{MDB_GET_BOTH}:{}:{}", k(i), hex(&dup(b'e', j, j)));
                replace(&mut ops, i, &pos, &dup(b'e', j, 900 + j), true);
            }
        } else if subpage_key(i) {
            for j in [20, 10, 30] {
                let pos = format!("{MDB_GET_BOTH}:{}:{}", k(i), hex(&dup(b'd', j, i)));
                replace(&mut ops, i, &pos, &dup(b'd', j, 500 + i), true);
            }
        } else if dupsort {
            replace(&mut ops, i, &set, &val(vs, i + 1), true);
            if !fixed {
                if larger_dup > vs {
                    replace(&mut ops, i, &set, &val(larger_dup, i + 2), false);
                    replace(&mut ops, i, &set, &val(larger_dup, i + 3), true);
                }
                replace(&mut ops, i, &set, &val(vs / 2, i + 4), false);
            }
        } else {
            let steps = [
                (vs, true),
                (2 * vs, false),
                (2 * vs, true),
                (vs / 2, false),
                // To overflow pages, three of them.
                (2 * ps + 50, false),
                // In place on them: as many pages, fewer.
                (2 * ps + 10, true),
                (ps + 100, true),
                // More pages: new ones.
                (4 * ps, false),
                // Back into the node.
                (vs, false),
            ];
            for (s, (len, in_place)) in steps.into_iter().enumerate() {
                replace(&mut ops, i, &set, &val(len, i + s), in_place);
            }
        }
        if let Some(end) = end {
            ops.push(end.into());
            ops.push(format!("g:0:{MDB_GET_CURRENT}"));
        }
    }
    let mut more = 0;
    for i in (0..N).step_by(3) {
        let mut b = key(i);
        b.push(b'm');
        ops.push(format!("p:{}:{}:0", hex(&b), hex(&val(vs, i))));
        more += 1;
    }
    ops.push(format!("g:0:{MDB_FIRST}"));
    let items = N + more + if dupsort { n_sub + 2 * (N / 5) } else { 0 };
    ops.extend(std::iter::repeat_n(format!("g:0:{MDB_NEXT}"), items));
    ops
}

/// What one case of [`single_item_on_overflow_pages`] does to the key
/// `k` in a write transaction: an op on `db` (cursor `c` is on `k`'s item)
/// and its result.
type RepairOp = fn(&mut RwTxn<'_>, rlmdb::Dbi, CursorId, &Items) -> Result<(), Error>;

/// A case of [`single_item_on_overflow_pages`]: its name, the op, the
/// op's result, `k`'s items afterwards and the overflow pages left.
type RepairCase = (
    &'static str,
    RepairOp,
    Result<(), MdbError>,
    Vec<Vec<u8>>,
    usize,
);

/// The items of [`single_item_on_overflow_pages`]: `big` is the one C
/// stored on overflow pages, the others are of its size.
pub struct Items {
    pub key: Vec<u8>,
    pub big: Vec<u8>,
    /// Sorts after `big`.
    pub after: Vec<u8>,
    /// Sorts before `big`.
    pub before: Vec<u8>,
}

/// `KNOWN_C_BUGS.md` row 38: C's `MDB_CURRENT` replacing a key's only item in
/// an `MDB_DUPSORT` DB (`dbflags`) by one of `len` bytes that does not fit
/// a node with the key (of `klen` bytes) stores it on overflow pages. The
/// oracle's `writeops` builds that state, between two other keys; rlmdb
/// reads it, then changes it in each way that converts, replaces or
/// deletes the item, starting from a copy of the C-built file each time:
/// a duplicate put converts the item to duplicates (read through its
/// overflow pages, which are freed), an equal one or `MDB_CURRENT`
/// replaces it with a sub-DB (or an inline item if it fits), deletes and
/// `mdb_drop` free the pages, and an `MDB_NODUPDATA` put of the item
/// changes nothing. After each, `md_overflow_pages` is what the pages say,
/// the oracle's page audit passes, and its dump is that of the expected
/// contents written by rlmdb from scratch. A compacting copy of the C-built
/// file passes the audit and dumps the same.
pub fn single_item_on_overflow_pages(o: &dyn Oracle, dbflags: u32, klen: usize, len: usize) {
    let dupfixed = dbflags & MDB_DUPFIXED != 0;
    let fill = |seed: u8| -> Vec<u8> {
        (0..len)
            .map(|b| (b as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    };
    let items = Items {
        key: vec![b'k'; klen],
        big: fill(0x41),
        after: fill(0x42),
        before: if dupfixed { vec![0; len] } else { vec![0] },
    };
    let (ka, kz) = (vec![b'a'; klen], vec![b'z'; klen]);
    let dbs = DatabaseFlags::from_bits_retain(dbflags);
    let new_dir = || {
        let dir = tempfile::tempdir().unwrap();
        if let Some(file) = o.data_file() {
            std::fs::write(dir.path().join("data.mdb"), file).unwrap();
        }
        dir
    };
    let open = |dir: &Path| {
        let mut b = Environment::builder();
        b.format(o.format()).max_dbs(8).map_size(64 << 20);
        if let (None, Some(ps)) = (o.data_file(), o.page_size()) {
            b.page_size(ps);
        }
        b.open(dir).unwrap()
    };
    let overflow_pages = |dir: &Path| {
        let env = open(dir);
        let txn = env.begin_ro_txn().unwrap();
        let db = txn.open_db(Some("t")).unwrap();
        txn.stat(db).unwrap().overflow_pages
    };
    let audit = |dir: &Path, what: &str| {
        let (code, stdout, stderr) = o.check_freelist(dir);
        assert_eq!(code, 0, "{what}: page audit:\n{stdout}\n{stderr}");
    };
    // The expected contents, written by rlmdb from scratch.
    let expected_dump = |k_items: &[Vec<u8>]| {
        let dir = new_dir();
        let env = open(dir.path());
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn
            .create_db(Some("t"), dbs | DatabaseFlags::CREATE)
            .unwrap();
        txn.put(db, &ka, b"aa", WriteFlags::empty()).unwrap();
        for d in k_items {
            txn.put(db, &items.key, d, WriteFlags::empty()).unwrap();
        }
        txn.put(db, &kz, b"zz", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        drop(env);
        o.dump(dir.path(), "t")
    };

    // The state, as C leaves it.
    let state = new_dir();
    let mut crun = CRun::start(o, state.path(), "t", dbflags, 1, 64 << 20);
    for op in [
        format!("p:{}:{}:0", hex(&ka), hex(b"aa")),
        format!("p:{}:{}:0", hex(&items.key), hex(b"first")),
        format!("p:{}:{}:0", hex(&kz), hex(b"zz")),
        format!("g:0:{MDB_SET}:{}", hex(&items.key)),
        format!("cp:0:{}:{}:{MDB_CURRENT}", hex(&items.key), hex(&items.big)),
    ] {
        let line = crun.step(&op).expect("C writeops");
        assert!(line.split(' ').nth(1) == Some("rc=0"), "{op}: {line}");
    }
    let (commit, err) = crun.finish();
    assert_eq!(commit.as_deref(), Some("commit rc=0"), "{err}");
    audit(state.path(), "C's state");
    let ovpages = overflow_pages(state.path());
    assert!(ovpages > 0, "C stored the item on overflow pages");
    {
        let env = open(state.path());
        let txn = env.begin_ro_txn().unwrap();
        let db = txn.open_db(Some("t")).unwrap();
        assert_eq!(txn.get(db, &items.key).unwrap(), Some(&items.big[..]));
        let mut c = txn.cursor(db).unwrap();
        c.get(Some(&items.key), Some(&items.big), CursorOp::GetBoth)
            .unwrap()
            .expect("MDB_GET_BOTH finds the item");
        assert_eq!(c.count().unwrap(), 1);
    }
    let c_dump = o.dump(state.path(), "t");
    assert_eq!(c_dump, expected_dump(std::slice::from_ref(&items.big)));

    let mut cases: Vec<RepairCase> = vec![
        (
            "a second duplicate",
            |t, db, _, it| t.put(db, &it.key, &it.after, WriteFlags::empty()),
            Ok(()),
            vec![items.big.clone(), items.after.clone()],
            0,
        ),
        (
            "a duplicate that sorts first",
            |t, db, _, it| t.put(db, &it.key, &it.before, WriteFlags::empty()),
            Ok(()),
            vec![items.before.clone(), items.big.clone()],
            0,
        ),
        (
            "MDB_NODUPDATA of the item",
            |t, db, _, it| t.put(db, &it.key, &it.big, WriteFlags::NO_DUP_DATA),
            Err(MdbError::KeyExist),
            vec![items.big.clone()],
            ovpages,
        ),
        (
            "the item again",
            |t, db, _, it| t.put(db, &it.key, &it.big, WriteFlags::empty()),
            Ok(()),
            vec![items.big.clone()],
            0,
        ),
        (
            "MDB_APPENDDUP of a smaller item",
            |t, db, _, it| t.put(db, &it.key, &it.before, WriteFlags::APPEND_DUP),
            Err(MdbError::KeyExist),
            vec![items.big.clone()],
            0,
        ),
        (
            "MDB_CURRENT of another item of the size",
            |t, _, c, it| t.cursor_put(c, &it.key, &it.after, WriteFlags::CURRENT),
            Ok(()),
            vec![items.after.clone()],
            0,
        ),
        (
            "mdb_del of the key",
            |t, db, _, it| t.del(db, &it.key, None).map(|_| ()),
            Ok(()),
            vec![],
            0,
        ),
        (
            "mdb_del of the item",
            |t, db, _, it| t.del(db, &it.key, Some(&it.big)).map(|_| ()),
            Ok(()),
            vec![],
            0,
        ),
        (
            "mdb_cursor_del",
            |t, _, c, _| t.cursor_del(c, WriteFlags::empty()),
            Ok(()),
            vec![],
            0,
        ),
    ];
    if dupfixed {
        cases.push((
            "MDB_MULTIPLE starting with the item",
            |t, _, c, it| {
                let both = [it.big.clone(), it.after.clone()].concat();
                let n =
                    t.cursor_put_multiple(c, &it.key, &both, it.big.len(), WriteFlags::empty())?;
                assert_eq!(n, 2, "MDB_MULTIPLE stored both");
                Ok(())
            },
            Ok(()),
            vec![items.big.clone(), items.after.clone()],
            0,
        ));
        cases.push((
            "MDB_CURRENT of another size",
            |t, _, c, it| t.cursor_put(c, &it.key, b"tiny", WriteFlags::CURRENT),
            Err(MdbError::BadValSize),
            vec![items.big.clone()],
            ovpages,
        ));
    } else {
        cases.push((
            "MDB_CURRENT of a small item",
            |t, _, c, it| t.cursor_put(c, &it.key, b"tiny", WriteFlags::CURRENT),
            Ok(()),
            vec![b"tiny".to_vec()],
            0,
        ));
    }
    for (name, op, rc, k_items, left) in cases {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(state.path().join("data.mdb"), dir.path().join("data.mdb")).unwrap();
        {
            let env = open(dir.path());
            let mut txn = env.begin_rw_txn().unwrap();
            let db = txn.create_db(Some("t"), dbs).unwrap();
            let c = txn.open_cursor(db).unwrap();
            txn.cursor_get(c, Some(&items.key), None, CursorOp::Set)
                .unwrap()
                .unwrap();
            let got = op(&mut txn, db, c, &items).map_err(|e| match e {
                Error::Mdb(e) => e,
                e => panic!("{name}: {e:?}"),
            });
            assert_eq!(got, rc, "{name}");
            txn.close_cursor(c);
            txn.commit().unwrap();
        }
        assert_eq!(
            overflow_pages(dir.path()),
            left,
            "{name}: md_overflow_pages"
        );
        audit(dir.path(), name);
        assert_eq!(o.dump(dir.path(), "t"), expected_dump(&k_items), "{name}");
    }
    {
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(state.path().join("data.mdb"), dir.path().join("data.mdb")).unwrap();
        let env = open(dir.path());
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(Some("t"), dbs).unwrap();
        txn.clear_db(db).unwrap();
        txn.commit().unwrap();
        drop(env);
        assert_eq!(overflow_pages(dir.path()), 0, "mdb_drop: md_overflow_pages");
        audit(dir.path(), "mdb_drop");
    }
    let copy = tempfile::tempdir().unwrap();
    open(state.path()).copy_to_path(copy.path(), true).unwrap();
    audit(copy.path(), "compacting copy");
    assert_eq!(o.dump(copy.path(), "t"), c_dump, "compacting copy");
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
            // sub-cursor (KNOWN_C_BUGS.md row 14).
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
                // KNOWN_C_BUGS.md row 18: C left these cursors' sub-cursors
                // stale; put them on Rust's (correct) item, which makes C
                // set the sub-cursor up again.
                for n in stale {
                    if let Some((k, d)) = item_of(&rl, n) {
                        self.heal.push_back(format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}"));
                    }
                }
                eprintln!("NOTE: {tag}: stale C sub-cursor after step {i} (known bug, row 18)");
                self.prev = Some((op, rl));
                self.i += 1;
                return true;
            }
        }
        if cl != rl {
            let prev = self.prev.as_ref().map(|(_, l)| l.as_str());
            if let Some(n) = stale_eof(&op, prev, &cl, &rl) {
                // KNOWN_C_BUGS.md row 28: C's cursor kept a C_EOF from before
                // and stopped although items were added after it; put it
                // where Rust's (correctly) went.
                if let Some((k, d)) = item_of(&rl, n) {
                    self.heal.push_back(if self.dupsort {
                        format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}")
                    } else {
                        format!("g:{n}:{MDB_SET}:{k}")
                    });
                }
                eprintln!("NOTE: {tag}: stale C_EOF in C at step {i} (known bug, row 28)");
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
                    // KNOWN_C_BUGS.md row 14: C's cached count for this cursor
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
    if let Some(file) = o.data_file() {
        std::fs::write(cdir.path().join("data.mdb"), &file).unwrap();
        std::fs::write(rdir.path().join("data.mdb"), &file).unwrap();
    }
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
        let (cd, rd) = (o.dump(cdir.path(), "t"), o.dump(rdir.path(), "t"));
        if cd != rd {
            // Dumped once more: a dump that changes although nothing wrote
            // the environment in between was misread, not miswritten.
            let redump = |side: &str, path: &Path, first: &str| {
                let second = o.dump(path, "t");
                if second == first {
                    format!("{side} dumped again: the same\n")
                } else {
                    format!(
                        "{side} dumped again: different\n{}",
                        dump_diff(("first", first), ("second", &second))
                    )
                }
            };
            let again = redump("C", cdir.path(), &cd) + &redump("Rust", rdir.path(), &rd);
            let (c, r) = (cdir.keep(), rdir.keep());
            panic!(
                "{tag}: dumps differ (environments kept: C {}, Rust {})\n{}{again}",
                c.display(),
                r.display(),
                dump_diff(("C", &cd), ("Rust", &rd))
            );
        }
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
        // KNOWN_C_BUGS.md row 14: C's MDB_NODUPDATA cursor delete subtracts a
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
        eprintln!("NOTE: {tag}: C miscounted entries (known bug, row 14)");
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

/// Where two dumps (the data lines of `mdb_dump`: odd lines keys, even
/// lines values) differ, for a failure message: the line counts, then the
/// lines between the common head and tail, at most 16 per side. Where both
/// sides have a line there, it is shown from a little before the first
/// character that differs (with its byte offset in the key or value),
/// otherwise from its start; 160 characters at most.
fn dump_diff((a_name, a): (&str, &str), (b_name, b): (&str, &str)) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (da, db) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let mut out = format!(
        "{a_name} {} lines, {b_name} {}; the same in the first {head} and the last {tail}\n",
        a.len(),
        b.len()
    );
    for (name, lines, other) in [(a_name, da, db), (b_name, db, da)] {
        out += &format!("{name}, {} lines:\n", lines.len());
        for (i, line) in lines.iter().enumerate().take(16) {
            // The dump's lines are a space and hex digits.
            let from = match other.get(i) {
                Some(o) => {
                    let k = line
                        .bytes()
                        .zip(o.bytes())
                        .take_while(|(x, y)| x == y)
                        .count();
                    let byte = k.saturating_sub(1) / 2;
                    out += &format!("{:>7} differs at byte {byte}:", head + i + 1);
                    // From 20 bytes before it: char 1 + 2 * byte.
                    match byte.saturating_sub(20) {
                        0 => 0,
                        start => 1 + 2 * start,
                    }
                }
                None => {
                    out += &format!("{:>7}:", head + i + 1);
                    0
                }
            };
            let cut = line.get(from..(from + 160).min(line.len())).unwrap_or("");
            out += &format!(
                " {}{cut}{} ({} chars)\n",
                if from > 0 { "..." } else { "" },
                if from + 160 < line.len() { "..." } else { "" },
                line.len()
            );
        }
    }
    out
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
        "g" | "cp" | "cd" | "cr" | "cw" => f[1].parse().ok(),
        _ => None,
    }
}

/// `KNOWN_C_BUGS.md` row 18: after a delete (`d`, `cd`) moved cursors onto
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

/// `KNOWN_C_BUGS.md` row 28: C's cursor kept a `C_EOF` from an earlier move
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
/// page (`KNOWN_C_BUGS.md` row 13): `MDB_CURRENT` on a cursor that is not on
/// an item. Rust reports `MDB_NOTFOUND`.
fn known_c_bug(op: &str, cl: &str, rl: &str) -> bool {
    op.starts_with("cp:")
        && op.ends_with(&format!(":{MDB_CURRENT}"))
        && rl.starts_with("cp rc=-30798")
        && cl != rl
}
