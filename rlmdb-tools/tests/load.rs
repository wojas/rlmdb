//! `mdb_load`: the Rust tool against the C tool on crafted inputs (same
//! messages and exit codes, and the C `mdb_dump`/`mdb_stat` of the loaded
//! environments identical), the documented fixes, and dump/load round trips
//! across the two implementations.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;

/// Where a load starts from: a new environment, or a copy of a test env.
#[derive(Clone, Copy)]
enum Start<'a> {
    New,
    Copy(&'a str),
}

struct Loaded {
    c: Run,
    r: Run,
    c_env: PathBuf,
    r_env: PathBuf,
}

/// Loads `input` with `extra` options into two environments, one with the
/// C and one with the Rust `mdb_load`.
fn load_both(dir: &Path, start: Start<'_>, extra: &[&str], input: &[u8]) -> Loaded {
    let mut envs = Vec::new();
    for side in ["c", "r"] {
        let base = dir.join(side);
        fs::create_dir_all(&base).unwrap();
        let env = match start {
            Start::New => {
                let e = base.join("env");
                fs::create_dir_all(&e).unwrap();
                e
            }
            Start::Copy(name) => env_copy(name, &base),
        };
        envs.push(env);
    }
    let mut runs = Vec::new();
    for (imp, env) in [(Impl::C, &envs[0]), (Impl::Rust, &envs[1])] {
        let mut argv = args(extra);
        argv.push(p(env));
        runs.push(run(imp, "mdb_load", &argv, input, dir));
    }
    let r = runs.pop().unwrap();
    let c = runs.pop().unwrap();
    let r_env = envs.pop().unwrap();
    let c_env = envs.pop().unwrap();
    Loaded { c, r, c_env, r_env }
}

/// The C view of an environment: its dumps and statistics. Only the
/// paths differ between the two sides, so they are replaced.
fn c_view(env: &Path) -> Vec<u8> {
    let mut out = c_dump_all(env, &[]);
    for opts in ["-efa", "-ffa"] {
        let mut argv = words(opts);
        argv.push(p(env));
        let r = run(Impl::C, "mdb_stat", &argv, b"", env.parent().unwrap());
        out.extend_from_slice(&r.stdout);
        out.extend_from_slice(&r.stderr);
    }
    let path = p(env);
    replace_all(&out, &path, b"<env>")
}

