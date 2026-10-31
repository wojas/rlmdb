//! Test 1: the unchanged C tools `mdb_stat` and `mdb_dump`, linked against
//! the Rust `liblmdb.a`, print byte for byte what the same tools linked
//! against the C library print, on databases written by C programs
//! (`mtest`..`mtest5`, `mdb_load`, `tests/c/fixture.c`).
//!
//! Compared per invocation: exit status, stdout and stderr (both programs
//! run with the same `argv[0]`).

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;

/// (exit code, stdout, stderr) of `tool` linked against `lib`.
fn run_tool(tool: &str, lib: Lib, args: &[&str]) -> (Option<i32>, String, String) {
    let exe = builder().tool(tool, lib);
    let out = command_as(&exe, tool).args(args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs `tool args` against both libraries and requires identical results.
fn compare(tool: &str, args: &[&str]) {
    let c = run_tool(tool, Lib::C, args);
    let rust = run_tool(tool, Lib::Rust, args);
    assert_eq!(
        c, rust,
        "{tool} {args:?}: C-linked output (left) differs from Rust-linked output (right)"
    );
}

/// The names of the named databases of `env` (`mdb_dump -l`).
fn named_dbs(env: &str) -> Vec<String> {
    let (code, out, err) = run_tool("mdb_dump", Lib::C, &["-l", env]);
    assert_eq!(code, Some(0), "{err}");
    out.lines().map(str::to_owned).collect()
}

/// Every comparison for one environment.
fn compare_all(env: &Path) {
    let env = env.to_str().unwrap();
    for args in [
        &[][..],
        &["-e"],
        &["-f"],
        &["-ff"],
        &["-fff"],
        &["-a"],
        &["-r"],
        &["-rr"],
        &["-e", "-a", "-f", "-r"],
        &["-n"],
    ] {
        let mut a = args.to_vec();
        a.push(env);
        compare("mdb_stat", &a);
    }
    for args in [&[][..], &["-a"], &["-l"], &["-p", "-a"]] {
        let mut a = args.to_vec();
        a.push(env);
        compare("mdb_dump", &a);
    }
    let names = named_dbs(env);
    for name in &names {
        compare("mdb_stat", &["-s", name, env]);
        compare("mdb_dump", &["-s", name, env]);
        compare("mdb_dump", &["-p", "-s", name, env]);
    }
    compare("mdb_stat", &["-s", "no-such-db", env]);
    compare("mdb_dump", &["-s", "no-such-db", env]);
    eprintln!("{env}: tools agree ({} named databases)", names.len());
}

#[test]
fn mtest_environments() {
    for prog in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
        let dir = builder().scratch(prog);
        let env = run_mtest(prog, &dir);
        // mtest creates its environment with MDB_FIXEDMAP: the recorded map
        // address is printed by `mdb_stat -e` and `mdb_dump`.
        compare_all(&env);
    }
}

#[test]
fn fixture_environment() {
    let dir = builder().scratch("fixture");
    make_fixture(&dir);
    compare_all(&dir);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Loads one named database with `mdb_load` (C). A leading empty main-DB
/// section works around mdb_load 0.9.36 dropping the first section's flags
/// (KNOWN_C_BUGS.md #1).
fn load(env: &Path, name: &str, flags: &[&str], pairs: &[(Vec<u8>, Vec<u8>)]) {
    let mut input = String::from(
        "VERSION=3\nformat=bytevalue\ntype=btree\nmapsize=67108864\nmaxreaders=126\nHEADER=END\nDATA=END\n",
    );
    input.push_str(&format!(
        "VERSION=3\nformat=bytevalue\ndatabase={name}\ntype=btree\nmapsize=67108864\n"
    ));
    for f in flags {
        input.push_str(&format!("{f}=1\n"));
    }
    input.push_str("HEADER=END\n");
    for (k, v) in pairs {
        input.push_str(&format!(" {}\n {}\n", hex(k), hex(v)));
    }
    input.push_str("DATA=END\n");
    let file = env.with_extension(format!("{name}.load"));
    fs::write(&file, input).unwrap();
    let exe = builder().tool("mdb_load", Lib::C);
    run_ok(Command::new(exe).arg("-f").arg(&file).arg(env));
}

#[test]
fn mdb_load_environment() {
    let env = builder().scratch("loaded");
    let kv = |k: Vec<u8>, v: Vec<u8>| (k, v);
    let ik8: Vec<_> = (0u64..300)
        .map(|i| {
            kv(
                (i * 7919).to_ne_bytes().to_vec(),
                format!("v{i}").into_bytes(),
            )
        })
        .collect();
    load(&env, "ik8", &["integerkey"], &ik8);
    let ik4: Vec<_> = (0u32..300)
        .map(|i| {
            kv(
                (i * 31).to_ne_bytes().to_vec(),
                vec![b'x'; (i % 50) as usize],
            )
        })
        .collect();
    load(&env, "ik4", &["integerkey"], &ik4);
    let rk: Vec<_> = (0..200)
        .map(|i| {
            kv(
                format!("{i:04}-rk").into_bytes(),
                format!("{i}").into_bytes(),
            )
        })
        .collect();
    load(&env, "rk", &["reversekey"], &rk);
    let rd: Vec<_> = (0..40)
        .flat_map(|i| {
            (0..20).map(move |j| {
                kv(
                    format!("k{i}").into_bytes(),
                    format!("d{j:03}").into_bytes(),
                )
            })
        })
        .collect();
    load(&env, "rd", &["dupsort", "reversedup"], &rd);
    let idup: Vec<_> = (0u32..30)
        .flat_map(|i| {
            (0u32..200).map(move |j| {
                kv(
                    format!("key{i}").into_bytes(),
                    (j * 3 + i).to_ne_bytes().to_vec(),
                )
            })
        })
        .collect();
    load(&env, "idup", &["dupsort", "dupfixed", "integerdup"], &idup);
    let big: Vec<_> = (0..20)
        .map(|i| {
            kv(
                format!("big{i}").into_bytes(),
                vec![i as u8; 5000 + i * 3000],
            )
        })
        .collect();
    load(&env, "big", &[], &big);
    compare_all(&env);
}

#[test]
fn missing_environment() {
    let dir = builder().scratch("missing");
    let missing = dir.join("nope");
    let path = missing.to_str().unwrap();
    compare("mdb_stat", &["-e", path]);
    compare("mdb_dump", &[path]);
    // An existing directory without a database (read-only opens need
    // data.mdb).
    compare("mdb_stat", &[dir.to_str().unwrap()]);
}
