//! Incremental backups of LMDB 1.0 environments through the Rust API
//! (`Environment::incr_dump`, `incr_dump_to_path`, `incr_load`; C 1.0's
//! `mdb_env_incr_dumpfd`, `mdb_env_incr_dump`, `mdb_env_incr_loadfd`): a
//! chain of dumps turns an old copy, or an empty environment, into the
//! source byte for byte at every page size; free pages that C's walk
//! takes for pages, which are no records here, and every page of the
//! snapshot in the dump (`KNOWN_C_BUGS.md` row 72); malformed input to the
//! load (row 66);
//! what the load needs (the environment to itself) and leaves (the loaded
//! snapshot current, row 65).
//!
//! That the bytes are C 1.0's, and that C 1.0 and rlmdb load each other's
//! dumps, is checked across processes in `interop/tests/incr10.rs`.

use std::io::{self, Read};
use std::path::Path;
use std::sync::{mpsc, Arc, Barrier};

use rlmdb::{
    DatabaseFlags, Dbi, Environment, EnvironmentBuilder, EnvironmentFlags, Error, Format, MdbError,
    Transaction, WriteFlags,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-incr-")
        .tempdir()
        .unwrap()
}

fn v10(page_size: usize) -> EnvironmentBuilder {
    let mut b = Environment::builder();
    b.format(Format::V10)
        .page_size(page_size)
        .max_dbs(8)
        .map_size(64 << 20);
    b
}

/// Opens an existing 1.0 environment with the map size of its file.
fn reopen(path: &Path) -> Environment {
    Environment::builder().max_dbs(8).open(path).unwrap()
}

fn data(path: &Path) -> Vec<u8> {
    std::fs::read(path.join("data.mdb")).unwrap()
}

fn last_txnid(env: &Environment) -> u64 {
    env.info().unwrap().last_txnid as u64
}

fn dump(env: &Environment, txnid: u64) -> Vec<u8> {
    let mut out = Vec::new();
    env.incr_dump(&mut out, txnid).unwrap();
    out
}

fn code(e: &Error) -> i32 {
    e.to_err_code()
}

/// The `(mp_pgno, mp_txnid, pages)` of the page records of a dump of
/// `ps`-byte pages, stepping over overflow runs as the load does.
fn records(dump: &[u8], ps: usize) -> Vec<(u64, u64, u64)> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < dump.len() {
        let u64_at = |o: usize| u64::from_ne_bytes(dump[off + o..off + o + 8].try_into().unwrap());
        let flags = u16::from_ne_bytes([dump[off + 18], dump[off + 19]]);
        let pages = if flags & 0x04 != 0 {
            u64::from(u32::from_ne_bytes(
                dump[off + 20..off + 24].try_into().unwrap(),
            ))
        } else {
            1
        };
        out.push((u64_at(0), u64_at(8), pages));
        off += pages as usize * ps;
    }
    assert_eq!(off, dump.len(), "the dump ends inside a record");
    out
}

