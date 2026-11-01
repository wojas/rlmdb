//! `mdb_dump`: the Rust tool against the C tool on every test environment
//! and option combination (identical stdout, stderr, exit code and `-f`
//! output files).

mod common;

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

use common::*;

fn db_names(env: &Path, extra: &[&str]) -> Vec<Vec<u8>> {
    let mut argv = args(extra);
    argv.extend(args(&["-l"]));
    argv.push(p(env));
    run(Impl::C, "mdb_dump", &argv, b"", env.parent().unwrap())
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

fn dump_matrix(name: &str) {
    let dir = tempdir();
    let env = env_copy(name, dir.path());
    let extra: &[&str] = if name == "nosubdir" { &["-n"] } else { &[] };
    let mut cases: Vec<Vec<Vec<u8>>> = [
        "",
        "-p",
        "-a",
        "-a -p",
        "-pa",
        "-l",
        "-lp",
        "-la",
        "-s missing",
    ]
    .iter()
    .map(|c| words(c))
    .collect();
    for db in db_names(&env, extra) {
        cases.push(vec![b"-s".to_vec(), db.clone()]);
        cases.push(vec![b"-p".to_vec(), b"-s".to_vec(), db.clone()]);
    }
    for case in cases {
        let mut argv = args(extra);
        argv.extend(case);
        argv.push(p(&env));
        assert_same("mdb_dump", &argv, b"", dir.path());
    }
    // -f: the same file contents.
    for case in ["", "-p", "-a", "-p -a"] {
        let c_file = dir.path().join("c.out");
        let r_file = dir.path().join("r.out");
        let mut outputs = Vec::new();
        for (imp, file) in [(Impl::C, &c_file), (Impl::Rust, &r_file)] {
            let mut argv = args(extra);
            argv.extend(words(case));
            argv.extend(args(&[b"-f".to_vec(), p(file), p(&env)]));
            let r = run(imp, "mdb_dump", &argv, b"", dir.path());
            outputs.push((r, fs::read(file).unwrap()));
        }
        assert_eq!(outputs[0], outputs[1], "-f {case}");
        // Same as on stdout.
        let mut argv = args(extra);
        argv.extend(words(case));
        argv.push(p(&env));
        assert_eq!(
            run(Impl::C, "mdb_dump", &argv, b"", dir.path()).stdout,
            outputs[0].1
        );
    }
}

#[test]
fn matrix_main() {
    dump_matrix("main");
}

#[test]
fn matrix_named() {
    dump_matrix("named");
}

#[test]
fn matrix_dupmain() {
    dump_matrix("dupmain");
}

#[test]
fn matrix_empty() {
    dump_matrix("empty");
}

#[test]
fn matrix_nosubdir() {
    dump_matrix("nosubdir");
}

#[test]
fn matrix_mtest() {
    for name in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
        dump_matrix(name);
    }
}

#[test]
fn errors_and_usage() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let e_owned = p(&env);
    let e: &[u8] = &e_owned;
    let out = p(&dir.path().join("out.txt"));
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![],
        args(&["-V"]),
        args(&[b"-a".as_slice(), b"-s", b"plain", e]),
        args(&[b"-s".as_slice(), b"plain", b"-a", e]),
        args(&[b"-l".as_slice(), b"-s", b"plain", e]),
        args(&[b"-s".as_slice(), b"plain", b"-l", e]),
        args(&[e, e]),
        args(&[b"-x".as_slice(), e]),
        args(&[b"-f".as_slice()]),
        args(&[b"-f".as_slice(), b"/nonexistent/dir/file", e]),
        args(&[b"-f".as_slice(), &p(dir.path()), e]),
        args(&[e, b"-p".as_slice()]),
        args(&[b"-".as_slice(), e]),
        args(&["/nonexistent/env"]),
        args(&[b"-n".as_slice(), e]),
        args(&[b"-s".as_slice(), b"zz-plain-value", e]),
        args(&[b"-s".as_slice(), b"a", e]),
        args(&[b"-x".as_slice(), b"-f", &out, e]),
    ];
    for argv in cases {
        assert_same("mdb_dump", &argv, b"", dir.path());
    }
    // -V after -f writes the version into the file.
    for imp in [Impl::C, Impl::Rust] {
        let r = run(
            imp,
            "mdb_dump",
            &args(&[b"-f".as_slice(), &out, b"-V"]),
            b"",
            dir.path(),
        );
        assert_eq!((r.code, r.stdout.len()), (0, 0));
        assert_eq!(
            fs::read(dir.path().join("out.txt")).unwrap(),
            version_line(imp)
        );
    }
}

