//! The data-file format through the tools, against the C 0.9 tools where
//! there is an LMDB 0.9 file: `mdb_load -F 0.9|1.0`, the experimental
//! `RLMDB_DEFAULT_FORMAT` variable (set only for the tool's process), and
//! that neither changes what the tools do with an existing 0.9 file.
//!
//! That the 1.0 files are the ones C 1.0's tools write and read is checked
//! in `interop/tests/tools10.rs`, where the C 1.0 tools are built.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use rlmdb::{Environment, EnvironmentFlags, Format, Transaction};

const MAPSIZE: usize = 32 << 20;

/// The format of the data file of `env`.
fn format_of(env: &Path) -> Option<Format> {
    rlmdb::sniff_format(env, EnvironmentFlags::empty()).unwrap()
}

/// Key/value pairs.
type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// The records of every database of `env` read through rlmdb: name, then
/// key/value pairs (the main DB without the named databases' records).
fn records(env: &Path) -> Vec<(Option<Vec<u8>>, Records)> {
    let e = Environment::builder()
        .max_dbs(64)
        .flags(EnvironmentFlags::READ_ONLY)
        .open(env)
        .unwrap();
    let txn = e.begin_ro_txn().unwrap();
    let mut names = vec![None];
    let mut out = Vec::new();
    let mut main = Vec::new();
    let mut c = txn.cursor(txn.open_db(None).unwrap()).unwrap();
    while let Some((k, v)) = c.next().unwrap() {
        if c.current_is_db() || (e.format() == Format::V09 && txn.open_db_bytes(Some(k)).is_ok()) {
            names.push(Some(k.strip_suffix(b"\0").unwrap_or(k).to_vec()));
        } else {
            main.push((k.to_vec(), v.to_vec()));
        }
    }
    for name in names {
        let recs = match &name {
            None => main.clone(),
            Some(n) => {
                let db = txn.open_db_bytes(Some(n)).unwrap();
                let mut c = txn.cursor(db).unwrap();
                let mut recs = Vec::new();
                while let Some((k, v)) = c.next().unwrap() {
                    recs.push((k.to_vec(), v.to_vec()));
                }
                recs
            }
        };
        out.push((name, recs));
    }
    out
}

/// `mdb_load`'s messages without the "unrecognized keyword ignored:
/// db_pagesize" warnings that only C 0.9 prints.
fn without_pagesize_warnings(stderr: &[u8]) -> Vec<u8> {
    stderr
        .split_inclusive(|&b| b == b'\n')
        .filter(|l| {
            let l = l.strip_suffix(b"\n").unwrap_or(l);
            // "\r\n" on Windows (text mode).
            let l = l.strip_suffix(b"\r").unwrap_or(l);
            !l.ends_with(b"unrecognized keyword ignored: db_pagesize")
        })
        .flatten()
        .copied()
        .collect()
}

/// A dump of `spec` (the shared test data sets) for both loaders.
fn named_dump() -> Vec<u8> {
    dump_text_for_c(&spec_named(), false, MAPSIZE)
}

/// Runs the Rust `mdb_load` with `extra` arguments into a new directory
/// `dir/name`, with `RLMDB_DEFAULT_FORMAT` set to `var` if given.
fn rust_load(dir: &Path, name: &str, extra: &[&str], var: Option<&str>, input: &[u8]) -> Run {
    let env = dir.join(name);
    fs::create_dir_all(&env).unwrap();
    let mut argv = args(extra);
    argv.push(p(&env));
    let vars: Vec<(&str, &str)> = var
        .map(|v| ("RLMDB_DEFAULT_FORMAT", v))
        .into_iter()
        .collect();
    run_env(Impl::Rust, "mdb_load", &argv, input, dir, &vars)
}

#[test]
fn load_format_option_and_default_variable() {
    let dir = tempdir();
    let input = named_dump();
    // The C 0.9 load: the reference for every 0.9 result.
    let c_env = dir.path().join("c");
    fs::create_dir_all(&c_env).unwrap();
    let c = run(Impl::C, "mdb_load", &args(&[p(&c_env)]), &input, dir.path());
    assert_eq!(c.code, 0, "{}", c.stderr_str());
    let reference = c_dump_all(&c_env, &[]);
    for (name, extra, var, expected) in [
        ("plain", &[][..], None, Format::V09),
        ("f09", &["-F", "0.9"][..], None, Format::V09),
        ("f10", &["-F", "1.0"][..], None, Format::V10),
        ("var10", &[][..], Some("1.0"), Format::V10),
        ("var09", &[][..], Some("0.9"), Format::V09),
        ("var-junk", &[][..], Some("2.0"), Format::V09),
        ("f09-var10", &["-F", "0.9"][..], Some("1.0"), Format::V09),
        ("f10-var09", &["-F", "1.0"][..], Some("0.9"), Format::V10),
    ] {
        let r = rust_load(dir.path(), name, extra, var, &input);
        let env = dir.path().join(name);
        assert_eq!(r.code, 0, "{name}: {}", r.stderr_str());
        assert_eq!(format_of(&env), Some(expected), "{name}");
        if expected == Format::V09 {
            // As C 0.9's mdb_load: the same messages (db_pagesize= is an
            // unknown keyword there) and the same environment.
            assert_eq!(r.stderr, c.stderr, "{name}");
            assert_eq!(c_dump_all(&env, &[]), reference, "{name}");
        } else {
            // C 1.0's mdb_load knows db_pagesize=, and warns about the rest
            // as 0.9's does (duplicates=).
            assert_eq!(r.stderr, without_pagesize_warnings(&c.stderr), "{name}");
            assert_eq!(records(&env), records(&c_env), "{name}");
        }
    }
}

