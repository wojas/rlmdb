//! The tools of `rlmdb-tools` on LMDB 1.0 environments against C 1.0's
//! (`mdb_load10`, `mdb_stat10`, `mdb_dump10`, `mdb_drop10`, `mdb_copy10`):
//! the same standard output, messages and exit codes, and the same files.
//! The Rust tools run as `rtools <tool>` (`src/bin/rtools.rs`); both sides
//! run with `argv[0]` set to the tool's name, so messages that name the
//! program compare byte for byte. The tools' options and usage lines are
//! those of 0.9 (`rlmdb-tools/tests` compares them with C 0.9's), 1.0's
//! `-v` and 1.0's `-i` of `mdb_dump` and `mdb_load` (incremental dumps,
//! compared here byte for byte), so only those options are compared here.
//!
//! One difference is not ported (`KNOWN_C_BUGS.md`, section "LMDB 1.0"):
//! C 1.0's `mdb_env_info` reports the process's own map address when the
//! file records none, so `mdb_stat10 -e` prints it and `mdb_dump10` writes
//! it as `mapaddr=` into every dump. rlmdb reports what the file records
//! (0), as 0.9 does; [`c10`] puts C 1.0's output in that form.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use interop::bins;
use interop::pagecmp::compare_data_files;
use interop::tools::audit10;
use rlmdb::{EnvironmentFlags, Format};

/// What a tool run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `cmd` with `stdin`, `RLMDB_DEFAULT_FORMAT` set to `var` or removed.
fn run_cmd(mut cmd: Command, stdin: &[u8], var: Option<&str>) -> Run {
    match var {
        Some(v) => cmd.env("RLMDB_DEFAULT_FORMAT", v),
        None => cmd.env_remove("RLMDB_DEFAULT_FORMAT"),
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn a tool");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    // The tool may exit without reading its input.
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    let _ = feeder.join();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The C 1.0 build of `tool`.
fn c10_bin(tool: &str) -> &'static Path {
    match tool {
        "mdb_stat" => bins::mdb_stat10(),
        "mdb_dump" => bins::mdb_dump10(),
        "mdb_load" => bins::mdb_load10(),
        "mdb_copy" => bins::mdb_copy10(),
        "mdb_drop" => bins::mdb_drop10(),
        other => panic!("no C 1.0 tool {other}"),
    }
}

/// C 1.0's `tool` with `args`, as `argv[0] = tool`, its output as
/// rlmdb's tool prints it for a file without a recorded map address (an
/// environment created without `MDB_FIXEDMAP`, as all of [`c10_env`]'s):
/// `mdb_stat -e`'s `Map address` is a NULL `%p` (`0x0` on macOS, `(nil)`
/// with glibc) and `mdb_dump` has no `mapaddr=` line (see the module
/// documentation).
fn c10(tool: &str, args: &[&str], stdin: &[u8]) -> Run {
    let mut run = c10_raw(tool, args, stdin);
    let null = rlmdb_tools::cfmt::pointer(0);
    run.stdout = run
        .stdout
        .split_inclusive('\n')
        .filter(|l| !l.starts_with("mapaddr="))
        .map(|l| match l.strip_prefix("  Map address: ") {
            // The line keeps its end ("\r\n" on Windows, text mode).
            Some(rest) => {
                let end = &rest[rest.trim_end_matches(['\r', '\n']).len()..];
                format!("  Map address: {null}{end}")
            }
            None => l.to_owned(),
        })
        .collect();
    run
}

/// C 1.0's `tool` with `args`, as `argv[0] = tool`, its output unchanged.
fn c10_raw(tool: &str, args: &[&str], stdin: &[u8]) -> Run {
    let mut cmd = interop::os::command_as(c10_bin(tool), tool);
    cmd.args(args);
    run_cmd(cmd, stdin, None)
}

/// The map address recorded in the newest meta page of the environment at
/// `env` (`mm_address`; not 0 only for `MDB_FIXEDMAP` creators).
fn recorded_map_address(env: &Path) -> u64 {
    let e = rlmdb::Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
        .open(env)
        .expect("open read-only");
    e.info().unwrap().map_address as u64
}

/// rlmdb's `tool` with `args`.
fn rust_var(tool: &str, args: &[&str], stdin: &[u8], var: Option<&str>) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rtools"));
    cmd.arg(tool).args(args);
    run_cmd(cmd, stdin, var)
}

fn rust(tool: &str, args: &[&str], stdin: &[u8]) -> Run {
    rust_var(tool, args, stdin, None)
}

