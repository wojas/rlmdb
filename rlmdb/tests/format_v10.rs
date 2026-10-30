//! LMDB 1.0 environments (`MDB_DATA_VERSION 3`) through the Rust API:
//! choosing the format at open, the page size of a new environment, the
//! per-environment key size limit, what an existing file decides, the 1.0
//! features that are refused (encryption, page checksums), and contents
//! that survive a reopen at every page size 1.0 allows.
//!
//! That the files are the ones C LMDB 1.0 writes, and that C 1.0 reads
//! them, is checked across processes in `interop` (`write_differential10.rs`,
//! `format10.rs`).

use std::collections::BTreeMap;
use std::path::Path;

use rlmdb::consts::{MDB_ADDR_BUSY, MDB_ENCRYPT};
use rlmdb::{
    sniff_format, DatabaseFlags, Environment, EnvironmentBuilder, EnvironmentFlags, Error, Format,
    MdbError, Transaction, WriteFlags,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-v10-")
        .tempdir()
        .unwrap()
}

fn v10(page_size: usize) -> EnvironmentBuilder {
    let mut b = Environment::builder();
    b.format(Format::V10)
        .page_size(page_size)
        .max_dbs(8)
        .map_size(256 << 20);
    b
}

/// `me_nodemax - (NODESIZE + sizeof(MDB_db))` of a 1.0 environment.
fn max_key(page_size: usize) -> usize {
    (((page_size - 24) / 2) & !1) - 2 - 56
}

#[test]
fn new_environment_and_reopen() {
    for ps in [256, 512, 1024, 4096, 16384, 65536] {
        let dir = tmp();
        let env = v10(ps).open(dir.path()).unwrap();
        assert_eq!(env.format(), Format::V10);
        assert_eq!(env.page_size(), ps);
        assert_eq!(env.max_key_size(), max_key(ps), "page size {ps}");
        assert_eq!(env.stat().unwrap().page_size as usize, ps);
        drop(env);
        // The first meta page: 24-byte header, P_META, magic and version 3.
        let data = std::fs::read(dir.path().join("data.mdb")).unwrap();
        assert_eq!(data.len(), 2 * ps);
        assert_eq!(u16::from_ne_bytes([data[18], data[19]]), 0x08);
        assert_eq!(&data[24..28], &0xBEEF_C0DEu32.to_ne_bytes());
        assert_eq!(&data[28..32], &3u32.to_ne_bytes());
        assert_eq!(
            sniff_format(dir.path(), EnvironmentFlags::empty()).unwrap(),
            Some(Format::V10)
        );
        // Without a request the file decides; its page size wins over a
        // requested one, as in C.
        let env = Environment::builder().open(dir.path()).unwrap();
        assert_eq!((env.format(), env.page_size()), (Format::V10, ps));
        drop(env);
        let other = if ps == 4096 { 1024 } else { 4096 };
        let env = v10(other).open(dir.path()).unwrap();
        assert_eq!(env.page_size(), ps);
        drop(env);
        let err = Environment::builder()
            .format(Format::V09)
            .open(dir.path())
            .unwrap_err();
        assert!(
            matches!(err, Error::Mdb(MdbError::VersionMismatch)),
            "{err:?}"
        );
    }
}

#[test]
fn default_page_size_is_the_os_page_size() {
    let dir = tmp();
    let env = Environment::builder()
        .format(Format::V10)
        .open(dir.path())
        .unwrap();
    // SAFETY: sysconf has no preconditions.
    let os = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert_eq!(env.page_size(), os.min(65536));
    assert_eq!(env.max_key_size(), max_key(env.page_size()));
}

