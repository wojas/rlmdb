//! Shared helpers for the integration tests: building the C LMDB library and
//! tools, generating databases with them, parsing their output, and checking
//! a Rust view of a database against an expected list of pairs.
//!
//! The C sources are copied from `libraries/liblmdb` into a private build
//! directory under `CARGO_TARGET_TMPDIR`, so the source tree stays clean and
//! concurrent builds (other test binaries, the interop crate) do not clash.
//! If the build fails, C-dependent tests print a message and return early.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::OnceLock;

use lmdb_core::{Dbi, Environment, RoTxn, Transaction};

pub mod dataset;

/// A key/data pair as owned bytes.
pub type Pair = (Vec<u8>, Vec<u8>);

/// The built C library and programs.
pub struct CTools {
    pub dir: PathBuf,
}

/// Returns the C tools, building them on first use; `None` (after printing
/// why) if they cannot be built.
pub fn c_tools() -> Option<&'static CTools> {
    static TOOLS: OnceLock<Option<CTools>> = OnceLock::new();
    TOOLS
        .get_or_init(|| match build_c_tools() {
            Ok(dir) => {
                eprintln!(
                    "C LMDB tools in {} (OS page size {})",
                    dir.display(),
                    os_page_size()
                );
                Some(CTools { dir })
            }
            Err(e) => {
                eprintln!("NOTE: skipping C compatibility checks: cannot build C LMDB: {e}");
                None
            }
        })
        .as_ref()
}

/// `let c = require_c!();` returns from the test if the C tools are missing.
#[macro_export]
macro_rules! require_c {
    () => {
        match common::c_tools() {
            Some(t) => t,
            None => {
                eprintln!("skipped: C LMDB tools unavailable");
                return;
            }
        }
    };
}

