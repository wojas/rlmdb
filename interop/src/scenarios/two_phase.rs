//! LMDB 1.0's two-phase commit across processes: `mdb_txn_prepare` in one
//! peer while the other reads, and `mdb_env_rollback`, in both formats.
//!
//! The peer that prepares and rolls back is the pair's writer if it can
//! (`prepare`, `rollback`): C 0.9 cannot (LMDB 0.9 has no two-phase
//! commit), so in a 0.9 pair whose writer is the C peer the reader does it
//! (with a SKIPPED note), and a pair of two C 0.9 peers skips the
//! scenario. In 1.0 pairs the C 1.0 peer prepares and rolls back for rlmdb
//! to read, and the other way round. Every step is checked by the other
//! peer of the pair and the reference C peer, and audited.

use super::{audit, check_freelist, new_env, skipped, stat, DataSet, Pair, MIB};
use crate::report::HoldReady;
use crate::{ensure, Peer, Result, TestEnv};

/// Whether `peer` sees the named database `db` in `env`.
fn has_db(peer: Peer, env: &TestEnv, db: &str) -> Result<bool> {
    Ok(stat(peer, env)?.dbs.iter().any(|d| d.name == db))
}

/// `peer`, and the reference C peer if it is another, see `env` with the
/// last committed transaction `txnid`, the data sets `present` and none
/// of the databases `absent`.
fn sees(
    pair: &Pair,
    peer: Peer,
    env: &TestEnv,
    txnid: u64,
    present: &[&DataSet],
    absent: &[&str],
) -> Result<()> {
    let mut peers = vec![peer];
    if peer != pair.reference() {
        peers.push(pair.reference());
    }
    for p in peers {
        let st = stat(p, env)?;
        ensure!(
            st.last_txnid == txnid,
            "{p} sees last txnid {}, expected {txnid}",
            st.last_txnid
        );
        for ds in present {
            ds.verify(p, env)?;
        }
        for db in absent {
            ensure!(!has_db(p, env, db)?, "{p} sees database {db}");
        }
    }
    Ok(())
}

/// The peer of `pair` that prepares and rolls back: the writer if it can,
/// else the reader; `None` if neither can.
fn two_phase_peer(pair: &Pair) -> Option<Peer> {
    for peer in pair.peers() {
        if peer.supports("prepare") && peer.supports("rollback") {
            if peer != pair.writer {
                skipped(format_args!(
                    "{} has no two-phase commit (LMDB 0.9): {peer} prepares and rolls back",
                    pair.writer
                ));
            }
            return Some(peer);
        }
    }
    None
}

