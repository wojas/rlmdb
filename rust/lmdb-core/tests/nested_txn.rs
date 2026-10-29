//! Nested write transactions: the scenarios of the interop C peer's
//! `nested-test` (rust/interop/c/cpeer_misc.c) written by Rust and checked
//! by C (and the other way round), plus the cursor and handle rules around
//! a child transaction.

mod common;

use common::CTools;
use lmdb_core::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error, MdbError, RwTxn,
    Transaction, WriteFlags,
};

const NESTED_N: usize = 2000;
const BIG_LEN: usize = 40000;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `make_big` of cpeer_misc.c: SplitMix64 words, little-endian.
fn big() -> Vec<u8> {
    let mut state = 5u64;
    let mut out = Vec::with_capacity(BIG_LEN);
    while out.len() < BIG_LEN {
        out.extend_from_slice(&splitmix64(&mut state).to_le_bytes());
    }
    out.truncate(BIG_LEN);
    out
}

fn open(path: &std::path::Path) -> Environment {
    Environment::builder()
        .map_size(64 << 20)
        .max_dbs(8)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

fn put(t: &mut RwTxn<'_>, db: Dbi, k: &str, v: &[u8]) {
    t.put(db, k.as_bytes(), v, WriteFlags::empty()).unwrap();
}

fn del(t: &mut RwTxn<'_>, db: Dbi, k: &str) {
    assert!(t.del(db, k.as_bytes(), None).unwrap(), "del {k}");
}

fn expect(t: &impl Transaction, db: Dbi, k: &str, v: Option<&[u8]>, at: &str) {
    assert_eq!(t.get(db, k.as_bytes()).unwrap(), v, "{at}: {k}");
}

fn entries(t: &impl Transaction, db: Dbi) -> usize {
    t.stat(db).unwrap().entries
}

fn expect_no_db(env: &Environment, name: &str) {
    let txn = env.begin_ro_txn().unwrap();
    assert!(
        txn.open_db(Some(name)).unwrap_err().not_found(),
        "{name} should not exist"
    );
}

/// `nested_write` of cpeer_misc.c, step by step.
fn nested_write(env: &Environment) {
    let big = big();
    let none: Option<&[u8]> = None;

    // 1. A committed child is visible to the parent and is persisted.
    let mut t = env.begin_rw_txn().unwrap();
    let db = t.create_db(Some("nested"), DatabaseFlags::CREATE).unwrap();
    put(&mut t, db, "k1", b"v1");
    let mut c = t.begin_nested_txn().unwrap();
    expect(&c, db, "k1", Some(b"v1"), "1/child");
    put(&mut c, db, "k2", b"v2");
    c.commit().unwrap();
    expect(&t, db, "k2", Some(b"v2"), "1/parent");
    t.commit().unwrap();

    // 2. An aborted child changes nothing, including overwrites.
    let mut t = env.begin_rw_txn().unwrap();
    put(&mut t, db, "k3", b"v3");
    let mut c = t.begin_nested_txn().unwrap();
    put(&mut c, db, "k4", b"v4");
    put(&mut c, db, "k1", b"v1b");
    expect(&c, db, "k3", Some(b"v3"), "2/child");
    c.abort();
    expect(&t, db, "k4", none, "2/parent");
    expect(&t, db, "k1", Some(b"v1"), "2/parent");
    t.commit().unwrap();

    // 3. Overflow pages, and a committed grandchild inside an aborted child.
    let mut t = env.begin_rw_txn().unwrap();
    let mut c = t.begin_nested_txn().unwrap();
    put(&mut c, db, "k5", &big);
    c.commit().unwrap();
    expect(&t, db, "k5", Some(&big), "3/parent");
    let mut c = t.begin_nested_txn().unwrap();
    del(&mut c, db, "k5");
    put(&mut c, db, "k6", b"v6");
    let mut g = c.begin_nested_txn().unwrap();
    put(&mut g, db, "k7", b"v7");
    del(&mut g, db, "k2");
    expect(&g, db, "k5", none, "3/grandchild");
    g.commit().unwrap();
    expect(&c, db, "k7", Some(b"v7"), "3/child");
    expect(&c, db, "k2", none, "3/child");
    c.abort();
    expect(&t, db, "k5", Some(&big), "3/parent");
    expect(&t, db, "k6", none, "3/parent");
    expect(&t, db, "k7", none, "3/parent");
    expect(&t, db, "k2", Some(b"v2"), "3/parent");
    t.commit().unwrap();

    // 4. A DB created in a committed child of an aborted parent is gone.
    let mut t = env.begin_rw_txn().unwrap();
    let mut c = t.begin_nested_txn().unwrap();
    let d2 = c.create_db(Some("nested2"), DatabaseFlags::CREATE).unwrap();
    put(&mut c, d2, "x", b"1");
    c.commit().unwrap();
    expect(&t, d2, "x", Some(b"1"), "4/parent");
    t.abort();
    expect_no_db(env, "nested2");

    // 5. Page splits: an aborted child with many keys leaves no trace; a
    //    committed child whose committed grandchild deletes half of them
    //    leaves the other half.
    let mut t = env.begin_rw_txn().unwrap();
    let mut c = t.begin_nested_txn().unwrap();
    for u in 0..NESTED_N {
        put(
            &mut c,
            db,
            &format!("n{u:05}"),
            format!("val{u:05}").as_bytes(),
        );
    }
    c.abort();
    expect(&t, db, "n00000", none, "5/parent");
    let mut c = t.begin_nested_txn().unwrap();
    for u in 0..NESTED_N {
        put(
            &mut c,
            db,
            &format!("n{u:05}"),
            format!("val{u:05}").as_bytes(),
        );
    }
    let mut g = c.begin_nested_txn().unwrap();
    for u in (0..NESTED_N).step_by(2) {
        del(&mut g, db, &format!("n{u:05}"));
    }
    g.commit().unwrap();
    c.commit().unwrap();
    assert_eq!(entries(&t, db), 4 + NESTED_N / 2, "5/parent");
    t.commit().unwrap();

    // 6. mdb_drop in children: emptying in an aborted child, deleting in a
    //    committed one.
    let mut t = env.begin_rw_txn().unwrap();
    let d3 = t.create_db(Some("nested3"), DatabaseFlags::CREATE).unwrap();
    for u in 0..10 {
        put(&mut t, d3, &format!("z{u}"), b"z");
    }
    t.commit().unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    let mut c = t.begin_nested_txn().unwrap();
    c.clear_db(d3).unwrap();
    assert_eq!(entries(&c, d3), 0, "6/child");
    c.abort();
    assert_eq!(entries(&t, d3), 10, "6/parent");
    let mut c = t.begin_nested_txn().unwrap();
    c.drop_db(d3).unwrap();
    c.commit().unwrap();
    t.commit().unwrap();
    expect_no_db(env, "nested3");
}

/// `nested_check` of cpeer_misc.c.
fn nested_check(env: &Environment) {
    let big = big();
    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("nested")).unwrap();
    for (k, v) in [("k1", "v1"), ("k2", "v2"), ("k3", "v3")] {
        expect(&txn, db, k, Some(v.as_bytes()), "check");
    }
    expect(&txn, db, "k5", Some(&big), "check");
    assert_eq!(entries(&txn, db), 4 + NESTED_N / 2);
    let mut want: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (b"k1".to_vec(), b"v1".to_vec()),
        (b"k2".to_vec(), b"v2".to_vec()),
        (b"k3".to_vec(), b"v3".to_vec()),
        (b"k5".to_vec(), big.clone()),
    ];
    for u in (1..NESTED_N).step_by(2) {
        want.push((
            format!("n{u:05}").into_bytes(),
            format!("val{u:05}").into_bytes(),
        ));
    }
    assert_eq!(common::walk_forward(&txn, db), want);
    drop(txn);
    expect_no_db(env, "nested2");
    expect_no_db(env, "nested3");
}

