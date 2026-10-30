//! Scenarios with all three peers at once ([`Peer::ALL`]): C, Rust, and C
//! on the Rust library. They do not take a pair; `tests/three_peers.rs`
//! runs them.

use super::{audit, finish_reader_loop, loop_secs, new_env, Pair, MIB};
use crate::report::WriterLoopReport;
use crate::{ensure, Peer, Result};

/// Writer loops of all three implementations at once (one `--nosync`),
/// next to reader loops of all three: the writer lock serialises the
/// writers (`#commits` ends at exactly 1 + the sum of their commits), and
/// every reader sees consistent snapshots throughout. Audited by all three.
pub fn three_writers() -> Result<()> {
    let pair = Pair::new(Peer::C, Peer::Rust);
    let env = new_env(&pair, 512 * MIB)?;
    let secs = loop_secs();
    Peer::C
        .on("writer-loop", &env)
        .args(["three", "0", "1"])
        .run()?;
    let reader_secs = (secs + 0.5).to_string();
    let mut readers = Vec::new();
    for peer in Peer::ALL {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["three", reader_secs.as_str()])
            .spawn()?;
        p.wait_ready()?;
        readers.push((peer, p));
    }
    let mut writers = Vec::new();
    for (k, peer) in Peer::ALL.into_iter().enumerate() {
        let mut cmd =
            peer.on("writer-loop", &env)
                .args(["three", &secs.to_string(), &(40 + k).to_string()]);
        if k == 1 {
            cmd = cmd.arg("--nosync");
        }
        writers.push(cmd.spawn()?);
    }
    let mut reports = Vec::new();
    for w in writers {
        let out = w.finish()?.ok()?;
        reports.push(WriterLoopReport::parse(&out.stdout)?);
    }
    for (peer, p) in readers {
        finish_reader_loop(peer, p)?;
    }
    let total: u64 = reports.iter().map(|r| r.commits).sum();
    let last = reports.iter().map(|r| r.last_commit).max().unwrap_or(0);
    ensure!(
        reports.iter().all(|r| r.commits > 0),
        "a writer did not commit: {reports:?}"
    );
    ensure!(
        last == 1 + total,
        "#commits is {last}, but the writers committed 1 + {total}: {reports:?}"
    );
    for peer in Peer::ALL {
        peer.on("reader-loop", &env).args(["three", "0"]).run()?;
        audit(&Pair::new(Peer::C, peer), &env)?;
    }
    Ok(())
}
