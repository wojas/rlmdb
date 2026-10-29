//! Scenarios about the reader table and the lock file: reader slots seen
//! across processes and implementations, stale slots of killed readers and
//! who clears them, the lock file created by the reader peer, reader loops
//! next to nested transactions, and `MDB_NOTLS` readers.

use std::fmt;

use super::{audit, log, loop_secs, new_env, reader_check, readers, stat, DataSet, Pair, MIB};
use crate::report::{HoldReady, ReaderLoopReport, WriterLoopReport};
use crate::{ensure, tools, Error, Peer, Result, TestEnv};

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
        .args(["60", "--until-stdin"])
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
        .args(["60", "--until-stdin"])
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
        .args(["60", "--until-stdin"])
        .spawn()?;
    keeper.wait_ready()?;

    let mut h = holder
        .on("hold-reader", &env)
        .args(["60", "--until-stdin"])
        .spawn()?;
    let ready = HoldReady::parse(&h.wait_ready()?)?;
    let pid = h.pid();
    let cleared = clearer.clear(&env)?;
    ensure!(
        cleared == 0,
        "{clearer} cleared {cleared} slots while {holder}'s reader {pid} was alive"
    );
    h.kill()?;
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
    let cleared = clearer.clear(&env)?;
    ensure!(
        cleared == 1,
        "{clearer} cleared {cleared} slots, expected the dead {holder} reader {pid}"
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
        .args(["60", "--until-stdin"])
        .args(ds.args())
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    let layout = tools::c_layout()?;
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
        loops.push(p);
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
    for p in loops {
        let out = p.finish()?.ok()?;
        let r = ReaderLoopReport::parse(&out.stdout)?;
        ensure!(r.reads > 1, "`{}` checked {} snapshots", out.label, r.reads);
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
        loops.push(p);
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
    for p in loops {
        let out = p.finish()?.ok()?;
        let r = ReaderLoopReport::parse(&out.stdout)?;
        ensure!(r.reads > 1, "`{}` checked {} snapshots", out.label, r.reads);
        log(format_args!("{}: {r:?}", out.label));
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
        .args(["60", "--until-stdin", "--notls"])
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
        loops.push(p);
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
    for p in loops {
        let out = p.finish()?.ok()?;
        let r = ReaderLoopReport::parse(&out.stdout)?;
        ensure!(r.reads > 1, "`{}` checked {} snapshots", out.label, r.reads);
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