#[test]
fn page_size_is_validated() {
    for bad in [0, 128, 255, 1000, 3000, 131072] {
        let dir = tmp();
        let err = v10(bad).open(dir.path()).unwrap_err();
        assert_eq!(err.to_err_code(), libc::EINVAL, "page size {bad}");
        assert!(!dir.path().join("lock.mdb").exists());
        assert!(!dir.path().join("data.mdb").exists());
    }
    // LMDB 0.9 has no mdb_env_set_pagesize: EINVAL, explicit or default.
    let dir = tmp();
    let err = Environment::builder()
        .format(Format::V09)
        .page_size(4096)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    let err = Environment::builder()
        .page_size(4096)
        .open(dir.path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL);
    assert!(!dir.path().join("data.mdb").exists());
}

#[test]
fn key_size_limit_follows_the_page_size() {
    for ps in [1024, 4096] {
        let dir = tmp();
        let env = v10(ps).open(dir.path()).unwrap();
        let mk = env.max_key_size();
        let mut txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::CREATE).unwrap();
        let dup = txn
            .create_db(Some("dup"), DatabaseFlags::DUP_SORT | DatabaseFlags::CREATE)
            .unwrap();
        txn.put(db, &vec![b'k'; mk], b"v", WriteFlags::empty())
            .unwrap();
        let err = txn
            .put(db, &vec![b'k'; mk + 1], b"v", WriteFlags::empty())
            .unwrap_err();
        assert!(matches!(err, Error::Mdb(MdbError::BadValSize)), "{err:?}");
        // A DUPSORT data item is a key of the sub-DB.
        txn.put(dup, b"k", &vec![b'd'; mk], WriteFlags::empty())
            .unwrap();
        let err = txn
            .put(dup, b"k", &vec![b'd'; mk + 1], WriteFlags::empty())
            .unwrap_err();
        assert!(matches!(err, Error::Mdb(MdbError::BadValSize)), "{err:?}");
        txn.commit().unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(db, &vec![b'k'; mk]).unwrap(), Some(&b"v"[..]));
    }
}

/// The main DB's keys, as stored.
fn main_keys(env: &Environment) -> Vec<Vec<u8>> {
    let txn = env.begin_ro_txn().unwrap();
    let main = txn.open_db(None).unwrap();
    let mut c = txn.cursor(main).unwrap();
    let mut keys = Vec::new();
    while let Some((k, _)) = c.next().unwrap() {
        keys.push(k.to_vec());
    }
    keys
}

#[test]
fn named_database_keys_keep_the_nul_in_1_0() {
    // C 1.0's mdb_dbi_open stores strlen(name) + 1 bytes, 0.9 strlen(name).
    for (format, key) in [(Format::V09, &b"named"[..]), (Format::V10, b"named\0")] {
        let dir = tmp();
        let env = Environment::builder()
            .format(format)
            .max_dbs(4)
            .open(dir.path())
            .unwrap();
        env.create_db(Some("named"), DatabaseFlags::CREATE).unwrap();
        assert_eq!(main_keys(&env), vec![key.to_vec()], "{format:?}");
        let db = env.open_db(Some("named")).unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(db, b"a", b"b", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        drop(env);
        let env = Environment::builder().max_dbs(4).open(dir.path()).unwrap();
        let db = env.open_db(Some("named")).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.get(db, b"a").unwrap(), Some(&b"b"[..]));
        drop(txn);
        // Dropping the database removes the key with its NUL.
        let mut txn = env.begin_rw_txn().unwrap();
        txn.drop_db(db).unwrap();
        txn.commit().unwrap();
        assert!(main_keys(&env).is_empty());
    }
}

