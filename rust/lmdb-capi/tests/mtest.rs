//! Acceptance test: the unmodified upstream C programs of
//! `libraries/liblmdb` run against the Rust library.
//!
//! * `mtest.c` .. `mtest6.c`, built once against the C library and once
//!   against the Rust `liblmdb.a` (with `tests/c/mtest_shim.h`: a fixed
//!   `rand()` seed, and `mdb_dkey()` for `mtest6`), run with the same seeds
//!   in fresh directories. Both must exit 0 with nothing on stderr and print
//!   the same stdout (pointer values aside). The environments they leave
//!   must be the same as far as the C tools can tell: `mdb_stat -e -f -ff
//!   -a` (pages, depths, entries, the free list page by page; the map
//!   address line is dropped, as C records `MDB_FIXEDMAP` addresses and
//!   Rust does not) and `mdb_dump -a` print the same, the C peer's
//!   `check-freelist` (a full page audit) passes on both, and the
//!   Rust-linked `mdb_stat`/`mdb_dump` read the Rust-written environment
//!   exactly as the C tools do.
//! * `mdb_load.c`, `mdb_dump.c`, `mdb_copy.c`, `mdb_stat.c` linked against
//!   either library work on each other's output: C `mdb_load` then
//!   Rust-linked `mdb_dump`, Rust-linked `mdb_load` then C `mdb_dump`, and
//!   Rust-linked `mdb_copy` (plain and `-c`) checked with the C tools
//!   against the source.

mod common;

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use common::*;

/// The seeds every `mtest*` program runs with.
const SEEDS: [u32; 2] = [MTEST_SEED, 7];

/// `0x...` (a `%p` value) replaced by `PTR`.
fn without_pointers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let n = tail
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(tail.len());
        out.push_str("PTR");
        rest = &tail[n..];
    }
    out.push_str(rest);
    out
}

/// stdout of `tool args env` linked against `lib`; it must succeed.
fn tool_output(tool: &str, lib: Lib, args: &[&str], env: &Path) -> String {
    let exe = builder().tool(tool, lib);
    let out = run_ok(Command::new(exe).arg0(tool).args(args).arg(env));
    String::from_utf8(out.stdout).unwrap()
}

/// `text` without the lines that show the recorded map address (C records
/// it for `MDB_FIXEDMAP` environments, Rust writes 0: `KNOWN_C_BUGS.md`
/// #9).
fn without_map_address(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("Map address") && !l.starts_with("mapaddr="))
        .map(|l| format!("{l}\n"))
        .collect()
}

/// What the C tools see in `env`: the statistics and the full dump,
/// without the map address.
fn c_view(env: &Path) -> (String, String) {
    let stat = tool_output("mdb_stat", Lib::C, &["-e", "-f", "-ff", "-a"], env);
    let dump = tool_output("mdb_dump", Lib::C, &["-a"], env);
    (without_map_address(&stat), without_map_address(&dump))
}

/// The C peer's page audit of `env` must pass.
fn check_freelist(env: &Path) {
    let exe = builder().cpeer(Lib::C);
    run_ok(Command::new(exe).arg("check-freelist").arg(env));
}

