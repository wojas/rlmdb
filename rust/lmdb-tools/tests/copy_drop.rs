//! `mdb_copy` and `mdb_drop`: the Rust tools against the C ones (for
//! `mdb_drop`, LMDB's first `mdb_drop.c` built against the 0.9.36 library,
//! see `tests/c/mdb_drop.c`). Copies must be byte-identical to C's and
//! read back by the C tools exactly like the source; drops must leave the
//! environment exactly as the C tool does, free list included.

mod common;

use std::fs;
use std::path::Path;

use common::*;

/// Everything the C tools say about an environment, with its path
/// replaced (so environments in different places compare equal).
fn c_view(env: &Path, extra: &[&str]) -> Vec<u8> {
    let mut out = c_dump_all(env, extra);
    for opts in ["-efa", "-ffa"] {
        let mut argv = args(extra);
        argv.extend(words(opts));
        argv.push(p(env));
        let r = run(Impl::C, "mdb_stat", &argv, b"", env.parent().unwrap());
        out.extend_from_slice(&r.stdout);
        out.extend_from_slice(&r.stderr);
    }
    let path = String::from_utf8_lossy(&p(env)).into_owned();
    String::from_utf8_lossy(&out)
        .replace(&path, "<env>")
        .into_bytes()
}

const ALL_ENVS: &[&str] = &[
    "main", "named", "dupmain", "empty", "nosubdir", "mtest", "mtest2", "mtest3", "mtest4",
    "mtest5",
];

#[test]
fn copies() {
    for &name in ALL_ENVS {
        let dir = tempdir();
        let src = env_copy(name, dir.path());
        let nosub = name == "nosubdir";
        let extra: &[&str] = if nosub { &["-n"] } else { &[] };
        let source_view = c_view(&src, extra);
        for compact in [false, true] {
            let mut files = Vec::new();
            for side in ["c", "r"] {
                let dst = dir.path().join(format!("{side}-{compact}"));
                let target = if nosub {
                    dst.clone()
                } else {
                    fs::create_dir_all(&dst).unwrap();
                    dst.join("data.mdb")
                };
                let imp = if side == "c" { Impl::C } else { Impl::Rust };
                let mut argv = args(extra);
                if compact {
                    argv.push(b"-c".to_vec());
                }
                argv.extend([p(&src), p(&dst)]);
                let r = run(imp, "mdb_copy", &argv, b"", dir.path());
                assert_eq!(
                    (r.code, r.stderr_str().as_str(), r.stdout.len()),
                    (0, "", 0),
                    "{name} {imp:?}"
                );
                // To standard output: the same bytes.
                let mut argv = args(extra);
                if compact {
                    argv.push(b"-c".to_vec());
                }
                argv.push(p(&src));
                let piped = run(imp, "mdb_copy", &argv, b"", dir.path());
                assert_eq!(piped.code, 0, "{}", piped.stderr_str());
                let bytes = fs::read(&target).unwrap();
                assert!(
                    piped.stdout == bytes,
                    "{name} {imp:?} compact={compact}: stdout differs from the file copy"
                );
                // No lock file is created.
                if !nosub {
                    assert!(!dst.join("lock.mdb").exists());
                }
                files.push((dst, bytes));
            }
            assert!(
                files[0].1 == files[1].1,
                "{name} compact={compact}: copies differ"
            );
            let copy_view = c_view(&files[1].0, extra);
            if compact {
                // Same contents as the source. The main DB's records of
                // named databases hold their (renumbered) root pages, so
                // with named databases only their contents compare.
                let named = ["named", "mtest2", "mtest3", "mtest4", "mtest5"].contains(&name);
                let dump = |env: &Path| {
                    let mut argv = args(extra);
                    if named {
                        argv.push(b"-a".to_vec());
                    }
                    argv.push(p(env));
                    strip_mapaddr(&run(Impl::C, "mdb_dump", &argv, b"", dir.path()).stdout)
                };
                assert!(
                    dump(&files[1].0) == dump(&src),
                    "{name}: compact copy has other contents"
                );
            } else {
                assert!(
                    copy_view == source_view,
                    "{name}: plain copy differs from source"
                );
            }
        }
    }
}

#[test]
fn copy_errors_and_usage() {
    let dir = tempdir();
    let src = env_copy("named", dir.path());
    let s = p(&src);
    let existing = dir.path().join("existing");
    fs::create_dir_all(&existing).unwrap();
    fs::write(existing.join("data.mdb"), b"x").unwrap();
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![],
        args(&["-V"]),
        args(&["-c", "-n", "-V"]),
        args(&["-V", "-x"]),
        args(&["-x", "-V"]),
        args(&["-n"]),
        args(&["-"]),
        args(&[b"-".as_slice(), &s]),
        args(&[b"-cn".as_slice(), &s]),
        args(&[b"--".as_slice(), &s]),
        args(&[s.as_slice(), b"a", b"b"]),
        args(&["/nonexistent/env"]),
        args(&["/nonexistent/env", "/tmp"]),
        args(&[s.as_slice(), &p(&existing)]),
        args(&[s.as_slice(), b"/nonexistent/dir"]),
        args(&[b"-c".as_slice(), &s, &p(&existing)]),
        args(&[b"-n".as_slice(), &s, &p(&dir.path().join("x"))]),
        args(&[b"".as_slice(), &s]),
    ];
    for argv in cases {
        assert_same("mdb_copy", &argv, b"", dir.path());
    }
}