/// Runs `tool args` (the environment last) on both sides and requires the
/// same result, but for the exit code of `mdb_stat -r` without `-a`, `-s`
/// or `-f`, which is 1 in C 1.0 as in 0.9 and 0 in rlmdb
/// (`KNOWN_C_BUGS.md` row 2).
fn assert_same(tool: &str, args: &[&str]) -> Run {
    let env = Path::new(args.last().expect("an environment"));
    let mut c = if recorded_map_address(env) == 0 {
        c10(tool, args, b"")
    } else {
        c10_raw(tool, args, b"")
    };
    let opts = &args[..args.len() - 1];
    if tool == "mdb_stat"
        && opts.iter().any(|a| a.starts_with("-r"))
        && opts.iter().all(|a| matches!(*a, "-r" | "-rr" | "-v"))
        && c.code == 1
    {
        c.code = 0;
    }
    let r = rust(tool, args, b"");
    assert_eq!(r, c, "{tool} {args:?}");
    r
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("interop-tools10-")
        .tempdir()
        .unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().expect("a UTF-8 temp path")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// C 1.0's `mdb_env_get_maxkeysize` for pages of `ps` bytes.
fn maxkey10(ps: usize) -> usize {
    (((ps - 24) / 2) & !1) - 2 - 56
}

/// `mdb_dump` input with `db_pagesize=ps`: a main DB with small values,
/// overflow values and (where the page allows) keys longer than 511 bytes;
/// a DUPSORT DB with keys whose duplicates fit a sub-page and keys whose
/// duplicates need a sub-DB; a DUPFIXED DB; an INTEGERKEY DB. With
/// `empty_key`, the main DB also has a record with an empty key.
fn dump_input(ps: usize, empty_key: bool) -> String {
    let max = maxkey10(ps);
    let header = |name: Option<&str>, flags: &str| {
        let mut h = String::from("VERSION=3\nformat=bytevalue\n");
        if let Some(n) = name {
            h.push_str(&format!("database={n}\n"));
        }
        h.push_str("type=btree\nmapsize=268435456\nmaxreaders=126\n");
        h.push_str(flags);
        h.push_str(&format!("db_pagesize={ps}\nHEADER=END\n"));
        h
    };
    let mut text = header(None, "");
    for i in 0..400usize {
        let mut k = format!("m{i:04}").into_bytes();
        let klen = if i % 50 == 7 {
            600.min(max)
        } else {
            5 + i % 30
        };
        k.resize(klen.max(k.len()), b'k');
        let vlen = if i % 9 == 0 { ps + i * 3 } else { 1 + i % 70 };
        text.push_str(&format!(
            " {}\n {}\n",
            hex(&k),
            hex(&vec![b'a' + (i % 26) as u8; vlen])
        ));
        if empty_key && i == 200 {
            text.push_str(" \n 7a\n");
        }
    }
    text.push_str("DATA=END\n");
    text.push_str(&header(Some("dup"), "duplicates=1\ndupsort=1\n"));
    for i in 0..30usize {
        let n = if i % 10 == 0 { 400 } else { 1 + i % 7 };
        for j in 0..n {
            let mut d = format!("{j:05}").into_bytes();
            d.resize((5 + (i * j) % 40).min(max), b'd');
            text.push_str(&format!(
                " {}\n {}\n",
                hex(format!("d{i:03}").as_bytes()),
                hex(&d)
            ));
        }
    }
    text.push_str("DATA=END\n");
    text.push_str(&header(
        Some("fix"),
        "duplicates=1\ndupsort=1\ndupfixed=1\n",
    ));
    for i in 0..20usize {
        let n = if i % 6 == 0 { 300 } else { 1 + i % 9 };
        for j in 0..n {
            text.push_str(&format!(
                " {}\n {}\n",
                hex(format!("f{i:03}").as_bytes()),
                hex(&(j as u64 * 7919).to_be_bytes())
            ));
        }
    }
    text.push_str("DATA=END\n");
    text.push_str(&header(Some("intk"), "integerkey=1\n"));
    for i in 0..200u64 {
        text.push_str(&format!(
            " {}\n {}\n",
            hex(&(i * 13).to_ne_bytes()),
            hex(b"int")
        ));
    }
    text.push_str("DATA=END\n");
    text
}

/// A new directory `name` under `dir`.
fn new_env(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// `rlmdb`'s view of a data file's format.
fn format_of(env: &Path) -> Option<Format> {
    rlmdb::sniff_format(env, EnvironmentFlags::empty()).unwrap()
}

/// The data files of two environments must be the same, page for page.
fn assert_same_files(c: &Path, r: &Path, what: &str) {
    compare_data_files(&c.join("data.mdb"), &r.join("data.mdb"))
        .unwrap_or_else(|e| panic!("{what}: {e}"));
}

/// `mdb_load -F 1.0` (and with `RLMDB_DEFAULT_FORMAT=1.0`) against
/// `mdb_load10`: the same messages, and page for page the same file
/// (`interop::pagecmp`). Not byte for byte: C 1.0 leaves stale bytes of its
/// scratch page in the unused space of pages and sub-pages, rlmdb zeros.
#[test]
fn mdb_load_f_1_0_matches_mdb_load10() {
    for ps in [1024, 4096, 65536] {
        let dir = tmp();
        let input = dump_input(ps, false);
        let c_env = new_env(dir.path(), "c");
        let c = c10("mdb_load", &[s(&c_env)], input.as_bytes());
        assert_eq!(c.code, 0, "{c:?}");
        // -F 1.0, and no option with the experimental variable.
        for (name, args, var) in [
            ("f10", &["-F", "1.0"][..], None),
            ("var10", &[][..], Some("1.0")),
        ] {
            let r_env = new_env(dir.path(), name);
            let mut all = args.to_vec();
            all.push(s(&r_env));
            let r = rust_var("mdb_load", &all, input.as_bytes(), var);
            assert_eq!(r, c, "page size {ps}, {name}");
            assert_eq!(format_of(&r_env), Some(Format::V10));
            assert_same_files(&c_env, &r_env, &format!("page size {ps}, {name}"));
            if let Err(e) = audit10(&r_env) {
                panic!("page size {ps}, {name}: {e}");
            }
            assert_eq!(
                c10("mdb_dump", &["-a", s(&r_env)], b""),
                c10("mdb_dump", &["-a", s(&c_env)], b""),
                "page size {ps}, {name}"
            );
        }
    }
}

#[test]
fn mdb_load_skips_empty_keys_like_mdb_load10() {
    let dir = tmp();
    let input = dump_input(4096, true);
    let c_env = new_env(dir.path(), "c");
    let r_env = new_env(dir.path(), "r");
    let c = c10("mdb_load", &[s(&c_env)], input.as_bytes());
    let r = rust("mdb_load", &["-F", "1.0", s(&r_env)], input.as_bytes());
    assert_eq!(r, c);
    assert!(c.stderr.contains("zero-length key(ignored)"), "{c:?}");
    assert_same_files(&c_env, &r_env, "empty key");
}

/// `mdb_load -N` commits after every 100 records it stores, not counting
/// the ones it skips (the key exists), as `mdb_load10` (and C 0.9's, in
/// `rlmdb-tools/tests/load.rs`): when the input breaks off, the same
/// records are committed and the same rolled back.
#[test]
fn mdb_load_n_batches_like_mdb_load10() {
    let dir = tmp();
    // No commit before the error; one, after the 100th stored record.
    let mut few = b"exists\nold\nnew\nvalue\n".to_vec();
    for _ in 0..98 {
        few.extend_from_slice(b"exists\nskip\n");
    }
    let mut many = b"exists\nold\n".to_vec();
    for i in 0..150 {
        many.extend_from_slice(format!("new{i:03}\nvalue\nexists\nskip\n").as_bytes());
    }
    for (name, mut input) in [("few", few), ("many", many)] {
        input.extend_from_slice(b"no value\n");
        let c_env = new_env(dir.path(), &format!("c-{name}"));
        let r_env = new_env(dir.path(), &format!("r-{name}"));
        let c = c10("mdb_load", &["-T", "-N", s(&c_env)], &input);
        let r = rust("mdb_load", &["-F", "1.0", "-T", "-N", s(&r_env)], &input);
        assert_eq!(r, c, "{name}");
        assert_eq!(c.code, 1, "{name}: {c:?}");
        assert_eq!(
            c10("mdb_dump", &["-p", s(&r_env)], b""),
            c10("mdb_dump", &["-p", s(&c_env)], b""),
            "{name}"
        );
        assert_same_files(&c_env, &r_env, name);
    }
}

/// A 1.0 environment of `ps`-byte pages for the read-only tools: the
/// [`dump_input`] data, loaded by C 1.0, with one database dropped by C
/// 1.0, so that the free list is not empty.
fn c10_env(dir: &Path, ps: usize) -> PathBuf {
    let env = new_env(dir, &format!("env{ps}"));
    let c = c10("mdb_load", &[s(&env)], dump_input(ps, false).as_bytes());
    assert_eq!(c.code, 0, "{c:?}");
    let c = c10("mdb_drop", &["-d", "-s", "intk", s(&env)], b"");
    assert_eq!(c.code, 0, "{c:?}");
    env
}

/// The `mdb_stat` and `mdb_dump` runs compared on 1.0 environments.
fn read_tool_runs(env: &str) -> Vec<(&'static str, Vec<&str>)> {
    let with = |opts: &[&'static str]| {
        let mut v: Vec<&str> = opts.to_vec();
        v.push(env);
        v
    };
    vec![
        ("mdb_stat", with(&[])),
        ("mdb_stat", with(&["-a"])),
        ("mdb_stat", with(&["-e"])),
        ("mdb_stat", with(&["-f"])),
        ("mdb_stat", with(&["-ff"])),
        ("mdb_stat", with(&["-fff", "-a"])),
        ("mdb_stat", with(&["-s", "dup"])),
        ("mdb_stat", with(&["-s", "nope"])),
        ("mdb_stat", with(&["-s", "m0001kkkkkkkkkkkkkkkkkkkkkkkkkk"])),
        ("mdb_stat", with(&["-r"])),
        ("mdb_stat", with(&["-rr", "-a"])),
        ("mdb_dump", with(&[])),
        ("mdb_dump", with(&["-a"])),
        ("mdb_dump", with(&["-l"])),
        ("mdb_dump", with(&["-p", "-s", "dup"])),
        ("mdb_dump", with(&["-a", "-p"])),
        ("mdb_dump", with(&["-s", "nope"])),
    ]
}

#[test]
fn mdb_stat_and_mdb_dump_match_c10() {
    let dir = tmp();
    for ps in [1024, 4096, 65536] {
        let env = c10_env(dir.path(), ps);
        for (tool, args) in read_tool_runs(s(&env)) {
            let r = assert_same(tool, &args);
            if tool == "mdb_stat" && args.len() == 2 && args[0] == "-a" {
                // 1.0's mdb_stat prints the page size per database and
                // lists the named databases (their keys end with a NUL).
                let out = r.stdout.replace("\r\n", "\n");
                assert!(out.contains(&format!("  Page size: {ps}\n")), "{r:?}");
                assert!(out.contains("Status of dup\n"), "{r:?}");
            }
        }
    }
}

/// The upstream 1.0 test programs leave 1 KiB-page environments with named
/// databases (`mtest3`..`mtest5`) and without (`mtest`, `mtest2`).
#[test]
fn mdb_stat_and_mdb_dump_of_mtest10_environments() {
    for name in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
        let dir = tmp();
        std::fs::create_dir(dir.path().join("testdb")).unwrap();
        let out = Command::new(bins::mtest10(name).unwrap())
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}");
        let env = dir.path().join("testdb");
        for opts in [&["-a", "-f"][..], &["-e"][..]] {
            let mut args = opts.to_vec();
            args.push(s(&env));
            assert_same("mdb_stat", &args);
        }
        for opts in [&[][..], &["-a"][..], &["-l"][..], &["-a", "-p"][..]] {
            let mut args = opts.to_vec();
            args.push(s(&env));
            assert_same("mdb_dump", &args);
        }
    }
}

