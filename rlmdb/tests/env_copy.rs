//! `Environment::copy_to_path` / `copy_to_fd`, plain and compacting,
//! compared byte for byte with C's `mdb_copy` (`mdb_env_copy2` /
//! `mdb_env_copyfd2`) on the same environments, and checked with the C
//! tools: `mdb_dump` must give the source's data, `cpeer check-freelist`
//! must find every page accounted for.

mod common;

use std::fs;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle as _;
use std::path::Path;

use common::CTools;
use rlmdb::format::layout::{META_MAPSIZE, META_SIZE};
use rlmdb::format::Meta;
use rlmdb::{
    DatabaseFlags, Dbi, Environment, EnvironmentFlags, Format, RwTxn, Transaction, WriteFlags,
};

fn open_rw(path: &Path) -> Environment {
    Environment::builder()
        .map_size(256 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

/// Opens the source as `mdb_copy` does: read-only, map size from the file.
fn open_ro(path: &Path) -> Environment {
    Environment::builder()
        .max_dbs(8)
        .flags(EnvironmentFlags::READ_ONLY)
        .open(path)
        .unwrap()
}

/// `mdb_copy [-c] src` (to stdout).
fn c_copy(c: &CTools, src: &Path, compact: bool) -> Vec<u8> {
    let mut args = Vec::new();
    if compact {
        args.push("-c");
    }
    args.push(src.to_str().unwrap());
    c.run("mdb_copy", &args, None).stdout
}

fn first_difference(a: &[u8], b: &[u8], ps: usize) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => format!(
            "first difference at byte {i} (page {}, offset {})",
            i / ps,
            i % ps
        ),
        None => format!("lengths {} and {}", a.len(), b.len()),
    }
}

/// Copies `src` with Rust (to a new directory and to a file descriptor)
/// and with C, plain and compacting, and checks the copies.
fn check_copies(c: &CTools, src: &Path, dbs: &[Option<&str>]) {
    let ps = open_ro(src).page_size();
    for compact in [false, true] {
        let what = if compact { "compacting" } else { "plain" };
        let dst = tempfile::tempdir().unwrap();
        {
            let env = open_ro(src);
            env.copy_to_path(dst.path(), compact).unwrap();
            // The target must not exist.
            let e = env.copy_to_path(dst.path(), compact).unwrap_err();
            // EEXIST; ERROR_FILE_EXISTS from CreateFile(CREATE_NEW) on Windows.
            let exists = if cfg!(windows) { 80 } else { libc::EEXIST };
            assert_eq!(e.to_err_code(), exists, "{e:?}");
        }
        let rust = fs::read(dst.path().join("data.mdb")).unwrap();
        let via_fd = {
            let env = open_ro(src);
            let mut file = tempfile::tempfile().unwrap();
            #[cfg(unix)]
            env.copy_to_fd(file.as_fd(), compact).unwrap();
            #[cfg(windows)]
            env.copy_to_fd(file.as_handle(), compact).unwrap();
            std::io::Seek::rewind(&mut file).unwrap();
            let mut v = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut v).unwrap();
            v
        };
        let cbytes = c_copy(c, src, compact);
        assert!(
            rust == cbytes,
            "{what} copy differs from C's: {}",
            first_difference(&rust, &cbytes, ps)
        );
        assert!(
            rust == via_fd,
            "{what} copy to fd differs from copy to path"
        );

        // The copy is a valid environment with the same data.
        let p = dst.path().to_str().unwrap();
        c.peer_ok(&["check-freelist", p]);
        for db in dbs {
            assert!(
                c.dump(dst.path(), *db) == c.dump(src, *db),
                "{what} copy: dump of {db:?} differs"
            );
        }
        if compact {
            let env = open_ro(dst.path());
            assert_eq!(env.freelist().unwrap(), 0);
            let info = env.info().unwrap();
            let st = env.stat().unwrap();
            if st.entries > 0 {
                assert_eq!(info.last_txnid, 1);
            }
        }
    }
}

fn put(t: &mut RwTxn<'_>, db: Dbi, k: &[u8], v: &[u8]) {
    t.put(db, k, v, WriteFlags::empty()).unwrap();
}

