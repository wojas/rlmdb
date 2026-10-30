//! The LMDB 1.0 format across processes: rlmdb and the C 1.0 oracle
//! (`mdb_load10`, `mdb_dump10`, `mdb_stat10`, `mdb_copy10`, `mtest*10`,
//! `writeops10`, `cpeer10`) reading and writing each other's files.
//!
//! * read side: environments C 1.0 writes at 512 B … 64 KiB pages read by
//!   rlmdb (cursor walks both ways, point lookups, statistics), compared
//!   with `mdb_dump10` and `mdb_stat10`;
//! * `mdb_load10` of a dump against rlmdb loading the same records the way
//!   `mdb_load` does: the same file, page for page;
//! * `mdb_copy10` and `mdb_copy10 -c` against rlmdb's plain and compacting
//!   copies: byte for byte the same file;
//! * C 1.0 and rlmdb taking turns on one environment (each writes with the
//!   other's pages under it, then the C page audit and the contents).
//!
//! The scenarios at the end have both sides open at once: rlmdb writes
//! 1.0's version 2 lock file (`interop/tests/lockfile10.rs` checks its
//! layout), so a C 1.0 reader pins pages against an rlmdb writer and the
//! reverse, and both see each other's reader slots.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use interop::bins;
use interop::tools::parse_dump;
use rlmdb::{DatabaseFlags, Environment, EnvironmentFlags, Format, RwTxn, Transaction, WriteFlags};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("interop-v10-")
        .tempdir()
        .unwrap()
}