fn check_with_c(c: &CTools, path: &std::path::Path) {
    let p = path.to_str().unwrap();
    let out = c.peer_ok(&["nested-test", p, "--check"]);
    assert!(
        out.stdout.contains("nested ok mode=check"),
        "{}",
        out.stdout
    );
    c.peer_ok(&["check-freelist", p]);
}

#[test]
fn rust_nested_scenarios_pass_c_check() {
    let dir = tempfile::tempdir().unwrap();
    {
        let env = open(dir.path());
        nested_write(&env);
        nested_check(&env);
        // The write mode is repeatable and reaches the same state.
        nested_write(&env);
        nested_check(&env);
    }
    let c = require_c!();
    check_with_c(c, dir.path());
}

#[test]
fn c_nested_scenarios_pass_rust_check() {
    let c = require_c!();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().to_str().unwrap();
    c.peer_ok(&["create", p, &(64u64 << 20).to_string(), "8"]);
    let out = c.peer_ok(&["nested-test", p]);
    assert!(
        out.stdout.contains("nested ok mode=write"),
        "{}",
        out.stdout
    );
    let env = open(dir.path());
    nested_check(&env);
    // And Rust can continue where C left off.
    nested_write(&env);
    nested_check(&env);
    drop(env);
    check_with_c(c, dir.path());
}