/// Two-phase commit and rollback across processes:
///
/// 1. one peer prepares a transaction and waits between the phases: the
///    other peer and the reference C peer read the snapshot before it
///    (and the page audit passes: the prepared pages are not in it), then
///    it commits and they read the new one;
/// 2. a prepared transaction is aborted: nothing changes, the other peer
///    writes on;
/// 3. a process dies between the phases (killed, holding the writer
///    lock): the environment holds the snapshot before (the audit finds
///    no page lost), and the other peer writes on;
/// 4. a commit is rolled back: the other peer and the reference see the
///    snapshot before, the audit passes, and the other peer writes on it;
///    the same peer's next commit gets the rolled-back txnid again.
pub fn two_phase_commit(pair: Pair) -> Result<()> {
    let Some(tp) = two_phase_peer(&pair) else {
        skipped(format_args!(
            "scenario two_phase_commit ({pair}): no peer has two-phase commit (LMDB 0.9)"
        ));
        return Ok(());
    };
    let other = pair.other(tp);
    let env = new_env(&pair, 64 * MIB)?;
    let base = DataSet::new("base", 500, 1);
    base.fill(pair.writer, &env, &[])?;
    let t0 = stat(pair.reference(), &env)?.last_txnid;

    // 1. Prepare, read, commit.
    let two = DataSet::new("two", 400, 2).with(&["--big", "5000"]);
    let mut prepared = tp
        .on("prepare", &env)
        .args([super::hold_secs().as_str(), "--until-stdin"])
        .args(two.args())
        .spawn()?;
    let ready = HoldReady::parse(&prepared.wait_ready()?)?;
    ensure!(ready.txnid == t0 + 1, "prepared txnid {}", ready.txnid);
    sees(&pair, other, &env, t0, &[&base], &["two"])?;
    audit(&pair, &env)?;
    prepared.release()?;
    let out = prepared.finish()?.ok()?;
    ensure!(
        out.stdout.contains(&format!("committed txnid={}", t0 + 1)),
        "{tp} prepare:\n{out}"
    );
    sees(&pair, other, &env, t0 + 1, &[&base, &two], &[])?;
    audit(&pair, &env)?;

    // 2. Prepare, then abort.
    let three = DataSet::new("three", 300, 3);
    let mut aborted = tp
        .on("prepare", &env)
        .args([super::hold_secs().as_str(), "--until-stdin", "--abort"])
        .args(three.args())
        .spawn()?;
    aborted.wait_ready()?;
    aborted.release()?;
    let out = aborted.finish()?.ok()?;
    ensure!(
        out.stdout.contains("aborted txnid="),
        "{tp} prepare --abort:\n{out}"
    );
    sees(&pair, other, &env, t0 + 1, &[&base, &two], &["three"])?;
    audit(&pair, &env)?;
    let four = DataSet::new("four", 200, 4);
    four.fill(other, &env, &[])?;
    sees(
        &pair,
        other,
        &env,
        t0 + 2,
        &[&base, &two, &four],
        &["three"],
    )?;

    // 3. A process dies between the phases. Nobody else has the
    // environment open, so the next opener re-initialises the lock file
    // (and with it a writer lock that is not robust, see killed_writer).
    let five = DataSet::new("five", 300, 5);
    let mut dying = tp
        .on("prepare", &env)
        .args([super::hold_secs().as_str(), "--until-stdin"])
        .args(five.args())
        .spawn()?;
    dying.wait_ready()?;
    dying.kill()?;
    sees(&pair, other, &env, t0 + 2, &[&base, &two, &four], &["five"])?;
    let report = check_freelist(pair.reference(), &env)?;
    super::log(format_args!(
        "audit after a death between the phases: {report:?}"
    ));
    audit(&pair, &env)?;
    let six = DataSet::new("six", 200, 6);
    six.fill(other, &env, &[])?;
    sees(
        &pair,
        other,
        &env,
        t0 + 3,
        &[&base, &two, &four, &six],
        &["five"],
    )?;
    audit(&pair, &env)?;

    // 4. A commit rolled back.
    let seven = DataSet::new("seven", 300, 7).with(&["--big", "5000"]);
    seven.fill(tp, &env, &[])?;
    let out = tp.on("rollback", &env).arg("last").run()?;
    ensure!(
        out.stdout
            .contains(&format!("rolled-back txnid={}", t0 + 4)),
        "{tp} rollback:\n{out}"
    );
    sees(
        &pair,
        other,
        &env,
        t0 + 3,
        &[&base, &two, &four, &six],
        &["seven"],
    )?;
    audit(&pair, &env)?;
    // A second rollback in a row is refused by rlmdb (C 1.0 refuses it
    // after an even txnid only, KNOWN_C_BUGS.md row 62: not asked of it).
    if matches!(tp, Peer::Rust | Peer::COnRust) {
        let again = tp.on("rollback", &env).arg("last").output()?;
        ensure!(
            again.code() == Some(3) && again.stderr.contains("-30774"),
            "{tp}: a second rollback in a row:\n{again}"
        );
    }
    let eight = DataSet::new("eight", 200, 8);
    eight.fill(other, &env, &[])?;
    sees(
        &pair,
        other,
        &env,
        t0 + 4,
        &[&base, &two, &four, &six, &eight],
        &["seven"],
    )?;
    audit(&pair, &env)?;
    Ok(())
}
