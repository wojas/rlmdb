//! Incremental backups of LMDB 1.0 environments across C 1.0 and rlmdb
//! (`mdb_env_incr_dumpfd`, `mdb_env_incr_loadfd`; `mdb_dump -i`,
//! `mdb_load -i`):
//!
//! * dumps of one environment, written in turns by the C 1.0 and the Rust
//!   peer, at several txnids and at 256 B, 1 KiB, the OS page size and
//!   64 KiB pages: `mdb_dump10 -i`, rlmdb's `mdb_dump -i` and
//!   `Environment::incr_dump` write the same bytes where no freed overflow
//!   run is involved;
//! * a chain (a plain copy, then two increments) loaded by rlmdb and by C
//!   1.0 into copies of the copy: both give the source's data file, byte
//!   for byte, which the C 1.0 page audit and `cpeer10 verify` accept;
//! * C's walk over free pages (`KNOWN_C_BUGS.md` row 72): after a freed
//!   overflow run is partly reused C dumps its stale pages (rlmdb's load
//!   refuses them) or fails, and steps over a live run under a stale
//!   header; rlmdb's dump holds neither, and C 1.0's `mdb_load -i` restores
//!   the source from it;
//! * what rlmdb's load checks that C's does not (row 65): a C 1.0 reader
//!   makes it fail with `EBUSY`, a C 1.0 writer makes it wait, and a C 1.0
//!   process that has the environment open afterwards reads the loaded
//!   snapshot (rlmdb publishes it in the lock file).
//!
//! The tools' options and messages against C 1.0's are in `tools10.rs`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use interop::pagecmp::compare_data_files;
use interop::tools::audit10;
use interop::{bins, Format, Peer, TestEnv};