/// Emptying the main DB of an environment with named databases: LMDB 0.9
/// leaves their pages behind (`KNOWN_C_BUGS.md` #21, ported), LMDB 1.0
/// deletes them first and refuses while one is open (C 1.0's
/// `mdb_drop`). That the files are C 1.0's, page for page, is
/// `interop/tests/tools10.rs` (`mdb_drop_matches_mdb_drop10`).
#[test]
fn emptying_the_main_db_with_named_dbs() {
    for format in [Format::V09, Format::V10] {
        let dir = tmp();
        let env = Environment::builder()
            .format(format)
            .max_dbs(4)
            .map_size(64 << 20)
            .open(dir.path())
            .unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        let mut handles = Vec::new();
        for name in ["a", "b", "c"] {
            let db = txn.create_db(Some(name), DatabaseFlags::CREATE).unwrap();
            handles.push(db);
            for i in 0..300u32 {
                txn.put(
                    db,
                    format!("{name}{i:05}").as_bytes(),
                    &[7; 40],
                    WriteFlags::empty(),
                )
                .unwrap();
            }
        }
        txn.commit().unwrap();
        let used = env.info().unwrap().last_pgno + 1;
        let free_before = env.freelist().unwrap();
        // The handles stay open after the commit, as in C.
        let mut txn = env.begin_rw_txn().unwrap();
        let rc = txn.clear_db(rlmdb::Dbi::MAIN);
        if format == Format::V10 {
            // A named database is open: C 1.0 says MDB_DBIS_BUSY, until the
            // last one is closed.
            assert!(matches!(rc, Err(Error::Mdb(MdbError::DbisBusy))), "{rc:?}");
            drop(txn);
            for db in handles.drain(1..) {
                env.close_db(db);
            }
            let mut txn = env.begin_rw_txn().unwrap();
            let rc = txn.clear_db(rlmdb::Dbi::MAIN);
            assert!(matches!(rc, Err(Error::Mdb(MdbError::DbisBusy))), "{rc:?}");
            drop(txn);
            env.close_db(handles[0]);
            let mut txn = env.begin_rw_txn().unwrap();
            txn.clear_db(rlmdb::Dbi::MAIN).unwrap();
            txn.commit().unwrap();
            // Every page but the two metas and the free DB's is free.
            let free = env.freelist().unwrap();
            assert!(free + 4 >= used, "{free} of {used} pages free");
            assert!(env.open_db(Some("b")).unwrap_err().not_found());
            // An empty main DB: nothing to do, no commit is needed.
            let txnid = env.info().unwrap().last_txnid;
            let mut txn = env.begin_rw_txn().unwrap();
            txn.clear_db(rlmdb::Dbi::MAIN).unwrap();
            txn.commit().unwrap();
            assert_eq!(env.info().unwrap().last_txnid, txnid);
        } else {
            rc.unwrap();
            txn.commit().unwrap();
            // The named databases' pages leak: only the main DB's leaf is
            // freed.
            assert!(env.freelist().unwrap() <= free_before + 3);
            assert!(main_keys(&env).is_empty());
        }
    }
}

/// Patches `len` bytes at `off` of the data file of `dir`.
fn patch(dir: &Path, off: usize, bytes: &[u8]) {
    let path = dir.join("data.mdb");
    let mut data = std::fs::read(&path).unwrap();
    data[off..off + bytes.len()].copy_from_slice(bytes);
    std::fs::write(&path, data).unwrap();
}

#[test]
fn encryption_and_checksums_are_refused() {
    let ps = 4096;
    // MDB_ENCRYPT in meta page 0's mm_flags (mm_dbs[FREE_DBI].md_flags, at
    // 24 + 24 + 4).
    let dir = tmp();
    drop(v10(ps).open(dir.path()).unwrap());
    let off = 24 + 24 + 4;
    let data = std::fs::read(dir.path().join("data.mdb")).unwrap();
    let flags = u16::from_ne_bytes([data[off], data[off + 1]]) | MDB_ENCRYPT as u16;
    patch(dir.path(), off, &flags.to_ne_bytes());
    let err = Environment::builder().open(dir.path()).unwrap_err();
    assert!(
        matches!(err, Error::Mdb(MdbError::EnvEncryption)),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "MDB_ENV_ENCRYPTION: Environment encryption mismatch"
    );

    // A checksum size in the last two bytes of page 0.
    let dir = tmp();
    drop(v10(ps).open(dir.path()).unwrap());
    patch(dir.path(), ps - 2, &8u16.to_ne_bytes());
    let err = Environment::builder().open(dir.path()).unwrap_err();
    assert!(matches!(err, Error::Mdb(MdbError::BadChecksum)), "{err:?}");
    assert_eq!(
        Error::from_err_code(MDB_ADDR_BUSY).to_string(),
        "MDB_ADDR_BUSY: Requested map address is unavailable"
    );
}

