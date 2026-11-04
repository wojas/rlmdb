//! Scenarios about the reader table and the lock file: reader slots seen
//! across processes and implementations, stale slots of killed readers and
//! who clears them, the lock file created by the reader peer, reader loops
//! next to nested transactions, `MDB_NOTLS` readers, and
//! `MDB_PREVSNAPSHOT`, whose writer holds the lock file exclusively until
//! its first commit.

use std::fmt;
use std::time::Duration;

use super::{
    audit, finish_reader_loop, hold_secs, log, loop_secs, new_env, reader_check, readers, stat,
    DataSet, Pair, MIB,
};
use crate::report::{HoldReady, WriterLoopReport};
use crate::{ensure, tools, Error, Format, Peer, Result, TestEnv};
use rlmdb::format::lockfile::{LockObjects, LOCK_V09, LOCK_V10};

/// Reader slots are visible across processes and implementations; the
/// lock table works whoever opened it first.
pub fn readers_list(pair: Pair) -> Result<()> {
    // The writer created and filled the env; the reader peer holds a snapshot.
    let env = new_env(&pair, 64 * MIB)?;
    let ds = DataSet::new("r", 100, 30);
    ds.fill(pair.writer, &env, &[])?;
    expect_holder_visible(pair.reader, pair.writer, &env, Some(&ds))?;

    // The reader peer is the first opener of the lock file (nobody has it
    // open when it starts), the writer joins while the snapshot is held.
    let env = new_env(&pair, 64 * MIB)?;
    let mut holder = pair
        .reader
        .on("hold-reader", &env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    ds.fill(pair.writer, &env, &[])?;
    let table = readers(pair.writer, &env)?;
    ensure!(
        table
            .of_pid(holder.pid())
            .iter()
            .any(|s| s.txnid == Some(ready.txnid)),
        "{} does not see the first opener's reader: {table:?}",
        pair.writer
    );
    holder.release()?;
    holder.finish()?.ok()?;
    ds.verify(pair.reader, &env)?;

    // And the other way around.
    expect_holder_visible(pair.writer, pair.reader, &env, None)?;
    Ok(())
}

/// `holder` pins a snapshot; `lister` and `mdb_stat -r` must list it; after
/// the holder exits its slot is free.
fn expect_holder_visible(
    holder_peer: Peer,
    lister: Peer,
    env: &TestEnv,
    ds: Option<&DataSet>,
) -> Result<()> {
    let mut holder = holder_peer
        .on("hold-reader", env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .args(ds.map(DataSet::args).unwrap_or_default())
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    ensure!(
        i64::from(ready.pid) == i64::from(holder.pid()),
        "hold-reader says pid {}, spawned {}",
        ready.pid,
        holder.pid()
    );
    let table = readers(lister, env)?;
    ensure!(
        table
            .of_pid(holder.pid())
            .iter()
            .any(|s| s.txnid == Some(ready.txnid)),
        "{lister} does not list holder {} at txnid {}: {table:?}",
        holder.pid(),
        ready.txnid
    );
    let ms = tools::mdb_stat(env, &["-r"])?;
    let listed = ms.readers.unwrap_or_default();
    ensure!(
        listed
            .iter()
            .any(|r| i64::from(r.pid) == i64::from(holder.pid()) && r.txnid == Some(ready.txnid)),
        "mdb_stat -r does not list holder {}: {listed:?}",
        holder.pid()
    );
    let pid = holder.pid();
    holder.release()?;
    let out = holder.finish()?.ok()?;
    if ds.is_some() {
        ensure!(
            out.stdout.contains("verified"),
            "holder did not verify:\n{out}"
        );
    }
    let table = readers(lister, env)?;
    ensure!(
        table.of_pid(pid).is_empty(),
        "slot of exited holder {pid} still in use: {table:?}"
    );
    Ok(())
}

/// Who clears a dead reader's slot in [`stale_reader`].
#[derive(Clone, Copy, Debug)]
enum Clearer {
    /// `reader-check` (`mdb_reader_check`) by a peer.
    Peer(Peer),
    /// The upstream `mdb_stat -rr`.
    MdbStat,
}

impl fmt::Display for Clearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Clearer::Peer(p) => write!(f, "{p} reader-check"),
            Clearer::MdbStat => f.write_str("mdb_stat -rr"),
        }
    }
}

