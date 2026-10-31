//! Scenarios in which both peers of a pair write (stage 3): turns of
//! writes by each implementation on the same databases, bulk deletes,
//! `mdb_drop`, copies, a full map, and writers that die with the writer
//! lock held.
//!
//! As everywhere, every write is verified by the other implementation and
//! by C, and page usage is audited by C and the reader (`audit`).

use std::time::Duration;

use super::{
    audit, copy_or_skip, create, drop_db, dump_all, log, new_env, stat, verify_by, DataSet, Pair,
    MIB,
};
use crate::report::{HoldReady, WriterLoopReport};
use crate::{bail, ensure, Output, Peer, Result, TestEnv};

/// `ds` verifies with the writer, the reader and C.
fn verify_all(pair: &Pair, env: &TestEnv, ds: &DataSet) -> Result<()> {
    verify_by(&pair.verifiers(), env, ds)
}

/// `reader-loop <db> 0` (one snapshot check of writer-loop data) with every
/// verifier.
fn check_loop_data(pair: &Pair, env: &TestEnv, db: &str) -> Result<()> {
    for peer in pair.verifiers() {
        peer.on("reader-loop", env).args([db, "0"]).run()?;
    }
    Ok(())
}

/// Both peers write the same databases in turns (fill, delete a third,
/// refill, delete half, delete the rest, refill with `MDB_APPEND`), and all
/// verifiers check the data after every turn. Then writer loops of both
/// take turns on the same counters: `#commits` continues exactly where the
/// other implementation left it.
pub fn alternating_writers(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let (a, b) = (pair.writer, pair.reader);
    let sets = [
        DataSet::new("alt", 2000, 71),
        DataSet::new("altd", 400, 72).with(&["--dupsort", "--dups", "12"]),
        DataSet::new("altb", 90, 73).with(&["--big", "20000"]),
        DataSet::new("altf", 30, 74).with(&["--dupfixed", "--dups", "700"]),
    ];
    for ds in &sets {
        ds.fill(a, &env, &["--per-txn", "300"])?;
        verify_all(&pair, &env, ds)?;
        ds.delete(b, &env, 3, &["--per-txn", "150"])?;
        verify_all(&pair, &env, &ds.after_delete(3))?;
        ds.fill(a, &env, &[])?; // overwrites, and adds the deleted items back
        verify_all(&pair, &env, ds)?;
        ds.delete(b, &env, 2, &[])?;
        verify_all(&pair, &env, &ds.after_delete(2))?;
        ds.after_delete(2)
            .delete(a, &env, 1, &["--per-txn", "100"])?;
        verify_all(&pair, &env, &ds.after_delete(1))?;
        ds.fill(b, &env, &["--append"])?;
        verify_all(&pair, &env, ds)?;
    }
    audit(&pair, &env)?;

    let mut last = 0;
    for (k, peer) in [a, b, a, b].into_iter().enumerate() {
        let seed = (80 + k).to_string();
        let out = peer
            .on("writer-loop", &env)
            .args(["wl", "0.2", &seed, "--max-commits", "40"])
            .run()?;
        let r = WriterLoopReport::parse(&out.stdout)?;
        ensure!(
            r.last_commit == last + r.commits,
            "turn {k}: {peer} committed {} times after #commits {last}, but ended at {}",
            r.commits,
            r.last_commit
        );
        last = r.last_commit;
        check_loop_data(&pair, &env, "wl")?;
    }
    audit(&pair, &env)?;
    Ok(())
}