#[test]
fn load_into_an_existing_environment_keeps_its_format() {
    let dir = tempdir();
    let input = named_dump();
    let env09 = env_copy("named", dir.path());
    // A 1.0 default does not apply to an existing file.
    let mut argv = args(&["-N"]);
    argv.push(p(&env09));
    let r = run_env(
        Impl::Rust,
        "mdb_load",
        &argv,
        &input,
        dir.path(),
        &[("RLMDB_DEFAULT_FORMAT", "1.0")],
    );
    assert_eq!(r.code, 0, "{}", r.stderr_str());
    assert_eq!(format_of(&env09), Some(Format::V09));
    // -F naming the other format: the open fails as C reports it.
    let env = dir.path().join("named");
    let mut argv = args(&["-F", "1.0"]);
    argv.push(p(&env));
    let r = run(Impl::Rust, "mdb_load", &argv, &input, dir.path());
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stderr_text(),
        "mdb_env_open failed, error -30794 MDB_VERSION_MISMATCH: Database environment \
         version mismatch\n"
    );
    let env10 = dir.path().join("ten");
    let r = rust_load(dir.path(), "ten", &["-F", "1.0"], None, &input);
    assert_eq!(r.code, 0, "{}", r.stderr_str());
    let warnings = r.stderr;
    let mut argv = args(&["-F", "0.9", "-N"]);
    argv.push(p(&env10));
    let r = run(Impl::Rust, "mdb_load", &argv, &input, dir.path());
    assert_eq!(r.code, 1);
    assert!(
        r.stderr_str().contains("MDB_VERSION_MISMATCH"),
        "{}",
        r.stderr_str()
    );
    // Without -F the file decides, whatever the variable says.
    let mut argv = args(&["-N"]);
    argv.push(p(&env10));
    let r = run_env(
        Impl::Rust,
        "mdb_load",
        &argv,
        &input,
        dir.path(),
        &[("RLMDB_DEFAULT_FORMAT", "0.9")],
    );
    assert_eq!((r.code, &r.stderr), (0, &warnings));
    assert_eq!(format_of(&env10), Some(Format::V10));
}

#[test]
fn load_page_size_from_the_header() {
    let dir = tempdir();
    for (ps, expected) in [
        ("1024", 1024),
        ("65536", 65536),
        ("256", 256),
        // Invalid sizes are ignored, as C 1.0's mdb_env_set_pagesize
        // result is: the OS page size.
        ("100", page_size()),
        ("3000", page_size()),
        ("131072", page_size()),
        ("0", page_size()),
    ] {
        let input = format!(
            "VERSION=3\nformat=bytevalue\ntype=btree\nmapsize={MAPSIZE}\ndb_pagesize={ps}\n\
             HEADER=END\n 6b\n 76\nDATA=END\n"
        );
        let name = format!("ps{ps}");
        let r = rust_load(dir.path(), &name, &["-F", "1.0"], None, input.as_bytes());
        assert_eq!((r.code, r.stderr_str().as_str()), (0, ""), "{ps}");
        let env = Environment::builder()
            .flags(EnvironmentFlags::READ_ONLY)
            .open(dir.path().join(&name))
            .unwrap();
        assert_eq!(env.page_size(), expected, "{ps}");
    }
    // Not a number: C 1.0's message.
    let input = "VERSION=3\ndb_pagesize=x\nHEADER=END\nDATA=END\n";
    let r = rust_load(dir.path(), "bad", &["-F", "1.0"], None, input.as_bytes());
    assert_eq!(r.code, 1);
    assert_eq!(r.stderr_text(), "mdb_load: line 2: invalid pagesize x\n");
}