impl Clearer {
    /// Run the reader check; the number of slots cleared.
    fn clear(self, env: &TestEnv) -> Result<u64> {
        match self {
            Clearer::Peer(p) => reader_check(p, env),
            Clearer::MdbStat => tools::mdb_stat(env, &["-rr"])?
                .stale_cleared
                .ok_or_else(|| Error::new("mdb_stat -rr printed no stale reader count")),
        }
    }

    /// Whether the check counts a process as alive while a handle to it is
    /// open: C's `mdb_reader_check` on Windows (`KNOWN_C_BUGS.md` row 42), in
    /// the C peers and C's `mdb_stat`. rlmdb's check (the Rust peer, the C
    /// peer on rlmdb) sees that the process has exited.
    fn needs_handles_closed(self) -> bool {
        cfg!(windows) && matches!(self, Clearer::MdbStat | Clearer::Peer(Peer::C | Peer::C10))
    }
}

/// Delete `old` entirely and fill `new` (the writer); returns how many pages
/// the data file grew (`last_pgno`).
fn rewrite(pair: &Pair, env: &TestEnv, old: &DataSet, new: &DataSet) -> Result<u64> {
    let before = stat(pair.reader, env)?.last_pgno;
    old.delete(pair.writer, env, 1, &[])?;
    new.fill(pair.writer, env, &[])?;
    Ok(stat(pair.reader, env)?.last_pgno - before)
}