/// Bulk deletes by one implementation, verified by the other and C, with a
/// page audit after each round: the reader deletes a third of everything,
/// the writer the rest (all pages but the DB records go to the free list),
/// the reader refills (reusing the free pages) and the writer deletes every
/// second item.
pub fn delete_heavy(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 512 * MIB)?;
    let (w, r) = (pair.writer, pair.reader);
    let sets = [
        DataSet::new("dh", 12000, 81).with(&["--valsize", "100"]),
        DataSet::new("dhd", 1200, 82).with(&["--dupsort", "--dups", "60"]),
        DataSet::new("dhs", 12, 86).with(&["--dupsort", "--dups", "1500", "--valsize", "40"]),
        DataSet::new("dhf", 40, 83).with(&["--dupfixed", "--dups", "2500"]),
        DataSet::new("dhb", 300, 84).with(&["--big", "12000"]),
        DataSet::new("dhi", 6000, 85).with(&["--intkey"]),
    ];
    for ds in &sets {
        ds.fill(w, &env, &["--per-txn", "3000"])?;
    }
    let full = audit(&pair, &env)?;

    for ds in &sets {
        ds.delete(r, &env, 3, &["--per-txn", "500"])?;
        verify_by(&[w, pair.reference()], &env, &ds.after_delete(3))?;
    }
    audit(&pair, &env)?;

    for ds in &sets {
        ds.after_delete(3)
            .delete(w, &env, 1, &["--per-txn", "2000"])?;
        verify_by(&[r, pair.reference()], &env, &ds.after_delete(1))?;
    }
    let empty = audit(&pair, &env)?;
    for ds in &sets {
        let db = empty.db(&ds.db)?;
        ensure!(
            db.entries == 0 && db.pages() == 0,
            "{} is not empty after deleting everything: {db:?}",
            ds.db
        );
    }
    ensure!(
        empty.pages_free + empty.pages_freedb + empty.pages_tree >= full.pages_tree,
        "deleting everything freed too few pages: {empty:?}, full: {full:?}"
    );

    for ds in &sets {
        ds.fill(r, &env, &[])?;
        ds.delete(w, &env, 2, &["--per-txn", "700"])?;
        verify_by(&[r, pair.reference()], &env, &ds.after_delete(2))?;
    }
    let refilled = audit(&pair, &env)?;
    log(format_args!(
        "delete_heavy: last_pgno {} full, {} empty, {} refilled",
        full.last_pgno, empty.last_pgno, refilled.last_pgno
    ));
    // Without reuse the refill and the deletes would at least double the
    // file; with reuse it grows by fragmentation only (overflow values need
    // runs of consecutive free pages): C ends at about 1.4 times.
    ensure!(
        refilled.last_pgno < full.last_pgno * 7 / 4,
        "the refill did not reuse the freed pages: last_pgno {} after, {} before",
        refilled.last_pgno,
        full.last_pgno
    );
    Ok(())
}

/// `mdb_drop` by the writer: two DBs emptied, two deleted (one with
/// sub-DBs). The others verify that the emptied ones exist with their flags
/// and no data and the deleted ones are gone; the audit finds every page
/// of the dropped trees in the free list. Then the reader writes into the
/// emptied DBs, re-creates the deleted ones, and reuses the freed pages.
pub fn drop_clear_dbs(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        (DataSet::new("dp", 3000, 91), false),
        (
            DataSet::new("dpd", 300, 92).with(&["--dupsort", "--dups", "300", "--valsize", "40"]),
            true,
        ),
        (
            DataSet::new("dpb", 100, 93).with(&["--big", "30000"]),
            false,
        ),
        (
            DataSet::new("dpf", 30, 94).with(&["--dupfixed", "--dups", "2000"]),
            true,
        ),
    ];
    let mut entries = Vec::new();
    for (ds, _) in &sets {
        entries.push(ds.fill(pair.writer, &env, &[])?.entries);
    }
    let full = audit(&pair, &env)?;

    let dropper = pair.capable(pair.writer, "drop");
    for ((ds, delete), n) in sets.iter().zip(&entries) {
        let out = drop_db(dropper, &env, &ds.db, *delete)?;
        ensure!(
            out.num::<u64>("entries")? == *n,
            "{dropper} dropped {} with {:?} entries, fill wrote {n}",
            ds.db,
            out.get("entries")
        );
    }
    for peer in pair.verifiers() {
        let st = stat(peer, &env)?;
        for (ds, delete) in &sets {
            if *delete {
                ensure!(
                    st.db(&ds.db).is_err(),
                    "{peer} still lists the deleted DB {}: {st:?}",
                    ds.db
                );
                let out = peer.on("verify", &env).args(ds.args()).output()?;
                ensure!(
                    out.code() == Some(1) && out.stderr.contains("database not found"),
                    "verify of the deleted DB {}: {out}",
                    ds.db
                );
            } else {
                let v = ds.with_count(0).verify(peer, &env)?;
                ensure!(v.entries == 0, "{peer}: emptied {} has {v:?}", ds.db);
            }
        }
    }
    let dropped = audit(&pair, &env)?;
    log(format_args!(
        "drop: tree pages {} -> {}, free pages {} -> {}",
        full.pages_tree, dropped.pages_tree, full.pages_free, dropped.pages_free
    ));
    // Only the main DB (records of the emptied DBs) is left: one leaf, or
    // with 256-byte pages a small tree. The drops may take a few new pages
    // for the grown free list, which lists every page of the dropped trees
    // (with 1 KiB pages one overflow page per 128).
    ensure!(
        dropped.pages_tree == dropped.db("-")?.pages()
            && dropped.last_pgno <= full.last_pgno + 16 + dropped.pages_freedb,
        "pages after the drops: {dropped:?}, before: {full:?}"
    );
    ensure!(
        dropped.pages_free + dropped.pages_freedb + dropped.pages_tree >= full.pages_tree,
        "the dropped trees' pages are not all free: {dropped:?}, before: {full:?}"
    );

    for (ds, _) in &sets {
        ds.fill(pair.reader, &env, &[])?;
        verify_by(&[pair.writer, pair.reference()], &env, ds)?;
    }
    let refilled = audit(&pair, &env)?;
    ensure!(
        refilled.last_pgno <= full.last_pgno + full.last_pgno / 10 + 16,
        "the refill did not reuse the dropped pages: last_pgno {} after, {} before",
        refilled.last_pgno,
        full.last_pgno
    );
    Ok(())
}