/// Runs a C program (with `stdin` as input), requires success, returns
/// stdout.
fn run(prog: &Path, args: &[&str], dir: Option<&Path>, stdin: Option<&str>) -> String {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(s) = stdin {
        input.write_all(s.as_bytes()).unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{} {args:?}: {}",
        prog.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// The C 1.0 page audit of `env`.
fn audit10(env: &Path) {
    if let Err(e) = interop::tools::audit10(env) {
        panic!("cpeer10 check-freelist {}: {e}", env.display());
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A database of a test data set: name, flags, records in put order.
struct Db {
    name: Option<&'static str>,
    flags: DatabaseFlags,
    records: Vec<(Vec<u8>, Vec<u8>)>,
}

/// A deterministic data set for page size `ps`: a main DB with small and
/// overflow values and odd key sizes, a DUPSORT DB whose keys hold a few
/// (sub-page) or many (sub-DB) duplicates, and a DUPFIXED DB. Keys stay
/// within the key size limit of `ps`.
fn data_set(ps: usize, seed: u64) -> Vec<Db> {
    let mut st = seed;
    let mut rnd = |n: u64| {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st % n
    };
    let maxkey = (((ps - 24) / 2) & !1) - 2 - 56;
    let mut main = Vec::new();
    for i in 0..1500u64 {
        let klen = (5 + rnd(40) as usize).min(maxkey);
        let mut k = format!("m{i:05}").into_bytes();
        k.resize(klen.max(k.len()), b'k');
        let vlen = if rnd(12) == 0 {
            ps + rnd(3 * ps as u64) as usize
        } else {
            1 + rnd(90) as usize
        };
        main.push((k, vec![b'a' + (i % 26) as u8; vlen]));
    }
    let mut dup = Vec::new();
    for i in 0..60u64 {
        let n = if i % 10 == 0 { 300 } else { 1 + rnd(12) };
        for j in 0..n {
            let mut d = format!("{j:05}").into_bytes();
            d.resize((5 + rnd(30) as usize).min(maxkey), b'd');
            dup.push((format!("d{i:03}").into_bytes(), d));
        }
    }
    let mut fix = Vec::new();
    for i in 0..30u64 {
        let n = if i % 7 == 0 { 400 } else { 1 + rnd(20) };
        for j in 0..n {
            fix.push((
                format!("f{i:03}").into_bytes(),
                (j * 7919).to_be_bytes().to_vec(),
            ));
        }
    }
    vec![
        Db {
            name: None,
            flags: DatabaseFlags::empty(),
            records: main,
        },
        Db {
            name: Some("dup"),
            flags: DatabaseFlags::DUP_SORT,
            records: dup,
        },
        Db {
            name: Some("fix"),
            flags: DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
            records: fix,
        },
    ]
}

/// `mdb_load` input for `dbs` with `db_pagesize=ps` (which makes
/// `mdb_load10` call `mdb_env_set_pagesize`).
fn dump_text(dbs: &[Db], ps: usize) -> String {
    let mut s = String::new();
    for db in dbs {
        s.push_str("VERSION=3\nformat=bytevalue\n");
        if let Some(n) = db.name {
            s.push_str(&format!("database={n}\n"));
        }
        s.push_str("type=btree\nmapsize=268435456\nmaxreaders=126\n");
        if db.flags.contains(DatabaseFlags::DUP_SORT) {
            s.push_str("duplicates=1\ndupsort=1\n");
        }
        if db.flags.contains(DatabaseFlags::DUP_FIXED) {
            s.push_str("dupfixed=1\n");
        }
        s.push_str(&format!("db_pagesize={ps}\nHEADER=END\n"));
        for (k, v) in &db.records {
            s.push_str(&format!(" {}\n {}\n", hex(k), hex(v)));
        }
        s.push_str("DATA=END\n");
    }
    s
}

/// What `mdb_load` does with `dbs`, through rlmdb: per database a write
/// transaction, `mdb_dbi_open` with `MDB_CREATE`, a cursor, `mdb_cursor_put`
/// with flags 0, and a commit after every 100 puts.
fn rust_load(dir: &Path, dbs: &[Db], ps: usize) {
    let env = Environment::builder()
        .format(Format::V10)
        .page_size(ps)
        .max_dbs(2)
        .map_size(268_435_456)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
        .unwrap();
    for db in dbs {
        let mut txn = env.begin_rw_txn().unwrap();
        let h = txn
            .create_db(db.name, db.flags | DatabaseFlags::CREATE)
            .unwrap();
        let mut cur = txn.open_cursor(h).unwrap();
        let mut batch = 0;
        for (k, v) in &db.records {
            txn.cursor_put(cur, k, v, WriteFlags::empty()).unwrap();
            batch += 1;
            if batch == 100 {
                txn.close_cursor(cur);
                txn.commit().unwrap();
                txn = env.begin_rw_txn().unwrap();
                cur = txn.open_cursor(h).unwrap();
                batch = 0;
            }
        }
        txn.close_cursor(cur);
        txn.commit().unwrap();
    }
}

/// The records of database `name` of `env`, walked forward.
fn rust_records(env: &Environment, name: Option<&str>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(name).unwrap();
    let mut c = txn.cursor(db).unwrap();
    let mut out = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        out.push((k.to_vec(), v.to_vec()));
    }
    // The same backwards.
    let mut back = Vec::new();
    let mut c = txn.cursor(db).unwrap();
    while let Some((k, v)) = c.prev().unwrap() {
        back.push((k.to_vec(), v.to_vec()));
    }
    back.reverse();
    assert_eq!(out, back, "{name:?}: forward and backward walks");
    out
}

/// `mdb_dump10 -s name` (or the main DB) of `env`, parsed.
fn c10_records(env: &Path, name: Option<&str>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut args = Vec::new();
    if let Some(n) = name {
        args.extend(["-s", n]);
    }
    args.push(path_str(env));
    let text = run(bins::mdb_dump10(), &args, None, None);
    let mut dbs = parse_dump(&text).unwrap();
    assert_eq!(dbs.len(), 1);
    dbs.remove(0).records
}

/// What `dbs` hold: sorted keys, sorted unique duplicates.
fn expected(db: &Db) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut m: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for (k, v) in &db.records {
        let e = m.entry(k.clone()).or_default();
        if db.flags.contains(DatabaseFlags::DUP_SORT) {
            if let Err(i) = e.binary_search(v) {
                e.insert(i, v.clone());
            }
        } else {
            *e = vec![v.clone()];
        }
    }
    m.into_iter()
        .flat_map(|(k, vs)| vs.into_iter().map(move |v| (k.clone(), v)))
        .collect()
}

/// The `label: number` lines of section `Status of <name>` of `mdb_stat`
/// output (1.0 also prints the page size there).
fn stat10_section(text: &str, name: &str) -> BTreeMap<String, u64> {
    let head = format!("Status of {name}");
    text.lines()
        .skip_while(|l| *l != head)
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(": ")?;
            Some((k.to_owned(), v.parse().ok()?))
        })
        .collect()
}