/// A reader killed with SIGKILL keeps its slot, which pins its snapshot:
/// rewriting the data cannot reuse freed pages and the file grows. A reader
/// check from another process must leave live readers alone, clear the dead
/// one, and then the writer reuses the free pages again.
///
/// On Windows the harness keeps a handle to the dead reader until the check
/// ([`PeerProcess::kill_keeping_pid`](crate::PeerProcess::kill_keeping_pid)),
/// so that no new process gets its id. C's check sees it as alive while
/// that handle is open (`KNOWN_C_BUGS.md` row 42), and clears it once the
/// handle is closed; rlmdb's clears it at once.
fn stale_reader(pair: &Pair, holder: Peer, clearer: Clearer) -> Result<()> {
    let env = new_env(pair, 512 * MIB)?;
    let sets: Vec<DataSet> = (0..3)
        .map(|k| DataSet::new("data", 3000, 41 + k).with(&["--valsize", "400"]))
        .collect();
    sets[0].fill(pair.writer, &env, &[])?;

    // The other peer keeps the environment open throughout, without a
    // snapshot: if nobody had it open, the next process to open it would be
    // exclusive and reset the reader table, stale slot and all.
    let keeper_peer = if holder == pair.writer {
        pair.reader
    } else {
        pair.writer
    };
    let mut keeper = keeper_peer
        .on("hold-env", &env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .spawn()?;
    keeper.wait_ready()?;

    let mut h = holder
        .on("hold-reader", &env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .spawn()?;
    let ready = HoldReady::parse(&h.wait_ready()?)?;
    let pid = h.pid();
    let cleared = clearer.clear(&env)?;
    ensure!(
        cleared == 0,
        "{clearer} cleared {cleared} slots while {holder}'s reader {pid} was alive"
    );
    let dead = h.kill_keeping_pid()?;
    for peer in pair.peers() {
        let table = readers(peer, &env)?;
        ensure!(
            table
                .of_pid(pid)
                .iter()
                .any(|s| s.txnid == Some(ready.txnid)),
            "{peer} does not list the dead reader {pid} at txnid {}: {table:?}",
            ready.txnid
        );
    }

    let grown_pinned = rewrite(pair, &env, &sets[0], &sets[1])?;
    ensure!(
        keeper.running()?,
        "{keeper_peer} hold-env exited before the reader check: the next opener \
         was exclusive and reset the reader table"
    );
    let cleared = if clearer.needs_handles_closed() {
        let cleared = clearer.clear(&env)?;
        ensure!(
            cleared == 0,
            "{clearer} cleared {cleared} slots while a handle to the dead {holder} \
             reader {pid} was open (C counts that as alive, KNOWN_C_BUGS.md row 42)"
        );
        dead.close();
        clearer.clear(&env)?
    } else {
        let cleared = clearer.clear(&env)?;
        dead.close();
        cleared
    };
    ensure!(
        cleared == 1,
        "{clearer} cleared {cleared} slots, expected the dead {holder} reader {pid} \
         (process {pid} now: {:?})",
        crate::os::process_name(pid)
    );
    for peer in pair.peers() {
        let table = readers(peer, &env)?;
        ensure!(
            table.of_pid(pid).is_empty(),
            "{peer} still lists the cleared reader {pid}: {table:?}"
        );
    }
    let grown_free = rewrite(pair, &env, &sets[1], &sets[2])?;
    log(format_args!(
        "rewrite grew the file by {grown_pinned} pages with the stale {holder} reader, \
         by {grown_free} after {clearer} cleared it"
    ));
    ensure!(
        grown_free * 4 <= grown_pinned,
        "after {clearer} the writer still does not reuse free pages: grew {grown_free} pages \
         (with the stale reader: {grown_pinned})"
    );
    for peer in pair.peers() {
        sets[2].verify(peer, &env)?;
    }
    audit(pair, &env)?;
    keeper.release()?;
    keeper.finish()?.ok()?;
    Ok(())
}

/// The writer's reader dies; the reader peer's `reader-check` clears it.
pub fn stale_reader_cleared_by_reader(pair: Pair) -> Result<()> {
    stale_reader(&pair, pair.writer, Clearer::Peer(pair.reader))
}

/// The reader peer's reader dies; C's `mdb_stat -rr` clears it.
pub fn stale_reader_cleared_by_mdb_stat(pair: Pair) -> Result<()> {
    stale_reader(&pair, pair.reader, Clearer::MdbStat)
}

/// The reader peer is the first to open an environment whose lock file was
/// removed: it creates and initialises the lock file (reader table,
/// semaphores or mutexes), and the writer does all the writing against that
/// table. The lock file has C's size; reader loops of both peers, a writer
/// loop and a held snapshot interoperate.
pub fn reader_creates_lock_file(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 1024 * MIB)?;
    let ds = DataSet::new("data", 1500, 51).with(&["--dupsort", "--dups", "4"]);
    ds.fill(pair.writer, &env, &[])?;
    pair.writer
        .on("writer-loop", &env)
        .args(["loop", "0", "1"])
        .run()?;
    std::fs::remove_file(env.lock_file())?;

    let mut holder = pair
        .reader
        .on("hold-reader", &env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .args(ds.args())
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    let layout = tools::c_layout_for(pair.format)?;
    let want = layout.get("offsetof(MDB_txninfo.mti_readers)")?
        + 126 * layout.get("sizeof(MDB_reader)")?;
    let len = std::fs::metadata(env.lock_file())?.len();
    ensure!(
        len == want,
        "lock file created by {} is {len} bytes, C makes {want}",
        pair.reader
    );

    let secs = loop_secs();
    let reader_secs = (secs + 0.5).to_string();
    let mut loops = Vec::new();
    for peer in pair.peers() {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["loop", reader_secs.as_str()])
            .spawn()?;
        p.wait_ready()?;
        loops.push((peer, p));
    }
    let wl = pair
        .writer
        .on("writer-loop", &env)
        // The held snapshot keeps every page the loop frees from reuse:
        // bound the commits so the map cannot fill.
        .args([
            "loop",
            &secs.to_string(),
            "52",
            "--nosync",
            "--max-commits",
            "1500",
        ])
        .run()?;
    let wl = WriterLoopReport::parse(&wl.stdout)?;
    ensure!(wl.commits > 0, "the writer did not commit: {wl:?}");
    log(format_args!(
        "writer: {wl:?}; last_pgno {} with the snapshot pinned",
        stat(pair.writer, &env)?.last_pgno
    ));
    ds.delete(pair.writer, &env, 2, &[])?;
    for (peer, p) in loops {
        finish_reader_loop(peer, p)?;
    }

    let table = readers(pair.writer, &env)?;
    ensure!(
        table
            .of_pid(holder.pid())
            .iter()
            .any(|s| s.txnid == Some(ready.txnid)),
        "{} does not see the lock file creator's reader: {table:?}",
        pair.writer
    );
    holder.release()?;
    let out = holder.finish()?.ok()?;
    ensure!(
        out.stdout.contains("verified db=data"),
        "held snapshot did not verify:\n{out}"
    );
    for peer in pair.peers() {
        ds.after_delete(2).verify(peer, &env)?;
    }
    audit(&pair, &env)?;
    Ok(())
}

/// Reader loops of both peers check snapshot after snapshot while the
/// writer runs `nested-test` (twice) and a writer loop at the same time.
pub fn reader_loop_during_nested(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 512 * MIB)?;
    pair.writer
        .on("writer-loop", &env)
        .args(["loop", "0", "1"])
        .run()?;
    let secs = loop_secs();
    let reader_secs = (secs + 1.0).to_string();
    let mut loops = Vec::new();
    for peer in pair.peers() {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["loop", reader_secs.as_str()])
            .spawn()?;
        p.wait_ready()?;
        loops.push((peer, p));
    }
    let wl = pair
        .writer
        .on("writer-loop", &env)
        .args(["loop", &secs.to_string(), "31"])
        .spawn()?;
    for _ in 0..2 {
        pair.writer.on("nested-test", &env).run()?;
    }
    let wl = WriterLoopReport::parse(&wl.finish()?.ok()?.stdout)?;
    for (peer, p) in loops {
        finish_reader_loop(peer, p)?;
    }
    log(format_args!("writer: {wl:?}"));
    for peer in pair.peers() {
        peer.on("nested-test", &env).arg("--check").run()?;
    }
    audit(&pair, &env)?;
    Ok(())
}