/// The dump's data pages come in file order, none beginning inside a
/// record before it: no stale header made a record overlap (row 72).
fn assert_sane(dump: &[u8], ps: usize) {
    let mut next = 2;
    for (pgno, _, pages) in records(dump, ps).into_iter().filter(|r| r.0 >= 2) {
        assert!(pgno >= next, "page {pgno} after page {}", next - 1);
        next = pgno + pages;
    }
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The databases of [`work`].
struct Dbs {
    main: Dbi,
    dup: Dbi,
    fix: Dbi,
}

fn create_dbs(env: &Environment) -> Dbs {
    let mut txn = env.begin_rw_txn().unwrap();
    let create = DatabaseFlags::CREATE;
    let main = txn.create_db(None, DatabaseFlags::empty()).unwrap();
    let dup = txn
        .create_db(Some("dup"), create | DatabaseFlags::DUP_SORT)
        .unwrap();
    let fix = txn
        .create_db(
            Some("fix"),
            create | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    txn.commit().unwrap();
    Dbs { main, dup, fix }
}

/// `txns` small write transactions of puts, updates and deletes in three
/// databases, with values that fit their pages and, now and then, a value
/// on overflow pages under a new key. Nothing frees an overflow run: the
/// big values are never overwritten or deleted, and transactions free few
/// enough pages that the free list keeps its records on its leaves. The
/// pages of a run freed after the copy change without a header of their
/// own, so no dump holds them, and the restored file would differ from the
/// source there (on pages its snapshot does not use); with C (row 72) the
/// dump would also differ from rlmdb's.
fn work(env: &Environment, dbs: &Dbs, rng: &mut Rng, txns: usize) {
    let ps = env.page_size() as u64;
    for _ in 0..txns {
        let mut txn = env.begin_rw_txn().unwrap();
        for _ in 0..1 + rng.next(2) {
            let k = rng.next(60);
            let key = format!("k{k:03}");
            match rng.next(10) {
                0..=3 => {
                    let v = vec![b'a' + (k % 26) as u8; 1 + rng.next(30) as usize];
                    txn.put(dbs.main, key.as_bytes(), &v, WriteFlags::empty())
                        .unwrap();
                }
                4 => {
                    let _ = txn.del(dbs.main, key.as_bytes(), None).unwrap();
                }
                5 | 6 => {
                    let v = format!("d{:04}", rng.next(40));
                    txn.put(dbs.dup, key.as_bytes(), v.as_bytes(), WriteFlags::empty())
                        .unwrap();
                }
                7 => {
                    let v = format!("d{:04}", rng.next(40));
                    let _ = txn
                        .del(dbs.dup, key.as_bytes(), Some(v.as_bytes()))
                        .unwrap();
                }
                8 => {
                    let v = rng.next(1 << 20).to_be_bytes();
                    txn.put(dbs.fix, key.as_bytes(), &v, WriteFlags::empty())
                        .unwrap();
                }
                _ => {
                    // A new key with a value of a few pages, kept for good.
                    let big = format!("big{:06}", rng.next(1_000_000));
                    let v = vec![b'z'; (ps * (1 + rng.next(3))) as usize];
                    txn.put(dbs.main, big.as_bytes(), &v, WriteFlags::NO_OVERWRITE)
                        .or_else(|e| match e {
                            Error::Mdb(MdbError::KeyExist) => Ok(()),
                            e => Err(e),
                        })
                        .unwrap();
                }
            }
        }
        txn.commit().unwrap();
    }
}

/// A plain copy of `env`'s data file into a new directory under `dir`.
fn copy(env: &Environment, dir: &Path, name: &str) -> std::path::PathBuf {
    let dst = dir.join(name);
    std::fs::create_dir(&dst).unwrap();
    env.copy_to_path(&dst, false).unwrap();
    dst
}

/// Loads the dumps into the environment at `path`, in order.
fn load_chain(path: &Path, dumps: &[&[u8]]) {
    let mut env = reopen(path);
    for d in dumps {
        env.incr_load(*d).unwrap();
    }
}

#[test]
fn chains_of_dumps_restore_the_source() {
    for ps in [256, 1024, 4096, 65536] {
        let dir = tmp();
        let src_path = dir.path().join("src");
        std::fs::create_dir(&src_path).unwrap();
        let src = v10(ps).open(&src_path).unwrap();
        let dbs = create_dbs(&src);
        let mut rng = Rng(ps as u64);
        work(&src, &dbs, &mut rng, 40);

        // A base copy and a full dump at t0, then two increments.
        let base = copy(&src, dir.path(), "base");
        let t0 = last_txnid(&src);
        let full = dump(&src, 0);
        work(&src, &dbs, &mut rng, 40);
        let d1 = dump(&src, t0);
        let t1 = last_txnid(&src);
        work(&src, &dbs, &mut rng, 40);
        let d2 = dump(&src, t1);
        let t2 = last_txnid(&src);
        for d in [&full, &d1, &d2] {
            assert_sane(d, ps);
        }
        // Every dump starts with the meta pages that changed: both, here.
        let metas: Vec<u64> = records(&d2, ps).iter().take(2).map(|r| r.0).collect();
        assert_eq!(metas, [0, 1], "page size {ps}");
        // Nothing changed since t2.
        assert!(dump(&src, t2).is_empty(), "page size {ps}");
        drop(src);

        // The copy at t0 plus the increments.
        load_chain(&base, &[&d1, &d2]);
        assert!(data(&base) == data(&src_path), "page size {ps}: copy");

        // An empty environment plus the full dump and the increments.
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        drop(v10(ps).map_size(1 << 20).open(&empty).unwrap());
        load_chain(&empty, &[&full, &d1, &d2]);
        assert!(data(&empty) == data(&src_path), "page size {ps}: full");

        // The restored environment opens and reads as the source.
        let env = reopen(&empty);
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), t2);
        let dup = txn.open_db(Some("dup")).unwrap();
        assert!(txn.stat(dup).unwrap().entries > 0);
    }
}

#[test]
fn incr_dump_to_path_writes_a_new_file() {
    let dir = tmp();
    let env = v10(4096).open(dir.path()).unwrap();
    let dbs = create_dbs(&env);
    work(&env, &dbs, &mut Rng(7), 10);
    let out = dir.path().join("dump.1");
    env.incr_dump_to_path(&out, 1).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), dump(&env, 1));
    // The file must not exist; with NO_SUB_DIR or without, the path is
    // the file.
    let e = env.incr_dump_to_path(&out, 1).unwrap_err();
    let exists = if cfg!(windows) { 80 } else { libc::EEXIST };
    assert_eq!(code(&e), exists, "{e:?}");
    assert_eq!(env.flags(), EnvironmentFlags::empty());
}

#[test]
fn lmdb_0_9_environments_are_not_supported() {
    let dir = tmp();
    let mut env = Environment::builder()
        .format(Format::V09)
        .open(dir.path())
        .unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, b"k", b"v", WriteFlags::empty()).unwrap();
    txn.commit().unwrap();
    let before = data(dir.path());
    let enotsup = |e: Error| assert_eq!(code(&e), libc::ENOTSUP, "{e:?}");
    enotsup(env.incr_dump(Vec::new(), 0).unwrap_err());
    let out = dir.path().join("dump");
    enotsup(env.incr_dump_to_path(&out, 0).unwrap_err());
    assert!(!out.exists());
    enotsup(env.incr_load(&before[..]).unwrap_err());
    assert!(data(dir.path()) == before);
}

