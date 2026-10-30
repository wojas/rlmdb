//! Scenarios with environments of both formats open at once: one rlmdb
//! process with an LMDB 0.9 and an LMDB 1.0 environment (`rpeer
//! formats-loop`), next to the C peer of each release on its own
//! environment. `tests/mixed_formats.rs` runs them.
//!
//! What can go wrong is shared state in the process: the per-format page
//! layouts and lock-file descriptors, the registry of open environments,
//! the semaphores and mutexes of two lock-file versions, the default
//! format. Each environment must still look to its C peer exactly as one
//! of its own release.

use super::{audit, create, finish_reader_loop, log, loop_secs, Pair, MIB};
use crate::report::{ReaderLoopReport, WriterLoopReport};
use crate::{ensure, Error, Format, Peer, Result, TestEnv};

/// The line of `rpeer formats-loop`'s output that starts with `prefix`
/// (`writer format=0.9`, `reader format=1.0`, ...).
fn line_of<'a>(stdout: &'a str, prefix: &str) -> Result<&'a str> {
    stdout
        .lines()
        .find(|l| l.starts_with(prefix))
        .ok_or_else(|| {
            Error::new(format!(
                "formats-loop printed no `{prefix}` line:\n{stdout}"
            ))
        })
}

/// Both formats in one process. `rpeer formats-loop` keeps a 0.9 and a 1.0
/// environment open and runs a writer loop on each (one thread each) while
/// its main thread checks snapshots of both; at the same time C 0.9 runs a
/// writer loop on the 0.9 environment and C 1.0 one on the 1.0
/// environment, with reader loops of the C peer and of C on rlmdb on each.
/// Per environment the writer lock serialises the writers (`#commits` ends
/// at exactly 1 + the sum of their commits), every reader sees consistent
/// snapshots, the file keeps its format and passes the audit of its C
/// release and of rlmdb.
pub fn formats_in_one_process() -> Result<()> {
    let envs = [
        TestEnv::new()?.with_format(Format::V09),
        TestEnv::new()?.with_format(Format::V10),
    ];
    let secs = loop_secs();
    for env in &envs {
        let c = Peer::reference(env.format());
        create(c, env, 512 * MIB)?;
        c.on("writer-loop", env).args(["mix", "0", "1"]).run()?;
    }

    let reader_secs = (secs + 0.5).to_string();
    let mut loops = Vec::new();
    for env in &envs {
        for peer in [Peer::reference(env.format()), Peer::COnRust] {
            let mut p = peer
                .on("reader-loop", env)
                .args(["mix", reader_secs.as_str()])
                .spawn()?;
            p.wait_ready()?;
            loops.push((peer, p));
        }
    }
    let mut rust = Peer::Rust
        .cmd("formats-loop")
        .arg(envs[0].path())
        .arg(envs[1].path())
        .args(["mix", &secs.to_string(), "21"])
        .spawn()?;
    rust.wait_ready()?;
    let c_writers = envs
        .iter()
        .zip(22..)
        .map(|(env, seed)| {
            Peer::reference(env.format())
                .on("writer-loop", env)
                .args(["mix", &secs.to_string(), &seed.to_string()])
                .spawn()
        })
        .collect::<Result<Vec<_>>>()?;
    let c_reports = c_writers
        .into_iter()
        .map(|w| WriterLoopReport::parse(&w.finish()?.ok()?.stdout))
        .collect::<Result<Vec<_>>>()?;
    let rust = rust.finish()?.ok()?;
    for (peer, p) in loops {
        finish_reader_loop(peer, p)?;
    }

    for (env, c) in envs.iter().zip(&c_reports) {
        let f = env.format().name();
        let w = WriterLoopReport::parse(line_of(&rust.stdout, &format!("writer format={f}"))?)?;
        let r = ReaderLoopReport::parse(line_of(&rust.stdout, &format!("reader format={f}"))?)?;
        log(format_args!(
            "format {f}: C writer {c:?}, rpeer writer {w:?}, rpeer reader {r:?}"
        ));
        ensure!(
            c.commits > 0 && w.commits > 0 && r.reads > 1,
            "format {f}: a loop did nothing: C {c:?}, rpeer {w:?} {r:?}"
        );
        let last = c.last_commit.max(w.last_commit);
        ensure!(
            last == 1 + c.commits + w.commits,
            "format {f}: #commits is {last}, but the writers committed 1 + {} + {}",
            c.commits,
            w.commits
        );
        // The file is still in its format, for rlmdb and for C.
        let sniffed = rlmdb::sniff_format(env.path(), rlmdb::EnvironmentFlags::empty())
            .map_err(|e| Error::new(format!("sniffing {}: {e}", env.path().display())))?;
        ensure!(
            sniffed == Some(env.format()),
            "format {f}: the file is now {sniffed:?}"
        );
        let c = Peer::reference(env.format());
        for peer in [c, Peer::Rust, Peer::COnRust] {
            peer.on("reader-loop", env).args(["mix", "0"]).run()?;
        }
        audit(&Pair::with_format(c, Peer::Rust, env.format()), env)?;
        audit(&Pair::with_format(c, Peer::COnRust, env.format()), env)?;
    }
    Ok(())
}
