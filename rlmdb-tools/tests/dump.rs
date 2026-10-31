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
