//! The Rust peer on its own terms: its outputs are byte-identical to the C
//! peer's where the harness compares them (also for the writing commands
//! and for non-UTF-8 database names), its checkers reject bad data, and it
//! implements every command of the protocol.

mod common;

use interop::scenarios::{self, DataSet};
use interop::{Output, Peer, TestEnv, EXIT_NOT_IMPLEMENTED};

fn register() {
    interop::register_rpeer(option_env!("CARGO_BIN_EXE_rpeer"));
}

/// Run `args` on both peers; return both outputs.
fn both(args: &[&str]) -> interop::Result<(Output, Output)> {
    let c = Peer::C.cmd(args[0]).args(&args[1..]).output()?;
    let r = Peer::Rust.cmd(args[0]).args(&args[1..]).output()?;
    Ok((c, r))
}

#[test]
fn rpeer_is_built() {
    register();
    let path = Peer::Rust.bin().expect("rpeer");
    assert!(path.is_file(), "{}", path.display());
}

/// The generator is byte-identical to `c/dataset.c`, for many seeds, items
/// and data options, including the usage errors of impossible combinations.
#[test]
fn gen_matches_c() -> interop::Result<()> {
    register();
    let flag_sets = [
        "",
        "--intkey",
        "--revkey",
        "--valsize 1",
        "--valsize 3",
        "--valsize 300",
        "--big 9000",
        "--big 5 --valsize 2",
        "--dupsort",
        "--dupsort --dups 1",
        "--dupsort --dups 40 --valsize 4",
        "--dupsort --valsize 511 --dups 7",
        "--revdup --dups 12",
        "--dupfixed --dups 9 --valsize 13",
        "--intdup",
        "--intdup --valsize 4 --dups 30",
        "--intkey --dupsort --dups 3",
        // usage errors: both must refuse with exit status 2
        "--intdup --valsize 6",
        "--dupsort --valsize 3",
        "--dupsort --big 10",
        "--dups 3",
        "--intkey --revkey",
    ];
    let mut compared = 0;
    for flags in flag_sets {
        for seed in [0u32, 1, 2, 77, 123_456, u32::MAX] {
            for i in [0u32, 1, 2, 3, 5, 1000, 65_536, 4_000_000_000, u32::MAX - 1] {
                let mut args = vec!["gen".to_owned(), seed.to_string(), i.to_string()];
                args.extend(flags.split_whitespace().map(str::to_owned));
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let (c, r) = both(&args)?;
                assert_eq!(
                    c.code(),
                    r.code(),
                    "exit status of {args:?}: C {c}\nRust {r}"
                );
                assert_eq!(c.stdout, r.stdout, "gen {args:?}");
                compared += 1;
            }
        }
    }
    // i = 2^32 - 1 is refused by both.
    let (c, r) = both(&["gen", "1", "4294967295"])?;
    assert_eq!((c.code(), r.code()), (Some(2), Some(2)));
    println!("compared {compared} gen outputs");
    Ok(())
}

/// rpeer implements every command of the protocol (the C peer's list), and
/// says so in `rpeer commands`, which is what `Peer::supports` asks. (A
/// command that needs a part of `rlmdb` that does not exist yet would
/// be listed with `implemented=0` and exit with status 4.)
#[test]
fn rpeer_implements_every_command() -> interop::Result<()> {
    register();
    let c = Peer::C.cmd("commands").run()?;
    let r = Peer::Rust.cmd("commands").run()?;
    let names = |out: &interop::Output| -> Vec<String> {
        let mut v: Vec<String> = out
            .lines()
            .filter_map(|l| l.strip_prefix("command="))
            .map(|l| l.split(' ').next().unwrap_or_default().to_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&c), names(&r), "the peers know different commands");
    for name in names(&c) {
        assert!(
            Peer::Rust.supports(&name),
            "rpeer does not implement {name}"
        );
    }
    assert!(!r.stdout.contains("implemented=0"), "{r}");
    assert_eq!(EXIT_NOT_IMPLEMENTED, 4);
    Ok(())
}