/// An environment in `dir/src` whose first overflow run (a value of
/// `byte`s in the named database "big", txn 1) is freed (txn 2) and partly
/// reused by four small transactions, as `interop/tests/incr10.rs` builds
/// it with C; and a plain copy of it after txn 1 in `dir/base`.
fn freed_run(dir: &Path, byte: u8) -> (Environment, std::path::PathBuf) {
    let src = dir.join("src");
    std::fs::create_dir(&src).unwrap();
    let env = v10(4096).open(&src).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let big = txn.create_db(Some("big"), DatabaseFlags::CREATE).unwrap();
    txn.put(big, b"bigkey", &vec![byte; 100_000], WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
    let base = copy(&env, dir, "base");
    let mut txn = env.begin_rw_txn().unwrap();
    txn.drop_db(big).unwrap();
    txn.commit().unwrap();
    for i in 0..4 {
        let mut txn = env.begin_rw_txn().unwrap();
        let k = format!("k{i}");
        txn.put(Dbi::MAIN, k.as_bytes(), b"v", WriteFlags::empty())
            .unwrap();
        txn.commit().unwrap();
    }
    (env, base)
}

/// Row 72: after an overflow run is freed and its first pages reused, C's
/// walk takes the rest of the old value for headers (records of page
/// 0x6161616161616161 with 'a's; with 'e's, which set `P_OVERFLOW` in the
/// bytes taken for the flags, a "run" past the file and `EFAULT`). Here
/// those pages name no page of their own position: no record. The reused
/// pages, which hold their own headers, are dumped, and the dump restores
/// the source.
#[test]
fn stale_overflow_pages_are_not_records() {
    for byte in *b"ae" {
        let dir = tmp();
        let (env, base) = freed_run(dir.path(), byte);
        let d = dump(&env, 1);
        let next = env.info().unwrap().last_pgno as u64 + 1;
        let recs = records(&d, 4096);
        assert!(recs.iter().all(|r| r.0 < next), "{recs:x?}");
        // The pages the four commits and the drop wrote.
        assert!(recs.iter().filter(|r| r.0 >= 2).count() >= 5, "{recs:x?}");
        assert_sane(&d, 4096);
        drop(env);
        load_chain(&base, &[&d]);
        assert!(
            data(&base) == data(&dir.path().join("src")),
            "'{}'",
            byte as char
        );
    }
}

/// The free pages of the snapshot of `env` (the page numbers in its free
/// list).
fn free_pages(env: &Environment) -> std::collections::BTreeSet<u64> {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::FREE).unwrap();
    let mut free = std::collections::BTreeSet::new();
    while let Some((_, idl)) = c.next().unwrap() {
        let ids: Vec<u64> = idl
            .chunks_exact(8)
            .map(|b| u64::from_ne_bytes(b.try_into().unwrap()))
            .collect();
        free.extend(&ids[1..=ids[0] as usize]);
    }
    free
}

/// A source with free pages below a live overflow run written after its
/// base copy: `(source, base, base txnid, the run's head, its pages, a free
/// page below it, next page)`. Txn 1 fills a database, txn 2 drops it (its
/// pages become free), two small commits make them reusable, the copy
/// follows, then a value of 64 pages, which no free range holds, goes to
/// the end of the file.
struct LiveRun {
    src: std::path::PathBuf,
    base: std::path::PathBuf,
    base_txnid: u64,
    head: u64,
    pages: u64,
    free: u64,
    next: u64,
}