#[test]
fn mdb_drop_matches_mdb_drop10() {
    let dir = tmp();
    let src = c10_env(dir.path(), 4096);
    drop_runs(dir.path(), &src, "two");
    // Three named databases: more than the two handles the tool opens with
    // (mdb_env_set_maxdbs(env, 2)), so emptying the main DB reuses handles.
    let three = new_env(dir.path(), "three");
    let c = c10("mdb_load", &[s(&three)], dump_input(1024, false).as_bytes());
    assert_eq!(c.code, 0, "{c:?}");
    drop_runs(dir.path(), &three, "three");
}

/// Each `mdb_drop` run of C 1.0 and rlmdb on its own copy of `src`: same
/// output, same file, same view of it through C 1.0's tools.
fn drop_runs(dir: &Path, src: &Path, tag: &str) {
    for (i, opts) in [
        &["-s", "dup"][..],
        &["-d", "-s", "fix"][..],
        &["-d", "-s", "dup"][..],
        // The main DB: C 1.0 deletes its named databases first, so their
        // pages are freed (0.9 leaks them, KNOWN_C_BUGS.md row 21).
        &[][..],
        &["-s", "nope"][..],
        &["-d", "-s", "intk"][..],
    ]
    .iter()
    .enumerate()
    {
        let (c_env, r_env) = (
            dir.join(format!("c{tag}{i}")),
            dir.join(format!("r{tag}{i}")),
        );
        for env in [&c_env, &r_env] {
            std::fs::create_dir_all(env).unwrap();
            std::fs::copy(src.join("data.mdb"), env.join("data.mdb")).unwrap();
        }
        let with = |env: &Path| {
            let mut v: Vec<String> = opts.iter().map(|o| o.to_string()).collect();
            v.push(s(env).to_owned());
            v
        };
        let (ca, ra) = (with(&c_env), with(&r_env));
        let ca: Vec<&str> = ca.iter().map(String::as_str).collect();
        let ra: Vec<&str> = ra.iter().map(String::as_str).collect();
        let c = c10("mdb_drop", &ca, b"");
        let r = rust("mdb_drop", &ra, b"");
        assert_eq!(r, c, "mdb_drop {opts:?}");
        assert_same_files(&c_env, &r_env, &format!("mdb_drop {opts:?}"));
        if let Err(e) = audit10(&r_env) {
            panic!("mdb_drop {opts:?}: {e}");
        }
        for args in [&["-a", "-ff"][..], &["-e"][..]] {
            let view = |env: &Path| {
                let mut a = args.to_vec();
                a.push(s(env));
                let out = c10("mdb_stat", &a, b"");
                out.stdout.replace(s(env), "<env>")
            };
            assert_eq!(view(&r_env), view(&c_env), "mdb_drop {opts:?}");
        }
        assert_eq!(
            c10("mdb_dump", &["-a", s(&r_env)], b"").stdout,
            c10("mdb_dump", &["-a", s(&c_env)], b"").stdout,
            "mdb_drop {opts:?}"
        );
    }
}