/// What a program wrote and how it ended.
struct Ran {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

/// Runs `prog` (as `argv[0] = name`) with `stdin`.
fn run(prog: &Path, name: &str, args: &[&str], stdin: &[u8]) -> Ran {
    let mut cmd = interop::os::command_as(prog, name);
    cmd.args(args)
        .env_remove("RLMDB_DEFAULT_FORMAT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    let _ = feeder.join();
    Ran {
        code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// `rtools <tool>` (the Rust tools), as `argv[0] = tool`.
fn rust(tool: &str, args: &[&str], stdin: &[u8]) -> Ran {
    let mut all = vec![tool];
    all.extend_from_slice(args);
    run(
        Path::new(env!("CARGO_BIN_EXE_rtools")),
        "rtools",
        &all,
        stdin,
    )
}

fn ok(r: Ran, what: &str) -> Vec<u8> {
    assert!(
        r.code == 0 && r.stderr.is_empty(),
        "{what}: {} {}",
        r.code,
        r.stderr
    );
    r.stdout
}

fn s(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 temp path")
}

/// rlmdb's `mdb_dump -i txnid env`, which must write what
/// `Environment::incr_dump` writes; returns it.
fn dump_rust(env: &Path, txnid: u64) -> Vec<u8> {
    let t = txnid.to_string();
    let r = ok(rust("mdb_dump", &["-i", &t, s(env)], b""), "mdb_dump -i");
    let e = rlmdb::Environment::builder()
        .flags(rlmdb::EnvironmentFlags::READ_ONLY)
        .max_dbs(8)
        .open(env)
        .unwrap();
    let mut api = Vec::new();
    e.incr_dump(&mut api, txnid).unwrap();
    assert!(
        api == r,
        "Environment::incr_dump({t}) and mdb_dump -i differ"
    );
    r
}

/// `mdb_dump10 -i txnid env` and rlmdb's `mdb_dump -i` and
/// `Environment::incr_dump` must write the same bytes (environments where
/// no stale page is involved, row 72); returns them.
fn dump_both(env: &Path, txnid: u64) -> Vec<u8> {
    let t = txnid.to_string();
    let c = ok(
        run(bins::mdb_dump10(), "mdb_dump", &["-i", &t, s(env)], b""),
        "mdb_dump10 -i",
    );
    assert!(
        dump_rust(env, txnid) == c,
        "mdb_dump -i {t}: C 1.0 and rlmdb differ"
    );
    c
}

/// The data pages of a dump of `ps`-byte pages come in file order, each
/// where the one before ends: the walk read no stale page as a header
/// (row 72), so the dump restores what the source holds.
fn sane(dump: &[u8], ps: usize) -> bool {
    let (mut off, mut next) = (0, 2u64);
    while off < dump.len() {
        let u64_at = |o: usize| u64::from_ne_bytes(dump[off + o..off + o + 8].try_into().unwrap());
        let flags = u16::from_ne_bytes([dump[off + 18], dump[off + 19]]);
        let pages = if flags & 0x04 != 0 {
            u64::from(u32::from_ne_bytes(
                dump[off + 20..off + 24].try_into().unwrap(),
            ))
        } else {
            1
        };
        let pgno = u64_at(0);
        if pgno >= 2 {
            if pgno < next || pages == 0 {
                return false;
            }
            next = pgno + pages;
        }
        off += pages.max(1) as usize * ps;
    }
    off == dump.len()
}

fn last_txnid(env: &Path) -> u64 {
    let e = rlmdb::Environment::builder()
        .flags(rlmdb::EnvironmentFlags::READ_ONLY)
        .open(env)
        .unwrap();
    e.info().unwrap().last_txnid as u64
}

fn page_size(env: &Path) -> usize {
    let e = rlmdb::Environment::builder()
        .flags(rlmdb::EnvironmentFlags::READ_ONLY)
        .open(env)
        .unwrap();
    e.page_size()
}

/// A plain copy of `env` by C 1.0 into the new directory `name` next to it.
fn copy10(env: &TestEnv, name: &str) -> TestEnv {
    let dst = env.derived(name);
    std::fs::create_dir(dst.path()).unwrap();
    ok(
        run(
            bins::mdb_copy10(),
            "mdb_copy",
            &[s(env.path()), s(dst.path())],
            b"",
        ),
        "mdb_copy10",
    );
    dst
}

/// A copy of the copy `base`'s data file in the new directory `name`.
fn copy_of(base: &TestEnv, name: &str) -> TestEnv {
    let dst = base.derived(name);
    std::fs::create_dir(dst.path()).unwrap();
    std::fs::copy(base.data_file(), dst.data_file()).unwrap();
    dst
}

/// Runs a peer command, which must succeed.
fn peer(p: Peer, cmd: &str, env: &TestEnv, args: &[&str]) {
    if let Err(e) = p.on(cmd, env).args(args).run() {
        panic!("{p:?} {cmd} {args:?}: {e}");
    }
}

/// The data sets of [`history`], for `cpeer10 verify`: `(db, count, seed,
/// data options)`, as the source holds them at the end.
fn data_sets(big: &str) -> Vec<(&'static str, &'static str, &'static str, Vec<String>)> {
    let v = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    vec![
        ("a", "120", "1", v(&["--big", big])),
        ("b", "150", "2", v(&["--deleted-every", "3"])),
        ("c", "100", "3", v(&["--dupsort", "--deleted-every", "2"])),
        ("d", "60", "4", v(&["--big", big])),
    ]
}

/// One step of the history of [`history`]: the C 1.0 and the Rust peer in
/// turns, `per_txn` records per transaction. Big values are only ever
/// added (a freed overflow run makes the dump read stale pages, row 72).
fn history(env: &TestEnv, step: usize, big: &str, per_txn: &str) {
    let (c, r) = (Peer::C10, Peer::Rust);
    let txn = ["--per-txn", per_txn];
    match step {
        0 => {
            peer(
                c,
                "fill",
                env,
                &[&["a", "120", "1", "--big", big][..], &txn].concat(),
            );
            peer(r, "fill", env, &[&["b", "150", "2"][..], &txn].concat());
        }
        1 => {
            peer(
                r,
                "delete",
                env,
                &[&["b", "150", "2", "--every", "3"][..], &txn].concat(),
            );
            peer(
                c,
                "fill",
                env,
                &[&["c", "100", "3", "--dupsort"][..], &txn].concat(),
            );
        }
        _ => {
            let del = ["c", "100", "3", "--every", "2", "--dupsort"];
            peer(c, "delete", env, &[&del[..], &txn].concat());
            peer(
                r,
                "fill",
                env,
                &[&["d", "60", "4", "--big", big][..], &txn].concat(),
            );
        }
    }
}

/// Loads `dumps` into `target` with `loader` (`mdb_load10` or rlmdb's
/// `mdb_load`), each from standard input.
fn load_chain(c_loads: bool, target: &TestEnv, dumps: &[&[u8]]) {
    for d in dumps {
        let r = if c_loads {
            run(bins::mdb_load10(), "mdb_load", &["-i", s(target.path())], d)
        } else {
            rust("mdb_load", &["-i", s(target.path())], d)
        };
        ok(
            r,
            if c_loads {
                "mdb_load10 -i"
            } else {
                "mdb_load -i"
            },
        );
    }
}

/// `target` is the source byte for byte; C 1.0 audits it and reads the
/// data sets in it.
fn assert_restored(target: &TestEnv, src: &TestEnv, big: &str, what: &str) {
    let (a, b) = (
        std::fs::read(target.data_file()).unwrap(),
        std::fs::read(src.data_file()).unwrap(),
    );
    assert!(a == b, "{what}: the restored file differs from the source");
    compare_data_files(&target.data_file(), &src.data_file()).unwrap();
    if let Err(e) = audit10(target.path()) {
        panic!("{what}: {e}");
    }
    for (db, count, seed, opts) in data_sets(big) {
        let mut args = vec![db, count, seed];
        args.extend(opts.iter().map(String::as_str));
        peer(Peer::C10, "verify", target, &args);
    }
}

fn chain(ps: Option<u32>) {
    let env = TestEnv::new()
        .unwrap()
        .with_format(Format::V10)
        .with_page_size(ps);
    peer(Peer::C10, "create", &env, &["67108864", "8"]);
    let psize = page_size(env.path());
    let big = (2 * psize + 100).to_string();
    // Few records per transaction on small pages: a transaction that frees
    // many pages stores its free-list record on overflow pages, and once
    // that is freed the walk reads its stale pages (row 72).
    let per_txn = if psize < 1024 { "2" } else { "10" };
    let what = format!("page size {psize}");

    history(&env, 0, &big, per_txn);
    let base = copy10(&env, "base");
    let t0 = last_txnid(env.path());
    history(&env, 1, &big, per_txn);
    let t1 = last_txnid(env.path());
    let inc1 = dump_both(env.path(), t0);
    history(&env, 2, &big, per_txn);
    let t2 = last_txnid(env.path());
    let inc2 = dump_both(env.path(), t1);
    assert!(sane(&inc1, psize) && sane(&inc2, psize), "{what}: row 72");
    // More txnids: the first commit's, one before the last, the last and
    // beyond (nothing), and SIZE_MAX.
    for t in [1, t0 + 1, t2 - 1] {
        dump_both(env.path(), t);
    }
    for t in [t2, t2 + 5, u64::MAX] {
        assert!(dump_both(env.path(), t).is_empty(), "{what}: since {t}");
    }

    // C's chain loaded by rlmdb, and rlmdb's by C (the dumps are the same
    // bytes).
    for c_loads in [false, true] {
        let target = copy_of(&base, if c_loads { "c-loads" } else { "r-loads" });
        load_chain(c_loads, &target, &[&inc1, &inc2]);
        assert_restored(&target, &env, &big, &format!("{what}, C loads: {c_loads}"));
    }
}

#[test]
fn chain_at_256_byte_pages() {
    chain(Some(256));
}

#[test]
fn chain_at_1k_pages() {
    chain(Some(1024));
}

#[test]
fn chain_at_os_pages() {
    chain(None);
}

#[test]
fn chain_at_64k_pages() {
    chain(Some(65536));
}

/// Row 72: C 1.0's tools build the environment (4 KiB pages, a 25-page
/// value in a named database, dropped, then four small commits that reuse
/// the run's first pages). C's dump takes the rest of the old value for
/// headers: records of page 0x6161616161616161 with 'a's, which rlmdb's
/// load refuses (`MDB_CORRUPTED`) as C's fails at them (`EFBIG` where the
/// file system refuses such an offset); with 'e's, whose bytes set
/// `P_OVERFLOW`, a "run" past the file and `EFAULT`. rlmdb's dump holds
/// the reused pages and nothing of the old value, and C 1.0's
/// `mdb_load -i` makes the copy the source with it.
#[test]
fn c10_restores_the_source_from_rlmdbs_dump_of_a_freed_run() {
    for byte in *b"ae" {
        let env = TestEnv::new().unwrap().with_format(Format::V10);
        let value: String = std::iter::repeat_n(format!("{byte:02x}"), 100_000).collect();
        let input = format!(
            "VERSION=3\nformat=bytevalue\ndatabase=big\ntype=btree\nmapsize=10485760\n\
             maxreaders=126\ndb_pagesize=4096\nHEADER=END\n {}\n {value}\nDATA=END\n",
            "6269676b6579"
        );
        let load = |args: &[&str], input: &[u8]| {
            let mut a = args.to_vec();
            a.push(s(env.path()));
            ok(run(bins::mdb_load10(), "mdb_load", &a, input), "mdb_load10");
        };
        load(&[], input.as_bytes());
        let base = copy10(&env, "base");
        ok(
            run(
                bins::mdb_drop10(),
                "mdb_drop",
                &["-d", "-s", "big", s(env.path())],
                b"",
            ),
            "mdb_drop10",
        );
        for i in 0..4 {
            load(&["-T"], format!("k{i}\nv{i}\n").as_bytes());
        }
        let c = run(
            bins::mdb_dump10(),
            "mdb_dump",
            &["-i", "1", s(env.path())],
            b"",
        );
        let r = ok(
            rust("mdb_dump", &["-i", "1", s(env.path())], b""),
            "mdb_dump -i 1",
        );
        assert!(sane(&r, 4096), "'{}'", byte as char);
        if byte == b'a' {
            let c = ok(c, "mdb_dump10 -i 1");
            let garbage = 0x6161_6161_6161_6161u64.to_ne_bytes();
            assert!(c.chunks(4096).any(|p| p[..8] == garbage));
            // rlmdb's dump is C's without the stale records.
            let kept: Vec<u8> = c
                .chunks(4096)
                .filter(|p| p[..8] != garbage)
                .flatten()
                .copied()
                .collect();
            assert!(kept == r);
            let target = copy_of(&base, "r-loads-c");
            let l = rust("mdb_load", &["-i", s(target.path())], &c);
            assert_eq!(l.code, 1);
            assert!(
                l.stderr
                    .starts_with("mdb_env_incr_load failed, error -30796 MDB_CORRUPTED"),
                "{}",
                l.stderr
            );
        } else {
            assert_eq!(c.code, 1);
            assert!(
                c.stderr.contains("mdb_env_incr_dump failed, error 14 "),
                "{}",
                c.stderr
            );
            // Before the stale "run" C wrote what rlmdb writes.
            let stale = 0x6565_6565_6565_6565u64.to_ne_bytes();
            let k = c.stdout.chunks(4096).position(|p| p[..8] == stale).unwrap() * 4096;
            assert!(k > 0 && r.len() > k && r[..k] == c.stdout[..k]);
        }
        let target = copy_of(&base, "c-loads-r");
        load_chain(true, &target, &[&r]);
        let what = format!("'{}'", byte as char);
        assert!(
            std::fs::read(target.data_file()).unwrap() == std::fs::read(env.data_file()).unwrap(),
            "{what}"
        );
        if let Err(e) = audit10(target.path()) {
            panic!("{what}: {e}");
        }
    }
}

/// The free pages of the newest snapshot of `env` (its free list).
fn free_pages(env: &Path) -> std::collections::BTreeSet<u64> {
    let e = rlmdb::Environment::builder()
        .flags(rlmdb::EnvironmentFlags::READ_ONLY)
        .max_dbs(8)
        .open(env)
        .unwrap();
    let txn = e.begin_ro_txn().unwrap();
    let mut c = rlmdb::Transaction::cursor(&txn, rlmdb::Dbi::FREE).unwrap();
    let mut free = std::collections::BTreeSet::new();
    while let Some((_, idl)) = c.next().unwrap() {
        let ids: Vec<u64> = idl
            .chunks_exact(8)
            .map(|b| u64::from_ne_bytes(b.try_into().unwrap()))
            .collect();
        free.extend(&ids[1..=ids[0] as usize]);
    }
    free
}

/// Row 72's other half: a free page whose stale overflow header, older
/// than the copy, covers a live run written after it (a freed run's head
/// left over while the run's pages were reused; written into the file
/// here). C's walk steps over the live run, so C's own chain restores a
/// file without it; rlmdb's walk goes on page by page, and C 1.0's
/// `mdb_load -i` of rlmdb's dump restores the source (but for the stale
/// page, which is free and no record), which C 1.0 audits and verifies.
#[test]
fn c10_restores_a_live_run_c10s_dump_steps_over() {
    let env = TestEnv::new()
        .unwrap()
        .with_format(Format::V10)
        .with_page_size(Some(4096));
    peer(Peer::C10, "create", &env, &["67108864", "8"]);
    peer(Peer::C10, "fill", &env, &["low", "400", "1"]);
    peer(Peer::C10, "drop", &env, &["low", "--delete"]);
    peer(Peer::C10, "fill", &env, &["m", "1", "2"]);
    peer(Peer::C10, "fill", &env, &["n", "1", "3"]);
    let base = copy10(&env, "base");
    let t0 = last_txnid(env.path());
    peer(
        Peer::Rust,
        "fill",
        &env,
        &["big", "1", "4", "--big", "262000"],
    );
    let t1 = last_txnid(env.path());

    // The live run and a free page below it.
    let free = free_pages(env.path());
    let mut file = std::fs::read(env.data_file()).unwrap();
    let u64_at = |f: &[u8], o: usize| u64::from_ne_bytes(f[o..o + 8].try_into().unwrap());
    let (head, pages) = (2..(file.len() / 4096) as u64)
        .filter(|p| !free.contains(p))
        .find_map(|p| {
            let o = p as usize * 4096;
            let flags = u16::from_ne_bytes([file[o + 18], file[o + 19]]);
            let n = u32::from_ne_bytes(file[o + 20..o + 24].try_into().unwrap());
            (u64_at(&file, o) == p && flags & 0x04 != 0 && u64_at(&file, o + 8) > t0)
                .then_some((p, u64::from(n)))
        })
        .expect("a live overflow run written after the copy");
    let stale = *free
        .range(..head)
        .next_back()
        .expect("a free page below it");
    let mut hdr = Vec::new();
    hdr.extend_from_slice(&stale.to_ne_bytes());
    hdr.extend_from_slice(&1u64.to_ne_bytes());
    hdr.extend_from_slice(&0u16.to_ne_bytes());
    hdr.extend_from_slice(&0x04u16.to_ne_bytes());
    hdr.extend_from_slice(&((head + pages - stale) as u32).to_ne_bytes());
    let at = stale as usize * 4096;
    file[at..at + hdr.len()].copy_from_slice(&hdr);
    std::fs::write(env.data_file(), &file).unwrap();
    assert_eq!(last_txnid(env.path()), t1);

    let c = ok(
        run(
            bins::mdb_dump10(),
            "mdb_dump",
            &["-i", &t0.to_string(), s(env.path())],
            b"",
        ),
        "mdb_dump10 -i",
    );
    let r = dump_rust(env.path(), t0);
    let run_bytes = head as usize * 4096..(head + pages) as usize * 4096;
    // C's chain: the live run is missing.
    let c_target = copy_of(&base, "c-loads-c");
    load_chain(true, &c_target, &[&c]);
    let got = std::fs::read(c_target.data_file()).unwrap();
    assert!(got.len() < run_bytes.end || got[run_bytes.clone()] != file[run_bytes.clone()]);
    // rlmdb's dump, loaded by C 1.0.
    let r_target = copy_of(&base, "c-loads-r");
    load_chain(true, &r_target, &[&r]);
    let got = std::fs::read(r_target.data_file()).unwrap();
    assert_eq!(got.len(), file.len());
    assert!(got[..at] == file[..at] && got[at + 4096..] == file[at + 4096..]);
    if let Err(e) = audit10(r_target.path()) {
        panic!("{e}");
    }
    peer(
        Peer::C10,
        "verify",
        &r_target,
        &["big", "1", "4", "--big", "262000"],
    );
    peer(Peer::C10, "verify", &r_target, &["m", "1", "2"]);
}

/// The source, a C 1.0 copy of it and the dump that brings the copy up to
/// date, for the exclusivity tests: `(source, copy, dump, txnid after)`.
fn source_copy_and_dump() -> (TestEnv, TestEnv, Vec<u8>, u64) {
    let env = TestEnv::new().unwrap().with_format(Format::V10);
    peer(Peer::C10, "create", &env, &["67108864", "8"]);
    peer(
        Peer::C10,
        "fill",
        &env,
        &["a", "300", "1", "--per-txn", "50"],
    );
    let base = copy10(&env, "base");
    let t0 = last_txnid(env.path());
    peer(
        Peer::Rust,
        "fill",
        &env,
        &["b", "300", "2", "--per-txn", "50"],
    );
    let dump = dump_both(env.path(), t0);
    let t1 = last_txnid(env.path());
    (env, base, dump, t1)
}

/// The txnid in a `READY txnid=N ...` line.
fn ready_txnid(line: &str) -> u64 {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix("txnid="))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no txnid in {line:?}"))
}

/// Row 65: a reader of another process (C 1.0) pinning a snapshot makes
/// rlmdb's load fail with `EBUSY`, changing nothing; a process that only
/// has the environment open does not. That process's next read
/// transaction reads the loaded snapshot: rlmdb published it in the lock
/// file, which that process keeps from being set up anew.
#[test]
fn a_c10_reader_keeps_the_load_out_and_then_sees_its_result() {
    let (src, base, dump, t1) = source_copy_and_dump();
    let target = copy_of(&base, "target");
    let before = std::fs::read(target.data_file()).unwrap();

    let mut reader = Peer::C10
        .on("hold-reader", &target)
        .args(["60", "--until-stdin"])
        .spawn()
        .unwrap();
    reader.wait_ready().unwrap();
    let r = rust("mdb_load", &["-i", s(target.path())], &dump);
    assert_eq!(r.code, 1);
    let busy = rlmdb::strerror(libc::EBUSY);
    assert_eq!(
        r.stderr,
        format!("mdb_env_incr_load failed, error {} {busy}\n", libc::EBUSY)
    );
    assert!(std::fs::read(target.data_file()).unwrap() == before);
    reader.release().unwrap();
    reader.finish().unwrap().ok().unwrap();

    let mut holder = Peer::C10
        .on("hold-env", &target)
        .args(["60", "--until-stdin"])
        .spawn()
        .unwrap();
    holder.wait_ready().unwrap();
    ok(
        rust("mdb_load", &["-i", s(target.path())], &dump),
        "mdb_load -i",
    );
    let mut reader = Peer::C10
        .on("hold-reader", &target)
        .args(["60", "--until-stdin"])
        .spawn()
        .unwrap();
    assert_eq!(ready_txnid(&reader.wait_ready().unwrap()), t1);
    reader.release().unwrap();
    reader.finish().unwrap().ok().unwrap();
    peer(Peer::C10, "verify", &target, &["b", "300", "2"]);
    holder.release().unwrap();
    holder.finish().unwrap().ok().unwrap();
    assert!(std::fs::read(target.data_file()).unwrap() == std::fs::read(src.data_file()).unwrap());
}

/// Row 65: rlmdb's load waits for a write transaction of another process
/// (C 1.0's `hold-writer`) to end, holding the writer lock itself.
#[test]
fn the_load_waits_for_a_c10_writer() {
    let (src, base, dump, _) = source_copy_and_dump();
    let target = copy_of(&base, "target");
    let mut writer = Peer::C10
        .on("hold-writer", &target)
        .args(["60", "--until-stdin"])
        .spawn()
        .unwrap();
    writer.wait_ready().unwrap();
    let path: PathBuf = target.path().to_path_buf();
    let loader = std::thread::spawn(move || rust("mdb_load", &["-i", s(&path)], &dump));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !loader.is_finished(),
        "the load did not wait for the writer"
    );
    writer.release().unwrap();
    writer.finish().unwrap().ok().unwrap();
    ok(loader.join().unwrap(), "mdb_load -i");
    assert!(std::fs::read(target.data_file()).unwrap() == std::fs::read(src.data_file()).unwrap());
}

/// Row 65 with `MDB_PREVSNAPSHOT`: rlmdb opens the copy with the flag,
/// which holds the lock file exclusively (no other process can open the
/// environment), and loads the dump, which ends the flag as a commit does:
/// a C 1.0 reader can then open the environment while rlmdb's handle is
/// still open, and reads the loaded snapshot.
#[test]
fn a_load_ends_prev_snapshot_for_other_processes() {
    let (src, base, dump, t1) = source_copy_and_dump();
    let target = copy_of(&base, "target");
    let mut env = rlmdb::Environment::builder()
        .max_dbs(8)
        .flags(rlmdb::EnvironmentFlags::PREV_SNAPSHOT)
        .open(target.path())
        .unwrap();
    env.incr_load(&dump[..]).unwrap();
    assert!(!env.flags().contains(rlmdb::EnvironmentFlags::PREV_SNAPSHOT));
    let mut reader = Peer::C10
        .on("hold-reader", &target)
        .args(["60", "--until-stdin"])
        .spawn()
        .unwrap();
    assert_eq!(
        ready_txnid(
            &reader
                .wait_for_line("READY", Duration::from_secs(30))
                .unwrap()
        ),
        t1
    );
    reader.release().unwrap();
    reader.finish().unwrap().ok().unwrap();
    drop(env);
    assert!(std::fs::read(target.data_file()).unwrap() == std::fs::read(src.data_file()).unwrap());
}