/// `fill`, `delete` and `create` print exactly what the C peer prints for
/// the same operations on fresh environments (the same transactions, the
/// same entries, and the same tree shapes: depth and page counts).
#[test]
fn write_outputs_equal_c() -> interop::Result<()> {
    register();
    let steps: &[&[&str]] = &[
        &["fill", "plain", "3000", "11", "--per-txn", "700"],
        &[
            "fill",
            "dups",
            "500",
            "3",
            "--dupsort",
            "--dups",
            "9",
            "--per-txn",
            "77",
        ],
        &[
            "fill",
            "sub",
            "20",
            "4",
            "--dupsort",
            "--dups",
            "900",
            "--valsize",
            "40",
        ],
        &["fill", "fixed", "20", "5", "--dupfixed", "--dups", "900"],
        &[
            "fill",
            "ints",
            "800",
            "6",
            "--intkey",
            "--big",
            "20000",
            "--per-txn",
            "100",
        ],
        &["fill", "app", "1000", "7", "--append", "--per-txn", "300"],
        &[
            "fill", "appd", "200", "8", "--revdup", "--dups", "7", "--append",
        ],
        &["fill", "res", "100", "9", "--reserve", "--big", "9000"],
        &["fill", "rev", "700", "10", "--revkey", "--valsize", "200"],
        &[
            "delete",
            "dups",
            "500",
            "3",
            "--dupsort",
            "--dups",
            "9",
            "--every",
            "3",
            "--per-txn",
            "50",
        ],
        &["delete", "plain", "3000", "11", "--every", "2"],
        &[
            "delete",
            "sub",
            "20",
            "4",
            "--dupsort",
            "--dups",
            "900",
            "--valsize",
            "40",
            "--every",
            "3",
        ],
        &[
            "delete",
            "fixed",
            "20",
            "5",
            "--dupfixed",
            "--dups",
            "900",
            "--every",
            "2",
        ],
        &[
            "delete",
            "ints",
            "800",
            "6",
            "--intkey",
            "--big",
            "20000",
            "--every",
            "1",
            "--per-txn",
            "99",
        ],
    ];
    let envs: Vec<TestEnv> = (0..2)
        .map(|_| TestEnv::new())
        .collect::<interop::Result<_>>()?;
    let peers = [Peer::C, Peer::Rust];
    let mut outs = [String::new(), String::new()];
    for (peer, env) in peers.iter().zip(&envs) {
        let k = usize::from(*peer == Peer::Rust);
        outs[k] += &peer
            .on("create", env)
            .args(["268435456", "64"])
            .run()?
            .stdout;
        for step in steps {
            outs[k] += &peer.on(step[0], env).args(&step[1..]).run()?.stdout;
        }
        outs[k] += &peer.on("stat", env).run()?.stdout;
    }
    assert!(
        outs[0] == outs[1],
        "outputs differ:\n--- C ---\n{}\n--- Rust ---\n{}",
        outs[0],
        outs[1]
    );
    // And both environments are valid for both.
    for env in &envs {
        for peer in peers {
            scenarios::check_freelist(peer, env)?;
        }
    }
    Ok(())
}

/// Bad command lines are usage errors (exit status 2) in both peers.
#[test]
fn usage_errors_match_c() -> interop::Result<()> {
    register();
    for args in [
        vec!["no-such-command"],
        vec!["verify", "/tmp"],
        vec!["verify", "/tmp", "-", "x", "1"],
        vec!["verify", "/tmp", "-", "10", "1", "--bogus"],
        vec!["verify", "/tmp", "-", "10", "1", "--valsize"],
        vec!["stat"],
        vec!["dump", "/tmp"],
        vec!["dump", "/tmp", "-", "--all"],
        vec!["hold-reader", "/tmp", "1", "-"],
        vec!["hold-reader", "/tmp", "-1"],
        vec!["gen", "1", "99999999999"],
    ] {
        let (c, r) = both(&args)?;
        assert_eq!(c.code(), Some(2), "C {args:?}: {c}");
        assert_eq!(r.code(), Some(2), "Rust {args:?}: {r}");
    }
    Ok(())
}

/// An environment with DBs of every kind the data set has, written by C.
fn varied_env() -> interop::Result<TestEnv> {
    let env = TestEnv::new()?;
    scenarios::create(Peer::C, &env, 256 << 20)?;
    let sets = [
        DataSet::new("plain", 700, 1),
        DataSet::new("dups", 300, 2).with(&["--dupsort", "--dups", "9"]),
        DataSet::new("subdb", 8, 3).with(&["--dupsort", "--dups", "1500", "--valsize", "40"]),
        DataSet::new("fixed", 30, 4).with(&["--dupfixed", "--dups", "1200", "--valsize", "8"]),
        DataSet::new("ints", 400, 5).with(&["--intkey", "--big", "20000"]),
        DataSet::new("rev", 300, 6).with(&["--revkey", "--revdup", "--dups", "4"]),
        DataSet::new("intdup", 50, 7).with(&["--intdup", "--valsize", "4", "--dups", "300"]),
    ];
    for ds in &sets {
        ds.fill(Peer::C, &env, &[])?;
        ds.delete(Peer::C, &env, 3, &[])?;
    }
    Ok(env)
}