#[test]
fn mdb_copy_matches_mdb_copy10() {
    let dir = tmp();
    for ps in [1024, 65536] {
        let src = c10_env(dir.path(), ps);
        for compact in [false, true] {
            let mut files = Vec::new();
            for side in ["c", "r"] {
                let dst = new_env(dir.path(), &format!("{side}-{ps}-{compact}"));
                let mut args = Vec::new();
                if compact {
                    args.push("-c");
                }
                args.extend([s(&src), s(&dst)]);
                let run = if side == "c" {
                    c10("mdb_copy", &args, b"")
                } else {
                    // The variable does not change what a copy is: the
                    // pages of the source, in its format.
                    rust_var("mdb_copy", &args, b"", Some("0.9"))
                };
                assert_eq!((run.code, run.stderr.as_str()), (0, ""), "{run:?}");
                files.push(std::fs::read(dst.join("data.mdb")).unwrap());
            }
            assert!(
                files[0] == files[1],
                "mdb_copy (page size {ps}, compact {compact}): files differ"
            );
        }
    }
}

/// `-v` (`MDB_PREVSNAPSHOT`) of `mdb_stat`, `mdb_dump` and `mdb_copy`
/// against C 1.0's, on environments whose last commit (C 1.0's `mdb_drop`)
/// freed pages: the same output, and with `mdb_copy -v -c` the same file.
/// `mdb_copy -v` without `-c` differs (`KNOWN_C_BUGS.md` row 69): C 1.0's
/// copy keeps the latest commit's meta page, rlmdb's is a copy of the
/// previous snapshot; the two files differ in that page only.
#[test]
fn dash_v_matches_c10() {
    let dir = tmp();
    for ps in [1024, 65536] {
        let env = c10_env(dir.path(), ps);
        for (tool, args) in read_tool_runs(s(&env)) {
            let mut args = args.clone();
            args.insert(0, "-v");
            assert_same(tool, &args);
        }
        // The previous snapshot still has the dropped database.
        let r = assert_same("mdb_stat", &["-v", "-s", "intk", s(&env)]);
        assert_eq!(r.code, 0, "{r:?}");
        assert_ne!(assert_same("mdb_stat", &["-s", "intk", s(&env)]).code, 0);

        let latest = {
            let e = rlmdb::Environment::builder()
                .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
                .open(&env)
                .unwrap();
            e.info().unwrap().last_txnid
        };
        for compact in [false, true] {
            let mut files = Vec::new();
            for side in ["c", "r"] {
                let dst = new_env(dir.path(), &format!("v-{side}-{ps}-{compact}"));
                let mut args = vec!["-v"];
                if compact {
                    args.push("-c");
                }
                args.extend([s(&env), s(&dst)]);
                let run = if side == "c" {
                    c10("mdb_copy", &args, b"")
                } else {
                    rust("mdb_copy", &args, b"")
                };
                assert_eq!((run.code, run.stderr.as_str()), (0, ""), "{run:?}");
                files.push(std::fs::read(dst.join("data.mdb")).unwrap());
                if side == "r" {
                    // A copy of the previous snapshot, for C 1.0 too.
                    if let Err(e) = audit10(&dst) {
                        panic!("mdb_copy -v (compact {compact}): {e}");
                    }
                    assert_eq!(
                        c10("mdb_dump", &["-a", s(&dst)], b"").stdout,
                        c10("mdb_dump", &["-v", "-a", s(&env)], b"").stdout,
                        "mdb_copy -v (page size {ps}, compact {compact})"
                    );
                }
            }
            if compact {
                assert!(
                    files[0] == files[1],
                    "mdb_copy -v -c (page size {ps}): files differ"
                );
            } else {
                assert_eq!(files[0].len(), files[1].len());
                let newer = latest & 1;
                for (i, (c, r)) in files[0].chunks(ps).zip(files[1].chunks(ps)).enumerate() {
                    assert!(
                        i == newer || c == r,
                        "mdb_copy -v (page size {ps}): page {i} differs"
                    );
                }
            }
        }
    }
}