/// A reader that closes the pipe early: C gets `SIGPIPE`, stops after the
/// current record and reports "Interrupted system call"; Rust sees `EPIPE`
/// and does the same. Windows has no `SIGPIPE`: C writes on, its writes
/// failing, and exits 0 without a word; so does Rust.
#[test]
fn closed_pipe() {
    let dir = tempdir();
    let env = env_copy("main", dir.path());
    let mut results = Vec::new();
    for imp in [Impl::C, Impl::Rust] {
        let mut child = command_as(&bin(imp, "mdb_dump"), "mdb_dump")
            .arg(&env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut first = [0u8; 100];
        stdout.read_exact(&mut first).unwrap();
        drop(stdout);
        let out = child.wait_with_output().unwrap();
        results.push((first.to_vec(), out.stderr, out.status.code()));
    }
    assert_eq!(results[0], results[1]);
    if cfg!(windows) {
        assert_eq!((results[1].1.len(), results[1].2), (0, Some(0)));
        return;
    }
    assert_eq!(results[1].2, Some(1));
    let msg = String::from_utf8_lossy(&results[1].1);
    assert!(msg.ends_with(": Interrupted system call\n"), "{msg}");
}

/// Write errors (here: a full device) are reported; C ignores them and
/// exits with 0 (see README.md).
#[cfg(target_os = "linux")]
#[test]
fn write_errors() {
    let dir = tempdir();
    let env = env_copy("main", dir.path());
    let r = run(
        Impl::Rust,
        "mdb_dump",
        &args(&[b"-f".to_vec(), b"/dev/full".to_vec(), p(&env)]),
        b"",
        dir.path(),
    );
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stderr_str(),
        format!("mdb_dump: {}: No space left on device\n", env.display())
    );
    let c = run(
        Impl::C,
        "mdb_dump",
        &args(&[b"-f".to_vec(), b"/dev/full".to_vec(), p(&env)]),
        b"",
        dir.path(),
    );
    assert_eq!((c.code, c.stderr.len()), (0, 0));
    // Only at the final flush.
    let empty = env_copy("empty", dir.path());
    let r = run(
        Impl::Rust,
        "mdb_dump",
        &args(&[b"-f".to_vec(), b"/dev/full".to_vec(), p(&empty)]),
        b"",
        dir.path(),
    );
    assert_eq!(
        (r.code, r.stderr_str().as_str()),
        (1, "mdb_dump: write error: No space left on device\n")
    );
    let r = run(Impl::Rust, "mdb_stat", &args(&[p(&empty)]), b"", dir.path());
    assert_eq!(r.code, 0);
}