#[test]
fn sniffing() {
    let dir = tmp();
    let flags = EnvironmentFlags::empty();
    assert_eq!(sniff_format(dir.path(), flags).unwrap(), None);
    std::fs::write(dir.path().join("data.mdb"), b"").unwrap();
    assert_eq!(sniff_format(dir.path(), flags).unwrap(), None);
    // NO_SUB_DIR: the path is the data file.
    let file = dir.path().join("env.mdb");
    assert_eq!(
        sniff_format(&file, EnvironmentFlags::NO_SUB_DIR).unwrap(),
        None
    );
    let env = Environment::builder()
        .format(Format::V10)
        .flags(EnvironmentFlags::NO_SUB_DIR)
        .open(&file)
        .unwrap();
    drop(env);
    assert_eq!(
        sniff_format(&file, EnvironmentFlags::NO_SUB_DIR).unwrap(),
        Some(Format::V10)
    );
    let env = Environment::builder()
        .flags(EnvironmentFlags::NO_SUB_DIR)
        .open(&file)
        .unwrap();
    assert_eq!(env.format(), Format::V10);
}

/// Opening a file whose first meta page is not one of the two formats
/// fails before the lock file exists, with the sniff's error.
#[test]
fn unknown_headers_are_refused_before_the_lock_file() {
    let fresh = |format: Format| {
        let dir = tmp();
        drop(
            Environment::builder()
                .format(format)
                .open(dir.path())
                .unwrap(),
        );
        std::fs::remove_file(dir.path().join("lock.mdb")).unwrap();
        dir
    };
    let cases: [(Format, usize, u32, MdbError); 4] = [
        // A 0.9-shaped header claiming version 3, a 1.0-shaped one claiming
        // 1: neither format.
        (Format::V09, 20, 3, MdbError::Invalid),
        (Format::V10, 28, 1, MdbError::Invalid),
        // Version 2 (lmdb-js prerelease) in either place.
        (Format::V09, 20, 2, MdbError::VersionMismatch),
        (Format::V10, 28, 2, MdbError::VersionMismatch),
    ];
    for (format, off, version, want) in cases {
        let dir = fresh(format);
        patch(dir.path(), off, &version.to_ne_bytes());
        let err = Environment::builder().open(dir.path()).unwrap_err();
        assert!(
            matches!(err, Error::Mdb(e) if e == want),
            "{format:?} version {version}: {err:?}"
        );
        assert!(!dir.path().join("lock.mdb").exists());
        let err = sniff_format(dir.path(), EnvironmentFlags::empty()).unwrap_err();
        assert!(matches!(err, Error::Mdb(e) if e == want));
    }
    let dir = tmp();
    std::fs::write(dir.path().join("data.mdb"), vec![0x5a; 8192]).unwrap();
    let err = Environment::builder()
        .format(Format::V10)
        .open(dir.path())
        .unwrap_err();
    assert!(matches!(err, Error::Mdb(MdbError::Invalid)), "{err:?}");
    assert!(!dir.path().join("lock.mdb").exists());
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

type Model = BTreeMap<(&'static str, Vec<u8>), Vec<Vec<u8>>>;

/// Writes a mix of plain, overflow, DUPSORT and DUPFIXED items with deletes
/// in several transactions (one of them nested and aborted, one spilling
/// with `dirty_limit`) and returns what should be in the databases.
fn fill(env: &Environment, seed: u64, dirty_limit: Option<usize>) -> Model {
    let ps = env.page_size() as u64;
    let mut rng = Rng(seed);
    let mut model: Model = BTreeMap::new();
    if let Some(n) = dirty_limit {
        env.set_dirty_limit(n);
    }
    let mut txn = env.begin_rw_txn().unwrap();
    let main = txn.create_db(None, DatabaseFlags::CREATE).unwrap();
    let dup = txn
        .create_db(Some("dup"), DatabaseFlags::DUP_SORT | DatabaseFlags::CREATE)
        .unwrap();
    let fix = txn
        .create_db(
            Some("fix"),
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED | DatabaseFlags::CREATE,
        )
        .unwrap();
    for round in 0..3 {
        for _ in 0..400 {
            let k = format!(
                "k{:05}{}",
                rng.below(700),
                "x".repeat(rng.below(9) as usize)
            );
            let len = if rng.below(10) == 0 {
                (ps + rng.below(3 * ps)) as usize
            } else {
                1 + rng.below(60) as usize
            };
            let v = vec![b'a' + rng.below(26) as u8; len];
            txn.put(main, k.as_bytes(), &v, WriteFlags::empty())
                .unwrap();
            model.insert(("main", k.into_bytes()), vec![v]);
            let dk = format!("d{:03}", rng.below(40)).into_bytes();
            let dv = format!(
                "{:06}{}",
                rng.below(5000),
                "y".repeat(rng.below(7) as usize)
            );
            txn.put(dup, &dk, dv.as_bytes(), WriteFlags::empty())
                .unwrap();
            let e = model.entry(("dup", dk)).or_default();
            if let Err(i) = e.binary_search(&dv.clone().into_bytes()) {
                e.insert(i, dv.into_bytes());
            }
            let fk = format!("f{:02}", rng.below(20)).into_bytes();
            let fv = rng.below(100_000).to_be_bytes().to_vec();
            txn.put(fix, &fk, &fv, WriteFlags::empty()).unwrap();
            let e = model.entry(("fix", fk)).or_default();
            if let Err(i) = e.binary_search(&fv) {
                e.insert(i, fv);
            }
        }
        // Deletes: whole keys and single duplicates.
        let victims: Vec<_> = model
            .keys()
            .filter(|_| rng.below(4) == 0)
            .cloned()
            .collect();
        for (db, k) in victims {
            let h = match db {
                "main" => main,
                "dup" => dup,
                _ => fix,
            };
            if db != "main" && rng.below(2) == 0 {
                let vals = model.get_mut(&(db, k.clone())).unwrap();
                let v = vals.remove(rng.below(vals.len() as u64) as usize);
                assert!(txn.del(h, &k, Some(&v)).unwrap());
                if vals.is_empty() {
                    model.remove(&(db, k));
                }
            } else {
                assert!(txn.del(h, &k, None).unwrap());
                model.remove(&(db, k));
            }
        }
        if round == 1 {
            // A nested transaction that is thrown away.
            let mut child = txn.begin_nested_txn().unwrap();
            for i in 0..200u32 {
                child
                    .put(
                        main,
                        format!("gone{i}").as_bytes(),
                        b"x",
                        WriteFlags::empty(),
                    )
                    .unwrap();
            }
            child.abort();
        }
        txn.commit().unwrap();
        txn = env.begin_rw_txn().unwrap();
    }
    txn.commit().unwrap();
    model
}

/// Every database's contents, read back.
fn read_back(env: &Environment) -> Model {
    let txn = env.begin_ro_txn().unwrap();
    let mut out: Model = BTreeMap::new();
    for name in ["main", "dup", "fix"] {
        let db = txn
            .open_db(if name == "main" { None } else { Some(name) })
            .unwrap();
        let mut c = txn.cursor(db).unwrap();
        while let Some((k, v)) = c.next().unwrap() {
            if name == "main" && (k == b"dup\0" || k == b"fix\0") {
                continue;
            }
            out.entry((name, k.to_vec())).or_default().push(v.to_vec());
        }
    }
    out
}

#[test]
fn contents_survive_reopen_at_every_page_size() {
    for ps in [1024, 4096, 16384, 65536] {
        let dir = tmp();
        let env = v10(ps).open(dir.path()).unwrap();
        let model = fill(&env, 0x5eed + ps as u64, None);
        assert_eq!(read_back(&env), model, "page size {ps}");
        drop(env);
        let env = Environment::builder()
            .max_dbs(8)
            .flags(EnvironmentFlags::READ_ONLY)
            .open(dir.path())
            .unwrap();
        assert_eq!(env.format(), Format::V10);
        assert_eq!(read_back(&env), model, "page size {ps}, reopened");
    }
}

/// Spilling (a dirty limit far below the transaction's size) and nested
/// transactions leave the same file as a transaction that does not spill:
/// the pages written early carry the same `mp_txnid` as the rest.
#[test]
fn spilling_leaves_the_same_file() {
    for ps in [1024, 4096] {
        let (a, b) = (tmp(), tmp());
        let env = v10(ps).open(a.path()).unwrap();
        let model = fill(&env, 0xabc, None);
        drop(env);
        let env = v10(ps).open(b.path()).unwrap();
        assert_eq!(fill(&env, 0xabc, Some(16)), model);
        assert_eq!(read_back(&env), model);
        drop(env);
        let x = std::fs::read(a.path().join("data.mdb")).unwrap();
        let y = std::fs::read(b.path().join("data.mdb")).unwrap();
        // Every page in use has the same header (pgno, txnid, flags,
        // lower/upper). Free pages may differ, and the files' lengths: the
        // spilling run wrote some pages that the transaction freed again.
        let (in_use, free) = page_usage(a.path());
        assert_eq!(page_usage(b.path()), (in_use, free.clone()));
        for p in (0..in_use).filter(|p| !free.contains(p)) {
            let r = p as usize * ps..p as usize * ps + 24;
            assert_eq!(x[r.clone()], y[r], "page size {ps}, header of page {p}");
        }
    }
}

/// The number of pages in use (`mm_last_pg + 1`) and the pages on the free
/// list of the environment at `dir`.
fn page_usage(dir: &Path) -> (u64, std::collections::BTreeSet<u64>) {
    let env = Environment::builder()
        .max_dbs(8)
        .flags(EnvironmentFlags::READ_ONLY)
        .open(dir)
        .unwrap();
    let last = env.info().unwrap().last_pgno as u64;
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(rlmdb::Dbi::FREE).unwrap();
    let mut free = std::collections::BTreeSet::new();
    while let Some((_, idl)) = c.next().unwrap() {
        let ids: Vec<u64> = idl
            .chunks(8)
            .map(|w| u64::from_ne_bytes(w.try_into().unwrap()))
            .collect();
        free.extend(&ids[1..]);
    }
    (last + 1, free)
}

/// With 1 KiB pages a new key's first duplicate can be too big for a node
/// and go straight to a new, empty sub-DB; `MDB_APPENDDUP` then appends to
/// that empty sub-DB. C reads a node out of bounds there (`KNOWN_C_BUGS.md`
/// #31); rlmdb appends at index 0.
#[test]
fn appenddup_into_a_new_sub_db() {
    let dir = tmp();
    let env = v10(1024).open(dir.path()).unwrap();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn
        .create_db(Some("d"), DatabaseFlags::DUP_SORT | DatabaseFlags::CREATE)
        .unwrap();
    // 8 + EVEN(407) + EVEN(89) = 506 > me_nodemax 498.
    let key = vec![b'k'; 407];
    let items: Vec<Vec<u8>> = (0..20u8).map(|i| vec![b'a' + i; 89]).collect();
    for item in &items {
        txn.put(db, &key, item, WriteFlags::APPEND_DUP).unwrap();
    }
    // Out of order: refused, as for any other append.
    let err = txn
        .put(db, &key, &items[3], WriteFlags::APPEND_DUP)
        .unwrap_err();
    assert!(matches!(err, Error::Mdb(MdbError::KeyExist)), "{err:?}");
    txn.commit().unwrap();
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    let mut got = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        assert_eq!(k, &key[..]);
        got.push(v.to_vec());
    }
    assert_eq!(got, items);
}