fn live_run(dir: &Path) -> LiveRun {
    let src = dir.join("src");
    std::fs::create_dir(&src).unwrap();
    let env = v10(4096).open(&src).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let low = txn.create_db(Some("low"), DatabaseFlags::CREATE).unwrap();
    for i in 0..400u32 {
        txn.put(low, &i.to_be_bytes(), &[7; 100], WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    txn.drop_db(low).unwrap();
    txn.commit().unwrap();
    for i in 0..2u8 {
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, &[i], b"v", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
    }
    let base = copy(&env, dir, "base");
    let base_txnid = last_txnid(&env);
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(
        Dbi::MAIN,
        b"big",
        &vec![b'z'; 64 * 4096 - 100],
        WriteFlags::empty(),
    )
    .unwrap();
    txn.commit().unwrap();
    let next = env.info().unwrap().last_pgno as u64 + 1;
    let free = free_pages(&env);
    let file = data(&src);
    let u64_at = |o: usize| u64::from_ne_bytes(file[o..o + 8].try_into().unwrap());
    let (head, pages) = (2..next)
        .filter(|p| !free.contains(p))
        .find_map(|p| {
            let o = p as usize * 4096;
            let flags = u16::from_ne_bytes([file[o + 18], file[o + 19]]);
            let pages = u32::from_ne_bytes(file[o + 20..o + 24].try_into().unwrap());
            (u64_at(o) == p && flags & 0x04 != 0 && u64_at(o + 8) > base_txnid)
                .then_some((p, u64::from(pages)))
        })
        .expect("a live overflow run written after the copy");
    assert!(pages >= 64, "{pages}");
    let free = *free
        .range(..head)
        .next_back()
        .expect("a free page below the run");
    LiveRun {
        src,
        base,
        base_txnid,
        head,
        pages,
        free,
        next,
    }
}

/// A copy of `run`'s source in `dir/name` whose free page `run.free`
/// starts with `hdr`, and its data file.
fn crafted(run: &LiveRun, dir: &Path, name: &str, hdr: &[u8]) -> (std::path::PathBuf, Vec<u8>) {
    let path = dir.join(name);
    std::fs::create_dir(&path).unwrap();
    let mut file = data(&run.src);
    let at = run.free as usize * 4096;
    file[at..at + hdr.len()].copy_from_slice(hdr);
    std::fs::write(path.join("data.mdb"), &file).unwrap();
    (path, file)
}

/// The base copy of `run` in `dir/name`, loaded with `dump`; its data file.
fn restored(run: &LiveRun, dir: &Path, name: &str, dump: &[u8]) -> Vec<u8> {
    let path = dir.join(name);
    std::fs::create_dir(&path).unwrap();
    std::fs::copy(run.base.join("data.mdb"), path.join("data.mdb")).unwrap();
    load_chain(&path, &[dump]);
    data(&path)
}

/// `a` and `b` are the same but for page `except` (of 4 KiB).
fn same_but(a: &[u8], b: &[u8], except: u64) -> bool {
    let (lo, hi) = (except as usize * 4096, (except as usize + 1) * 4096);
    a.len() == b.len() && a[..lo] == b[..lo] && a[hi..] == b[hi..]
}

/// Free pages that hold headers C's walk follows (crafted here as they
/// arise from freed and reused runs): every page of the snapshot written
/// since the copy is still dumped, and the dump restores the source.
///
/// * A stale overflow header in the range whose run covers the head of a
///   live run that goes on past it: both runs are dumped (the live one
///   overlapping), not just the bytes up to the stale run's end.
/// * A stale overflow header older than the copy over the live run: not a
///   record, and the walk goes on page by page (C steps over the live run
///   and leaves it out).
/// * A stale header whose run ends past the snapshot, one of 0 pages (C:
///   `EFAULT`, an endless loop), and a page from a transaction after the
///   snapshot (a free page a writer rewrote): no record.
///
/// Where the crafted page itself is no record, the restored file lacks it
/// (it is free in the snapshot).
#[test]
fn every_page_of_the_snapshot_is_dumped() {
    let dir = tmp();
    let run = live_run(dir.path());
    let snapshot = run.base_txnid + 1;
    let (f, h, n) = (run.free, run.head, run.pages);
    let cases: [(&str, Vec<u8>, bool); 5] = [
        (
            "covers-head",
            header(f, snapshot, 0x04, (h - f + 1) as u32),
            true,
        ),
        (
            "old-over-run",
            header(f, 1, 0x04, (h + n - f) as u32),
            false,
        ),
        (
            "past-end",
            header(f, snapshot, 0x04, (run.next - f + 5) as u32),
            false,
        ),
        ("empty-run", header(f, snapshot, 0x04, 0), false),
        ("future", header(f, snapshot + 3, 0x02, 0), false),
    ];
    for (name, hdr, record) in cases {
        let (src, file) = crafted(&run, dir.path(), name, &hdr);
        let env = reopen(&src);
        assert_eq!(last_txnid(&env), snapshot);
        let d = dump(&env, run.base_txnid);
        drop(env);
        let recs = records(&d, 4096);
        assert_eq!(recs.iter().any(|r| r.0 == f), record, "{name}: {recs:?}");
        assert!(recs.contains(&(h, snapshot, n)), "{name}: {recs:?}");
        let got = restored(&run, dir.path(), &format!("{name}-restored"), &d);
        if record {
            assert!(got == file, "{name}");
        } else {
            assert!(same_but(&got, &file, f), "{name}");
        }
    }
}

/// A 1.0 environment with a few commits, and its data file.
fn small_env(dir: &Path, ps: usize) -> (Environment, Vec<u8>) {
    let env = v10(ps).open(dir).unwrap();
    let dbs = create_dbs(&env);
    work(&env, &dbs, &mut Rng(3), 8);
    let bytes = data(dir);
    (env, bytes)
}

/// A page header of 1.0: `mp_pgno`, `mp_txnid`, flags and `mp_pages`.
fn header(pgno: u64, txnid: u64, flags: u16, pages: u32) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&pgno.to_ne_bytes());
    h.extend_from_slice(&txnid.to_ne_bytes());
    h.extend_from_slice(&0u16.to_ne_bytes());
    h.extend_from_slice(&flags.to_ne_bytes());
    h.extend_from_slice(&pages.to_ne_bytes());
    h
}