/// `mdb_drop` on copies of an environment, C and Rust: same messages,
/// same resulting environment.
fn drop_case(name: &str, argv_opts: &[&[u8]]) {
    let dir = tempdir();
    let mut views = Vec::new();
    let mut runs = Vec::new();
    let nosub = name == "nosubdir";
    let extra: &[&str] = if nosub { &["-n"] } else { &[] };
    for side in ["c", "r"] {
        let base = dir.path().join(side);
        fs::create_dir_all(&base).unwrap();
        let env = env_copy(name, &base);
        let mut argv = args(extra);
        argv.extend(argv_opts.iter().map(|a| a.to_vec()));
        argv.push(p(&env));
        let imp = if side == "c" { Impl::C } else { Impl::Rust };
        runs.push(run(imp, "mdb_drop", &argv, b"", dir.path()));
        views.push(c_view(&env, extra));
    }
    let what = format!(
        "{name} {:?}",
        argv_opts
            .iter()
            .map(|a| String::from_utf8_lossy(a))
            .collect::<Vec<_>>()
    );
    let paths = |r: &Run, side: &str| {
        String::from_utf8_lossy(&r.stderr).replace(
            &String::from_utf8_lossy(&p(&dir.path().join(side))).into_owned(),
            "<base>",
        )
    };
    assert_eq!(
        (runs[0].code, paths(&runs[0], "c")),
        (runs[1].code, paths(&runs[1], "r")),
        "{what}"
    );
    assert!(
        views[0] == views[1],
        "{what}\nC:\n{}\nRust:\n{}",
        String::from_utf8_lossy(&views[0]),
        String::from_utf8_lossy(&views[1])
    );
}

#[test]
fn drops_named() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let names = run(
        Impl::C,
        "mdb_dump",
        &args(&[b"-l".to_vec(), p(&env)]),
        b"",
        dir.path(),
    )
    .stdout;
    for db in names.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        drop_case("named", &[b"-s", db]);
        drop_case("named", &[b"-d", b"-s", db]);
    }
    drop_case("named", &[]);
    drop_case("named", &[b"-d"]);
    drop_case("named", &[b"-s", b"missing"]);
    drop_case("named", &[b"-d", b"-s", b"zz-plain-value"]);
}

#[test]
fn drops_main() {
    for name in [
        "main", "dupmain", "empty", "nosubdir", "mtest", "mtest3", "mtest5",
    ] {
        drop_case(name, &[]);
        drop_case(name, &[b"-d"]);
    }
    for db in [&b"id2"[..], b"id1"] {
        drop_case("mtest3", &[b"-s", db]);
        drop_case("mtest4", &[b"-d", b"-s", db]);
    }
}

#[test]
fn drop_usage() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let e_owned = p(&env);
    let e: &[u8] = &e_owned;
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![],
        args(&["-V"]),
        args(&[b"-x".as_slice(), e]),
        args(&[b"-s".as_slice()]),
        args(&[e, e]),
        args(&["/nonexistent/env"]),
        args(&[b"-n".as_slice(), e]),
        args(&[b"-L".as_slice(), e]),
    ];
    for argv in cases {
        assert_same("mdb_drop", &argv, b"", dir.path());
    }
}

/// A closed pipe on standard output: both report the write error
/// (C ignores `SIGPIPE`, Rust programs too).
#[test]
fn copy_to_closed_pipe() {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let dir = tempdir();
    // Larger than any pipe buffer.
    let src = dir.path().join("big");
    let mut db = DbSpec::new(None, &[]);
    for i in 0..5000u32 {
        db = db.rec(format!("key{i:06}"), vec![b'v'; 200]);
    }
    c_load(&src, &[db], 32 << 20, &[]);
    assert!(fs::metadata(src.join("data.mdb")).unwrap().len() > 1 << 20);
    let mut results = Vec::new();
    for imp in [Impl::C, Impl::Rust] {
        let mut child = Command::new(bin(imp, "mdb_copy"))
            .arg0("mdb_copy")
            .arg(&src)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut first = [0u8; 100];
        stdout.read_exact(&mut first).unwrap();
        drop(stdout);
        let out = child.wait_with_output().unwrap();
        results.push((
            first.to_vec(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.code(),
        ));
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[1].1,
        "mdb_copy: copying failed, error 32 (Broken pipe)\n"
    );
}
