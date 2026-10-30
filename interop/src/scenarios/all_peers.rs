//! Scenarios with all three peers of a format at once ([`Peer::all`]): the
//! C peer of the format's release, Rust, and C on the Rust library. They
//! take the format, not a pair; `tests/three_peers.rs` (LMDB 0.9) and
//! `tests/three_peers10.rs` (LMDB 1.0) run them.

use super::{audit, finish_reader_loop, loop_secs, new_env, Pair, MIB};
use crate::report::WriterLoopReport;
use crate::{ensure, Format, Peer, Result};

/// Writer loops of all three implementations at once (one `--nosync`),
/// next to reader loops of all three, on an environment of `format`: the
/// writer lock serialises the writers (`#commits` ends at exactly 1 + the
/// sum of their commits), and every reader sees consistent snapshots
/// throughout. Audited by all three.
pub fn three_writers(format: Format) -> Result<()> {
    let reference = Peer::reference(format);
    let peers = Peer::all(format);
    let pair = Pair::with_format(reference, Peer::Rust, format);
    let env = new_env(&pair, 512 * MIB)?;
    let secs = loop_secs();
    reference
        .on("writer-loop", &env)
        .args(["three", "0", "1"])
        .run()?;
    let reader_secs = (secs + 0.5).to_string();
    let mut readers = Vec::new();
    for peer in peers {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["three", reader_secs.as_str()])
            .spawn()?;
        p.wait_ready()?;
        readers.push((peer, p));
    }
    let mut writers = Vec::new();
    for (k, peer) in peers.into_iter().enumerate() {
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
    for peer in peers {
        peer.on("reader-loop", &env).args(["three", "0"]).run()?;
        audit(&Pair::with_format(reference, peer, format), &env)?;
    }
    Ok(())
}