/// The bytes of a data file up to its last used page.
fn data_pages(env: &TestEnv, last_pgno: u64, psize: u32) -> Result<Vec<u8>> {
    let mut data = std::fs::read(env.data_file())?;
    let len = (last_pgno + 1) * u64::from(psize);
    ensure!(
        data.len() as u64 >= len,
        "{} is {} bytes, shorter than {} pages",
        env.data_file().display(),
        data.len(),
        last_pgno + 1
    );
    data.truncate(len as usize);
    Ok(data)
}

/// Where two files first differ, for messages: page and offset in it.
fn first_difference(a: &[u8], b: &[u8], psize: u32) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(off) => format!(
            "first difference at byte {off} (page {}, offset {})",
            off / psize as usize,
            off % psize as usize
        ),
        None => format!("lengths {} and {}", a.len(), b.len()),
    }
}

/// Plain and compacting copies of one snapshot by the writer, the reader
/// and C, each verified and dumped by the other implementations and
/// audited. Plain copies of the same snapshot must be byte-identical up to
/// the last page (they copy the pages as they are), and so must compacted
/// ones (C's compaction is a deterministic tree walk).
pub fn copy_cross(pair: Pair) -> Result<()> {
    let env = new_env(&pair, 256 * MIB)?;
    let sets = [
        DataSet::new("cp", 2500, 121),
        DataSet::new("cpd", 25, 122).with(&["--dupsort", "--dups", "1200", "--valsize", "40"]),
        DataSet::new("cpb", 60, 123).with(&["--big", "25000"]),
        DataSet::new("cpi", 1000, 124).with(&["--intkey"]),
    ];
    for ds in &sets {
        ds.fill(pair.writer, &env, &[])?;
        ds.delete(pair.capable(pair.reader, "delete"), &env, 4, &[])?;
    }
    let orig = audit(&pair, &env)?;
    let original = dump_all(pair.reference(), &env)?;

    // Every copier copies the same snapshot: nobody writes meanwhile.
    let mut copies: Vec<(Peer, bool, TestEnv)> = Vec::new();
    for peer in pair.verifiers() {
        for compact in [false, true] {
            let target = env.derived(&format!(
                "{peer}-{}",
                if compact { "compact" } else { "plain" }
            ));
            let copier = pair.capable(peer, "copy");
            let mut cmd = copier.on("copy", &env).arg(target.path());
            if compact {
                cmd = cmd.arg("--compact");
            }
            if copy_or_skip(copier, &pair, cmd.output()?, "copy_cross")?.is_none() {
                continue;
            }
            copies.push((peer, compact, target));
        }
    }

    let mut images: Vec<(Peer, bool, Vec<u8>)> = Vec::new();
    for (maker, compact, target) in &copies {
        let what = format!(
            "{} copy by {maker}",
            if *compact { "compacted" } else { "plain" }
        );
        for peer in pair.verifiers().into_iter().filter(|p| p != maker) {
            for ds in &sets {
                ds.after_delete(4)
                    .verify(peer, target)
                    .map_err(|e| e.context(format!("{what}, verified by {peer}")))?;
            }
            ensure!(
                dump_all(peer, target)? == original,
                "{peer} dumps the {what} differently from the original"
            );
        }
        let a = audit(&pair, target)?;
        if *compact {
            ensure!(
                a.pages_free == 0 && a.pages_freedb == 0 && a.txnid == 1,
                "{what}: {a:?}"
            );
        } else {
            ensure!(
                (a.txnid, a.last_pgno, a.pages_free)
                    == (orig.txnid, orig.last_pgno, orig.pages_free),
                "{what}: {a:?}, original: {orig:?}"
            );
        }
        images.push((*maker, *compact, data_pages(target, a.last_pgno, a.psize)?));
    }
    for compact in [false, true] {
        let mut same: Vec<&(Peer, bool, Vec<u8>)> =
            images.iter().filter(|i| i.1 == compact).collect();
        let (first, _, reference) = same.remove(0);
        for (maker, _, image) in same {
            ensure!(
                image == reference,
                "{} copies of one snapshot by {first} and {maker} differ: {}",
                if compact { "compacted" } else { "plain" },
                first_difference(reference, image, orig.psize)
            );
        }
    }
    Ok(())
}