/// rlmdb's statistics of database `name` against `mdb_stat10`'s.
fn compare_stat(env: &Environment, path: &Path, name: Option<&str>) {
    let mut args = vec!["-a"];
    args.push(path_str(path));
    let text = run(bins::mdb_stat10(), &args, None, None);
    let c = stat10_section(&text, name.unwrap_or("Main DB"));
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(name).unwrap();
    let r = txn.stat(db).unwrap();
    assert_eq!(
        (
            r.depth,
            r.branch_pages as u64,
            r.leaf_pages as u64,
            r.overflow_pages as u64,
            r.entries as u64
        ),
        (
            c["Tree depth"] as u32,
            c["Branch pages"],
            c["Leaf pages"],
            c["Overflow pages"],
            c["Entries"]
        ),
        "{name:?}"
    );
}

#[test]
fn rlmdb_reads_c10_files_at_every_page_size() {
    for ps in [512, 1024, 4096, 16384, 65536] {
        let dir = tmp();
        let dbs = data_set(ps, 0x11 + ps as u64);
        run(
            bins::mdb_load10(),
            &[path_str(dir.path())],
            None,
            Some(&dump_text(&dbs, ps)),
        );
        audit10(dir.path());
        // NO_LOCK: a read-only look at the file; the C tools below open
        // the environment while this handle exists.
        let env = Environment::builder()
            .max_dbs(4)
            .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
            .open(dir.path())
            .unwrap();
        assert_eq!((env.format(), env.page_size()), (Format::V10, ps));
        for db in &dbs {
            let mut got = rust_records(&env, db.name);
            if db.name.is_none() {
                // The main DB also holds the named DBs' records (keys with
                // their NUL in 1.0).
                got.retain(|(k, _)| k.last() != Some(&0));
            }
            assert_eq!(got, expected(db), "page size {ps}, {:?}", db.name);
            if db.name.is_some() {
                assert_eq!(got, c10_records(dir.path(), db.name), "page size {ps}");
            }
            compare_stat(&env, dir.path(), db.name);
            // Point lookups of every key (the first duplicate).
            let txn = env.begin_ro_txn().unwrap();
            let h = txn.open_db(db.name).unwrap();
            let mut seen = std::collections::BTreeSet::new();
            for (k, v) in &got {
                if seen.insert(k.clone()) {
                    assert_eq!(txn.get(h, k).unwrap(), Some(&v[..]), "page size {ps}");
                }
            }
        }
    }
}

/// C 1.0's own test programs write 1 KiB-page environments
/// (`mdb_env_set_pagesize(env, 1024)` in `mtest.c`); rlmdb reads what
/// they leave like `mdb_dump10` does.
#[test]
fn rlmdb_reads_mtest10_environments() {
    for name in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
        let dir = tmp();
        std::fs::create_dir(dir.path().join("testdb")).unwrap();
        let prog = bins::mtest10(name).unwrap();
        run(prog, &[], Some(dir.path()), None);
        let env_path = dir.path().join("testdb");
        audit10(&env_path);
        let env = Environment::builder()
            .max_dbs(4)
            .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
            .open(&env_path)
            .unwrap();
        assert_eq!(env.format(), Format::V10, "{name}");
        // The main DB (its raw records, named DBs' included), and every
        // named DB if there are any (`-a` fails otherwise).
        let text = run(bins::mdb_dump10(), &[path_str(&env_path)], None, None);
        let mut dumped = parse_dump(&text).unwrap();
        let all = Command::new(bins::mdb_dump10())
            .args(["-a", path_str(&env_path)])
            .output()
            .unwrap();
        if all.status.success() {
            dumped.extend(parse_dump(&String::from_utf8_lossy(&all.stdout)).unwrap());
        }
        for d in &dumped {
            assert_eq!(
                rust_records(&env, d.name.as_deref()),
                d.records,
                "{name}: {:?}",
                d.name
            );
            compare_stat(&env, &env_path, d.name.as_deref());
        }
        eprintln!("{name}: {} databases agree", dumped.len());
    }
}