/// A data file whose main or free DB (`free`) is a damaged tree of 20
/// branch levels over one leaf, every branch page naming the page below
/// four times: 24 pages that a walk crosses 4^20 times.
fn shared_children_file(layout: &'static rlmdb::format::Layout, free: bool) -> Vec<u8> {
    use rlmdb::format::page::{NodeData, P_BRANCH, P_LEAF};
    use rlmdb::format::{Db, Meta, PageMut};
    const PS: usize = 4096;
    const LEVELS: u64 = 20;
    let key = |i: u64| -> Vec<u8> {
        match (i, free) {
            (0, _) => vec![],
            (_, true) => i.to_ne_bytes().to_vec(),
            (_, false) => vec![b'a' + i as u8],
        }
    };
    let leaf = |pgno: u64, k: &[u8], v: &[u8]| {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut buf, layout);
        p.init(pgno, P_LEAF);
        p.add_node(0, k, NodeData::Inline(v), 0).unwrap();
        buf
    };
    let record = (1u64.to_ne_bytes(), 0u64.to_ne_bytes());
    let mut pages = vec![if free {
        leaf(2, &record.0, &record.1)
    } else {
        leaf(2, b"key", b"value")
    }];
    for pgno in 3..3 + LEVELS {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::with_layout(&mut buf, layout);
        p.init(pgno, P_BRANCH);
        for i in 0..4 {
            p.add_node(i as usize, &key(i), NodeData::Child(pgno - 1), 0)
                .unwrap();
        }
        pages.push(buf);
    }
    let other = 3 + LEVELS;
    pages.push(if free {
        leaf(other, b"a", b"b")
    } else {
        leaf(other, &record.0, &record.1)
    });
    let shared = Db {
        depth: LEVELS as u16 + 1,
        branch_pages: LEVELS,
        leaf_pages: 1,
        entries: 1,
        root: 2 + LEVELS,
        ..Db::default()
    };
    let one = Db {
        depth: 1,
        leaf_pages: 1,
        entries: 1,
        root: other,
        ..Db::default()
    };
    let m0 = Meta::new_env_with_layout(layout, PS as u32, 1 << 24, 0);
    let mut m1 = m0;
    // Snapshot 9 on meta page 1: the free record (txnid 1) is old.
    m1.txnid = 9;
    m1.last_pgno = 1 + pages.len() as u64;
    let (f, m) = if free { (shared, one) } else { (one, shared) };
    m1.dbs[0] = Db {
        flags: m0.dbs[0].flags,
        pad: m0.dbs[0].pad,
        ..f
    };
    m1.dbs[1] = m;
    let mut file = vec![0u8; 2 * PS];
    m0.write_initial_pages_with_layout(&mut file, PS, layout);
    m1.write(&mut file[PS + layout.meta_offset()..]);
    file.extend(pages.concat());
    file
}

/// Runs the Rust `tool` on `env`, killing it and failing if it takes more
/// than a minute.
fn run_bounded(tool: &str, argv: &[&str], env: &Path) -> Run {
    let mut child = command_as(&bin(Impl::Rust, tool), tool)
        .args(argv)
        .arg(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            r.read_to_end(&mut buf).unwrap();
            buf
        })
    };
    let (out, err) = (read(Box::new(out)), read(Box::new(err)));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("{tool} {argv:?}: still running after a minute");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    Run {
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
        code: status.code().unwrap_or(-1),
    }
}

/// Trees whose branch pages share their children: C's `mdb_dump` prints
/// the main DB's record 4^20 times and `mdb_stat -f` reads the free DB's
/// as often, hours for a 24-page file. The library stops a walk that steps
/// onto more leaves than the file has pages (`KNOWN_C_BUGS.md` #50), so
/// `mdb_dump` fails with `MDB_CORRUPTED` after a few copies of the record
/// and `mdb_stat -f`, which ends its walk at an error as C does, reports
/// what it read.
#[test]
fn walks_over_shared_children_stop() {
    use rlmdb::format::layout::{V09, V10};
    for layout in [&V09, &V10] {
        let what = format!("{:?}", layout.format);
        let dir = tempdir();
        let main = dir.path().join("main");
        let free = dir.path().join("free");
        for (env, is_free) in [(&main, false), (&free, true)] {
            fs::create_dir(env).unwrap();
            fs::write(env.join("data.mdb"), shared_children_file(layout, is_free)).unwrap();
        }
        let r = run_bounded("mdb_dump", &[], &main);
        assert_eq!(r.code, 1, "{what}: {r:?}");
        assert!(r.stderr_str().contains("MDB_CORRUPTED"), "{what}: {r:?}");
        // One copy per leaf step; the walk stops at the file's page count
        // (24 pages).
        let copies = r.stdout_text().matches("\n 6b6579\n").count();
        assert!((2..=25).contains(&copies), "{what}: {copies} copies");
        let r = run_bounded("mdb_stat", &["-f"], &free);
        assert_eq!(r.code, 0, "{what}: {r:?}");
        assert!(
            r.stdout_text().contains("  Free pages: 0\n"),
            "{what}: {r:?}"
        );
    }
}
