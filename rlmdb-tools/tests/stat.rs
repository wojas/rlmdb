//! `mdb_stat`: the Rust tool against the C tool on every test environment
//! and option combination (identical stdout, stderr and exit code), plus
//! the documented exit-code fix for `-r` (KNOWN_C_BUGS.md #2).

mod common;

use std::fs;
use std::path::Path;

use common::*;

/// The names of the named databases in `env` (from the C `mdb_dump -l`).
fn db_names(env: &Path, extra: &[&str]) -> Vec<Vec<u8>> {
    let mut argv = args(extra);
    argv.extend(args(&["-l"]));
    argv.push(p(env));
    let r = run(Impl::C, "mdb_dump", &argv, b"", env.parent().unwrap());
    r.stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Every option combination on environment `name`.
fn stat_matrix(name: &str) {
    let dir = tempdir();
    let env = env_copy(name, dir.path());
    let extra: &[&str] = if name == "nosubdir" { &["-n"] } else { &[] };
    let mut cases: Vec<Vec<Vec<u8>>> = [
        "",
        "-e",
        "-f",
        "-ff",
        "-fff",
        "-ffff",
        "-a",
        "-ea",
        "-efa",
        "-e -ff -a",
        "-eee",
        "-s missing",
        "-e -s missing",
        "-f -s missing",
    ]
    .iter()
    .map(|c| words(c))
    .collect();
    for db in db_names(&env, extra) {
        cases.push(vec![b"-s".to_vec(), db.clone()]);
        let mut joined = b"-efs".to_vec();
        joined.extend_from_slice(&db);
        cases.push(vec![joined]);
    }
    for case in cases {
        let mut argv = args(extra);
        argv.extend(case);
        argv.push(p(&env));
        assert_same("mdb_stat", &argv, b"", dir.path());
    }
}

#[test]
fn matrix_main() {
    stat_matrix("main");
}

#[test]
fn matrix_named() {
    stat_matrix("named");
}

#[test]
fn matrix_dupmain() {
    stat_matrix("dupmain");
}

#[test]
fn matrix_empty() {
    stat_matrix("empty");
}

#[test]
fn matrix_nosubdir() {
    stat_matrix("nosubdir");
}

#[test]
fn matrix_mtest() {
    for name in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
        stat_matrix(name);
    }
}

/// The free list of an environment with freed pages: `-ff`/`-fff` print
/// every record and its page runs.
#[test]
fn freelist_details() {
    let dir = tempdir();
    // mtest5 deletes; dropping a big DB frees many pages in runs.
    let env = env_copy("named", dir.path());
    let r = run(
        Impl::C,
        "mdb_drop",
        &args(&[b"-s".as_slice(), b"dupfixed", &p(&env)]),
        b"",
        dir.path(),
    );
    assert_eq!(r.code, 0, "{}", r.stderr_str());
    for case in ["-f", "-ff", "-fff", "-efff"] {
        let mut argv = words(case);
        argv.push(p(&env));
        let out = assert_same("mdb_stat", &argv, b"", dir.path());
        if case == "-fff" {
            assert!(
                out.stdout_str().contains("Transaction "),
                "{}",
                out.stdout_str()
            );
        }
    }
}

#[test]
fn errors_and_usage() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let nosub = env_copy("nosubdir", dir.path());
    let junk = dir.path().join("junk");
    fs::create_dir_all(&junk).unwrap();
    fs::write(junk.join("data.mdb"), vec![0x42u8; 8192]).unwrap();
    let e_owned = p(&env);
    let e: &[u8] = &e_owned;
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![],
        args(&["-V"]),
        args(&[b"-V".as_slice(), b"-x"]),
        args(&[b"-x".as_slice(), b"-V"]),
        args(&[b"-a".as_slice(), b"-s", b"plain", e]),
        args(&[b"-s".as_slice(), b"plain", b"-a", e]),
        args(&[e, e]),
        args(&[b"-x".as_slice(), e]),
        args(&[b"-ex".as_slice(), e]),
        args(&[b"-s".as_slice()]),
        args(&[e, b"-s".as_slice()]),
        args(&[b"--".as_slice(), e]),
        args(&[b"-e".as_slice(), b"--", e]),
        args(&[b"--".as_slice(), b"-e", e]),
        args(&[e, b"-e".as_slice()]),
        args(&[e, b"-a".as_slice(), b"-e"]),
        args(&[b"-".as_slice(), e]),
        args(&[b"--foo".as_slice(), e]),
        args(&[b"-:".as_slice(), e]),
        args(&[b"-splain".as_slice(), e]),
        args(&["/nonexistent/env"]),
        args(&[p(&junk)]),
        args(&[b"-n".as_slice(), e]),
        args(&[p(&nosub)]),
        args(&[b"-n".as_slice(), &p(&nosub)]),
        args(&[b"-s".as_slice(), b"zz-plain-value", e]),
    ];
    for argv in cases {
        assert_same("mdb_stat", &argv, b"", dir.path());
    }
    // getopt's own messages use argv[0] (glibc) or its last component
    // (macOS): run both with a path as argv[0]. (Windows cannot give a
    // program an argv[0] other than its own name.)
    #[cfg(unix)]
    {
        let c = run_with_arg0(Impl::C, "/some/dir/mdb_stat", &args(&["-x", "db"]));
        let r = run_with_arg0(Impl::Rust, "/some/dir/mdb_stat", &args(&["-x", "db"]));
        assert_runs_equal("mdb_stat", &args(&["-x", "db"]), &c, &r);
    }
}