/// What a tool with binary output (`-i`) produced.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinRun {
    code: i32,
    stdout: Vec<u8>,
    stderr: String,
}

fn run_bin(mut cmd: Command, stdin: &[u8]) -> BinRun {
    cmd.env_remove("RLMDB_DEFAULT_FORMAT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn a tool");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().unwrap();
    let _ = feeder.join();
    BinRun {
        code: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// C 1.0's `tool`, as `argv[0] = tool`, binary output.
fn c10_bin_run(tool: &str, args: &[&str], stdin: &[u8]) -> BinRun {
    let mut cmd = interop::os::command_as(c10_bin(tool), tool);
    cmd.args(args);
    run_bin(cmd, stdin)
}

/// rlmdb's `tool`, binary output.
fn rust_bin_run(tool: &str, args: &[&str], stdin: &[u8]) -> BinRun {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rtools"));
    cmd.arg(tool).args(args);
    run_bin(cmd, stdin)
}

/// The newest txnid of the environment at `env`.
fn last_txnid(env: &Path) -> u64 {
    let e = rlmdb::Environment::builder()
        .flags(EnvironmentFlags::READ_ONLY | EnvironmentFlags::NO_LOCK)
        .open(env)
        .expect("open read-only");
    e.info().unwrap().last_txnid as u64
}

/// The page size of a new 1.0 environment that asks for none (the OS
/// page size, as in C 1.0).
fn default_page_size10() -> usize {
    let dir = tmp();
    let e = rlmdb::Environment::builder()
        .format(Format::V10)
        .open(dir.path())
        .unwrap();
    e.page_size()
}

/// A copy of `src`'s data file in the new environment directory `name`.
fn copy_env(src: &Path, dir: &Path, name: &str) -> PathBuf {
    let dst = new_env(dir, name);
    std::fs::copy(src.join("data.mdb"), dst.join("data.mdb")).unwrap();
    dst
}

/// `mdb_dump -i` against `mdb_dump10 -i`: the same incremental dumps on
/// standard output and in `-f` files, and the same messages and exit
/// codes for the txnids `sscanf("%zu")` reads or refuses, for a missing
/// environment and for `-f` files that exist or cannot be created.
#[test]
fn mdb_dump_i_matches_mdb_dump10() {
    let dir = tmp();
    let env = c10_env(dir.path(), 4096);
    let e = s(&env);
    let last = last_txnid(&env);
    let (mid, last_s) = ((last / 2).to_string(), last.to_string());
    let data = env.join("data.mdb");
    let missing = dir.path().join("missing");
    let cases: Vec<Vec<&str>> = vec![
        vec!["-i", "1", e],
        vec!["-i", &mid, e],
        vec!["-i", &last_s, e],
        vec!["-i", "5x", e],
        vec!["-i", "+2", e],
        vec!["-i", "-1", e],
        vec!["-i", "99999999999999999999", e],
        vec!["-a", "-p", "-i", "2", e],
        vec!["-n", "-i", "1", s(&data)],
        vec!["-i", "0", e],
        vec!["-i", "abc", e],
        vec!["-i", "", e],
        vec!["-i", "0x10", e],
        vec!["-i", "99999999999999999999", "-i", "0", e],
        vec!["-i", "1", s(&missing)],
    ];
    for args in &cases {
        let c = c10_bin_run("mdb_dump", args, b"");
        let r = rust_bin_run("mdb_dump", args, b"");
        assert_eq!(r, c, "mdb_dump {args:?}");
    }
    let since_mid = c10_bin_run("mdb_dump", &["-i", &mid, e], b"");
    assert!(since_mid.code == 0 && !since_mid.stdout.is_empty());

    // -f before or after -i: a new file with the dump, which must not
    // exist yet; the same in a directory that does not exist.
    for (i, f_first) in [true, false].into_iter().enumerate() {
        let (out_c, out_r) = (
            dir.path().join(format!("c{i}.incr")),
            dir.path().join(format!("r{i}.incr")),
        );
        let args = |out: &Path| -> Vec<String> {
            let mut a = vec!["-i".to_string(), mid.clone()];
            let f = ["-f".to_string(), s(out).to_string()];
            if f_first {
                a.splice(0..0, f);
            } else {
                a.extend(f);
            }
            a.push(e.to_string());
            a
        };
        for _ in 0..2 {
            let (ac, ar) = (args(&out_c), args(&out_r));
            let ac: Vec<&str> = ac.iter().map(String::as_str).collect();
            let ar: Vec<&str> = ar.iter().map(String::as_str).collect();
            let c = c10_bin_run("mdb_dump", &ac, b"");
            let r = rust_bin_run("mdb_dump", &ar, b"");
            assert_eq!(r, c, "mdb_dump {ar:?}");
        }
        assert_eq!(
            std::fs::read(&out_r).unwrap(),
            std::fs::read(&out_c).unwrap()
        );
        assert_eq!(std::fs::read(&out_c).unwrap(), since_mid.stdout);
    }
    let (in_missing_c, in_missing_r) = (missing.join("c.incr"), missing.join("r.incr"));
    let c = c10_bin_run("mdb_dump", &["-i", "1", "-f", s(&in_missing_c), e], b"");
    let r = rust_bin_run("mdb_dump", &["-i", "1", "-f", s(&in_missing_r), e], b"");
    assert_eq!(r, c, "mdb_dump -f in a missing directory");
    assert_eq!(c.code, 1, "{c:?}");
}

/// `mdb_load -i` against `mdb_load10 -i`: the dump of a C 1.0 environment
/// since a copy of it, from standard input and with `-f`, makes copies of
/// that copy the same file with both; a new environment (1.0 in both, the
/// OS page size) gets the same pages; a missing directory and a missing
/// `-f` file give the same messages.
#[test]
fn mdb_load_i_matches_mdb_load10() {
    let dir = tmp();
    let src = c10_env(dir.path(), default_page_size10());
    let base = new_env(dir.path(), "base");
    let c = c10("mdb_copy", &[s(&src), s(&base)], b"");
    assert_eq!(c.code, 0, "{c:?}");
    let t0 = last_txnid(&src);
    let c = c10(
        "mdb_load",
        &["-T", "-s", "dup", s(&src)],
        b"d999\nnew\nd998\nnewer\n",
    );
    assert_eq!(c.code, 0, "{c:?}");
    let c = c10("mdb_drop", &["-s", "fix", s(&src)], b"");
    assert_eq!(c.code, 0, "{c:?}");
    let dump = c10_bin_run("mdb_dump", &["-i", &t0.to_string(), s(&src)], b"");
    assert_eq!(dump.code, 0, "{dump:?}");
    let dump_file = dir.path().join("dump.incr");
    std::fs::write(&dump_file, &dump.stdout).unwrap();

    for (i, with_f) in [false, true].into_iter().enumerate() {
        let (tc, tr) = (
            copy_env(&base, dir.path(), &format!("c{i}")),
            copy_env(&base, dir.path(), &format!("r{i}")),
        );
        let (input, f): (&[u8], Vec<&str>) = if with_f {
            (b"", vec!["-f", s(&dump_file)])
        } else {
            (&dump.stdout, vec![])
        };
        let (ac, ar) = (
            [&f[..], &["-i", s(&tc)]].concat(),
            [&f[..], &["-i", s(&tr)]].concat(),
        );
        let c = c10_bin_run("mdb_load", &ac, input);
        let r = rust_bin_run("mdb_load", &ar, input);
        assert_eq!(r, c, "-f: {with_f}");
        assert_eq!(c.code, 0, "{c:?}");
        assert_eq!(
            std::fs::read(tr.join("data.mdb")).unwrap(),
            std::fs::read(tc.join("data.mdb")).unwrap()
        );
        assert_eq!(
            std::fs::read(tc.join("data.mdb")).unwrap(),
            std::fs::read(src.join("data.mdb")).unwrap()
        );
        if let Err(e) = audit10(&tr) {
            panic!("-f: {with_f}: {e}");
        }
    }

    // A new environment.
    let full = c10_bin_run("mdb_dump", &["-i", "1", s(&src)], b"");
    let (nc, nr) = (new_env(dir.path(), "new-c"), new_env(dir.path(), "new-r"));
    let c = c10_bin_run("mdb_load", &["-i", s(&nc)], &full.stdout);
    let r = rust_bin_run("mdb_load", &["-i", s(&nr)], &full.stdout);
    assert_eq!(r, c, "a new environment");
    assert_eq!(c.code, 0, "{c:?}");
    assert_eq!(format_of(&nr), Some(Format::V10));
    assert_eq!(
        std::fs::read(nr.join("data.mdb")).unwrap(),
        std::fs::read(nc.join("data.mdb")).unwrap()
    );

    // Errors.
    let missing = dir.path().join("missing");
    let in_missing = missing.join("env");
    for args in [
        vec!["-i", s(&in_missing)],
        vec!["-f", s(&missing), "-i", s(&base)],
    ] {
        let c = c10_bin_run("mdb_load", &args, &dump.stdout);
        let r = rust_bin_run("mdb_load", &args, &dump.stdout);
        assert_eq!(r, c, "mdb_load {args:?}");
        assert_eq!(c.code, 1, "{c:?}");
    }
}

/// Where rlmdb's tools differ from C 1.0's on purpose (rlmdb only): a dump
/// cut inside a header or a page is an error (`KNOWN_C_BUGS.md` row 66;
/// C 1.0 reports success, or reads for ever); an LMDB 0.9 environment
/// cannot be dumped or loaded incrementally (C 1.0 does not open it);
/// `mdb_load -i` makes a new environment 1.0 whatever the variable says,
/// and refuses `-F 0.9` as a usage error.
#[test]
fn incremental_tools_beyond_c10() {
    let dir = tmp();
    let src = c10_env(dir.path(), 4096);
    let base = copy_env(&src, dir.path(), "base");
    let t0 = last_txnid(&src);
    let c = c10("mdb_drop", &["-s", "fix", s(&src)], b"");
    assert_eq!(c.code, 0, "{c:?}");
    let dump = rust_bin_run("mdb_dump", &["-i", &t0.to_string(), s(&src)], b"").stdout;
    assert!(dump.len() > 4096);
    let eio = format!(
        "mdb_env_incr_load failed, error {} {}\n",
        libc::EIO,
        rlmdb::strerror(libc::EIO)
    );
    for cut in [10, dump.len() - 100] {
        let t = copy_env(&base, dir.path(), &format!("cut{cut}"));
        let r = rust_bin_run("mdb_load", &["-i", s(&t)], &dump[..cut]);
        assert_eq!(
            (r.code, r.stderr.replace("\r\n", "\n").as_str()),
            (1, eio.as_str()),
            "cut at {cut}"
        );
    }

    // LMDB 0.9.
    let v09 = new_env(dir.path(), "v09");
    let r = rust_bin_run("mdb_load", &["-T", s(&v09)], b"k\nv\n");
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(format_of(&v09), Some(Format::V09));
    let enotsup = |what: &str| {
        format!(
            "{what} failed, error {} {}\n",
            libc::ENOTSUP,
            rlmdb::strerror(libc::ENOTSUP)
        )
    };
    let r = rust_bin_run("mdb_dump", &["-i", "1", s(&v09)], b"");
    let stderr = r.stderr.replace("\r\n", "\n");
    assert_eq!((r.code, stderr), (1, enotsup("mdb_env_incr_dump")));
    let r = rust_bin_run("mdb_load", &["-i", s(&v09)], &dump);
    let stderr = r.stderr.replace("\r\n", "\n");
    assert_eq!((r.code, stderr), (1, enotsup("mdb_env_incr_load")));
    // -i always means 1.0: -F 0.9 with it is a usage error before
    // anything is opened, in either order.
    let new09 = new_env(dir.path(), "new09");
    for args in [["-F", "0.9", "-i"], ["-i", "-F", "0.9"]] {
        let mut all = args.to_vec();
        all.push(s(&new09));
        let r = rust_bin_run("mdb_load", &all, &dump);
        let usage = "usage: mdb_load [-V] [-a] [-f input] [-F 0.9|1.0] [-i] [-n] [-s name] [-N] [-T] dbpath";
        let expected =
            format!("mdb_load: -i loads LMDB 1.0 environments only, not with -F 0.9\n{usage}\n");
        assert_eq!(
            (r.code, r.stderr.replace("\r\n", "\n")),
            (1, expected),
            "{all:?}"
        );
        assert_eq!(format_of(&new09), None, "{all:?}");
    }
    // A new environment is 1.0, also with -F 1.0, and whatever the
    // experimental variable says.
    for (name, args, var) in [
        ("new10", &[][..], None),
        ("f10", &["-F", "1.0"][..], None),
        ("var09", &[][..], Some("0.9")),
    ] {
        let env = new_env(dir.path(), name);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rtools"));
        cmd.arg("mdb_load").args(args).args(["-i", s(&env)]);
        let r = run_cmd(cmd, b"", var);
        assert_eq!((r.code, r.stderr.as_str()), (0, ""), "{name}");
        assert_eq!(format_of(&env), Some(Format::V10), "{name}");
    }
}