/// Did the command fail with `MDB_MAP_FULL`?
fn is_map_full(out: &Output) -> bool {
    out.code() == Some(3) && out.stderr.contains("MDB_MAP_FULL")
}

/// The output of a command that must fail with `MDB_MAP_FULL` (a peer
/// that does not implement a step fails as not implemented instead).
fn expect_map_full(out: Output) -> Result<()> {
    if is_map_full(&out) {
        return Ok(());
    }
    let msg = format!("expected MDB_MAP_FULL: {out}");
    match out.ok() {
        Err(e) if e.is_not_implemented() => Err(e),
        _ => Err(crate::Error::new(msg)),
    }
}

/// A 4 MiB map: a fill too large for it fails with `MDB_MAP_FULL` in both
/// implementations and leaves the environment intact (the failed
/// transaction left nothing, earlier commits stay, the audit is clean). A
/// writer loop on the nearly full map fails cleanly or not at all. Then the
/// reader grows the map (`--mapsize`), the fill succeeds, and the writer
/// opens with the new size recorded in the meta page.
pub fn map_full(pair: Pair) -> Result<()> {
    let env = pair.env()?;
    create(pair.writer, &env, 4 * MIB)?;
    let base = DataSet::new("base", 500, 101);
    base.fill(pair.writer, &env, &[])?;
    // 100 values of 60000 bytes: more than the map at every page size
    // (with 16 KiB pages each takes 64 KiB, with 1 KiB pages 59 KiB).
    let big = DataSet::new("huge", 300, 102).with(&["--big", "60000"]);

    for peer in pair.peers() {
        expect_map_full(peer.on("fill", &env).args(big.args()).output()?)?;
        for v in pair.verifiers() {
            base.verify(v, &env)?;
            ensure!(
                stat(v, &env)?.db("huge").is_err(),
                "{v} sees the DB of {peer}'s failed transaction"
            );
        }
        audit(&pair, &env)?;
    }

    // Batches committed before the map filled up stay.
    let out = pair
        .writer
        .on("fill", &env)
        .args(big.args())
        .args(["--per-txn", "5"])
        .output()?;
    expect_map_full(out)?;
    let partial = audit(&pair, &env)?;
    ensure!(
        partial.db("huge")?.entries > 0,
        "no batch of the --per-txn fill was committed: {partial:?}"
    );
    base.verify(pair.reader, &env)?;

    let out = pair
        .reader
        .on("writer-loop", &env)
        .args(["wl", "0.3", "7", "--max-commits", "200"])
        .output()?;
    if !out.success() {
        expect_map_full(out)?;
    }
    audit(&pair, &env)?;

    let grown = (64 * MIB).to_string();
    pair.reader
        .on("fill", &env)
        .args(big.args())
        .args(["--mapsize", grown.as_str()])
        .run()?;
    let st = stat(pair.writer, &env)?;
    ensure!(
        st.mapsize == 64 * MIB,
        "{} opens with map size {} after {} grew it to {grown}",
        pair.writer,
        st.mapsize,
        pair.reader
    );
    verify_all(&pair, &env, &big)?;
    base.verify(pair.writer, &env)?;
    DataSet::new("more", 300, 103)
        .with(&["--big", "20000"])
        .fill(pair.writer, &env, &[])?;
    audit(&pair, &env)?;
    Ok(())
}