/// A record of page `pgno`, or of an overflow run of `pages` pages.
fn record(ps: usize, pgno: u64, pages: Option<u32>) -> Vec<u8> {
    let mut r = match pages {
        Some(n) => header(pgno, 9, 0x04, n),
        None => header(pgno, 9, 0x02, 0),
    };
    r.resize(ps * pages.unwrap_or(1).max(1) as usize, 0x5a);
    r
}

/// A reader that fails after `ok` bytes.
struct Failing {
    data: Vec<u8>,
    ok: usize,
}

impl Read for Failing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.ok == 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO + 1));
        }
        let n = buf.len().min(self.ok).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data.drain(..n);
        self.ok -= n;
        Ok(n)
    }
}

/// Row 66: the load reports input C mishandles, never panics, and leaves
/// the file alone when it fails before the first page.
#[test]
fn the_load_refuses_malformed_input() {
    let ps = 1024;
    let dir = tmp();
    let (env, before) = small_env(dir.path(), ps);
    drop(env);
    let mut env = reopen(dir.path());
    let eio = |e: Error| {
        assert_eq!(code(&e), libc::EIO, "{e:?}");
        assert!(matches!(&e, Error::Io(io) if io.kind() == io::ErrorKind::UnexpectedEof));
    };
    let corrupted = |e: Error| assert!(matches!(e, Error::Mdb(MdbError::Corrupted)), "{e:?}");

    // Nothing to load.
    env.incr_load(&b""[..]).unwrap();
    assert!(data(dir.path()) == before);
    // A short header (C: success) and a short page (C: loops for ever).
    let page = record(ps, 5, None);
    eio(env.incr_load(&page[..10]).unwrap_err());
    assert!(data(dir.path()) == before);
    eio(env.incr_load(&page[..ps / 2]).unwrap_err());
    assert!(data(dir.path()) == before);
    // An overflow header of 0 pages (C: overruns its buffer).
    corrupted(env.incr_load(&record(ps, 5, Some(0))[..]).unwrap_err());
    // Runs and pages past the map (C: writes them, or fails with EFBIG).
    let huge = header(5, 9, 0x04, u32::MAX);
    corrupted(env.incr_load(&huge[..]).unwrap_err());
    for pgno in [1 << 40, u64::MAX / ps as u64 + 1, u64::MAX] {
        let mut r = header(pgno, 9, 0x02, 0);
        r.resize(ps, 0);
        corrupted(env.incr_load(&r[..]).unwrap_err());
    }
    assert!(data(dir.path()) == before);
    // A read error (C: success if in a header).
    let failing = Failing {
        data: page.clone(),
        ok: 10,
    };
    let e = env.incr_load(failing).unwrap_err();
    assert_eq!(code(&e), libc::EIO + 1, "{e:?}");
    assert!(data(dir.path()) == before);
    // A run cut short writes what came before it, then fails.
    let mut cut = record(ps, 5, Some(3));
    cut.truncate(2 * ps);
    eio(env.incr_load(&cut[..]).unwrap_err());
    assert!(data(dir.path())[5 * ps..6 * ps] == cut[..ps]);
}

/// `incr_load` of `dump` through the shared form the C API uses.
fn load_shared(env: &Environment, dump: &[u8]) -> Result<(), Error> {
    env.incr_load_shared(dump)
}

/// The load needs the environment to itself: transactions of this process
/// make it fail with `EBUSY`; a reset one may stay and sees the loaded
/// snapshot when it renews, as does every later transaction.
#[test]
fn the_load_needs_the_environment_to_itself() {
    let dir = tmp();
    let src_path = dir.path().join("src");
    std::fs::create_dir(&src_path).unwrap();
    let src = v10(4096).open(&src_path).unwrap();
    let dbs = create_dbs(&src);
    work(&src, &dbs, &mut Rng(11), 10);
    let base = copy(&src, dir.path(), "base");
    let t0 = last_txnid(&src);
    work(&src, &dbs, &mut Rng(12), 10);
    let d = dump(&src, t0);
    let t1 = last_txnid(&src);

    let env = Environment::builder().max_dbs(8).open(&base).unwrap();
    let busy = |r: Result<(), Error>| assert_eq!(code(&r.unwrap_err()), libc::EBUSY);
    // A read transaction of this handle.
    let txn = env.begin_ro_txn().unwrap();
    busy(load_shared(&env, &d));
    // A write transaction, of another thread.
    let (begun, end) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    std::thread::scope(|s| {
        let (b, e) = (Arc::clone(&begun), Arc::clone(&end));
        let env = &env;
        s.spawn(move || {
            let w = env.begin_rw_txn().unwrap();
            b.wait();
            e.wait();
            w.abort();
        });
        begun.wait();
        busy(load_shared(env, &d));
        end.wait();
    });
    // A reset one stays and renews on the loaded snapshot.
    let inactive = txn.reset();
    assert!(data(&base) != data(&src_path));
    load_shared(&env, &d).unwrap();
    assert!(data(&base) == data(&src_path));
    let txn = inactive.renew().unwrap();
    assert_eq!(txn.id(), t1);
    drop(txn);
    assert_eq!(env.begin_ro_txn().unwrap().id(), t1);
    // A write transaction builds on it.
    let mut w = env.begin_rw_txn().unwrap();
    assert_eq!(w.id(), t1 + 1);
    w.put(Dbi::MAIN, b"after", b"load", WriteFlags::empty())
        .unwrap();
    w.commit().unwrap();
    assert_eq!(last_txnid(&env), t1 + 1);
}