#[test]
fn rlmdb_loads_what_mdb_load10_loads() {
    for ps in [1024, 4096, 16384, 65536] {
        let (c, r) = (tmp(), tmp());
        let dbs = data_set(ps, 0x22 + ps as u64);
        run(
            bins::mdb_load10(),
            &[path_str(c.path())],
            None,
            Some(&dump_text(&dbs, ps)),
        );
        rust_load(r.path(), &dbs, ps);
        interop::pagecmp::compare_data_files(
            &c.path().join("data.mdb"),
            &r.path().join("data.mdb"),
        )
        .unwrap_or_else(|e| panic!("page size {ps}: {e}"));
        audit10(r.path());
        for db in &dbs {
            if let Some(n) = db.name {
                assert_eq!(
                    c10_records(r.path(), Some(n)),
                    expected(db),
                    "page size {ps}"
                );
            }
        }
    }
}

/// Runs `writeops10` (C 1.0) on `dir` with `script`, database `t` with
/// `flags`, pages of `ps` bytes (for a new environment).
fn writeops10(dir: &Path, flags: u32, ps: usize, script: &str) {
    let out = run(
        bins::writeops10(),
        &[
            path_str(dir),
            "t",
            &flags.to_string(),
            "1",
            "268435456",
            &ps.to_string(),
        ],
        None,
        Some(script),
    );
    assert!(out.lines().last().unwrap_or("") == "commit rc=0", "{out}");
}

/// A `writeops` script: bulk loads with deletes in between, overflow
/// values, a nested transaction that is aborted and one that commits.
fn script(seed: u64, len: usize) -> String {
    let mut s = format!("b:800:{seed}:{len}\n");
    for i in 0..300u32 {
        let k = hex(format!("key{i:04}{}", "q".repeat(i as usize % 5)).as_bytes());
        s.push_str(&format!(
            "p:{k}:{}:0\n",
            hex(&vec![b'v'; 1 + i as usize % 37])
        ));
    }
    for i in (0..300u32).step_by(3) {
        let k = hex(format!("key{i:04}{}", "q".repeat(i as usize % 5)).as_bytes());
        s.push_str(&format!("d:{k}\n"));
    }
    s.push_str(&format!("nb\nb:200:{}:{}\nna\n", seed + 1, 2 * len));
    s.push_str(&format!("nb\nb:100:{}:{}\nnc\n", seed + 2, len + 1));
    s
}

#[test]
fn copies_match_mdb_copy10() {
    for ps in [1024, 4096, 16384, 65536] {
        let src = tmp();
        writeops10(src.path(), 0, ps, &script(7, ps / 3));
        writeops10(src.path(), 0, ps, &script(9, ps + 100));
        for compact in [false, true] {
            let (c, r) = (tmp(), tmp());
            let mut args = Vec::new();
            if compact {
                args.push("-c");
            }
            args.extend([path_str(src.path()), path_str(c.path())]);
            run(bins::mdb_copy10(), &args, None, None);
            let env = Environment::builder()
                .max_dbs(4)
                .flags(EnvironmentFlags::READ_ONLY)
                .open(src.path())
                .unwrap();
            env.copy_to_path(r.path(), compact).unwrap();
            drop(env);
            let a = std::fs::read(c.path().join("data.mdb")).unwrap();
            let b = std::fs::read(r.path().join("data.mdb")).unwrap();
            assert!(
                a == b,
                "page size {ps}, compact {compact}: the copies differ ({} and {} bytes)",
                a.len(),
                b.len()
            );
            audit10(r.path());
        }
    }
}

