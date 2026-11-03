//! An external peer ([`interop::Peer::External`]) on its own terms: it
//! knows the protocol's commands, its generator and its outputs on a C
//! environment are byte-identical to the C peer's; and writer loops of all
//! peers of a format at once, the external peer included.
//!
//! Skipped (`[interop] SKIPPED`) unless `INTEROP_PEER` names the peer's
//! executable (README.md, "External peers").

use interop::scenarios::{self, all_peers, DataSet};
use interop::{Format, Peer, TestEnv};

fn register() {
    interop::register_rpeer(option_env!("CARGO_BIN_EXE_rpeer"));
}

/// Runs `test` unless the external peer cannot run on `format` here (reported
/// as skipped); fails the test, or reports it as skipped if a peer lacks a
/// command.
fn run(name: &str, format: Format, test: impl FnOnce() -> interop::Result<()>) {
    register();
    if let Some(why) = Peer::External.unavailable(format) {
        interop::skipped(format_args!("{name}: {why}"));
        return;
    }
    match test() {
        Ok(()) => {}
        Err(e) if e.is_not_implemented() => interop::skipped(format_args!(
            "{name}: {}",
            e.message().lines().next().unwrap_or_default()
        )),
        Err(e) => panic!("{name} failed: {e}"),
    }
}

/// `<peer> commands` lists every command the C peer implements, and no
/// command the C peer does not know; the commands it lists as not
/// implemented where C implements them are reported as skipped (the
/// scenarios that need them run them on another peer or are skipped). (C
/// 0.9 lists `prepare` and `rollback` as not implemented, LMDB 0.9 having
/// no two-phase commit: a peer of a 0.9 implementation may leave them out.)
#[test]
fn external_peer_knows_every_command() {
    run("external_peer_knows_every_command", Format::V09, || {
        let commands = |peer: Peer| -> interop::Result<Vec<(String, bool)>> {
            let out = peer.cmd("commands").run()?;
            let mut v: Vec<(String, bool)> = out
                .lines()
                .filter_map(|line| {
                    let kv = interop::report::Kv::from_lines([line]);
                    Some((kv.get("command")?.to_owned(), kv.get("implemented")? == "1"))
                })
                .collect();
            v.sort();
            Ok(v)
        };
        let c = commands(Peer::C)?;
        let external = commands(Peer::External)?;
        for (name, _) in &external {
            assert!(
                c.iter().any(|(n, _)| n == name),
                "{} knows `{name}`, which the C peer does not",
                Peer::External
            );
        }
        for (name, in_c) in c.iter().filter(|(_, in_c)| *in_c) {
            match external.iter().find(|(n, _)| n == name) {
                None => panic!("{} does not know `{name}`", Peer::External),
                Some((_, false)) if *in_c => interop::skipped(format_args!(
                    "{} does not implement `{name}`",
                    Peer::External
                )),
                Some(_) => {}
            }
        }
        Ok(())
    });
}

/// The generator is byte-identical to `c/dataset.c` (`gen`, including the
/// usage errors of impossible combinations).
#[test]
fn external_peer_gen_matches_c() {
    run("external_peer_gen_matches_c", Format::V09, || {
        for flags in [
            "",
            "--intkey",
            "--revkey",
            "--valsize 3",
            "--big 9000",
            "--dupsort --dups 40 --valsize 4",
            "--revdup --dups 12",
            "--dupfixed --dups 9 --valsize 13",
            "--intdup --valsize 4 --dups 30",
            "--intdup --valsize 6",
            "--dups 3",
        ] {
            for seed in ["0", "77", "4294967295"] {
                for i in ["0", "5", "4000000000", "4294967295"] {
                    let mut args = vec![seed, i];
                    args.extend(flags.split_whitespace());
                    let c = Peer::C.cmd("gen").args(&args).output()?;
                    let ext = Peer::External.cmd("gen").args(&args).output()?;
                    assert_eq!(c.code(), ext.code(), "gen {args:?}:\n{c}\n{ext}");
                    assert_eq!(c.stdout, ext.stdout, "gen {args:?}");
                }
            }
        }
        Ok(())
    });
}

/// On an environment C wrote (every kind of DB, deletions, free pages),
/// `verify`, `dump --all`, `stat` and `check-freelist` of the external peer
/// print exactly what the C peer prints.
#[test]
fn external_peer_reads_like_c() {
    run("external_peer_reads_like_c", Format::V09, || {
        let env = TestEnv::new()?;
        scenarios::create(Peer::C, &env, 256 << 20)?;
        let sets = [
            DataSet::new("plain", 3000, 11).with(&["--big", "20000"]),
            DataSet::new("dups", 500, 3).with(&["--dupsort", "--dups", "9"]),
            DataSet::new("sub", 20, 4).with(&["--dupsort", "--dups", "900", "--valsize", "40"]),
            DataSet::new("fixed", 20, 5).with(&["--dupfixed", "--dups", "900"]),
            DataSet::new("ints", 800, 6).with(&["--intkey", "--intdup", "--dups", "7"]),
            DataSet::new("rev", 700, 10).with(&["--revkey", "--revdup", "--dups", "3"]),
        ];
        for ds in &sets {
            ds.fill(Peer::C, &env, &[])?;
            ds.delete(Peer::C, &env, 3, &[])?;
        }
        let mut commands: Vec<Vec<String>> = vec![
            vec!["dump".into(), "--all".into()],
            vec!["stat".into()],
            vec!["check-freelist".into()],
        ];
        for ds in &sets {
            let mut v = vec!["verify".to_owned()];
            v.extend(ds.after_delete(3).args());
            commands.push(v);
        }
        for args in &commands {
            let c = Peer::C.on(&args[0], &env).args(&args[1..]).run()?;
            let ext = Peer::External.on(&args[0], &env).args(&args[1..]).run()?;
            assert_eq!(c.stdout, ext.stdout, "{args:?}:\n{c}\n{ext}");
        }
        Ok(())
    });
}

/// Writer loops of all peers of LMDB 0.9 at once, the external peer
/// included, next to reader loops of all of them (`all_peers::writers`).
#[test]
fn four_writers() {
    let peers = [Peer::C, Peer::Rust, Peer::COnRust, Peer::External];
    run("four_writers", Format::V09, || {
        all_peers::writers(Format::V09, &peers)
    });
}

/// The same on LMDB 1.0, with C 1.0 (needs an external peer that opens 1.0).
#[test]
fn four_writers10() {
    let peers = [Peer::C10, Peer::Rust, Peer::COnRust, Peer::External];
    run("four_writers10", Format::V10, || {
        all_peers::writers(Format::V10, &peers)
    });
}