/// Without a lock file, every handle of the file in this process counts:
/// a read transaction of another handle, even a reset one, makes the load
/// fail; once there is none, the other handle sees the loaded snapshot.
#[test]
fn without_a_lock_file_other_handles_count() {
    let dir = tmp();
    let src_path = dir.path().join("src");
    std::fs::create_dir(&src_path).unwrap();
    let src = v10(4096).open(&src_path).unwrap();
    let dbs = create_dbs(&src);
    work(&src, &dbs, &mut Rng(21), 10);
    let base = copy(&src, dir.path(), "base");
    let t0 = last_txnid(&src);
    work(&src, &dbs, &mut Rng(22), 10);
    let d = dump(&src, t0);
    let t1 = last_txnid(&src);

    let open = || {
        Environment::builder()
            .max_dbs(8)
            .flags(EnvironmentFlags::NO_LOCK)
            .open(&base)
            .unwrap()
    };
    let mut a = open();
    let b = open();
    let busy = |r: Result<(), Error>| assert_eq!(code(&r.unwrap_err()), libc::EBUSY);
    let txn = b.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), t0);
    busy(a.incr_load(&d[..]));
    let inactive = txn.reset();
    busy(a.incr_load(&d[..]));
    drop(inactive);
    a.incr_load(&d[..]).unwrap();
    assert_eq!(b.begin_ro_txn().unwrap().id(), t1);
    assert_eq!(a.begin_ro_txn().unwrap().id(), t1);
    assert!(data(&base) == data(&src_path));
}