/// Adds the records `i in from..to` to database `t` of `env` with rlmdb,
/// deleting every fifth earlier one and overwriting overflow values.
fn rust_turn(env: &Environment, from: u32, to: u32, ps: usize) {
    let mut txn: RwTxn<'_> = env.begin_rw_txn().unwrap();
    let t = txn.create_db(Some("t"), DatabaseFlags::CREATE).unwrap();
    for i in from..to {
        let k = format!("rust{i:05}");
        let len = if i % 9 == 0 {
            2 * ps + i as usize
        } else {
            3 + i as usize % 21
        };
        txn.put(t, k.as_bytes(), &vec![b'r'; len], WriteFlags::empty())
            .unwrap();
    }
    for i in (0..from).step_by(5) {
        let _ = txn.del(t, format!("rust{i:05}").as_bytes(), None);
    }
    for i in (1..from).step_by(9) {
        let _ = txn.put(
            t,
            format!("rust{i:05}").as_bytes(),
            &vec![b'o'; ps],
            WriteFlags::empty(),
        );
    }
    txn.commit().unwrap();
}

#[test]
fn c10_and_rlmdb_take_turns() {
    for ps in [1024, 4096, 65536] {
        for c_first in [true, false] {
            let dir = tmp();
            let open = || {
                let mut b = Environment::builder();
                b.format(Format::V10)
                    .page_size(ps)
                    .max_dbs(4)
                    .map_size(268_435_456);
                b.open(dir.path()).unwrap()
            };
            if c_first {
                writeops10(dir.path(), 0, ps, &script(3, ps / 2));
            }
            for round in 0..3u32 {
                let env = open();
                rust_turn(&env, round * 400, (round + 1) * 400, ps);
                drop(env);
                audit10(dir.path());
                // C 1.0 writes on top: it must treat rlmdb's pages as clean
                // pages of committed snapshots (mp_txnid).
                writeops10(dir.path(), 0, ps, &script(11 + u64::from(round), ps / 4));
                audit10(dir.path());
            }
            // Both see the same contents.
            let env = open();
            let records = rust_records(&env, Some("t"));
            drop(env);
            assert_eq!(
                records,
                c10_records(dir.path(), Some("t")),
                "page size {ps}"
            );
        }
    }
}

/// `hold-reader` of a peer on `env`, with `dataset` (`<db> <count>
/// <seed>` of the peer's `fill`, verified in the snapshot when the reader
/// is released: a failure is the process's exit status): the process and
/// its snapshot's txnid.
fn hold_reader(peer: &Path, env: &Path, dataset: &[&str]) -> (std::process::Child, u64) {
    let mut child = Command::new(peer)
        .args(["hold-reader", path_str(env), "60", "--until-stdin"])
        .args(dataset)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let txnid = line
        .split_whitespace()
        .find_map(|t| t.strip_prefix("txnid="))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("hold-reader said {line:?}"));
    (child, txnid)
}