/// `MDB_NOTLS` readers (`--notls` on every reader command; C then ties
/// reader slots to transactions instead of threads): a held snapshot,
/// reader loops with reset/renew, listing and verify across peers while the
/// writer loops.
pub fn notls_readers(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 1024 * MIB)?;
    let ds = DataSet::new("data", 1000, 61).with(&["--intkey"]);
    ds.fill(pair.writer, &env, &[])?;
    pair.writer
        .on("writer-loop", &env)
        .args(["loop", "0", "1"])
        .run()?;

    let mut holder = pair
        .reader
        .on("hold-reader", &env)
        .args([hold_secs().as_str(), "--until-stdin", "--notls"])
        .args(ds.args())
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    let secs = loop_secs();
    let reader_secs = (secs + 0.5).to_string();
    let mut loops = Vec::new();
    for peer in pair.peers() {
        let mut p = peer
            .on("reader-loop", &env)
            .args(["loop", reader_secs.as_str(), "--notls"])
            .spawn()?;
        p.wait_ready()?;
        loops.push((peer, p));
    }
    // Bounded: the held snapshot keeps freed pages from reuse.
    pair.writer
        .on("writer-loop", &env)
        .args(["loop", &secs.to_string(), "62", "--max-commits", "1500"])
        .run()?;
    let table = readers(pair.writer, &env)?;
    ensure!(
        table
            .of_pid(holder.pid())
            .iter()
            .any(|s| s.txnid == Some(ready.txnid)),
        "{} does not list the --notls holder: {table:?}",
        pair.writer
    );
    for (peer, p) in loops {
        finish_reader_loop(peer, p)?;
    }
    holder.release()?;
    let out = holder.finish()?.ok()?;
    ensure!(
        out.stdout.contains("verified db=data"),
        "held snapshot did not verify:\n{out}"
    );
    for peer in pair.peers() {
        peer.on("verify", &env)
            .args(ds.args())
            .arg("--notls")
            .run()?;
    }
    audit(&pair, &env)?;
    Ok(())
}