#[cfg(unix)]
fn run_with_arg0(imp: Impl, arg0: &str, argv: &[Vec<u8>]) -> Run {
    use std::os::unix::process::CommandExt;
    let out = std::process::Command::new(bin(imp, "mdb_stat"))
        .arg0(arg0)
        .args(argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()))
        .output()
        .unwrap();
    Run {
        stdout: out.stdout,
        stderr: out.stderr,
        code: out.status.code().unwrap_or(-1),
    }
}

/// glibc stops permuting with `POSIXLY_CORRECT`; macOS never permutes.
#[test]
fn posixly_correct() {
    let dir = tempdir();
    let env = env_copy("main", dir.path());
    let argv = args(&[p(&env), b"-e".to_vec()]);
    for vars in [&[][..], &[("POSIXLY_CORRECT", "1")][..]] {
        let c = run_env(Impl::C, "mdb_stat", &argv, b"", dir.path(), vars);
        let r = run_env(Impl::Rust, "mdb_stat", &argv, b"", dir.path(), vars);
        assert_runs_equal("mdb_stat", &argv, &c, &r);
    }
}

/// `-r` alone: same output, but exit code 0 where C exits 1
/// (KNOWN_C_BUGS.md #2: the result of the last `fputs`, which is 0 with
/// the Windows C runtime, so there C exits 0 too). With `-a`/`-s`/`-f` both
/// exit 0.
#[test]
fn reader_table_exit_code() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    for case in ["-r", "-rr", "-er", "-rrr"] {
        let mut argv = words(case);
        argv.push(p(&env));
        let (c, r) = both("mdb_stat", &argv, b"", dir.path());
        assert_eq!(c.stdout, r.stdout, "{case}");
        assert_eq!(c.stderr, r.stderr, "{case}");
        assert_eq!((c.code, r.code), (i32::from(!cfg!(windows)), 0), "{case}");
    }
    for case in ["-r -a", "-rr -f", "-r -s plain", "-err -ffa"] {
        let mut argv = words(case);
        argv.push(p(&env));
        let out = assert_same("mdb_stat", &argv, b"", dir.path());
        assert_eq!(out.code, 0, "{case}");
    }
}

/// Database names C's `mdb_dbi_open` refuses: an empty name fails with
/// `MDB_BAD_VALSIZE` (C's `mdb_cursor_set` rejects the zero-length key);
/// a name over the 511-byte key limit is `MDB_NOTFOUND` when opening (the
/// lookup finds nothing) but `MDB_BAD_VALSIZE` when creating (`mdb_load`
/// opens with `MDB_CREATE`). The C tools are the oracle: the expected
/// errors are checked in their output, and the Rust tools must print the
/// same.
#[test]
fn empty_db_name() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let long = vec![b'n'; 512];
    let cases: [(&[u8], &str); 2] = [(b"", "MDB_BAD_VALSIZE"), (&long, "MDB_NOTFOUND")];
    for (name, error) in cases {
        for tool in ["mdb_stat", "mdb_dump"] {
            let argv = args(&[b"-s".as_slice(), name, &p(&env)]);
            let (c, r) = both(tool, &argv, b"", dir.path());
            assert_runs_equal(tool, &argv, &c, &r);
            assert_eq!(c.code, 1, "{tool} -s <{} bytes>", name.len());
            assert!(
                c.stderr_str().contains(error),
                "{tool} -s <{} bytes>: expected {error}, C said {}",
                name.len(),
                c.stderr_str()
            );
        }
    }
    // Creating: both names are MDB_BAD_VALSIZE.
    let input = b"VERSION=3\nformat=bytevalue\ntype=btree\nHEADER=END\n 6b\n 76\nDATA=END\n";
    for name in [&b""[..], &long] {
        let mut runs = Vec::new();
        for imp in [Impl::C, Impl::Rust] {
            let target = dir.path().join(format!("load-{}-{imp:?}", name.len()));
            std::fs::create_dir_all(&target).unwrap();
            let argv = args(&[b"-s".as_slice(), name, &p(&target)]);
            runs.push(run(imp, "mdb_load", &argv, input, dir.path()));
        }
        let argv = args(&[b"-s".as_slice(), name, b"<env>"]);
        assert_runs_equal("mdb_load", &argv, &runs[0], &runs[1]);
        assert_eq!(runs[0].code, 1, "mdb_load -s <{} bytes>", name.len());
        assert!(
            runs[0].stderr_str().contains("MDB_BAD_VALSIZE"),
            "mdb_load -s <{} bytes>: C said {}",
            name.len(),
            runs[0].stderr_str()
        );
    }
}