/// The same scenarios on both sides give the same data and the same page
/// usage per database.
#[test]
fn nested_scenarios_match_c_page_counts() {
    let c = require_c!();
    let cdir = tempfile::tempdir().unwrap();
    let rdir = tempfile::tempdir().unwrap();
    let cp = cdir.path().to_str().unwrap();
    c.peer_ok(&["create", cp, &(64u64 << 20).to_string(), "8"]);
    c.peer_ok(&["nested-test", cp]);
    nested_write(&open(rdir.path()));
    assert_eq!(
        c.dump(cdir.path(), Some("nested")),
        c.dump(rdir.path(), Some("nested"))
    );
    let cs = common::parse_stat(&c.stat_text(cdir.path(), &["-a", "-f"]));
    let rs = common::parse_stat(&c.stat_text(rdir.path(), &["-a", "-f"]));
    assert_eq!(cs.dbs, rs.dbs);
}

fn cursor_at(t: &mut RwTxn<'_>, id: lmdb_core::CursorId) -> Option<(Vec<u8>, Vec<u8>)> {
    t.cursor_get(id, None, None, CursorOp::GetCurrent)
        .unwrap()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
}

#[test]
fn parent_cursors_follow_child_commit_and_restore_on_abort() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path());
    let mut t = env.begin_rw_txn().unwrap();
    let db = t.create_db(Some("c"), DatabaseFlags::CREATE).unwrap();
    for i in 0..500u32 {
        t.put(db, &i.to_be_bytes(), &[7; 100], WriteFlags::empty())
            .unwrap();
    }
    t.commit().unwrap();

    let mut t = env.begin_rw_txn().unwrap();
    let cur = t.open_cursor(db).unwrap();
    t.cursor_get(cur, Some(&250u32.to_be_bytes()), None, CursorOp::Set)
        .unwrap()
        .unwrap();
    let at250 = Some((250u32.to_be_bytes().to_vec(), vec![7; 100]));

    // The child's splits and page copies move the parent's cursor along.
    // As in C, the parent's cursor can be used through the child.
    let mut c = t.begin_nested_txn().unwrap();
    assert_eq!(cursor_at(&mut c, cur), at250);
    c.cursor_get(cur, None, None, CursorOp::Next)
        .unwrap()
        .unwrap();
    // Closing it through the child does nothing (C: it has an mc_backup).
    c.close_cursor(cur);
    assert_eq!(
        cursor_at(&mut c, cur).map(|(k, _)| k),
        Some(251u32.to_be_bytes().to_vec())
    );
    for i in 0..500u32 {
        c.put(
            db,
            &(i * 2 + 10_000).to_be_bytes(),
            &[1; 300],
            WriteFlags::empty(),
        )
        .unwrap();
    }
    for i in (0..250u32).step_by(3) {
        c.del(db, &i.to_be_bytes(), None).unwrap();
    }
    let own = c.open_cursor(db).unwrap();
    c.cursor_get(own, None, None, CursorOp::First).unwrap();
    c.abort();
    // Aborted: the cursor is back where it was, on the parent's pages.
    assert_eq!(cursor_at(&mut t, cur), at250);
    assert!(t.cursor_get(own, None, None, CursorOp::First).is_err());

    let mut c = t.begin_nested_txn().unwrap();
    for i in 0..2000u32 {
        c.put(
            db,
            &(i + 100_000).to_be_bytes(),
            &[2; 200],
            WriteFlags::empty(),
        )
        .unwrap();
    }
    c.put(db, &250u32.to_be_bytes(), &[9; 50], WriteFlags::empty())
        .unwrap();
    // A grandchild moves and writes through the grandparent's cursor.
    let mut g = c.begin_nested_txn().unwrap();
    assert_eq!(
        cursor_at(&mut g, cur),
        Some((250u32.to_be_bytes().to_vec(), vec![9; 50]))
    );
    g.cursor_get(cur, None, None, CursorOp::Next)
        .unwrap()
        .unwrap();
    g.cursor_put(cur, &251u32.to_be_bytes(), &[8; 70], WriteFlags::CURRENT)
        .unwrap();
    g.commit().unwrap();
    c.commit().unwrap();
    // Committed: the cursor followed the child's page changes and kept the
    // grandchild's move.
    assert_eq!(
        cursor_at(&mut t, cur),
        Some((251u32.to_be_bytes().to_vec(), vec![8; 70]))
    );
    let prev = t
        .cursor_get(cur, None, None, CursorOp::Prev)
        .unwrap()
        .map(|(k, v)| (k.to_vec(), v.to_vec()));
    assert_eq!(prev, Some((250u32.to_be_bytes().to_vec(), vec![9; 50])));
    t.close_cursor(cur);
    t.commit().unwrap();
}

