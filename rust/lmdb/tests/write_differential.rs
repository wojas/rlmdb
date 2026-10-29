//! Differential tests of the write path against C: the same script of
//! puts, deletes and cursor moves on several cursors of one write
//! transaction runs in C (`tests/c/writeops.c`) and in Rust (the
//! handle-based cursor API), each in a fresh environment. After every step
//! the position of every cursor must agree, which checks the cursor fixups
//! after splits, merges, rebalances, sub-page growth and sub-DB promotion;
//! the committed databases must be identical and pass the C page audit.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use common::{hex, CTools, Rng};
use lmdb::consts::*;
use lmdb::{
    CursorId, CursorOp, DatabaseFlags, Environment, EnvironmentFlags, Error, RwTxn, WriteFlags,
};

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
    db: lmdb::Dbi,
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
    fn start(c: &CTools, path: &Path, name: &str, dbflags: u32, ncur: usize) -> CRun {
        let mut child = Command::new(c.bin("writeops"))
            .arg(path)
            .args([name, &dbflags.to_string(), &ncur.to_string()])
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
struct Profile {
    dbflags: u32,
    keys: usize,
    dupfixed_size: Option<usize>,
    big: bool,
    /// Keys of 200..480 bytes: deep trees, branch and root splits, merges.
    long_keys: bool,
}

impl Profile {
    fn new(dbflags: u32, keys: usize) -> Profile {
        Profile {
            dbflags,
            keys,
            dupfixed_size: None,
            big: false,
            long_keys: false,
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
/// cursor left by a failed `MDB_SET`/`MDB_GET_BOTH` on a key with
/// duplicates is repositioned with `MDB_FIRST` before anything else uses it
/// (a correct client would not move it; C would use the stale sub-cursor,
/// `KNOWN_C_BUGS.md` #6/#14).
struct Gen<'a> {
    rng: &'a mut Rng,
    p: &'a Profile,
    ncur: usize,
    prefill: usize,
    heal: Option<usize>,
}

impl Gen<'_> {
    fn next(&mut self, prev: Option<(&str, &str)>) -> String {
        if let Some((op, line)) = prev {
            let f: Vec<&str> = op.split(':').collect();
            let failed = line.split_whitespace().nth(1) != Some("rc=0");
            if f[0] == "g"
                && failed
                && self.p.dbflags & MDB_DUPSORT != 0
                && matches!(f[2].parse::<u32>(), Ok(2 | 3 | 15 | 16))
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
        let op = gen_op(rng, p, self.ncur);
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
            format!("p:{}:{}:{f}", hex(&gen_key(rng, p)), hex(&gen_data(rng, p)))
        }
        5..=8 => {
            let mut f = put_flags[rng.below(put_flags.len())];
            if f == MDB_CURRENT && p.dbflags & MDB_DUPSORT != 0 {
                // The new item would have to sort like the old one.
                f = 0;
            }
            format!(
                "cp:{c}:{}:{}:{f}",
                hex(&gen_key(rng, p)),
                hex(&gen_data(rng, p))
            )
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
enum Ops<'a> {
    Gen(Gen<'a>, usize),
    List(Vec<String>),
}

/// Outcome of a round.
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// Every step and the committed result agreed.
    Same,
    /// C died or took a known-buggy path at this step; all before agreed.
    CStopped(usize),
}

/// Runs one round in lock step: each op goes to C, then to Rust, and the
/// two outputs must be equal.
fn run_round(c: &CTools, p: &Profile, ncur: usize, mut ops: Ops<'_>, tag: &str) -> Round {
    let cdir = tempfile::tempdir().unwrap();
    let rdir = tempfile::tempdir().unwrap();
    let dupsort = p.dbflags & MDB_DUPSORT != 0;
    let env = Environment::builder()
        .map_size(64 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(rdir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(
            Some("t"),
            DatabaseFlags::from_bits_retain(p.dbflags) | DatabaseFlags::CREATE,
        )
        .unwrap();
    let cur: Vec<CursorId> = (0..ncur).map(|_| txn.open_cursor(db).unwrap()).collect();
    let mut crun = CRun::start(c, cdir.path(), "t", p.dbflags, ncur);
    let mut history: Vec<String> = Vec::new();
    let mut prev: Option<(String, String)> = None;
    // The unmasked outputs of the previous step, for spotting stale counts.
    let mut prev_raw: Option<(String, String)> = None;
    let mut heal: std::collections::VecDeque<String> = Default::default();
    let mut i = 0;
    loop {
        let op = match &mut ops {
            Ops::Gen(g, n) => {
                if i >= *n {
                    break;
                }
                match heal.pop_front() {
                    Some(op) => op,
                    None => g.next(prev.as_ref().map(|(o, l)| (o.as_str(), l.as_str()))),
                }
            }
            Ops::List(l) => match l.get(i) {
                Some(o) => o.clone(),
                None => break,
            },
        };
        if let Ok(dir) = std::env::var("WRITEOPS_DUMP") {
            history.push(op.clone());
            let f = format!("{dir}/script-{tag}-{}-{ncur}.txt", p.dbflags);
            std::fs::write(
                &f,
                history.iter().map(|o| format!("{o}\n")).collect::<String>(),
            )
            .unwrap();
        }
        if let (Some(n), Some((pc, pr)), "cd") = (
            cursor_of(&op),
            &prev_raw,
            op.split(':').next().unwrap_or(""),
        ) {
            if count_of(pc, n) != count_of(pr, n) {
                // KNOWN_C_BUGS.md #14: C's cached count for this cursor is
                // stale; its delete would act on it (and may lose data).
                eprintln!("NOTE: {tag}: stale C count before step {i}");
                return Round::CStopped(i);
            }
        }
        let Some(cl) = crun.step(&op) else {
            let (_, err) = crun.finish();
            assert!(
                err.contains("Assertion") || err.is_empty(),
                "{tag}: C writeops failed at step {i}: {err}"
            );
            eprintln!(
                "NOTE: {tag}: C died at step {i} {}: {}",
                &op[..op.len().min(60)],
                err.trim()
            );
            return Round::CStopped(i);
        };
        let rl_raw = rust_step(&mut txn, db, &cur, &op, dupsort);
        let rl = mask_counts(&op, &rl_raw);
        if matches!(ops, Ops::Gen(..)) {
            // A cursor whose cached duplicate count C let go stale is put
            // back on its item with MDB_GET_BOTH, which makes C rebuild its
            // sub-cursor (KNOWN_C_BUGS.md #14).
            for n in 0..ncur {
                if count_of(&cl, n) != count_of(&rl_raw, n) {
                    if let Some((k, d)) = item_of(&rl_raw, n) {
                        heal.push_back(format!("g:{n}:{MDB_GET_BOTH}:{k}:{d}"));
                    }
                }
            }
        }
        prev_raw = Some((cl.clone(), rl_raw));
        let cl = mask_counts(&op, &cl);
        if cl != rl {
            if known_c_bug(&op, &cl, &rl) {
                // C's result came from an unchecked node read; the states
                // agree only if C changed nothing.
                if cl.split_once(" | ").map(|x| x.1) != rl.split_once(" | ").map(|x| x.1) {
                    eprintln!("NOTE: {tag}: C went its own way at step {i}");
                    return Round::CStopped(i);
                }
            } else {
                panic!(
                    "{tag}: step {i} {:?} differs\n  C:    {cl}\n  Rust: {rl}",
                    &op[..op.len().min(200)]
                );
            }
        }
        prev = Some((op, cl));
        i += 1;
    }
    let (c_commit, err) = crun.finish();
    for id in cur {
        txn.close_cursor(id);
    }
    let r_commit = format!("commit rc={}", code(&txn.commit()));
    assert_eq!(
        c_commit.as_deref(),
        Some(r_commit.as_str()),
        "{tag}: commit; C stderr {err}"
    );
    // Same committed content, valid page usage on both.
    assert!(
        c.dump(cdir.path(), Some("t")) == c.dump(rdir.path(), Some("t")),
        "{tag}: dumps differ"
    );
    let out = c.peer(&["check-freelist", rdir.path().to_str().unwrap()]);
    assert_eq!(
        out.code, 0,
        "{tag}: check-freelist of Rust:\n{}\n{}",
        out.stdout, out.stderr
    );
    let out = c.peer(&["check-freelist", cdir.path().to_str().unwrap()]);
    if out.code != 0 {
        // KNOWN_C_BUGS.md #14: C's MDB_NODUPDATA cursor delete subtracts a
        // stale duplicate count, so only md_entries may be off.
        let only_entries = out.stderr.lines().all(|l| {
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
            "{tag}: check-freelist of C:\n{}\n{}",
            out.stdout, out.stderr
        );
        eprintln!("NOTE: {tag}: C miscounted entries (known bug #14)");
    }
    Round::Same
}

fn run_profile(p: Profile, seed: u64, rounds: usize, n: usize) {
    let c = require_c!();
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
        };
        let tag = format!("{seed:x}-{round}");
        if run_round(c, &p, ncur, Ops::Gen(gen, n + n / 2), &tag) == Round::Same {
            full += 1;
        }
    }
    eprintln!("{full} of {rounds} rounds compared in full");
    assert!(
        full * 2 >= rounds,
        "too few rounds compared in full: {full} of {rounds}"
    );
}

#[test]
fn diff_plain() {
    let p = Profile {
        big: true,
        ..Profile::new(0, 300)
    };
    run_profile(p, 0xd1, 12, 600);
}

#[test]
fn diff_plain_small_keyspace() {
    let p = Profile {
        big: true,
        ..Profile::new(0, 40)
    };
    run_profile(p, 0xd2, 12, 400);
}

#[test]
fn diff_intkey() {
    run_profile(Profile::new(MDB_INTEGERKEY, 500), 0xd3, 8, 600);
}

#[test]
fn diff_revkey() {
    let p = Profile {
        big: true,
        ..Profile::new(MDB_REVERSEKEY, 200)
    };
    run_profile(p, 0xd8, 6, 600);
}

#[test]
fn diff_dupsort() {
    run_profile(Profile::new(MDB_DUPSORT, 30), 0xd4, 12, 800);
}

#[test]
fn diff_dupsort_wide() {
    run_profile(Profile::new(MDB_DUPSORT, 200), 0xd5, 8, 800);
}

#[test]
fn diff_revdup() {
    run_profile(Profile::new(MDB_DUPSORT | MDB_REVERSEDUP, 25), 0xd9, 8, 800);
}

#[test]
fn diff_dupfixed() {
    let p = Profile {
        dupfixed_size: Some(16),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
    };
    run_profile(p, 0xd6, 12, 800);
}

#[test]
fn diff_intdup() {
    let p = Profile {
        dupfixed_size: Some(8),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP, 20)
    };
    run_profile(p, 0xd7, 10, 800);
}

#[test]
fn diff_deep_plain() {
    let p = Profile {
        big: true,
        long_keys: true,
        ..Profile::new(0, 3000)
    };
    run_profile(p, 0xda, 3, 3000);
}

#[test]
fn diff_deep_dupsort() {
    let p = Profile {
        long_keys: true,
        ..Profile::new(MDB_DUPSORT, 600)
    };
    run_profile(p, 0xdb, 3, 3000);
}

#[test]
fn diff_deep_dupfixed() {
    let p = Profile {
        long_keys: true,
        dupfixed_size: Some(24),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 400)
    };
    run_profile(p, 0xdc, 3, 3000);
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

/// C results that come from reading an unchecked node past the end of a
/// page (`KNOWN_C_BUGS.md` #13): `MDB_CURRENT` on a cursor that is not on
/// an item. Rust reports `MDB_NOTFOUND`.
fn known_c_bug(op: &str, cl: &str, rl: &str) -> bool {
    op.starts_with("cp:")
        && op.ends_with(&format!(":{MDB_CURRENT}"))
        && rl.starts_with("cp rc=-30798")
        && cl != rl
}

/// Replays a dumped script (`WRITEOPS_REPLAY=<file>`, written with
/// `WRITEOPS_DUMP=<dir>`).
#[test]
#[ignore]
fn replay() {
    let c = require_c!();
    let Ok(file) = std::env::var("WRITEOPS_REPLAY") else {
        return;
    };
    let name = Path::new(&file)
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let parts: Vec<&str> = name.rsplitn(3, '-').collect();
    let ncur: usize = parts[0].parse().unwrap();
    let dbflags: u32 = parts[1].parse().unwrap();
    let ops: Vec<String> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let p = Profile::new(dbflags, 0);
    let r = run_round(c, &p, ncur, Ops::List(ops), "replay");
    eprintln!("{r:?}");
}