/// A C 1.0 reader holds a snapshot while rlmdb writes: rlmdb must see the
/// reader slot in the (version 2) lock file and not reuse the snapshot's
/// pages; afterwards the snapshot C held is intact.
#[test]
fn c10_reader_pins_pages_against_rlmdb_writer() {
    let ps = 4096;
    let dir = tmp();
    writeops10(dir.path(), 0, ps, &script(5, 100));
    // A dataset the C reader verifies in its snapshot when released; rlmdb
    // rewrites that database underneath it.
    let dataset = ["held", "2000", "7"];
    let fill = Command::new(bins::cpeer10())
        .args(["fill", path_str(dir.path())])
        .args(dataset)
        .output()
        .unwrap();
    assert!(
        fill.status.success(),
        "{}",
        String::from_utf8_lossy(&fill.stderr)
    );
    let (mut reader, txnid) = hold_reader(bins::cpeer10(), dir.path(), &dataset);
    let env = Environment::builder()
        .max_dbs(4)
        .map_size(268_435_456)
        .open(dir.path())
        .unwrap();
    let before = c10_records(dir.path(), Some("t"));
    let held_keys: Vec<Vec<u8>> = {
        let txn = env.begin_ro_txn().unwrap();
        let db = txn.open_db(Some("held")).unwrap();
        let mut c = txn.cursor(db).unwrap();
        let mut keys = Vec::new();
        while let Some((k, _)) = c.next().unwrap() {
            keys.push(k.to_vec());
        }
        keys
    };
    assert_eq!(held_keys.len(), 2000);
    for round in 0..5 {
        rust_turn(&env, round * 300, (round + 1) * 300, ps);
        // Free the snapshot's pages of `held`: delete some keys, resize
        // the values of the rest.
        let mut txn = env.begin_rw_txn().unwrap();
        let held = txn.create_db(Some("held"), DatabaseFlags::empty()).unwrap();
        for (i, k) in held_keys.iter().enumerate() {
            if (i + round as usize) % 7 == 0 {
                txn.del(held, k, None).unwrap();
            } else {
                txn.put(
                    held,
                    k,
                    &vec![b'x'; 40 + round as usize * 30],
                    WriteFlags::empty(),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
    }
    let txn = env.begin_ro_txn().unwrap();
    assert!(txn.id() > txnid);
    drop(txn);
    // Released, the C reader verifies its snapshot of `held` as it was.
    drop(reader.stdin.take());
    let out = reader.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "C 1.0's snapshot changed under rlmdb's writes:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    drop(env);
    audit10(dir.path());
    assert_ne!(before, c10_records(dir.path(), Some("t")));
    // The control: the same verification of the current contents fails.
    let now = Command::new(bins::cpeer10())
        .args(["verify", path_str(dir.path())])
        .args(dataset)
        .output()
        .unwrap();
    assert!(
        !now.status.success(),
        "rlmdb's writes to `held` are visible"
    );
}

/// rlmdb holds a read transaction while C 1.0 writes: C must honour
/// rlmdb's reader slot, and rlmdb's snapshot stays readable.
#[test]
fn rlmdb_reader_pins_pages_against_c10_writer() {
    let ps = 4096;
    let dir = tmp();
    writeops10(dir.path(), 0, ps, &script(5, 100));
    let env = Environment::builder().max_dbs(4).open(dir.path()).unwrap();
    let before = rust_records(&env, Some("t"));
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("t")).unwrap();
    for round in 0..5u64 {
        writeops10(dir.path(), 0, ps, &script(20 + round, 2000));
    }
    let mut c = txn.cursor(db).unwrap();
    let mut held = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        held.push((k.to_vec(), v.to_vec()));
    }
    assert_eq!(held, before);
    drop(txn);
    drop(env);
    audit10(dir.path());
}

/// C 1.0 and rlmdb write the same environment in turns while both keep it
/// open (C's `hold-env` keeps its environment open across rlmdb's commits).
#[test]
fn c10_and_rlmdb_share_an_open_environment() {
    let ps = 4096;
    let dir = tmp();
    writeops10(dir.path(), 0, ps, &script(5, 100));
    let mut holder = Command::new(bins::cpeer10())
        .args(["hold-env", path_str(dir.path()), "--until-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let env = Environment::builder()
        .max_dbs(4)
        .map_size(268_435_456)
        .open(dir.path())
        .unwrap();
    for round in 0..3u32 {
        rust_turn(&env, round * 300, (round + 1) * 300, ps);
        writeops10(dir.path(), 0, ps, &script(30 + u64::from(round), 500));
    }
    assert_eq!(
        rust_records(&env, Some("t")),
        c10_records(dir.path(), Some("t"))
    );
    drop(env);
    drop(holder.stdin.take());
    holder.wait().unwrap();
    audit10(dir.path());
}

/// `rpeer check-freelist` (the Rust port of the page audit, decoding with
/// `rlmdb::format`) prints exactly what `cpeer10 check-freelist` prints on
/// 1.0 environments written by C 1.0 and by rlmdb.
#[test]
fn rpeer_audits_1_0_files_like_cpeer10() {
    let rpeer = interop::Peer::Rust.bin().unwrap();
    for ps in [1024, 4096, 65536] {
        let (c, r) = (tmp(), tmp());
        writeops10(c.path(), 4, ps, &script(13, ps / 3));
        rust_load(r.path(), &data_set(ps, 0x33), ps);
        for env in [c.path(), r.path()] {
            let run_audit = |prog: &Path| {
                let out = Command::new(prog)
                    .args(["check-freelist", path_str(env)])
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).into_owned()
            };
            assert_eq!(
                run_audit(&rpeer),
                run_audit(bins::cpeer10()),
                "page size {ps}"
            );
        }
    }
}