fn value(i: usize, len: usize) -> Vec<u8> {
    (0..len).map(|j| (i * 31 + j * 7) as u8).collect()
}

/// The reported case: one 20000-byte value in the main DB, loaded by C.
#[test]
fn overflow_value_in_main_db_loaded_by_c() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("env");
    c.load(&env, None, &[], &[(b"k1".to_vec(), value(1, 20000))]);
    check_copies(c, &env, &[None]);
}

#[test]
fn overflow_values_in_main_and_named_dbs() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open_rw(dir.path());
        let mut t = env.begin_rw_txn().unwrap();
        for i in 0..40 {
            put(
                &mut t,
                Dbi::MAIN,
                format!("main{i:03}").as_bytes(),
                &value(i, 20 + i * 900),
            );
        }
        t.commit().unwrap();
        let mut t = env.begin_rw_txn().unwrap();
        for name in ["a", "b"] {
            let db = t.create_db(Some(name), DatabaseFlags::CREATE).unwrap();
            for i in 0..300 {
                let len = if i % 9 == 0 { 30000 + i } else { 10 + i % 200 };
                put(
                    &mut t,
                    db,
                    format!("{name}{i:04}").as_bytes(),
                    &value(i, len),
                );
            }
        }
        t.commit().unwrap();
    }
    check_copies(c, dir.path(), &[Some("a"), Some("b")]);
}

#[test]
fn dupsort_dupfixed_and_free_pages() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open_rw(dir.path());
        let mut t = env.begin_rw_txn().unwrap();
        let d = t
            .create_db(
                Some("dups"),
                DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
            )
            .unwrap();
        let f = t
            .create_db(
                Some("fixed"),
                DatabaseFlags::CREATE
                    | DatabaseFlags::DUP_SORT
                    | DatabaseFlags::DUP_FIXED
                    | DatabaseFlags::INTEGER_DUP,
            )
            .unwrap();
        let n = t
            .create_db(
                Some("int"),
                DatabaseFlags::CREATE | DatabaseFlags::INTEGER_KEY,
            )
            .unwrap();
        for i in 0..400usize {
            // Some keys with a sub-page, some with a sub-DB.
            for j in 0..(1 + (i % 7) * (i % 7) * 10) {
                put(
                    &mut t,
                    d,
                    format!("k{i:04}").as_bytes(),
                    &value(j, 20 + j % 50),
                );
                put(
                    &mut t,
                    f,
                    format!("k{:02}", i % 30).as_bytes(),
                    &((i * 1000 + j) as u64).to_ne_bytes(),
                );
            }
            put(&mut t, n, &(i as u64 * 3).to_ne_bytes(), &value(i, 100));
        }
        t.commit().unwrap();
        // Free some pages so the compacting copy has to renumber.
        let mut t = env.begin_rw_txn().unwrap();
        for i in (0..400usize).step_by(3) {
            t.del(d, format!("k{i:04}").as_bytes(), None).unwrap();
            t.del(n, &(i as u64 * 3).to_ne_bytes(), None).unwrap();
        }
        t.commit().unwrap();
    }
    check_copies(c, dir.path(), &[Some("dups"), Some("fixed"), Some("int")]);
}

/// More than two of C's 1 MiB write buffers, with multi-page overflow runs
/// at the hand-over points: the unused middle of a copied page holds what
/// an earlier page left in the buffer, and must match C's.
#[test]
fn large_environment_with_buffer_reuse() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open_rw(dir.path());
        for round in 0..4usize {
            let mut t = env.begin_rw_txn().unwrap();
            let db = t.create_db(Some("big"), DatabaseFlags::CREATE).unwrap();
            for i in 0..3000usize {
                let len = match i % 13 {
                    0 => 5000 + i * 3,
                    1 => 70000,
                    _ => 30 + (i * round) % 400,
                };
                put(
                    &mut t,
                    db,
                    format!("key{i:05}").as_bytes(),
                    &value(i + round, len),
                );
            }
            for i in (round..3000).step_by(5) {
                t.del(db, format!("key{i:05}").as_bytes(), None).unwrap();
            }
            t.commit().unwrap();
        }
        assert!(env.info().unwrap().last_pgno * env.page_size() > 4 << 20);
    }
    check_copies(c, dir.path(), &[Some("big")]);
}