/// A dump whose meta pages raise the map size: the load writes the pages
/// past the loading handle's map, whose next transaction then fails with
/// `MDB_MAP_RESIZED` until the map size is adopted, as for a commit of
/// another process.
#[test]
fn the_load_may_grow_the_file_past_the_map() {
    let dir = tmp();
    let src_path = dir.path().join("src");
    std::fs::create_dir(&src_path).unwrap();
    let src = v10(4096).map_size(1 << 20).open(&src_path).unwrap();
    let mut txn = src.begin_rw_txn().unwrap();
    for i in 0..100u32 {
        txn.put(Dbi::MAIN, &i.to_be_bytes(), &[1; 600], WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    let base = copy(&src, dir.path(), "base");
    let t0 = last_txnid(&src);
    src.set_map_size(16 << 20).unwrap();
    for chunk in 0..10u32 {
        let mut txn = src.begin_rw_txn().unwrap();
        for i in 0..200u32 {
            let k = (1000 + chunk * 200 + i).to_be_bytes();
            txn.put(Dbi::MAIN, &k, &[2; 600], WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    let d = dump(&src, t0);
    assert!(data(&src_path).len() > 1 << 20);

    let mut env = reopen(&base);
    assert_eq!(env.map_size(), 1 << 20);
    env.incr_load(&d[..]).unwrap();
    assert!(data(&base) == data(&src_path));
    let e = env.begin_ro_txn().unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::MapResized)), "{e:?}");
    env.set_map_size(0).unwrap();
    assert_eq!(env.map_size(), 16 << 20);
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.stat(Dbi::MAIN).unwrap().entries, 2100);
}

/// A dump taken while another thread commits shows the snapshot of its
/// start: loaded into the base copy, it gives an environment that reads
/// whole, holding a prefix of the writer's records that matches its txnid.
#[test]
fn a_dump_while_writing_is_a_snapshot() {
    let dir = tmp();
    let src_path = dir.path().join("src");
    std::fs::create_dir(&src_path).unwrap();
    let src = v10(1024).open(&src_path).unwrap();
    let mut txn = src.begin_rw_txn().unwrap();
    txn.put(Dbi::MAIN, &0u64.to_be_bytes(), b"0", WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
    let base = copy(&src, dir.path(), "base");
    let t0 = last_txnid(&src);

    // Txn t0 + i adds key i.
    let (stop_tx, stop) = mpsc::channel::<()>();
    let dumped = std::thread::scope(|s| {
        let src = &src;
        let writer = s.spawn(move || {
            let mut i = 1u64;
            while stop.try_recv().is_err() && i < 3000 {
                let mut txn = src.begin_rw_txn().unwrap();
                let v = i.to_string();
                txn.put(
                    Dbi::MAIN,
                    &i.to_be_bytes(),
                    v.as_bytes(),
                    WriteFlags::empty(),
                )
                .unwrap();
                txn.commit().unwrap();
                i += 1;
            }
        });
        let mut dumps = Vec::new();
        for _ in 0..5 {
            dumps.push(dump(src, t0));
        }
        stop_tx.send(()).unwrap();
        writer.join().unwrap();
        dumps
    });
    for (n, d) in dumped.iter().enumerate() {
        let restored = dir.path().join(format!("r{n}"));
        std::fs::create_dir(&restored).unwrap();
        std::fs::copy(base.join("data.mdb"), restored.join("data.mdb")).unwrap();
        let mut env = reopen(&restored);
        env.incr_load(&d[..]).unwrap();
        env.set_map_size(0).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(Dbi::MAIN).unwrap();
        let mut expect = 0u64;
        while let Some((k, v)) = c.next().unwrap() {
            assert_eq!(k, expect.to_be_bytes(), "dump {n}");
            assert_eq!(v, expect.to_string().as_bytes(), "dump {n}");
            expect += 1;
        }
        assert_eq!(txn.id(), t0 + expect - 1, "dump {n}");
    }
}

/// `lmdb.h` asks for `MDB_WRITEMAP`; neither C's load nor this one needs
/// it, and with it the load writes the same file (pages go through the
/// file either way). OpenBSD refuses a write open with the flag.
#[cfg(not(target_os = "openbsd"))]
#[test]
fn the_load_writes_the_same_with_write_map() {
    let dir = tmp();
    let src_path = dir.path().join("src");
    std::fs::create_dir(&src_path).unwrap();
    let src = v10(4096).open(&src_path).unwrap();
    let dbs = create_dbs(&src);
    work(&src, &dbs, &mut Rng(31), 10);
    let base = copy(&src, dir.path(), "base");
    let t0 = last_txnid(&src);
    work(&src, &dbs, &mut Rng(32), 10);
    let d = dump(&src, t0);
    for write_map in [false, true] {
        let target = dir.path().join(format!("t{write_map}"));
        std::fs::create_dir(&target).unwrap();
        std::fs::copy(base.join("data.mdb"), target.join("data.mdb")).unwrap();
        let flags = if write_map {
            EnvironmentFlags::WRITE_MAP
        } else {
            EnvironmentFlags::empty()
        };
        let mut env = Environment::builder()
            .max_dbs(8)
            .flags(flags)
            .open(&target)
            .unwrap();
        env.incr_load(&d[..]).unwrap();
        assert_eq!(env.begin_ro_txn().unwrap().id(), last_txnid(&src));
        drop(env);
        assert!(data(&target) == data(&src_path), "WRITE_MAP: {write_map}");
    }
}

/// A read-only environment: an empty input loads, and the first page
/// fails with the error of the write, as in C (`EBADF` on Unix, whose
/// descriptor is read-only); nothing changes.
#[test]
fn a_read_only_environment_fails_at_the_first_write() {
    let dir = tmp();
    let (env, before) = small_env(dir.path(), 4096);
    let d = dump(&env, 1);
    drop(env);
    let mut ro = Environment::builder()
        .max_dbs(8)
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir.path())
        .unwrap();
    ro.incr_load(&b""[..]).unwrap();
    let e = ro.incr_load(&d[..]).unwrap_err();
    if cfg!(unix) {
        assert_eq!(code(&e), libc::EBADF, "{e:?}");
    }
    assert!(matches!(e, Error::Io(_)), "{e:?}");
    drop(ro);
    assert!(data(dir.path()) == before);
}

/// What the databases of [`work`] (and the main DB's records of the named
/// ones) hold in the newest snapshot of `env`.
fn contents(env: &Environment) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
    let txn = env.begin_ro_txn().unwrap();
    let mut out = Vec::new();
    for (i, name) in [None, Some("dup"), Some("fix")].into_iter().enumerate() {
        let db = txn.open_db(name).unwrap();
        let mut c = txn.cursor(db).unwrap();
        while let Some((k, v)) = c.next().unwrap() {
            out.push((i as u8, k.to_vec(), v.to_vec()));
        }
    }
    out
}

/// A source after 30 write transactions, its copy and contents then, and
/// after `commits` more (some may change nothing, and commit no txnid) the
/// dump since the copy.
struct Increment {
    src: std::path::PathBuf,
    base: std::path::PathBuf,
    base_txnid: u64,
    base_contents: Vec<(u8, Vec<u8>, Vec<u8>)>,
    dump: Vec<u8>,
    /// The source's newest txnid, the dump's snapshot.
    txnid: u64,
}

fn increment(dir: &Path, commits: usize) -> Increment {
    let src = dir.join("src");
    std::fs::create_dir(&src).unwrap();
    let env = v10(4096).open(&src).unwrap();
    let dbs = create_dbs(&env);
    let mut rng = Rng(41);
    work(&env, &dbs, &mut rng, 30);
    let base = copy(&env, dir, "base");
    let base_txnid = last_txnid(&env);
    let base_contents = contents(&env);
    work(&env, &dbs, &mut rng, commits);
    let dump = dump(&env, base_txnid);
    let txnid = last_txnid(&env);
    assert!(txnid > base_txnid && txnid <= base_txnid + commits as u64);
    Increment {
        src,
        base,
        base_txnid,
        base_contents,
        dump,
        txnid,
    }
}