#[test]
fn load_empty_keys_in_1_0() {
    let dir = tempdir();
    let input = b"VERSION=3\nformat=bytevalue\ntype=btree\nHEADER=END\n 6b\n 76\n \n 77\n 6c\n 78\nDATA=END\n";
    // C 1.0 skips a record with an empty key and says so.
    let r = rust_load(dir.path(), "ten", &["-F", "1.0"], None, input);
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stderr_text(),
        "mdb_load: line 8: zero-length key(ignored)\n"
    );
    let recs = records(&dir.path().join("ten"));
    assert_eq!(
        recs[0].1,
        vec![
            (b"k".to_vec(), b"v".to_vec()),
            (b"l".to_vec(), b"x".to_vec())
        ]
    );
    // C 0.9 fails on it, and so does the 0.9 load.
    let c_env = dir.path().join("c");
    fs::create_dir_all(&c_env).unwrap();
    let c = run(Impl::C, "mdb_load", &args(&[p(&c_env)]), input, dir.path());
    let r = rust_load(dir.path(), "nine", &[], None, input);
    assert_eq!((r.code, &r.stderr), (c.code, &c.stderr));
    assert_ne!(c.code, 0);
}

#[test]
fn load_format_option_errors() {
    let dir = tempdir();
    let env = dir.path().join("env");
    fs::create_dir_all(&env).unwrap();
    for bad in ["2.0", "1", "10", "", "1.0 "] {
        let mut argv = args(&["-F", bad]);
        argv.push(p(&env));
        let r = run(Impl::Rust, "mdb_load", &argv, b"", dir.path());
        assert_eq!(r.code, 1, "{bad:?}");
        assert_eq!(
            r.stderr_text(),
            format!("mdb_load: {bad}: unknown format, use 0.9 or 1.0\n"),
            "{bad:?}"
        );
    }
    // A missing argument: getopt's message, then the usage line with -F.
    let r = run(Impl::Rust, "mdb_load", &args(&["-F"]), b"", dir.path());
    assert_eq!(r.code, 1);
    let usage = [
        b"usage: mdb_load ".as_slice(),
        &platform_lines(LOAD_USAGE_RUST),
    ]
    .concat();
    assert!(r.stderr.ends_with(&usage), "{}", r.stderr_str());
    // getopt's wording: "-- F" (macOS), "-- 'F'" (glibc).
    assert!(
        r.stderr_str()
            .starts_with("mdb_load: option requires an argument -- "),
        "{}",
        r.stderr_str()
    );
}

/// With `RLMDB_DEFAULT_FORMAT=1.0` the tools treat an existing 0.9 file
/// exactly as without it: the C 0.9 tools are still the oracle.
#[test]
fn default_variable_does_not_touch_existing_files() {
    let var = [("RLMDB_DEFAULT_FORMAT", "1.0")];
    for name in ["named", "mtest3"] {
        let env = &envs().envs[name];
        for (tool, opts) in [
            ("mdb_stat", "-a"),
            ("mdb_stat", "-ef"),
            ("mdb_dump", "-a"),
            ("mdb_dump", "-l"),
        ] {
            let mut argv = words(opts);
            argv.push(p(env));
            let c = run(Impl::C, tool, &argv, b"", envs().root.as_path());
            let r = run_env(Impl::Rust, tool, &argv, b"", envs().root.as_path(), &var);
            assert_runs_equal(tool, &argv, &c, &r);
        }
    }
    // Copies keep the source's format: byte-identical to C's.
    let dir = tempdir();
    let src = env_copy("named", dir.path());
    for compact in [false, true] {
        let mut files = Vec::new();
        for imp in [Impl::C, Impl::Rust] {
            let dst = dir.path().join(format!("{imp:?}-{compact}"));
            fs::create_dir_all(&dst).unwrap();
            let mut argv = Vec::new();
            if compact {
                argv.push(b"-c".to_vec());
            }
            argv.extend([p(&src), p(&dst)]);
            let r = run_env(imp, "mdb_copy", &argv, b"", dir.path(), &var);
            assert_eq!((r.code, r.stderr_str().as_str()), (0, ""));
            files.push(fs::read(dst.join("data.mdb")).unwrap());
        }
        assert!(files[0] == files[1], "mdb_copy (compact {compact})");
        assert_eq!(
            format_of(&dir.path().join(format!("Rust-{compact}"))),
            Some(Format::V09)
        );
    }
    // mdb_drop on a copy: the same file as C's tool leaves.
    let (c_env, r_env) = (dir.path().join("dc"), dir.path().join("dr"));
    copy_env(&src, &c_env);
    copy_env(&src, &r_env);
    let argv = |env: &Path| [words("-d -s dupsort"), vec![p(env)]].concat();
    let c = run(Impl::C, "mdb_drop", &argv(&c_env), b"", dir.path());
    let r = run_env(Impl::Rust, "mdb_drop", &argv(&r_env), b"", dir.path(), &var);
    assert_eq!((c.code, &c.stderr), (r.code, &r.stderr));
    assert_eq!(c_dump_all(&c_env, &[]), c_dump_all(&r_env, &[]));
    assert_eq!(format_of(&r_env), Some(Format::V09));
}