/// `MDB_PREVSNAPSHOT` (LMDB 1.0's open flag; rlmdb takes it for 0.9
/// environments too) across processes, run by the peers of the pair that
/// have it ([`Peer::opens_prev_snapshot`]: not `cpeer`, LMDB 0.9):
///
/// 1. The writer fills a data set to 300 keys, then to 600 in one more
///    transaction: the previous snapshot is the 300-key one.
/// 2. A peer alone opens it read-only with the flag and verifies 300 keys.
/// 3. While the other peer keeps the environment open (`hold-env`), a
///    writer with the flag gets `MDB_ENV_BUSY`. A read-only peer with the
///    flag still verifies 300 keys, and every verifier then finds the 600:
///    the lock file still names the latest commit. (C 1.0's read-only
///    handle publishes the previous snapshot there, which takes the latest
///    commit out of the other processes' view and lets their next writer
///    overwrite it, `KNOWN_C_BUGS.md` row 68: not run with `cpeer10` as that
///    peer.)
/// 4. With the environment to itself, a writer with the flag fills the
///    data set to 450 keys in one transaction, on top of the 300: the
///    600-key commit is gone. Every verifier finds 450 keys.
/// 5. A read-only peer holds the environment with the flag, alone (so it
///    keeps the exclusive lock until it closes), while the other peer's
///    writer waits to open it; that writer then commits on top of the
///    latest snapshot, which the holder published when it closed: it
///    deletes every second of the 450 keys. (C 1.0 leaves the previous
///    snapshot in the lock file, and the waiting writer overwrites the
///    latest one, `KNOWN_C_BUGS.md` row 70: not run with `cpeer10` as the
///    holder.) Where the shared locks die with their last user (not
///    Linux or FreeBSD), the holder's peer waits instead of a C peer,
///    whose open fails when the holder closes (row 47). The auditors audit
///    the file.
pub fn prev_snapshot(pair: Pair) -> Result<()> {
    let first = |peers: [Peer; 2]| peers.into_iter().find(Peer::opens_prev_snapshot);
    let (Some(reader), Some(writer)) = (
        first([pair.reader, pair.writer]),
        first([pair.writer, pair.reader]),
    ) else {
        crate::peer::skipped(format_args!(
            "prev_snapshot: neither {} nor {} opens environments with MDB_PREVSNAPSHOT",
            pair.writer, pair.reader
        ));
        return Ok(());
    };
    let env = new_env(&pair, 64 * MIB)?;
    let ds = |n| DataSet::new("-", n, 77);
    ds(300).fill(pair.writer, &env, &[])?;
    ds(600).fill(pair.writer, &env, &[])?;
    let verify_prev = |peer: Peer, n| {
        peer.on("verify", &env)
            .args(ds(n).args())
            .arg("--prevsnapshot")
            .run()
    };
    verify_prev(reader, 300)?;

    let keeper_peer = pair.other(reader);
    let mut keeper = keeper_peer
        .on("hold-env", &env)
        .args([hold_secs().as_str(), "--until-stdin"])
        .spawn()?;
    keeper.wait_ready()?;
    let busy = writer
        .on("fill", &env)
        .args(ds(450).args())
        .arg("--prevsnapshot")
        .output()?;
    ensure!(
        !busy.success() && busy.stderr.contains("MDB_ENV_BUSY"),
        "a writer with MDB_PREVSNAPSHOT next to {keeper_peer}'s hold-env: {busy}"
    );
    if reader == Peer::C10 {
        log(
            "prev_snapshot: cpeer10 does not open read-only with MDB_PREVSNAPSHOT next to \
             another process (it would publish the previous snapshot, KNOWN_C_BUGS.md row 68)",
        );
    } else {
        verify_prev(reader, 300)?;
        for peer in pair.verifiers() {
            ds(600).verify(peer, &env)?;
        }
    }
    keeper.release()?;
    keeper.finish()?.ok()?;

    ds(450).fill(writer, &env, &["--prevsnapshot"])?;
    for peer in pair.verifiers() {
        ds(450).verify(peer, &env)?;
    }

    if reader == Peer::C10 {
        log(
            "prev_snapshot: cpeer10 does not hold the environment with MDB_PREVSNAPSHOT \
             while another process waits (it would leave the previous snapshot in the lock \
             file, KNOWN_C_BUGS.md row 70)",
        );
    } else {
        let mut holder = reader
            .on("hold-env", &env)
            .args([hold_secs().as_str(), "--until-stdin", "--prevsnapshot"])
            .spawn()?;
        holder.wait_ready()?;
        // The holder is the last user. Where the shared locks do not
        // outlive their last user, a C peer that waited fails its open once
        // the holder is gone (KNOWN_C_BUGS.md row 47; rlmdb probes again),
        // so an rlmdb peer waits there: semaphores, which the last user
        // removes (macOS, OpenBSD, NetBSD, DragonFly), and Windows' named
        // mutexes, which the kernel destroys with the last handle (C 1.0
        // holder and waiter fail alike there). Only mutexes in the lock file
        // (Linux, FreeBSD) outlive it.
        let lock = match pair.format {
            Format::V09 => &LOCK_V09,
            Format::V10 => &LOCK_V10,
        };
        let mut waiter_peer = pair.other(reader);
        if lock.objects != LockObjects::PthreadMutexes && !waiter_peer.is_rust() {
            waiter_peer = reader;
        }
        // Deleting keys of the 450 fails outright on the 300-key snapshot.
        let waiter = waiter_peer
            .on("delete", &env)
            .args(ds(450).args())
            .args(["--every", "2"])
            .spawn()?;
        // Long enough for the waiter to wait for the holder's lock (if it
        // is late, it opens alone afterwards: the check passes vacuously).
        std::thread::sleep(Duration::from_millis(300));
        holder.release()?;
        holder.finish()?.ok()?;
        waiter.finish()?.ok()?;
        for peer in pair.verifiers() {
            ds(450).after_delete(2).verify(peer, &env)?;
        }
    }
    audit(&pair, &env)?;
    Ok(())
}