fn replace_all(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Loads with both and asserts identical messages, exit codes and results.
fn assert_load_same(start: Start<'_>, extra: &[&str], input: &[u8]) -> Loaded {
    let dir = tempdir();
    let l = load_both(dir.path(), start, extra, input);
    let what = format!(
        "mdb_load {extra:?} of\n{}",
        String::from_utf8_lossy(&input[..input.len().min(400)])
    );
    assert_runs_equal(&what, &[], &l.c, &l.r);
    let (cv, rv) = (c_view(&l.c_env), c_view(&l.r_env));
    assert!(
        cv == rv,
        "{what}\nC result:\n{}\nRust result:\n{}",
        String::from_utf8_lossy(&cv),
        String::from_utf8_lossy(&rv)
    );
    // Keep the directory alive until the comparison is done.
    drop(dir);
    l
}

const MAPSIZE: usize = 32 << 20;

#[test]
fn crafted_dumps() {
    for spec in [
        spec_main(),
        spec_named(),
        spec_dupmain(),
        vec![DbSpec::new(None, &[])],
    ] {
        for print in [false, true] {
            // The leading empty block keeps C from dropping the first
            // database's flags (KNOWN_C_BUGS.md #1).
            let input = dump_text_for_c(&spec, print, MAPSIZE);
            assert_load_same(Start::New, &[], &input);
        }
    }
}

#[test]
fn crafted_dumps_into_existing() {
    // Loading over existing data: values are replaced (or kept with -N),
    // duplicates added.
    let mut spec = spec_named();
    for db in &mut spec {
        for (i, rec) in db.records.iter_mut().enumerate() {
            if i % 3 == 0 && !db.flags.contains(&"dupfixed") && !db.flags.contains(&"integerdup") {
                rec.1.extend_from_slice(b"-changed");
            }
        }
    }
    let input = dump_text_for_c(&spec, false, MAPSIZE);
    assert_load_same(Start::Copy("named"), &[], &input);
    assert_load_same(Start::Copy("named"), &["-N"], &input);
    let input = dump_text_for_c(&spec_dupmain(), true, MAPSIZE);
    assert_load_same(Start::Copy("dupmain"), &["-N"], &input);
    assert_load_same(Start::Copy("dupmain"), &[], &input);
}

/// The fix for KNOWN_C_BUGS.md #1: the first database keeps its flags.
#[test]
fn first_database_keeps_flags() {
    for spec in [spec_dupmain(), spec_named()[2..4].to_vec()] {
        let dir = tempdir();
        let plain = dump_text(&spec, false, MAPSIZE);
        let l = load_both(dir.path(), Start::New, &[], &plain);
        assert_eq!((l.c.code, l.r.code), (0, 0));
        assert_eq!(l.c.stderr, l.r.stderr);
        // C lost the flags: its result differs from what was dumped.
        let with_dummy = load_both(
            &dir.path().join("x"),
            Start::New,
            &[],
            &dump_text_for_c(&spec, false, MAPSIZE),
        );
        let expected = c_view(&with_dummy.c_env);
        assert_ne!(c_view(&l.c_env), expected, "C 0.9.36 should drop the flags");
        // Rust keeps them: the same result as C with the workaround.
        assert_eq!(c_view(&l.r_env), expected);
    }
}

/// `-T`: plain text pairs of lines (print encoding, no header).
#[test]
fn plain_text() {
    let mut input = Vec::new();
    for i in 0..250 {
        input.extend_from_slice(
            format!("key {i:03}\nvalue\\5c {i} \\0a\\00\\ff\\\\ end\n").as_bytes(),
        );
    }
    input.extend_from_slice(b"VERSION=3\nnot a header\n");
    for start in [Start::New, Start::Copy("named")] {
        assert_load_same(start, &["-T"], &input);
        assert_load_same(start, &["-T", "-s", "plain"], &input);
        assert_load_same(start, &["-T", "-s", "newdb"], &input);
        assert_load_same(start, &["-T", "-N", "-s", "plain"], &input);
    }
    assert_load_same(Start::Copy("named"), &["-T", "-s", "dupsort"], &input);
    // An empty line is an empty key or value; an empty key is rejected.
    assert_load_same(Start::New, &["-T"], b"k\n\n");
    assert_load_same(Start::New, &["-T"], b"\nv\n");
    assert_load_same(Start::New, &["-T"], b"k\nv");
    assert_load_same(Start::New, &["-T"], b"k\n");
    assert_load_same(Start::New, &["-T"], b"k\\4\nv\n");
    assert_load_same(Start::New, &["-T"], b"");
}

/// `-N` commits after every 100 records it stores; records it skips (the
/// key exists) do not count, as in C. When the input then breaks off, the
/// same records are committed and the same rolled back.
#[test]
fn no_overwrite_batches_count_stored_records() {
    // Two stored records and 98 skipped ones: no commit before the error.
    let mut few = b"exists\nold\nnew\nvalue\n".to_vec();
    for _ in 0..98 {
        few.extend_from_slice(b"exists\nskip\n");
    }
    // 151 stored records, each new one followed by a skipped one: one
    // commit, after the 100th stored.
    let mut many = b"exists\nold\n".to_vec();
    for i in 0..150 {
        many.extend_from_slice(format!("new{i:03}\nvalue\nexists\nskip\n").as_bytes());
    }
    for mut input in [few, many] {
        input.extend_from_slice(b"no value\n");
        let l = assert_load_same(Start::New, &["-T", "-N"], &input);
        assert_eq!(l.c.code, 1, "{}", l.c.stderr_text());
    }
}

/// `-a`: records stored in input order without comparisons, including
/// input that is not sorted and more than one batch of 100.
#[test]
fn append_mode() {
    // Sorted input, as mdb_dump writes it. C 0.9.36 leaves the "always
    // greater" comparator on the main DB after loading it with -a, so a
    // named database after a main-DB block breaks (it crashes): only
    // main-DB blocks, or only named ones.
    for spec in [spec_main(), spec_dupmain()] {
        assert_load_same(Start::New, &["-a"], &dump_text_for_c(&spec, false, MAPSIZE));
    }
    let named = dump_text(&spec_named()[1..], false, MAPSIZE);
    let mut input = empty_block_like(&named, Some(b"0-first"));
    input.extend_from_slice(&named);
    assert_load_same(Start::New, &["-a"], &input);
    // Unsorted keys and duplicates, spanning several batches.
    let mut plain = DbSpec::new(Some(b"unsorted"), &[]);
    let mut dups = DbSpec::new(Some(b"unsorted-dups"), &["dupsort"]);
    for i in 0..350u32 {
        let k = (i * 7919) % 1000;
        plain = plain.rec(format!("k{k:04}"), format!("v{i}"));
        for d in [3u32, 1, 2] {
            dups = dups.rec(format!("d{:03}", i % 120), format!("{}", d * 1000 + i));
        }
    }
    let unsorted = dump_text(&[plain, dups], false, MAPSIZE);
    let mut input = empty_block_like(&unsorted, Some(b"0-first"));
    input.extend_from_slice(&unsorted);
    assert_load_same(Start::New, &["-a"], &input);
    assert_load_same(Start::New, &["-a", "-N"], &input);
    // The main DB with -a, then more records into it without -a.
    let main = DbSpec::new(None, &[])
        .rec("b", "1")
        .rec("a", "2")
        .rec("c", "3");
    assert_load_same(
        Start::New,
        &["-a"],
        &dump_text_for_c(&[main], true, MAPSIZE),
    );
}

/// Header and data errors: same messages (with line numbers), exit codes
/// and partial results as C.
#[test]
fn malformed_input() {
    let hdr = "VERSION=3\nformat=bytevalue\ntype=btree\nmapsize=1048576\nmaxreaders=126\ndb_pagesize=4096\nHEADER=END\n";
    let phdr = hdr.replace("bytevalue", "print");
    let cases: Vec<String> = vec![
        String::new(),
        "VERSION=4\nHEADER=END\n".into(),
        "VERSION=99999999999\nHEADER=END\n".into(),
        "VERSION=x\nHEADER=END\nDATA=END\n".into(),
        "format=foo\n".into(),
        "format=printable\nHEADER=END\n 41\n 42\nDATA=END\n".into(),
        "type=hash\n".into(),
        "mapsize=abc\n".into(),
        "mapsize= 2097152xyz\nHEADER=END\n".into(),
        "maxreaders=-1x\n".into(),
        "maxreaders=7\nHEADER=END\n 6b\n 76\nDATA=END\n".into(),
        "mapaddr=zz\n".into(),
        "mapaddr=0x0\nHEADER=END\n".into(),
        "foo=bar\nduplicates=1\nHEADER=END\n".into(),
        "HEADER=END\nDATA=END\ndupsort=0\nHEADER=END\n 6b\n 31\n 6b\n 32\nDATA=END\n".into(),
        format!("{hdr} 6b\n 76\n"),
        format!("{hdr} 6b\n 76"),
        format!("{hdr} 6b\nDATA=END\n"),
        format!("{hdr} 6b\n"),
        format!("{hdr} 6b"),
        format!("{hdr} 6\n 76\nDATA=END\n"),
        format!("{hdr} 6g\n 76\nDATA=END\n"),
        format!("{hdr} 6B\n 7A\nDATA=END\n"),
        format!("{hdr} 6b\r\n 76\nDATA=END\n"),
        format!("{hdr} 6b\n 76\ngarbage\n"),
        format!("{hdr} 6b\n 76\n\n 6c\n 77\nDATA=END\n"),
        format!("{hdr} 6b\n 76\nDATA=ENDXYZ\nVERSION=3\nHEADER=END\n 6c\n 77\nDATA=END\n"),
        format!("{hdr} 6b\n 76\nD\n"),
        format!("{hdr} \n 76\nDATA=END\n"),
        format!("{hdr} {}\n 76\nDATA=END\n", "61".repeat(511)),
        format!("{hdr} {}\n 76\nDATA=END\n", "61".repeat(512)),
        format!("{hdr} 6b\n {}\nDATA=END\n", "76".repeat(100_000)),
        format!("{phdr} a\\\\b\n \\41\\4a\\4A\\0a\nDATA=END\n"),
        format!("{phdr} a\\\n v\nDATA=END\n"),
        format!("{phdr} a\\4\n v\nDATA=END\n"),
        format!("{phdr} a\\xy\n v\nDATA=END\n"),
        format!("{phdr} k\r\n v\r\nDATA=END\n"),
        format!("{phdr} {}\n v\nDATA=END\n", "\\00".repeat(400)),
        format!("{hdr} 6b\n 76\nDATA=END\nVERSION=5\n"),
        format!("{hdr} 6b\n 76\nDATA=END\nformat=bad\n"),
        format!(
            "{hdr} 6b\n 76\nDATA=END\nVERSION=3\ndatabase=sub\nHEADER=END\n 6b\n 76\nDATA=END\n"
        ),
        format!("{hdr}DATA=END\n{hdr}DATA=END\n"),
        format!("{hdr} 6b\n 76\nDATA=END\ntrailing=1\n"),
    ];
    for case in &cases {
        if cfg!(windows) && case.starts_with("maxreaders=-1") {
            // C passes the lock-file size for this many readers through 32
            // bits on Windows, and what follows depends on the compiler
            // (KNOWN_C_BUGS.md #43): the MinGW-w64 build fails to map the
            // lock file (error 5), the cl build opens it at the wrapped
            // size. Only C runs: rlmdb makes the lock file its real size,
            // 256 GiB, which Windows backs in full when it is mapped (a
            // volume without sparse files, exFAT, writes it out at
            // `set_len` already), so the outcome is the scratch volume's
            // (a disk-full error, or an environment that opens after a
            // very long time).
            let dir = tempdir();
            let env = dir.path().join("env");
            fs::create_dir_all(&env).unwrap();
            let c = run(
                Impl::C,
                "mdb_load",
                &args(&[p(&env)]),
                case.as_bytes(),
                dir.path(),
            );
            let err = c.stderr_text();
            if cfg!(target_env = "msvc") {
                assert_eq!(c.code, 0, "{err}");
            } else {
                assert_eq!(c.code, 1, "{err}");
                assert!(err.starts_with("mdb_env_open failed, error 5 "), "{err}");
            }
            continue;
        }
        assert_load_same(Start::New, &[], case.as_bytes());
    }
    // A NUL right after the leading space ("strlen() == 0").
    let mut nul = hdr.as_bytes().to_vec();
    nul.extend_from_slice(b" \x006b\n 76\nDATA=END\n");
    assert_load_same(Start::New, &[], &nul);
    // A named DB into an environment whose main DB is DUPSORT.
    assert_load_same(
        Start::Copy("dupmain"),
        &[],
        format!("VERSION=3\ndatabase=x\n{hdr} 6b\n 76\nDATA=END\n").as_bytes(),
    );
    // The map fills up.
    let mut big = "VERSION=3\nmapsize=65536\nHEADER=END\n".to_owned();
    for i in 0..2000 {
        big.push_str(&format!(
            " {}\n {}\n",
            hex(&format!("key{i:05}")),
            "76".repeat(200)
        ));
    }
    big.push_str("DATA=END\n");
    assert_load_same(Start::New, &[], big.as_bytes());
    // DUPSORT data over 511 bytes.
    assert_load_same(
        Start::New,
        &[],
        format!("VERSION=3\nHEADER=END\nDATA=END\nVERSION=3\ndupsort=1\nHEADER=END\n 6b\n {}\nDATA=END\n", "76".repeat(512)).as_bytes(),
    );
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn options_and_usage() {
    let dir = tempdir();
    let input = dump_text(&[DbSpec::new(None, &[]).rec("k", "v")], false, MAPSIZE);
    let file = dir.path().join("in.txt");
    fs::write(&file, &input).unwrap();
    let env = dir.path().join("env");
    fs::create_dir_all(&env).unwrap();
    let e = p(&env);
    let f = p(&file);
    let cases: Vec<Vec<Vec<u8>>> = vec![
        vec![],
        args(&["-V"]),
        args(&[e.as_slice(), &e]),
        args(&[b"-x".as_slice(), &e]),
        args(&[b"-s".as_slice()]),
        args(&[b"-f".as_slice(), b"/nonexistent/input", &e]),
        args(&[b"-f".as_slice(), b"/nonexistent/input", b"-V"]),
        args(&[b"-V".as_slice(), b"-f", b"/nonexistent/input"]),
        args(&[b"-f".as_slice(), &f]),
        args(&["/nonexistent/env"]),
    ];
    for argv in cases {
        assert_same("mdb_load", &argv, &input, dir.path());
    }
    for extra in [&["-f"][..], &["-Q", "-f"], &["-N", "-f"]] {
        let mut argv = args(extra);
        argv.push(f.clone());
        let d = tempdir();
        let lc = d.path().join("c");
        let lr = d.path().join("r");
        fs::create_dir_all(&lc).unwrap();
        fs::create_dir_all(&lr).unwrap();
        let mut ac = argv.clone();
        ac.push(p(&lc));
        let mut ar = argv.clone();
        ar.push(p(&lr));
        let c = run(Impl::C, "mdb_load", &ac, b"", d.path());
        let r = run(Impl::Rust, "mdb_load", &ar, b"", d.path());
        assert_runs_equal("mdb_load", &ar, &c, &r);
        assert_eq!(c_view(&lc), c_view(&lr));
    }
    // -n: a data file without directory.
    let d = tempdir();
    let input = dump_text_for_c(&spec_dupmain(), false, MAPSIZE);
    let c = run(
        Impl::C,
        "mdb_load",
        &args(&[b"-n".to_vec(), p(&d.path().join("c.mdb"))]),
        &input,
        d.path(),
    );
    let r = run(
        Impl::Rust,
        "mdb_load",
        &args(&[b"-n".to_vec(), p(&d.path().join("r.mdb"))]),
        &input,
        d.path(),
    );
    assert_runs_equal("mdb_load -n", &[], &c, &r);
    assert_eq!(
        c_dump_all(&d.path().join("c.mdb"), &["-n"]),
        c_dump_all(&d.path().join("r.mdb"), &["-n"])
    );
}

/// What the Rust tool does where it deliberately differs from C (see
/// `README.md`).
#[test]
fn documented_differences() {
    let dir = tempdir();
    let env = dir.path().join("env");
    fs::create_dir_all(&env).unwrap();
    let load = |input: &str| {
        run(
            Impl::Rust,
            "mdb_load",
            &args(&[p(&env)]),
            input.as_bytes(),
            dir.path(),
        )
    };
    // A header line without '=' is fatal.
    let r = load("VERSION=3\nfoo\nHEADER=END\n");
    assert_eq!(r.code, 1);
    assert_eq!(r.stderr_text(), "mdb_load: line 2: unexpected format\n");
    // format= applies per database: the second (bytevalue) database is
    // not read in print mode.
    let r = load("VERSION=3\nformat=print\nHEADER=END\n a\n b\nDATA=END\nVERSION=3\nformat=bytevalue\ndatabase=x\nHEADER=END\n 41\n 42\nDATA=END\n");
    assert_eq!((r.code, r.stderr_str().as_str()), (0, ""));
    let dumped = run(
        Impl::C,
        "mdb_dump",
        &args(&[b"-p".to_vec(), b"-s".to_vec(), b"x".to_vec(), p(&env)]),
        b"",
        dir.path(),
    );
    assert!(
        dumped.stdout_text().contains("\n A\n B\n"),
        "{}",
        dumped.stdout_str()
    );
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

/// The environments to round-trip and whether they have named databases
/// (then only `-a` dumps can be reloaded: the main DB's records include the
/// database records).
const ROUND_TRIP: &[(&str, bool)] = &[
    ("main", false),
    ("named", true),
    ("dupmain", false),
    ("empty", false),
    ("mtest", false),
    ("mtest2", true),
    ("mtest3", true),
    ("mtest4", true),
    ("mtest5", true),
];

fn dump_with(imp: Impl, env: &Path, opts: &[&str]) -> Vec<u8> {
    let mut argv = args(opts);
    argv.push(p(env));
    let r = run(imp, "mdb_dump", &argv, b"", env.parent().unwrap());
    assert_eq!(r.code, 0, "{}", r.stderr_str());
    r.stdout
}

/// Rust `mdb_dump` -> C `mdb_load` -> C `mdb_dump` gives the source's C
/// dump, and C `mdb_dump` -> Rust `mdb_load` -> C `mdb_dump` too.
#[test]
fn round_trips() {
    let dir = tempdir();
    for &(name, named) in ROUND_TRIP {
        let src = env_copy(name, dir.path());
        let mode: &[&str] = if named { &["-a"] } else { &[] };
        let expected = {
            let mut a = args(mode);
            a.push(p(&src));
            strip_mapaddr(&c_ok("mdb_dump", &a, dir.path()))
        };
        for print in [false, true] {
            let mut opts = mode.to_vec();
            if print {
                opts.push("-p");
            }
            let expected_p = {
                let mut a = args(&opts);
                a.push(p(&src));
                strip_mapaddr(&c_ok("mdb_dump", &a, dir.path()))
            };
            // Rust dump -> C load (with the empty first block for bug #1).
            let rust_dump = dump_with(Impl::Rust, &src, &opts);
            assert_eq!(strip_mapaddr(&rust_dump), expected_p, "{name} {opts:?}");
            let mut c_input = empty_block_like(&rust_dump, None);
            c_input.extend_from_slice(&rust_dump);
            let c_dst = dir.path().join(format!("{name}-{print}-c"));
            fs::create_dir_all(&c_dst).unwrap();
            let r = run(
                Impl::C,
                "mdb_load",
                &args(&[p(&c_dst)]),
                &c_input,
                dir.path(),
            );
            assert_eq!(r.code, 0, "{}", r.stderr_str());
            assert_eq!(
                strip_mapaddr(&dump_with(Impl::C, &c_dst, mode)),
                expected,
                "{name}: Rust dump, C load"
            );
            // C dump -> Rust load (no workaround needed).
            let c_dump = dump_with(Impl::C, &src, &opts);
            let r_dst = dir.path().join(format!("{name}-{print}-r"));
            fs::create_dir_all(&r_dst).unwrap();
            let r = run(
                Impl::Rust,
                "mdb_load",
                &args(&[p(&r_dst)]),
                &c_dump,
                dir.path(),
            );
            assert_eq!(r.code, 0, "{}", r.stderr_str());
            assert_eq!(
                strip_mapaddr(&dump_with(Impl::C, &r_dst, mode)),
                expected,
                "{name}: C dump, Rust load"
            );
            // Both loads give the same environment.
            let c_dst2 = dir.path().join(format!("{name}-{print}-c2"));
            fs::create_dir_all(&c_dst2).unwrap();
            let mut c_input = empty_block_like(&c_dump, None);
            c_input.extend_from_slice(&c_dump);
            let r2 = run(
                Impl::Rust,
                "mdb_load",
                &args(&[p(&c_dst2)]),
                &c_input,
                dir.path(),
            );
            assert_eq!(r2.code, 0);
            let mut sa = words("-a");
            sa.push(p(&c_dst));
            let mut sb = words("-a");
            sb.push(p(&c_dst2));
            assert_eq!(
                run(Impl::C, "mdb_stat", &sa, b"", dir.path()).stdout,
                run(Impl::C, "mdb_stat", &sb, b"", dir.path()).stdout,
                "{name}: page layout of C and Rust loads"
            );
        }
    }
}