/// A copy of the base in `dir/name`, loaded with `input`: the result.
fn load_copy(
    inc: &Increment,
    dir: &Path,
    name: &str,
    input: &[u8],
) -> (std::path::PathBuf, Result<(), Error>) {
    let path = dir.join(name);
    std::fs::create_dir(&path).unwrap();
    std::fs::copy(inc.base.join("data.mdb"), path.join("data.mdb")).unwrap();
    let rc = reopen(&path).incr_load(input);
    (path, rc)
}

/// Row 65: the meta pages are written last, after a sync, so a load that
/// stops inside a page leaves the base copy's meta pages, and with an
/// increment of two commits (which cannot reuse a page the base tree
/// names) the base snapshot whole. A stream cut at a record boundary
/// cannot be told from a whole one (C's format has neither a count nor a
/// checksum): the load commits the new snapshot without the dropped page.
/// Loading the whole dump again completes either.
#[test]
fn an_interrupted_load_keeps_the_base_snapshot() {
    let dir = tmp();
    let inc = increment(dir.path(), 2);
    let recs = records(&inc.dump, 4096);
    let last = recs.last().unwrap();
    assert!(recs.len() > 3 && last.0 >= 2, "{recs:?}");
    let source = data(&inc.src);

    // Cut inside the last page: an error, and the base snapshot.
    let cut = inc.dump.len() - 2048;
    let (path, rc) = load_copy(&inc, dir.path(), "cut", &inc.dump[..cut]);
    let e = rc.unwrap_err();
    assert_eq!(code(&e), libc::EIO, "{e:?}");
    let env = reopen(&path);
    assert_eq!(last_txnid(&env), inc.base_txnid);
    assert_eq!(env.begin_ro_txn().unwrap().id(), inc.base_txnid);
    assert!(contents(&env) == inc.base_contents);
    let mut env = env;
    env.incr_load(&inc.dump[..]).unwrap();
    drop(env);
    assert!(data(&path) == source);

    // The last record dropped: a "whole" stream, the new snapshot, the
    // dropped page as the base copy had it.
    let boundary = inc.dump.len() - last.2 as usize * 4096;
    let (path, rc) = load_copy(&inc, dir.path(), "dropped", &inc.dump[..boundary]);
    rc.unwrap();
    assert_eq!(last_txnid(&reopen(&path)), inc.txnid);
    let got = data(&path);
    let at = last.0 as usize * 4096;
    assert!(got.len() < at + 4096 || got[at..at + 4096] != source[at..at + 4096]);
    reopen(&path).incr_load(&inc.dump[..]).unwrap();
    assert!(data(&path) == source);
}

/// With more commits since the base, the increment may hold pages the base
/// tree still names (LMDB reuses a page from the third commit after it
/// was freed on): a load cut inside a page leaves the base meta pages, but
/// not necessarily the base tree. Loading the whole dump again makes the
/// file the source's.
#[test]
fn a_longer_interrupted_increment_needs_the_dump_again() {
    let dir = tmp();
    let inc = increment(dir.path(), 20);
    let cut = inc.dump.len() - 2048;
    let (path, rc) = load_copy(&inc, dir.path(), "cut", &inc.dump[..cut]);
    assert_eq!(code(&rc.unwrap_err()), libc::EIO);
    assert!(data(&path)[..2 * 4096] == data(&inc.base)[..2 * 4096]);
    assert_eq!(last_txnid(&reopen(&path)), inc.base_txnid);
    reopen(&path).incr_load(&inc.dump[..]).unwrap();
    assert!(data(&path) == data(&inc.src));
}

/// Two dumps back to back in one stream: each dump's meta pages are
/// written once its data pages are (a meta page after data pages begins
/// the next dump), so a stream cut inside the second leaves the first
/// loaded; the whole stream gives what the two loads in turn give.
#[test]
fn dumps_back_to_back_commit_one_by_one() {
    let dir = tmp();
    let src = dir.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let env = v10(4096).open(&src).unwrap();
    let dbs = create_dbs(&env);
    let mut rng = Rng(51);
    work(&env, &dbs, &mut rng, 30);
    let base = copy(&env, dir.path(), "base");
    let t0 = last_txnid(&env);
    work(&env, &dbs, &mut rng, 2);
    let (t1, at_t1) = (last_txnid(&env), contents(&env));
    let d1 = dump(&env, t0);
    work(&env, &dbs, &mut rng, 2);
    let d2 = dump(&env, t1);
    drop(env);
    let both = [&d1[..], &d2[..]].concat();

    let target = |name: &str| {
        let path = dir.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::copy(base.join("data.mdb"), path.join("data.mdb")).unwrap();
        path
    };
    let cut = target("cut");
    let e = reopen(&cut)
        .incr_load(&both[..both.len() - 2048])
        .unwrap_err();
    assert_eq!(code(&e), libc::EIO);
    let env = reopen(&cut);
    assert_eq!(last_txnid(&env), t1);
    assert!(contents(&env) == at_t1);
    drop(env);
    let whole = target("whole");
    reopen(&whole).incr_load(&both[..]).unwrap();
    assert!(data(&whole) == data(&src));
}