/// Spawn `writer-loop` of `peer` and kill it with SIGKILL `delay` seconds
/// after its first commit: usually inside a write transaction, sometimes in
/// the middle of a commit.
fn kill_writer_loop(peer: Peer, env: &TestEnv, seed: u32, delay: f64) -> Result<()> {
    let mut wl = peer
        .on("writer-loop", env)
        .args(["kw", "60", &seed.to_string(), "--nosync"])
        .spawn()?;
    wl.wait_ready()?;
    std::thread::sleep(Duration::from_secs_f64(delay));
    wl.kill()
}

/// A short writer loop of `peer` that must get the writer lock (within 30
/// seconds, else the lock was not recovered).
fn writer_gets_lock(peer: Peer, env: &TestEnv, seed: u32) -> Result<()> {
    peer.on("writer-loop", env)
        .args(["kw", "0.1", &seed.to_string(), "--max-commits", "20"])
        .timeout(Duration::from_secs(30))
        .run()
        .map(|_| ())
        .map_err(|e| e.context(format!("{peer} could not write after a writer was killed")))
}

/// A writer killed with SIGKILL while it holds the writer lock, inside a
/// write transaction (`hold-writer`, then writer loops killed at random
/// points). The other implementation must get the lock and write; nothing
/// of the killed transactions may be visible; the data and page usage stay
/// consistent.
///
/// Linux: the writer lock is a robust process-shared mutex. With another
/// process keeping the environment open, the next writer gets `EOWNERDEAD`,
/// recovers (`mdb_mutex_failed`) and continues.
///
/// macOS: the writer lock is a POSIX semaphore, which is not robust (in C
/// too): while another process has the environment open, the next writer
/// waits forever; the scenario checks that it does wait. Once no process
/// has the environment open, the next opener is exclusive and re-creates
/// the semaphores, and writing works again.
pub fn killed_writer(pair: Pair) -> Result<()> {
    let robust = cfg!(target_os = "linux");
    let env = new_env(&pair, 512 * MIB)?;
    let ds = DataSet::new("data", 1000, 111);
    ds.fill(pair.writer, &env, &[])?;
    pair.writer
        .on("writer-loop", &env)
        .args(["kw", "0", "1"])
        .run()?;

    // Keeps the environment open, so that the next opener is not exclusive
    // (an exclusive opener re-initialises the lock file).
    let mut keeper = pair
        .reference()
        .on("hold-env", &env)
        .args(["120", "--until-stdin"])
        .spawn()?;
    keeper.wait_ready()?;
    let mut keeper = Some(keeper);

    let mut holder = pair
        .capable(pair.writer, "hold-writer")
        .on("hold-writer", &env)
        .args(["120", "--until-stdin"])
        .spawn()?;
    let ready = HoldReady::parse(&holder.wait_ready()?)?;
    holder.kill()?;
    log(format_args!(
        "{} killed in write txn {}",
        pair.writer, ready.txnid
    ));
    if robust {
        writer_gets_lock(pair.reader, &env, 2)?;
    } else {
        let mut waiting = pair
            .reader
            .on("writer-loop", &env)
            .args(["kw", "0", "2"])
            .spawn()?;
        match waiting.wait_for_line("READY", Duration::from_millis(1500)) {
            Ok(line) => bail!(
                "{} got the writer semaphore of a killed writer while the environment \
                 stayed open (POSIX semaphores are not robust): {line}",
                pair.reader
            ),
            // Still waiting for the semaphore, as expected.
            Err(e) if e.message().contains("did not print") => {}
            Err(e) => return Err(e),
        }
        drop(waiting); // kills it
        if let Some(mut k) = keeper.take() {
            k.release()?;
            k.finish()?.ok()?;
        }
        writer_gets_lock(pair.reader, &env, 2)?;
    }

    for (k, delay) in [0.05, 0.2, 0.4].into_iter().enumerate() {
        let k = k as u32;
        kill_writer_loop(pair.writer, &env, 10 + k, delay)?;
        writer_gets_lock(pair.reader, &env, 20 + k)?;
    }
    if let Some(mut k) = keeper.take() {
        k.release()?;
        k.finish()?.ok()?;
    }

    check_loop_data(&pair, &env, "kw")?;
    verify_all(&pair, &env, &ds)?;
    // "#held" in hex, as `dump` prints keys.
    let main = pair.reference().on("dump", &env).arg("-").run()?.stdout;
    ensure!(
        !main.lines().any(|l| l.starts_with("2368656c64=")),
        "the killed transaction's #held is visible:\n{main}"
    );
    audit(&pair, &env)?;
    Ok(())
}