#[test]
fn empty_environments() {
    let c = require_c!();
    // Brand new.
    let dir = tempfile::tempdir().unwrap();
    drop(open_rw(dir.path()));
    check_copies(c, dir.path(), &[None]);
    // Empty main DB with flags: meta 1 of the compacted copy has txnid 1.
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open_rw(dir.path());
        let mut t = env.begin_rw_txn().unwrap();
        t.create_db(None, DatabaseFlags::INTEGER_KEY).unwrap();
        t.commit().unwrap();
    }
    check_copies(c, dir.path(), &[None]);
    // Everything deleted again.
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open_rw(dir.path());
        let mut t = env.begin_rw_txn().unwrap();
        for i in 0..1000usize {
            put(&mut t, Dbi::MAIN, &i.to_be_bytes(), &value(i, 300));
        }
        t.commit().unwrap();
        let mut t = env.begin_rw_txn().unwrap();
        t.clear_db(Dbi::MAIN).unwrap();
        t.commit().unwrap();
    }
    check_copies(c, dir.path(), &[None]);
}

#[test]
fn no_sub_dir_copy_and_plain_copy_while_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flat.mdb");
    let env = Environment::builder()
        .map_size(16 << 20)
        .flags(EnvironmentFlags::NO_SUB_DIR | EnvironmentFlags::NO_SYNC)
        .open(&path)
        .unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    for i in 0..500usize {
        put(&mut t, Dbi::MAIN, &i.to_be_bytes(), &value(i, 3000));
    }
    t.commit().unwrap();
    // A reader holds an older snapshot; the copy shows the newest.
    let old = env.begin_ro_txn().unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    put(&mut t, Dbi::MAIN, b"new", b"value");
    t.commit().unwrap();
    for compact in [false, true] {
        let target = dir.path().join(format!("copy-{compact}.mdb"));
        env.copy_to_path(&target, compact).unwrap();
        let copy = Environment::builder()
            .flags(EnvironmentFlags::NO_SUB_DIR | EnvironmentFlags::READ_ONLY)
            .open(&target)
            .unwrap();
        let txn = copy.begin_ro_txn().unwrap();
        assert_eq!(
            rlmdb::Transaction::get(&txn, Dbi::MAIN, b"new").unwrap(),
            Some(&b"value"[..])
        );
        assert_eq!(copy.stat().unwrap().entries, 501);
    }
    drop(old);
}