pub fn os_page_size() -> usize {
    let out = Command::new("getconf").arg("PAGESIZE").output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4096)
}

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn run_ok(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn build_c_tools() -> Result<PathBuf, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("../../libraries/liblmdb");
    let helpers = manifest.join("tests/c");
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in [&src, &helpers] {
        for e in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let p = e.map_err(|e| e.to_string())?.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "Makefile" || name.ends_with(".c") || name.ends_with(".h") {
                files.push(p);
            }
        }
    }
    files.sort();
    let targets = [
        "liblmdb.a",
        "mdb_stat",
        "mdb_dump",
        "mdb_load",
        "mdb_copy",
        "mtest",
        "mtest2",
        "mtest3",
        "mtest4",
        "mtest5",
    ];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for t in targets {
        fnv(&mut h, t.as_bytes());
    }
    for f in &files {
        fnv(&mut h, f.file_name().unwrap().to_string_lossy().as_bytes());
        fnv(&mut h, &fs::read(f).map_err(|e| e.to_string())?);
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let dir = base.join(format!("clmdb-{h:016x}"));
    if dir.join(".ok").exists() {
        return Ok(dir);
    }
    let tmp = base.join(format!("clmdb-{h:016x}.tmp{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    for f in &files {
        fs::copy(f, tmp.join(f.file_name().unwrap())).map_err(|e| e.to_string())?;
    }
    // mtest6 calls mdb_dkey(), which liblmdb only defines with MDB_DEBUG.
    fs::write(
        tmp.join("dkey_shim.h"),
        "#include \"lmdb.h\"\nchar *mdb_dkey(MDB_val *key, char *buf);\n",
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        tmp.join("dkey_shim.c"),
        "#include <stdio.h>\n#include \"lmdb.h\"\n\
         char *mdb_dkey(MDB_val *key, char *buf) {\n\
           char *p = buf; unsigned char *c = key->mv_data; size_t i;\n\
           buf[0] = 0; for (i = 0; i < key->mv_size && i < 511; i++) p += sprintf(p, \"%02x\", c[i]);\n\
           return buf; }\n",
    )
    .map_err(|e| e.to_string())?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    run_ok(
        Command::new("make")
            .current_dir(&tmp)
            .arg(format!("CC={cc}"))
            .args(targets),
    )?;
    run_ok(Command::new(&cc).current_dir(&tmp).args([
        "-pthread",
        "-O2",
        "-g",
        "-include",
        "dkey_shim.h",
        "-o",
        "mtest6",
        "mtest6.c",
        "dkey_shim.c",
        "liblmdb.a",
    ]))?;
    for helper in ["holder", "cursorops", "writeops"] {
        run_ok(Command::new(&cc).current_dir(&tmp).args([
            "-pthread",
            "-O2",
            "-g",
            "-I.",
            "-o",
            helper,
            &format!("{helper}.c"),
            "liblmdb.a",
        ]))?;
    }
    fs::write(tmp.join(".ok"), b"ok").map_err(|e| e.to_string())?;
    if fs::rename(&tmp, &dir).is_err() {
        // Another process finished first; use its build.
        let _ = fs::remove_dir_all(&tmp);
        if !dir.join(".ok").exists() {
            return Err("concurrent build left no usable directory".into());
        }
    }
    Ok(dir)
}

/// Builds the interop C peer (`rust/interop/c/cpeer*.c`, see
/// `rust/interop/README.md`) against the C library of `tools`, once per
/// version of its sources. Built here rather than through the `interop`
/// crate so that these tests do not depend on that crate building.
fn build_cpeer(tools: &CTools) -> Result<PathBuf, String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("../interop/c");
    let names = [
        "cpeer.h",
        "cpeer.c",
        "cpeer_data.c",
        "cpeer_loops.c",
        "cpeer_misc.c",
        "cpeer_pagewalk.c",
        "dataset.c",
    ];
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    fnv(&mut h, tools.dir.to_string_lossy().as_bytes());
    for n in names {
        fnv(&mut h, n.as_bytes());
        fnv(
            &mut h,
            &fs::read(src.join(n)).map_err(|e| format!("{n}: {e}"))?,
        );
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let dir = base.join(format!("cpeer-{h:016x}"));
    let bin = dir.join("cpeer");
    if dir.join(".ok").exists() {
        return Ok(bin);
    }
    let tmp = base.join(format!("cpeer-{h:016x}.tmp{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut cmd = Command::new(&cc);
    cmd.current_dir(&tmp)
        .args(["-pthread", "-O2", "-g", "-I"])
        .arg(&tools.dir)
        .arg("-o")
        .arg(tmp.join("cpeer"));
    for n in names.iter().filter(|n| n.ends_with(".c")) {
        cmd.arg(src.join(n));
    }
    cmd.arg(tools.dir.join("liblmdb.a"));
    run_ok(&mut cmd)?;
    fs::write(tmp.join(".ok"), b"ok").map_err(|e| e.to_string())?;
    if fs::rename(&tmp, &dir).is_err() {
        let _ = fs::remove_dir_all(&tmp);
        if !dir.join(".ok").exists() {
            return Err("concurrent cpeer build left no usable directory".into());
        }
    }
    Ok(bin)
}

/// Output of a `cpeer` command: exit code, stdout, stderr.
#[derive(Debug)]
pub struct PeerOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl PeerOutput {
    /// The value of `key=` on the first line that has it.
    pub fn field(&self, key: &str) -> Option<String> {
        let prefix = format!("{key}=");
        self.stdout
            .split_whitespace()
            .find_map(|t| t.strip_prefix(&prefix).map(str::to_owned))
    }
}

impl CTools {
    pub fn bin(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The interop C peer, built on first use.
    pub fn cpeer(&self) -> PathBuf {
        static CPEER: OnceLock<Result<PathBuf, String>> = OnceLock::new();
        match CPEER.get_or_init(|| build_cpeer(self)) {
            Ok(p) => p.clone(),
            Err(e) => panic!("cannot build cpeer: {e}"),
        }
    }

    /// Runs `cpeer <args>` and returns its output whatever the exit code.
    pub fn peer(&self, args: &[&str]) -> PeerOutput {
        let out = Command::new(self.cpeer())
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("cpeer: {e}"));
        PeerOutput {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs `cpeer <args>` and panics unless it exits 0.
    pub fn peer_ok(&self, args: &[&str]) -> PeerOutput {
        let out = self.peer(args);
        assert_eq!(
            out.code, 0,
            "cpeer {args:?} failed:\n{}\n{}",
            out.stdout, out.stderr
        );
        out
    }

    /// Runs a C program and returns its output, panicking on failure.
    pub fn run(&self, prog: &str, args: &[&str], cwd: Option<&Path>) -> Output {
        let mut cmd = Command::new(self.bin(prog));
        cmd.args(args);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let out = cmd.output().unwrap_or_else(|e| panic!("{prog}: {e}"));
        assert!(
            out.status.success(),
            "{prog} {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    /// Runs `mtestN` in `dir` (it always uses `./testdb`) and returns the
    /// environment path.
    pub fn mtest(&self, prog: &str, dir: &Path) -> PathBuf {
        let env = dir.join("testdb");
        fs::create_dir_all(&env).unwrap();
        self.run(prog, &[], Some(dir));
        env
    }

    /// `mdb_dump [-s name] path`, parsed into its key/data pairs.
    pub fn dump(&self, env: &Path, sub: Option<&str>) -> Vec<Pair> {
        let path = env.to_str().unwrap();
        let out = match sub {
            Some(s) => self.run("mdb_dump", &["-s", s, path], None),
            None => self.run("mdb_dump", &[path], None),
        };
        parse_dump(&String::from_utf8(out.stdout).unwrap())
    }

    /// `mdb_stat <args> path` as text.
    ///
    /// With `-r`, `mdb_stat` exits with status 1 whenever it printed a
    /// reader (it returns `fputs`'s positive result as its error code), so
    /// the exit status is only checked without `-r`.
    pub fn stat_text(&self, env: &Path, args: &[&str]) -> String {
        let mut a: Vec<&str> = args.to_vec();
        a.push(env.to_str().unwrap());
        if !args.iter().any(|x| x.starts_with("-r")) {
            return String::from_utf8(self.run("mdb_stat", &a, None).stdout).unwrap();
        }
        let out = Command::new(self.bin("mdb_stat"))
            .args(&a)
            .output()
            .unwrap();
        assert!(
            out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// Loads `pairs` into database `name` (main DB if `None`) of the
    /// environment at `env` with `mdb_load`. `flags` are header keywords
    /// such as `"dupsort"`, `"integerkey"`.
    pub fn load(&self, env: &Path, name: Option<&str>, flags: &[&str], pairs: &[Pair]) {
        fs::create_dir_all(env).unwrap();
        // mdb_load (mdb_load.c line 402) resets `flags` before using the
        // first section's header, so a first section's dupsort/integerkey/…
        // would be lost. Lead with an empty section for the main DB.
        let mut input = String::new();
        if !flags.is_empty() {
            assert!(name.is_some(), "flags need a named DB (mdb_load quirk)");
            input.push_str("VERSION=3\nformat=bytevalue\ntype=btree\nmapsize=268435456\n");
            input.push_str("maxreaders=126\nHEADER=END\nDATA=END\n");
        }
        input.push_str("VERSION=3\nformat=bytevalue\n");
        if let Some(n) = name {
            input.push_str(&format!("database={n}\n"));
        }
        input.push_str("type=btree\nmapsize=268435456\nmaxreaders=126\n");
        for f in flags {
            input.push_str(&format!("{f}=1\n"));
        }
        input.push_str("HEADER=END\n");
        for (k, v) in pairs {
            input.push(' ');
            input.push_str(&hex(k));
            input.push_str("\n ");
            input.push_str(&hex(v));
            input.push('\n');
        }
        input.push_str("DATA=END\n");
        let file = env.with_extension(format!("load{}", rand_suffix()));
        fs::write(&file, input).unwrap();
        self.run(
            "mdb_load",
            &["-f", file.to_str().unwrap(), env.to_str().unwrap()],
            None,
        );
        let _ = fs::remove_file(file);
    }

    /// Starts the `holder` helper on `env`; see `tests/c/holder.c`.
    pub fn holder(&self, env: &Path, args: &[&str]) -> Holder {
        let mut cmd = Command::new(self.bin("holder"));
        cmd.args(args).arg(env);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn holder");
        let stdin = child.stdin.take();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
        assert!(line.starts_with("READY"), "holder said {line:?}");
        let mut pid = 0;
        let mut txnid = None;
        for part in line.split_whitespace() {
            if let Some(p) = part.strip_prefix("pid=") {
                pid = p.parse().unwrap();
            }
            if let Some(t) = part.strip_prefix("txnid=") {
                txnid = t.parse().ok();
            }
        }
        Holder {
            child,
            stdin,
            stdout,
            pid,
            txnid,
        }
    }
}

/// A running `holder` process.
pub struct Holder {
    pub child: Child,
    stdin: Option<ChildStdin>,
    stdout: std::io::BufReader<ChildStdout>,
    pub pid: i32,
    pub txnid: Option<u64>,
}

impl Holder {
    /// Asks the holder to reset and renew its read txn; returns the new id.
    pub fn renew(&mut self) -> u64 {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(b"renew\n").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut self.stdout, &mut line).unwrap();
        line.trim().strip_prefix("TXN ").unwrap().parse().unwrap()
    }

    /// Lets the holder close its environment cleanly and waits for it.
    pub fn quit(mut self) {
        if let Some(mut s) = self.stdin.take() {
            let _ = s.write_all(b"quit\n");
        }
        let status = self.child.wait().unwrap();
        assert!(status.success());
    }

    /// Kills the holder without letting it clean up (SIGKILL).
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{n}", std::process::id())
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Parses `mdb_dump` bytevalue output (one database).
pub fn parse_dump(text: &str) -> Vec<Pair> {
    let mut lines = text.lines().skip_while(|l| *l != "HEADER=END").skip(1);
    let mut pairs = Vec::new();
    while let Some(k) = lines.next() {
        if k == "DATA=END" {
            break;
        }
        let v = lines.next().expect("data line");
        pairs.push((unhex(k.trim_start()), unhex(v.trim_start())));
    }
    pairs
}

/// The statistics `mdb_stat -e -a` prints.
#[derive(Debug, Default)]
pub struct StatOutput {
    /// "Environment Info" lines, by label.
    pub env: BTreeMap<String, String>,
    /// Per database ("Main DB" or the name): label -> number.
    pub dbs: BTreeMap<String, BTreeMap<String, u64>>,
    /// "Free pages" with `-f`.
    pub free_pages: Option<u64>,
}

pub fn parse_stat(text: &str) -> StatOutput {
    let mut out = StatOutput::default();
    let mut section = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("Status of ") {
            section = name.to_owned();
            out.dbs.entry(section.clone()).or_default();
            continue;
        }
        if line == "Environment Info" || line == "Freelist Status" {
            section = line.to_owned();
            continue;
        }
        let Some((label, value)) = line.trim().split_once(": ") else {
            continue;
        };
        match section.as_str() {
            "Environment Info" => {
                out.env.insert(label.to_owned(), value.to_owned());
            }
            "Freelist Status" => {
                if label == "Free pages" {
                    out.free_pages = value.parse().ok();
                } else if let Ok(v) = value.parse() {
                    out.dbs
                        .entry("(free)".to_owned())
                        .or_default()
                        .insert(label.to_owned(), v);
                }
            }
            "" => {}
            db => {
                if let Ok(v) = value.parse() {
                    out.dbs.get_mut(db).unwrap().insert(label.to_owned(), v);
                }
            }
        }
    }
    out
}

/// Compares a Rust `Stat` with one block of `mdb_stat` output.
pub fn assert_stat_matches(name: &str, rust: &lmdb_core::Stat, c: &BTreeMap<String, u64>) {
    assert_eq!(u64::from(rust.depth), c["Tree depth"], "{name}: depth");
    assert_eq!(
        rust.branch_pages as u64, c["Branch pages"],
        "{name}: branch pages"
    );
    assert_eq!(
        rust.leaf_pages as u64, c["Leaf pages"],
        "{name}: leaf pages"
    );
    assert_eq!(
        rust.overflow_pages as u64, c["Overflow pages"],
        "{name}: overflow pages"
    );
    assert_eq!(rust.entries as u64, c["Entries"], "{name}: entries");
}

// ---------------------------------------------------------------------------
// Rust-side walkers and checks
// ---------------------------------------------------------------------------

pub fn walk_forward(txn: &RoTxn<'_>, db: Dbi) -> Vec<Pair> {
    let mut c = txn.cursor(db).unwrap();
    let mut out = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        out.push((k.to_vec(), v.to_vec()));
    }
    // Further NEXT stays at the end.
    assert!(c.next().unwrap().is_none());
    out
}

pub fn walk_backward(txn: &RoTxn<'_>, db: Dbi) -> Vec<Pair> {
    let mut c = txn.cursor(db).unwrap();
    let mut out = Vec::new();
    while let Some((k, v)) = c.prev().unwrap() {
        out.push((k.to_vec(), v.to_vec()));
    }
    out.reverse();
    out
}

/// Groups sorted pairs by key: key -> its data items in order.
pub fn group(pairs: &[Pair]) -> Vec<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut out: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    for (k, v) in pairs {
        match out.last_mut() {
            Some((lk, vs)) if lk == k => vs.push(v.clone()),
            _ => out.push((k.clone(), vec![v.clone()])),
        }
    }
    out
}

/// Checks every read path of `db` against `expected` (all pairs in database
/// order): forward and backward walks, `get` of every key, `SET`,
/// `SET_KEY`, `SET_RANGE`, `NEXT_NODUP`/`PREV_NODUP`, and for `DUP_SORT`
/// databases the dup walks, `count`, `GET_BOTH`, `GET_BOTH_RANGE`,
/// `FIRST_DUP`/`LAST_DUP`.
pub fn verify_db(env: &Environment, db: Dbi, expected: &[Pair]) {
    let txn = env.begin_ro_txn().unwrap();
    let dupsort = txn
        .db_flags(db)
        .unwrap()
        .contains(lmdb_core::DatabaseFlags::DUP_SORT);
    assert_eq!(walk_forward(&txn, db), expected, "forward walk");
    assert_eq!(walk_backward(&txn, db), expected, "backward walk");
    let stat = txn.stat(db).unwrap();
    assert_eq!(stat.entries, expected.len(), "entries");

    let groups = group(expected);
    let mut c = txn.cursor(db).unwrap();
    for (i, (k, vs)) in groups.iter().enumerate() {
        assert_eq!(txn.get(db, k).unwrap(), Some(&vs[0][..]), "get {}", hex(k));
        let (sk, sv) = c.set(k).unwrap().expect("set");
        assert_eq!((sk, sv), (&k[..], &vs[0][..]));
        let (sk, _) = c.set_key(k).unwrap().expect("set_key");
        assert_eq!(sk, &k[..]);
        let (rk, _) = c.set_range(k).unwrap().expect("set_range exact");
        assert_eq!(rk, &k[..]);
        if dupsort {
            assert_eq!(c.count().unwrap(), vs.len() as u64, "count {}", hex(k));
            // Walk the duplicates both ways.
            let first = c.first_dup().unwrap().unwrap();
            assert_eq!(first.1, &vs[0][..]);
            let mut seen = vec![first.1.to_vec()];
            while let Some((dk, dv)) = c.next_dup().unwrap() {
                assert_eq!(dk, &k[..]);
                seen.push(dv.to_vec());
            }
            assert_eq!(&seen, vs, "next_dup walk of {}", hex(k));
            let last = c.last_dup().unwrap().unwrap();
            assert_eq!(last.1, &vs[vs.len() - 1][..]);
            let mut seen = vec![last.1.to_vec()];
            while let Some((_, dv)) = c.prev_dup().unwrap() {
                seen.push(dv.to_vec());
            }
            seen.reverse();
            assert_eq!(&seen, vs, "prev_dup walk of {}", hex(k));
            for v in vs {
                let (bk, bv) = c.get_both(k, v).unwrap().expect("get_both");
                assert_eq!((bk, bv), (&k[..], &v[..]));
                let (_, rv) = c.get_both_range(k, v).unwrap().expect("get_both_range");
                assert_eq!(rv, &v[..]);
            }
        }
        // NEXT_NODUP from this key lands on the next key.
        c.set(k).unwrap().unwrap();
        let next = c.next_nodup().unwrap();
        match groups.get(i + 1) {
            Some((nk, nvs)) => assert_eq!(next, Some((&nk[..], &nvs[0][..]))),
            None => assert_eq!(next, None),
        }
        // PREV_NODUP from this key lands on the last item of the previous key.
        c.set(k).unwrap().unwrap();
        let prev = c.prev_nodup().unwrap();
        match i.checked_sub(1).map(|j| &groups[j]) {
            Some((pk, pvs)) => assert_eq!(prev, Some((&pk[..], &pvs[pvs.len() - 1][..]))),
            None => assert_eq!(prev, None),
        }
    }
    // Absent keys: get misses, SET misses, SET_RANGE finds the successor.
    // (Not for INTEGER_KEY: a longer probe is not a valid integer key, and
    // C compares only the probe's first bytes.)
    if txn
        .db_flags(db)
        .unwrap()
        .contains(lmdb_core::DatabaseFlags::INTEGER_KEY)
    {
        return;
    }
    let mut c = txn.cursor(db).unwrap();
    for (i, (k, _)) in groups.iter().enumerate() {
        let mut probe = k.clone();
        probe.push(0);
        if groups.iter().any(|(g, _)| *g == probe) {
            continue;
        }
        assert_eq!(txn.get(db, &probe).unwrap(), None);
        assert_eq!(c.set(&probe).unwrap(), None);
        let succ = c.set_range(&probe).unwrap().map(|(k, _)| k.to_vec());
        // With the default byte order the successor of k+"\0" is the next key.
        if !txn
            .db_flags(db)
            .unwrap()
            .contains(lmdb_core::DatabaseFlags::REVERSE_KEY)
        {
            assert_eq!(
                succ,
                groups.get(i + 1).map(|g| g.0.clone()),
                "set_range {}",
                hex(&probe)
            );
        }
    }
}

/// Opens `path` (an environment directory) read-only from Rust.
pub fn open_ro(path: &Path, max_dbs: u32) -> Environment {
    Environment::builder()
        .max_dbs(max_dbs)
        .flags(lmdb_core::EnvironmentFlags::READ_ONLY)
        .open(path)
        .unwrap()
}

/// Opens `path` read-write (with a lock file) from Rust.
pub fn open_rw(path: &Path, max_dbs: u32) -> Environment {
    Environment::builder().max_dbs(max_dbs).open(path).unwrap()
}

// ---------------------------------------------------------------------------
// Differential cursor testing against C's mdb_cursor_get
// ---------------------------------------------------------------------------

/// A small deterministic PRNG (xorshift64*).
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

/// A probe close to `k`. With `integer` set, only probes for which C's
/// integer comparators are deterministic: same length, or (`grow`) two
/// bytes longer. Shorter or odd-length probes make C read bytes before or
/// after the caller's buffer.
fn probe_near(rng: &mut Rng, k: &[u8]) -> Vec<u8> {
    probe(rng, k, None)
}

fn probe(rng: &mut Rng, k: &[u8], integer: Option<bool>) -> Vec<u8> {
    let mut p = k.to_vec();
    if let Some(grow) = integer {
        if grow && rng.below(3) == 0 {
            p.extend_from_slice(&[0, 0]);
        } else if let Some(last) = p.last_mut() {
            *last = last.wrapping_add(if rng.below(2) == 0 { 1 } else { 255 });
        }
        return p;
    }
    match rng.below(4) {
        0 => p.push(0),
        1 if !p.is_empty() => {
            let i = p.len() - 1;
            p[i] = p[i].wrapping_add(1);
        }
        2 if !p.is_empty() => {
            let i = p.len() - 1;
            p[i] = p[i].wrapping_sub(1);
        }
        _ => {
            p.truncate(p.len().saturating_sub(1).max(1));
        }
    }
    p
}

/// Random cursor operations over the keys and data of `pairs`, in the
/// `cursorops` syntax.
pub fn gen_ops(
    rng: &mut Rng,
    pairs: &[Pair],
    n: usize,
    flags: lmdb_core::DatabaseFlags,
) -> Vec<String> {
    use lmdb_core::DatabaseFlags as F;
    let dupfixed = flags.contains(F::DUP_FIXED);
    let ikey = flags.contains(F::INTEGER_KEY).then_some(true);
    let idup = flags.contains(F::INTEGER_DUP).then_some(false);
    const MOVES: &[&str] = &[
        "first",
        "last",
        "next",
        "next",
        "next",
        "prev",
        "prev",
        "next_dup",
        "prev_dup",
        "next_nodup",
        "prev_nodup",
        "first_dup",
        "last_dup",
        "get_current",
        "count",
    ];
    const MULTI: &[&str] = &["get_multiple", "next_multiple", "prev_multiple"];
    let mut ops = Vec::with_capacity(n);
    for _ in 0..n {
        let pick = rng.below(20);
        let op = if pairs.is_empty() || pick < 9 {
            if dupfixed && rng.below(3) == 0 {
                MULTI[rng.below(MULTI.len())].to_owned()
            } else {
                MOVES[rng.below(MOVES.len())].to_owned()
            }
        } else {
            let (k, v) = &pairs[rng.below(pairs.len())];
            // Keep command lines short: huge values are not useful probes.
            let v = &v[..v.len().min(64)];
            let key = if rng.below(4) == 0 {
                probe(rng, k, ikey)
            } else {
                k.clone()
            };
            match rng.below(5) {
                0 => format!("set:{}", hex(&key)),
                1 => format!("set_key:{}", hex(&key)),
                2 => format!("set_range:{}", hex(&key)),
                3 => {
                    let d = if rng.below(3) == 0 {
                        probe(rng, v, idup)
                    } else {
                        v.to_vec()
                    };
                    format!("get_both:{}:{}", hex(&key), hex(&d))
                }
                _ => {
                    let d = if rng.below(2) == 0 {
                        probe(rng, v, idup)
                    } else {
                        v.to_vec()
                    };
                    format!("get_both_range:{}:{}", hex(&key), hex(&d))
                }
            }
        };
        ops.push(op);
    }
    ops
}

fn rc_of<T>(r: &lmdb_core::Result<Option<T>>) -> i32 {
    match r {
        Ok(Some(_)) => 0,
        Ok(None) => lmdb_core::consts::MDB_NOTFOUND,
        Err(e) => e.to_err_code(),
    }
}

/// Runs `ops` on a fresh Rust cursor, printing the same lines as
/// `cursorops`.
pub fn rust_cursorops(txn: &RoTxn<'_>, db: Dbi, ops: &[String]) -> Vec<String> {
    let mut c = txn.cursor(db).unwrap();
    let mut out = Vec::new();
    for spec in ops {
        let mut parts = spec.split(':');
        let name = parts.next().unwrap();
        let key = parts.next().map(unhex);
        let data = parts.next().map(unhex);
        let first = if name == "count" {
            let r = c.count();
            match r {
                Ok(n) => format!("count rc=0 n={n}"),
                Err(e) => format!("count rc={} n=0", e.to_err_code()),
            }
        } else {
            let op = match name {
                "first" => lmdb_core::CursorOp::First,
                "first_dup" => lmdb_core::CursorOp::FirstDup,
                "get_both" => lmdb_core::CursorOp::GetBoth,
                "get_both_range" => lmdb_core::CursorOp::GetBothRange,
                "get_current" => lmdb_core::CursorOp::GetCurrent,
                "get_multiple" => lmdb_core::CursorOp::GetMultiple,
                "last" => lmdb_core::CursorOp::Last,
                "last_dup" => lmdb_core::CursorOp::LastDup,
                "next" => lmdb_core::CursorOp::Next,
                "next_dup" => lmdb_core::CursorOp::NextDup,
                "next_multiple" => lmdb_core::CursorOp::NextMultiple,
                "next_nodup" => lmdb_core::CursorOp::NextNoDup,
                "prev" => lmdb_core::CursorOp::Prev,
                "prev_dup" => lmdb_core::CursorOp::PrevDup,
                "prev_nodup" => lmdb_core::CursorOp::PrevNoDup,
                "set" => lmdb_core::CursorOp::Set,
                "set_key" => lmdb_core::CursorOp::SetKey,
                "set_range" => lmdb_core::CursorOp::SetRange,
                "prev_multiple" => lmdb_core::CursorOp::PrevMultiple,
                other => panic!("bad op {other}"),
            };
            // C passes an empty (not NULL) key for ops without a key input.
            let key = key.or(Some(Vec::new()));
            let r = c.get(key.as_deref(), data.as_deref(), op);
            match &r {
                Ok(Some((_, d))) => format!("{name} rc=0 d={}", hex(d)),
                _ => format!("{name} rc={} d=-", rc_of(&r)),
            }
        };
        let cur = c.get_current();
        let second = match &cur {
            Ok(Some((k, d))) => format!("cur rc=0 k={} d={}", hex(k), hex(d)),
            _ => format!("cur rc={} k=- d=-", rc_of(&cur)),
        };
        out.push(format!("{first} | {second}"));
    }
    out
}

/// Runs the same random cursor operations through C and Rust and asserts
/// identical results and positions.
pub fn differential(
    c: &CTools,
    env_path: &Path,
    env: &Environment,
    name: Option<&str>,
    pairs: &[Pair],
    seed: u64,
    rounds: usize,
) {
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(name).unwrap();
    let flags = txn.db_flags(db).unwrap();
    let mut rng = Rng(seed | 1);
    let mut compared = 0;
    let mut c_crashes = 0;
    let mut c_bugs = 0;
    for _ in 0..rounds {
        let ops = gen_ops(&mut rng, pairs, 60, flags);
        let mut args: Vec<String> = vec![
            env_path.to_str().unwrap().to_owned(),
            name.unwrap_or("-").to_owned(),
        ];
        args.extend(ops.iter().cloned());
        let out = Command::new(c.bin("cursorops"))
            .args(&args)
            .output()
            .unwrap();
        let c_lines: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let r_lines = rust_cursorops(&txn, db, &ops);
        if !out.status.success() {
            // C asserted (or crashed) on some op; compare what it printed.
            let i = c_lines.len();
            eprintln!(
                "NOTE: C cursorops died at op {i} {:?}: {}",
                ops.get(i),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            c_crashes += 1;
        } else {
            assert_eq!(c_lines.len(), r_lines.len());
        }
        for (i, (cl, rl)) in c_lines.iter().zip(&r_lines).enumerate() {
            // KNOWN_C_BUGS.md: past the end of a page (after a failed SET)
            // C's mdb_cursor_next reads NODEPTR(mp, ki) unchecked; a stale
            // pointer slot can make it "succeed" with garbage while staying
            // past the end, which shows as a successful move followed by a
            // failing GET_CURRENT. Rust moves to the next item instead, so
            // the cursors diverge from here on: stop comparing this round.
            if cl != rl
                && cl.starts_with("next")
                && cl.contains(" rc=0 d=")
                && cl.contains("| cur rc=-30798")
            {
                eprintln!(
                    "NOTE: known C bug (unchecked NODEPTR in mdb_cursor_next) at op {i}: {cl}"
                );
                c_bugs += 1;
                break;
            }
            if cl != rl {
                // C leaves `data` untouched in a few successful cases
                // (MDB_GET_MULTIPLE on a single item); only rc and the
                // position must agree then.
                let (c_op, c_cur) = cl.split_once(" | ").unwrap();
                let (r_op, r_cur) = rl.split_once(" | ").unwrap();
                let untouched =
                    c_op.ends_with("rc=0 d=-") && r_op.starts_with(c_op.trim_end_matches('-'));
                // KNOWN_C_BUGS.md: C's mdb_cursor_count reads an unchecked
                // node when the cursor is past the end (no current item)
                // and usually reports 1; Rust reports MDB_NOTFOUND.
                let count_past_end = c_op.starts_with("count rc=0")
                    && r_op == "count rc=-30798 n=0"
                    && c_cur.starts_with("cur rc=-30798");
                assert!(
                    (untouched || count_past_end) && c_cur == r_cur,
                    "op {i} {:?} differs\n  C:    {cl}\n  Rust: {rl}\n  ops so far: {:?}",
                    ops[i],
                    &ops[..=i]
                );
            }
            compared += 1;
        }
    }
    eprintln!(
        "differential: {compared} cursor ops identical to C \
         ({c_crashes} C aborts, {c_bugs} rounds cut short by known C bugs)"
    );
}