/// The write script of [`same_writes_same_pages`]: every fill variant,
/// deletes of each kind, `mdb_drop` both ways, a deterministic writer loop
/// (nested transactions, aborts; bounded by commits, not time) and
/// `nested-test`.
const SAME_WRITES: &[&[&str]] = &[
    &["fill", "plain", "3000", "1", "--per-txn", "700"],
    &[
        "fill",
        "dups",
        "500",
        "2",
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
        "3",
        "--dupsort",
        "--dups",
        "900",
        "--valsize",
        "40",
    ],
    &["fill", "fixed", "20", "4", "--dupfixed", "--dups", "900"],
    // 1-byte items: mp_upper of their P_LEAF2 pages lies past the page end.
    &[
        "fill",
        "fixed1",
        "20",
        "11",
        "--dupfixed",
        "--dups",
        "256",
        "--valsize",
        "1",
    ],
    &[
        "fill",
        "fixed2",
        "20",
        "12",
        "--dupfixed",
        "--dups",
        "1500",
        "--valsize",
        "2",
    ],
    &[
        "fill",
        "intdup",
        "30",
        "5",
        "--intdup",
        "--dups",
        "300",
        "--valsize",
        "4",
    ],
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
        "2",
        "--dupsort",
        "--dups",
        "9",
        "--every",
        "3",
        "--per-txn",
        "50",
    ],
    &["delete", "plain", "3000", "1", "--every", "2"],
    &[
        "delete",
        "sub",
        "20",
        "3",
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
        "4",
        "--dupfixed",
        "--dups",
        "900",
        "--every",
        "2",
    ],
    &[
        "delete",
        "fixed1",
        "20",
        "11",
        "--dupfixed",
        "--dups",
        "256",
        "--valsize",
        "1",
        "--every",
        "3",
    ],
    &[
        "delete",
        "fixed2",
        "20",
        "12",
        "--dupfixed",
        "--dups",
        "1500",
        "--valsize",
        "2",
        "--every",
        "3",
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
    &["drop", "rev"],
    &["drop", "appd", "--delete"],
    &[
        "writer-loop",
        "wl",
        "600",
        "5",
        "--nosync",
        "--max-commits",
        "60",
    ],
    &["nested-test"],
    &["fill", "plain", "3000", "1", "--per-txn", "1000"],
];

/// The writer and the reader each run the same script of writes on an
/// environment of their own; after every step the two data files must
/// hold the same pages: the same trees on the same page numbers with the
/// same contents, the same free lists and the same meta pages
/// ([`crate::pagecmp`]; bytes C leaves uninitialised are not compared).
/// So the Rust write path makes exactly C's decisions: page allocation and
/// reuse, split points, free-list records, sub-page growth, overflow
/// placement.
pub fn same_writes_same_pages(pair: Pair) -> Result<()> {
    let envs = [pair.env()?, pair.env()?];
    let peers = [pair.writer, pair.reader];
    for (peer, env) in peers.iter().zip(&envs) {
        create(*peer, env, 256 * MIB)?;
    }
    for step in SAME_WRITES {
        for (peer, env) in peers.iter().zip(&envs) {
            peer.on(step[0], env).args(&step[1..]).run()?;
        }
        crate::pagecmp::compare_data_files(&envs[0].data_file(), &envs[1].data_file()).map_err(
            |e| {
                e.context(format!(
                    "after `{}` by {} and {}",
                    step.join(" "),
                    pair.writer,
                    pair.reader
                ))
            },
        )?;
    }
    log(format_args!(
        "same pages: {} reachable pages",
        crate::pagecmp::reachable_pages(&envs[0].data_file())?
    ));
    for env in &envs {
        audit(&pair, env)?;
    }
    Ok(())
}
