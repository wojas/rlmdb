//! Incremental backups of LMDB 1.0 environments through the Rust API
//! (`Environment::incr_dump`, `incr_dump_to_path`, `incr_load`; C 1.0's
//! `mdb_env_incr_dumpfd`, `mdb_env_incr_dump`, `mdb_env_incr_loadfd`): a
//! chain of dumps turns an old copy, or an empty environment, into the
//! source byte for byte at every page size; the walk over free pages that
//! C has (`KNOWN_C_BUGS.md` row 72); malformed input to the load (row 66);
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

/// The dump's data pages come in file order, each where the one before
/// ends: the walk read no stale page as a header (row 72), so loading the
/// dump puts back exactly what the source holds.
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
/// enough pages that the free list keeps its records on its leaves. A
/// freed run, partly reused, makes the walk read stale pages as headers
/// (row 72), in C as here, and the restore then differs from the source.
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

/// An environment whose first overflow run (a value of `byte`s in the
/// named database "big", txn 2) is freed (txn 3) and partly reused by four
/// small transactions, as `interop/tests/incr10.rs` builds it with C.
fn freed_run(dir: &Path, byte: u8) -> Environment {
    let env = v10(4096).open(dir).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let big = txn.create_db(Some("big"), DatabaseFlags::CREATE).unwrap();
    txn.put(big, b"bigkey", &vec![byte; 100_000], WriteFlags::empty())
        .unwrap();
    txn.commit().unwrap();
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
    env
}

/// Row 61: the walk reads the pages of a freed overflow run as headers,
/// as C does. With 'a's (no `P_OVERFLOW` in the bytes taken for the
/// flags) each such page is a record of page 0x6161616161616161, which no
/// load can place; with 'e's the "run" reaches past the file and the dump
/// fails (C: `EFAULT`).
#[test]
fn the_walk_reads_stale_pages_as_headers() {
    let dir = tmp();
    let env = freed_run(dir.path(), b'a');
    let d = dump(&env, 1);
    let garbage = records(&d, 4096)
        .iter()
        .filter(|r| r.0 == 0x6161_6161_6161_6161)
        .count();
    assert!(garbage > 0, "{:x?}", records(&d, 4096));
    drop(env);
    let e = reopen(dir.path()).incr_load(&d[..]).unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::Corrupted)), "{e:?}");

    let dir = tmp();
    let env = freed_run(dir.path(), b'e');
    let mut out = Vec::new();
    let e = env.incr_dump(&mut out, 1).unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::Corrupted)), "{e:?}");
    // The pages before the stale one were written.
    assert!(!out.is_empty() && out.len() % 4096 == 0);
    // As from any txnid: the bytes taken for mp_txnid are huge.
    let e = env.incr_dump(Vec::new(), last_txnid(&env)).unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::Corrupted)), "{e:?}");
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