#[test]
fn child_handles_and_failures() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path());
    let mut t = env.begin_rw_txn().unwrap();
    let a = t.create_db(Some("a"), DatabaseFlags::CREATE).unwrap();
    t.put(a, b"k", b"v", WriteFlags::empty()).unwrap();

    // A DB created in an aborted child: the handle is closed.
    let mut c = t.begin_nested_txn().unwrap();
    let b = c.create_db(Some("b"), DatabaseFlags::CREATE).unwrap();
    c.put(b, b"x", b"y", WriteFlags::empty()).unwrap();
    c.abort();
    assert!(t.get(b, b"x").is_err());
    assert!(t.open_db(Some("b")).unwrap_err().not_found());

    // A DB created in the parent and written by a committed child.
    let mut c = t.begin_nested_txn().unwrap();
    let b = c.create_db(Some("b"), DatabaseFlags::CREATE).unwrap();
    c.put(b, b"x", b"y", WriteFlags::empty()).unwrap();
    let mut g = c.begin_nested_txn().unwrap();
    g.put(a, b"k", b"g", WriteFlags::empty()).unwrap();
    g.commit().unwrap();
    c.commit().unwrap();
    assert_eq!(t.get(b, b"x").unwrap(), Some(&b"y"[..]));
    assert_eq!(t.get(a, b"k").unwrap(), Some(&b"g"[..]));

    // Errors that do not fail a transaction leave the child usable;
    // dropping it aborts it.
    let mut c = t.begin_nested_txn().unwrap();
    let err = c.put(a, &[], b"v", WriteFlags::empty()).unwrap_err();
    assert_eq!(err.to_err_code(), MdbError::BadValSize.to_err_code());
    c.put(a, b"k2", b"v2", WriteFlags::empty()).unwrap();
    drop(c);
    assert_eq!(t.get(a, b"k2").unwrap(), None);
    t.commit().unwrap();

    let txn = env.begin_ro_txn().unwrap();
    let b = txn.open_db(Some("b")).unwrap();
    assert_eq!(txn.get(b, b"x").unwrap(), Some(&b"y"[..]));
}

#[test]
fn nested_txn_on_failed_parent_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let env = Environment::builder()
        .map_size(1 << 20)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    // Fill the 1 MiB map: MDB_MAP_FULL fails the transaction.
    let mut i = 0u32;
    let err = loop {
        if let Err(e) = t.put(Dbi::MAIN, &i.to_be_bytes(), &[0; 1000], WriteFlags::empty()) {
            break e;
        }
        i += 1;
    };
    assert!(matches!(err, Error::Mdb(MdbError::MapFull)), "{err:?}");
    let e = t.begin_nested_txn().unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
}

#[test]
fn failed_child_commit_fails_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let env = Environment::builder()
        .map_size(1 << 20)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir.path())
        .unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    t.put(Dbi::MAIN, b"a", b"b", WriteFlags::empty()).unwrap();
    let mut c = t.begin_nested_txn().unwrap();
    let mut i = 0u32;
    while c
        .put(Dbi::MAIN, &i.to_be_bytes(), &[0; 1000], WriteFlags::empty())
        .is_ok()
    {
        i += 1;
    }
    let e = c.commit().unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
    let e = t
        .put(Dbi::MAIN, b"c", b"d", WriteFlags::empty())
        .unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
    let e = t.commit().unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(Dbi::MAIN, b"a").unwrap(), None);
}

#[test]
fn comparator_set_in_child_survives_abort() {
    fn rev(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        b.cmp(a)
    }
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path());
    let mut t = env.begin_rw_txn().unwrap();
    let db = t.create_db(Some("r"), DatabaseFlags::CREATE).unwrap();
    let c = t.begin_nested_txn().unwrap();
    c.set_compare(db, Some(rev)).unwrap();
    c.abort();
    for k in [b"a", b"c", b"b"] {
        t.put(db, k, b"", WriteFlags::empty()).unwrap();
    }
    let keys: Vec<Vec<u8>> = {
        let mut cur = t.cursor(db).unwrap();
        let mut v = Vec::new();
        while let Some((k, _)) = cur.next().unwrap() {
            v.push(k.to_vec());
        }
        v
    };
    assert_eq!(keys, [b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
}

/// A nested transaction leaked with `mem::forget` keeps the parent's
/// pages: the parent then refuses everything, commit included, so nothing
/// half-written reaches the file.
#[test]
fn leaked_child_blocks_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path());
    let mut t = env.begin_rw_txn().unwrap();
    t.put(Dbi::MAIN, b"a", b"1", WriteFlags::empty()).unwrap();
    std::mem::forget(t.begin_nested_txn().unwrap());
    let e = t
        .put(Dbi::MAIN, b"b", b"2", WriteFlags::empty())
        .unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
    let e = t.commit().unwrap_err();
    assert!(matches!(e, Error::Mdb(MdbError::BadTxn)), "{e:?}");
    let txn = env.begin_ro_txn().unwrap();
    assert_eq!(txn.get(Dbi::MAIN, b"a").unwrap(), None);
}