/// Runs `prog` against both libraries with every seed and compares.
fn acceptance(prog: &str) {
    for seed in SEEDS {
        let base = builder().scratch(&format!("{prog}-{seed}"));
        let (c_dir, rust_dir) = (base.join("c"), base.join("rust"));
        let c = run_mtest_seeded(prog, Lib::C, &c_dir, seed);
        let rust = run_mtest_seeded(prog, Lib::Rust, &rust_dir, seed);
        for (lib, out) in [("C", &c), ("Rust", &rust)] {
            assert!(
                out.stderr.is_empty(),
                "{prog} (seed {seed}, {lib} library) wrote to stderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let c_out = without_pointers(&String::from_utf8_lossy(&c.stdout));
        let rust_out = without_pointers(&String::from_utf8_lossy(&rust.stdout));
        assert_eq!(c_out, rust_out, "{prog} (seed {seed}): stdout differs");
        let (c_env, rust_env) = (c_dir.join("testdb"), rust_dir.join("testdb"));
        let c_view_c = c_view(&c_env);
        assert_eq!(
            c_view_c,
            c_view(&rust_env),
            "{prog} (seed {seed}): the C tools see different environments"
        );
        check_freelist(&c_env);
        check_freelist(&rust_env);
        // The Rust library reads the Rust-written file as C does.
        for (tool, args) in [
            ("mdb_stat", &["-e", "-f", "-ff", "-a"][..]),
            ("mdb_dump", &["-a"][..]),
        ] {
            assert_eq!(
                tool_output(tool, Lib::C, args, &rust_env),
                tool_output(tool, Lib::Rust, args, &rust_env),
                "{prog} (seed {seed}): Rust-linked {tool} {args:?}"
            );
        }
        eprintln!(
            "{prog} seed {seed}: {} lines of identical output, identical environments",
            c_out.lines().count()
        );
    }
}

#[test]
fn mtest() {
    acceptance("mtest");
}

#[test]
fn mtest2() {
    acceptance("mtest2");
}

#[test]
fn mtest3() {
    acceptance("mtest3");
}

#[test]
fn mtest4() {
    acceptance("mtest4");
}

#[test]
fn mtest5() {
    acceptance("mtest5");
}

#[test]
fn mtest6() {
    acceptance("mtest6");
}

// ---------------------------------------------------------------------------
// mdb_load / mdb_dump / mdb_copy
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One database section of an `mdb_dump` file.
fn section(out: &mut String, name: Option<&str>, flags: &[&str], pairs: &[(Vec<u8>, Vec<u8>)]) {
    out.push_str("VERSION=3\nformat=bytevalue\n");
    if let Some(name) = name {
        out.push_str(&format!("database={name}\n"));
    }
    out.push_str("type=btree\nmapsize=67108864\nmaxreaders=126\n");
    for f in flags {
        out.push_str(&format!("{f}=1\n"));
    }
    out.push_str("HEADER=END\n");
    for (k, v) in pairs {
        out.push_str(&format!(" {}\n {}\n", hex(k), hex(v)));
    }
    out.push_str("DATA=END\n");
}

/// A dump file with every kind of database: integer keys (4 and 8 bytes),
/// reverse keys, sorted and reverse-sorted duplicates, fixed-size integer
/// duplicates (enough for sub-databases), overflow values, and plain data
/// in the main DB. The first section is the main DB, so `mdb_load`
/// dropping its flags does not matter (`KNOWN_C_BUGS.md` #1).
fn dump_input() -> String {
    let kv = |k: Vec<u8>, v: Vec<u8>| (k, v);
    let mut s = String::new();
    let main: Vec<_> = (0..50)
        .map(|i| kv(format!("main{i:03}").into_bytes(), vec![b'm'; i]))
        .collect();
    section(&mut s, None, &[], &main);
    let ik8: Vec<_> = (0u64..400)
        .map(|i| {
            kv(
                (i * 7919).to_ne_bytes().to_vec(),
                format!("v{i}").into_bytes(),
            )
        })
        .collect();
    section(&mut s, Some("ik8"), &["integerkey"], &ik8);
    let ik4: Vec<_> = (0u32..300)
        .map(|i| {
            kv(
                (i * 31).to_ne_bytes().to_vec(),
                vec![b'x'; (i % 50) as usize],
            )
        })
        .collect();
    section(&mut s, Some("ik4"), &["integerkey"], &ik4);
    let mut rk: Vec<_> = (0..200)
        .map(|i| {
            kv(
                format!("{i:04}-rk").into_bytes(),
                format!("{i}").into_bytes(),
            )
        })
        .collect();
    // In dump order: reverse keys sort by their bytes from the end.
    rk.sort_by(|a, b| a.0.iter().rev().cmp(b.0.iter().rev()));
    section(&mut s, Some("rk"), &["reversekey"], &rk);
    let rd: Vec<_> = (0..40)
        .flat_map(|i| {
            (0..20).rev().map(move |j| {
                kv(
                    format!("k{i:02}").into_bytes(),
                    format!("d{j:03}").into_bytes(),
                )
            })
        })
        .collect();
    section(&mut s, Some("rd"), &["dupsort", "reversedup"], &rd);
    let idup: Vec<_> = (0u32..30)
        .flat_map(|i| {
            (0u32..200).map(move |j| {
                kv(
                    format!("key{i:02}").into_bytes(),
                    (j * 3 + i).to_ne_bytes().to_vec(),
                )
            })
        })
        .collect();
    section(
        &mut s,
        Some("idup"),
        &["dupsort", "dupfixed", "integerdup"],
        &idup,
    );
    let big: Vec<_> = (0..20)
        .map(|i| {
            kv(
                format!("big{i:02}").into_bytes(),
                vec![i as u8; 5000 + i * 3000],
            )
        })
        .collect();
    section(&mut s, Some("big"), &[], &big);
    s
}

/// `mdb_load -f <file> <env>` linked against `lib`.
fn load(lib: Lib, file: &Path, env: &Path) {
    fs::create_dir_all(env).unwrap();
    let exe = builder().tool("mdb_load", lib);
    run_ok(Command::new(exe).arg("-f").arg(file).arg(env));
}

#[test]
fn mdb_load_and_mdb_dump_across_libraries() {
    let dir = builder().scratch("load");
    let file = dir.join("input.dump");
    fs::write(&file, dump_input()).unwrap();
    let (c_env, rust_env) = (dir.join("c-loaded"), dir.join("rust-loaded"));
    load(Lib::C, &file, &c_env);
    load(Lib::Rust, &file, &rust_env);
    // Same pages and the same data, as the C tools see them.
    assert_eq!(c_view(&c_env), c_view(&rust_env));
    check_freelist(&rust_env);
    // Dumps: C-loaded by the Rust-linked mdb_dump, Rust-loaded by the C
    // mdb_dump; also per database and in printable form.
    let names = tool_output("mdb_dump", Lib::C, &["-l"], &c_env);
    assert_eq!(names.lines().count(), 6, "{names}");
    for args in [&["-a"][..], &["-a", "-p"][..]] {
        let reference = tool_output("mdb_dump", Lib::C, args, &c_env);
        assert_eq!(tool_output("mdb_dump", Lib::Rust, args, &c_env), reference);
        assert_eq!(tool_output("mdb_dump", Lib::C, args, &rust_env), reference);
    }
    for name in names.lines() {
        assert_eq!(
            tool_output("mdb_dump", Lib::Rust, &["-s", name], &c_env),
            tool_output("mdb_dump", Lib::C, &["-s", name], &rust_env),
            "database {name}"
        );
    }
    // The dump of the loaded environment loads again to the same thing
    // (with the Rust-linked tools only).
    let again = dir.join("again.dump");
    fs::write(
        &again,
        tool_output("mdb_dump", Lib::Rust, &["-a"], &rust_env),
    )
    .unwrap();
    let rust_env2 = dir.join("rust-loaded-again");
    load(Lib::Rust, &again, &rust_env2);
    assert_eq!(
        tool_output("mdb_dump", Lib::C, &["-a"], &rust_env2),
        tool_output("mdb_dump", Lib::C, &["-a"], &c_env)
    );
}

/// `mdb_copy [-c] <src> <dst>` linked against `lib`.
fn copy(lib: Lib, compact: bool, src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    let exe = builder().tool("mdb_copy", lib);
    let mut cmd = Command::new(exe);
    if compact {
        cmd.arg("-c");
    }
    run_ok(cmd.arg(src).arg(dst));
}

#[test]
fn mdb_copy_across_libraries() {
    let dir = builder().scratch("copy");
    // A source with free pages, so that compaction changes something.
    run_mtest_seeded("mtest3", Lib::C, &dir, MTEST_SEED);
    let src = dir.join("testdb");
    let file = dir.join("input.dump");
    fs::write(&file, dump_input()).unwrap();
    let loaded = dir.join("loaded");
    load(Lib::C, &file, &loaded);
    for (n, src) in [src, loaded].iter().enumerate() {
        for compact in [false, true] {
            let tag = format!("{n}-{}", if compact { "compact" } else { "plain" });
            let (c_dst, rust_dst) = (
                dir.join(format!("c-{tag}")),
                dir.join(format!("rust-{tag}")),
            );
            copy(Lib::C, compact, src, &c_dst);
            copy(Lib::Rust, compact, src, &rust_dst);
            // The Rust copy is what the C copy is, page for page.
            assert_eq!(c_view(&c_dst), c_view(&rust_dst), "copy {tag}");
            // And holds the source's data.
            assert_eq!(
                tool_output("mdb_dump", Lib::C, &["-a"], &rust_dst),
                tool_output("mdb_dump", Lib::C, &["-a"], src),
                "copy {tag}"
            );
            check_freelist(&rust_dst);
        }
    }
}