/// `stat`, `dump --all`, `check-freelist` and `verify` print exactly what
/// the C peer prints.
#[test]
fn outputs_equal_c() -> interop::Result<()> {
    register();
    let env = varied_env()?;
    let mut cmds: Vec<Vec<String>> = vec![
        vec!["stat".into()],
        vec!["dump".into(), "--all".into()],
        vec!["dump".into(), "fixed".into()],
        vec!["check-freelist".into()],
    ];
    for (db, n, seed, flags) in [
        ("rev", "300", "6", "--revkey --revdup --dups 4"),
        ("fixed", "30", "4", "--dupfixed --dups 1200 --valsize 8"),
        ("ints", "400", "5", "--intkey --big 20000"),
    ] {
        let mut v: Vec<String> = vec!["verify".into(), db.into(), n.into(), seed.into()];
        v.extend(flags.split_whitespace().map(str::to_owned));
        v.extend(["--deleted-every".into(), "3".into()]);
        cmds.push(v);
    }
    for cmd in cmds {
        let c = Peer::C.on(&cmd[0], &env).args(&cmd[1..]).run()?;
        let r = Peer::Rust.on(&cmd[0], &env).args(&cmd[1..]).run()?;
        assert!(
            c.stdout == r.stdout,
            "`{}` differs:\n--- C ---\n{}\n--- Rust ---\n{}",
            cmd.join(" "),
            c.stdout,
            r.stdout
        );
    }
    Ok(())
}

#[test]
fn verify_rejects_wrong_data() -> interop::Result<()> {
    register();
    let (env, ds) = common::filled_env()?;
    ds.verify(Peer::Rust, &env)?;
    for wrong in common::wrong_data_sets() {
        let out = Peer::Rust.on("verify", &env).args(wrong.args()).output()?;
        assert_eq!(
            out.code(),
            Some(1),
            "verify {:?} should fail: {out}",
            wrong.args()
        );
        assert!(out.stderr.contains("rpeer: verify db=d"), "{out}");
    }
    // A DB that does not exist.
    let out = Peer::Rust
        .on("verify", &env)
        .args(["nope", "1", "1"])
        .output()?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stderr.contains("database not found"), "{out}");
    Ok(())
}

/// Any step back of `#commits` fails rpeer's reader loop (`rlmdb` readers
/// cannot see C's reader meta race).
#[test]
fn reader_loop_rejects_a_step_back() -> interop::Result<()> {
    register();
    let out = common::reader_loop_after_step_back(Peer::Rust)?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(
        out.stderr
            .contains("snapshot went back in time: #commits 2 after 4"),
        "{out}"
    );
    Ok(())
}

#[test]
fn check_freelist_rejects_a_leaked_page() -> interop::Result<()> {
    register();
    let (env, _) = common::filled_env()?;
    scenarios::check_freelist(Peer::Rust, &env)?;
    common::leak_a_page(&env)?;
    let c = Peer::C.on("check-freelist", &env).output()?;
    let r = Peer::Rust.on("check-freelist", &env).output()?;
    assert_eq!(r.code(), Some(1), "{r}");
    assert!(r.stderr.contains("is leaked"), "{r}");
    assert_eq!(c.stdout, r.stdout, "report differs from C's");
    assert_eq!(
        c.stderr.replace("cpeer: ", ""),
        r.stderr.replace("rpeer: ", "")
    );
    Ok(())
}

#[test]
fn check_freelist_rejects_a_page_used_twice() -> interop::Result<()> {
    register();
    let (env, _) = common::filled_env()?;
    common::use_a_page_twice(&env)?;
    let c = Peer::C.on("check-freelist", &env).output()?;
    let r = Peer::Rust.on("check-freelist", &env).output()?;
    assert_eq!(r.code(), Some(1), "{r}");
    assert!(r.stderr.contains("used twice"), "{r}");
    assert_eq!(c.stdout, r.stdout, "report differs from C's");
    assert_eq!(
        c.stderr.replace("cpeer: ", ""),
        r.stderr.replace("rpeer: ", "")
    );
    Ok(())
}

/// Database names are bytes, not necessarily UTF-8: a DB that C creates
/// with a non-UTF-8 name dumps and stats in rpeer exactly as in C.
#[test]
fn non_utf8_db_names_match_c() -> interop::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    register();
    let env = TestEnv::new()?;
    scenarios::create(Peer::C, &env, 64 << 20)?;
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9\xff");
    Peer::C
        .on("fill", &env)
        .arg(name)
        .args(["50", "3", "--dupsort"])
        .run()?;
    scenarios::DataSet::new("plain", 20, 4).fill(Peer::C, &env, &[])?;
    for cmd in [&["dump", "--all"][..], &["stat"]] {
        let c = Peer::C.on(cmd[0], &env).args(&cmd[1..]).run()?;
        let r = Peer::Rust.on(cmd[0], &env).args(&cmd[1..]).run()?;
        // The DB has its own section (dump) or line (stat).
        assert!(c.stdout.contains("db=caf"), "{c}");
        assert_eq!(c.stdout, r.stdout, "`{}` differs", cmd.join(" "));
    }
    Ok(())
}