/// A meta page newer than the snapshot a plain copy pins: what a commit
/// whose meta page reached the file while its write or sync failed leaves
/// behind (C on macOS after a failed `F_FULLFSYNC`; anyone when writing the
/// old txnid back fails too), forged here while the environment is open, so
/// that its lock file still names the older snapshot. The copy holds the
/// snapshot's pages, so it gives that page the snapshot's meta; C's copy
/// keeps the page, and reading it fails (`MDB_PAGE_NOTFOUND`). Once a
/// commit has overwritten the page, the copy is C's again.
#[test]
fn plain_copy_never_emits_a_meta_newer_than_its_snapshot() {
    for format in [Format::V09, Format::V10] {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Environment::builder();
        b.map_size(64 << 20)
            .flags(EnvironmentFlags::NO_SYNC)
            .format(format);
        if format == Format::V10 {
            b.page_size(1024);
        }
        let env = b.open(dir.path()).unwrap();
        let ps = env.page_size();
        let l = format.layout();
        for round in 0..3usize {
            let mut t = env.begin_rw_txn().unwrap();
            for i in round * 200..(round + 1) * 200 {
                put(&mut t, Dbi::MAIN, &i.to_be_bytes(), &value(i, 100));
            }
            t.commit().unwrap();
        }
        let txnid = env.info().unwrap().last_txnid as u64;
        let data = dir.path().join("data.mdb");
        let source = fs::read(&data).unwrap();
        let (own, other) = ((txnid & 1) as usize, (!txnid & 1) as usize);
        let meta_at = |i: usize| i * ps + l.meta_offset();
        let meta = Meta::read(&source[meta_at(own)..]).unwrap();
        assert_eq!(meta.txnid, txnid);

        // The next commit's meta page, naming pages past the file's end.
        let mut newer = meta;
        newer.txnid = txnid + 1;
        newer.last_pgno = meta.last_pgno + 40;
        newer.dbs[1].root = meta.last_pgno + 20;
        let mut bytes = [0u8; META_SIZE];
        newer.write(&mut bytes);
        let f = fs::OpenOptions::new().write(true).open(&data).unwrap();
        common::write_all_at(&f, &bytes, meta_at(other) as u64).unwrap();
        drop(f);
        assert_eq!(env.info().unwrap().last_txnid as u64, txnid + 1);
        assert_eq!(env.begin_ro_txn().unwrap().id(), txnid);

        let target = tempfile::tempdir().unwrap();
        env.copy_to_path(target.path(), false).unwrap();
        let copy = fs::read(target.path().join("data.mdb")).unwrap();
        // The source's bytes up to the snapshot's last page, but for the
        // part of the newer meta page that a commit writes, which is the
        // snapshot's.
        let mut expected = fs::read(&data).unwrap();
        expected.truncate(source.len().min((meta.last_pgno as usize + 1) * ps));
        let commit_part = |i: usize| meta_at(i) + META_MAPSIZE..meta_at(i) + META_SIZE;
        expected.copy_within(commit_part(own), commit_part(other).start);
        assert!(
            copy == expected,
            "{format:?}: {}",
            first_difference(&copy, &expected, ps)
        );
        {
            let copied = Environment::builder()
                .flags(EnvironmentFlags::READ_ONLY)
                .open(target.path())
                .unwrap();
            assert_eq!(copied.info().unwrap().last_txnid as u64, txnid);
            assert_eq!(copied.stat().unwrap().entries, 600);
            let t = copied.begin_ro_txn().unwrap();
            let mut c = t.cursor(Dbi::MAIN).unwrap();
            for i in 0..600usize {
                let (k, v) = c.next().unwrap().unwrap();
                assert_eq!((k, v), (&i.to_be_bytes()[..], &value(i, 100)[..]));
            }
            assert!(c.next().unwrap().is_none());
        }
        // C 0.9's copy keeps the newer page as it is.
        let c = (format == Format::V09).then(common::c_tools).flatten();
        if let Some(c) = c {
            let mut theirs = c_copy(c, dir.path(), false);
            assert!(theirs[commit_part(other)] == bytes[META_MAPSIZE..]);
            let theirs_dir = tempfile::tempdir().unwrap();
            fs::write(theirs_dir.path().join("data.mdb"), &theirs).unwrap();
            let copied = open_ro(theirs_dir.path());
            assert_eq!(copied.info().unwrap().last_txnid as u64, txnid + 1);
            let t = copied.begin_ro_txn().unwrap();
            let e = t.cursor(Dbi::MAIN).unwrap().next().unwrap_err();
            assert_eq!(e.to_err_code(), rlmdb::consts::MDB_PAGE_NOTFOUND, "{e:?}");
            drop(t);
            drop(copied);
            theirs.copy_within(commit_part(own), commit_part(other).start);
            assert!(theirs == copy, "{}", first_difference(&theirs, &copy, ps));
        }

        // The next commit overwrites the newer page; copies are C's again.
        let mut t = env.begin_rw_txn().unwrap();
        put(&mut t, Dbi::MAIN, b"next", b"value");
        t.commit().unwrap();
        assert_eq!(env.info().unwrap().last_txnid as u64, txnid + 1);
        let target = tempfile::tempdir().unwrap();
        env.copy_to_path(target.path(), false).unwrap();
        let copy = fs::read(target.path().join("data.mdb")).unwrap();
        if let Some(c) = c {
            let theirs = c_copy(c, dir.path(), false);
            assert!(theirs == copy, "{}", first_difference(&theirs, &copy, ps));
        }
        let copied = Environment::builder()
            .flags(EnvironmentFlags::READ_ONLY)
            .open(target.path())
            .unwrap();
        assert_eq!(copied.info().unwrap().last_txnid as u64, txnid + 1);
        assert_eq!(copied.stat().unwrap().entries, 601);
    }
}
