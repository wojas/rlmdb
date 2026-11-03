//! Nested write transactions: the scenarios of the interop C peer's
//! `nested-test` (interop/c/cpeer_misc.c) written by Rust and checked
//! by C (and the other way round), plus the cursor and handle rules around
//! a child transaction.

mod common;

use common::CTools;
use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Error, Format, MdbError, RwTxn,
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

fn cursor_at(t: &mut RwTxn<'_>, id: rlmdb::CursorId) -> Option<(Vec<u8>, Vec<u8>)> {
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

fn open_format(path: &std::path::Path, format: Format) -> Environment {
    Environment::builder()
        .format(format)
        .map_size(64 << 20)
        .max_dbs(16)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(path)
        .unwrap()
}

fn assert_bad_txn<T: std::fmt::Debug>(r: rlmdb::Result<T>, at: &str) {
    match r {
        Err(Error::Mdb(MdbError::BadTxn)) => {}
        other => panic!("{at}: {other:?}, expected BadTxn"),
    }
}

/// A transaction whose state went with a leaked nested transaction
/// refuses everything.
fn assert_blocked(t: &mut RwTxn<'_>, at: &str) {
    assert_bad_txn(t.put(Dbi::MAIN, b"key", b"x", WriteFlags::empty()), at);
    assert_bad_txn(t.get(Dbi::MAIN, b"key"), at);
    assert_bad_txn(t.del(Dbi::MAIN, b"key", None), at);
    assert_bad_txn(t.cursor(Dbi::MAIN).map(|_| ()), at);
    assert_bad_txn(t.open_cursor(Dbi::MAIN), at);
    assert_bad_txn(t.create_db(Some("late"), DatabaseFlags::CREATE), at);
    assert_bad_txn(t.begin_nested_txn().map(|_| ()), at);
}

/// Begins `ends.len() + 1` transactions below `t` (at `level`), each
/// nested in the one before, each writing `key` and creating a database
/// `made at <level>`. The innermost is leaked; then the others, innermost
/// first, must refuse everything and are committed (`true`, which must
/// fail) or aborted (`false`) as `ends` says.
fn leak_innermost(t: &mut RwTxn<'_>, level: usize, ends: &[bool]) {
    let mut c = t.begin_nested_txn().unwrap();
    let value = format!("level {level}");
    c.put(Dbi::MAIN, b"key", value.as_bytes(), WriteFlags::empty())
        .unwrap();
    c.create_db(Some(&format!("made at {level}")), DatabaseFlags::CREATE)
        .unwrap();
    let Some((&commit, rest)) = ends.split_first() else {
        std::mem::forget(c);
        return;
    };
    leak_innermost(&mut c, level + 1, rest);
    let at = format!("{ends:?} at level {level}");
    assert_blocked(&mut c, &at);
    if commit {
        assert_bad_txn(c.commit(), &at);
    } else {
        c.abort();
    }
}

/// A nested transaction leaked at any depth blocks every ancestor, as a
/// leaked child blocks its parent: committing one of them fails, aborting
/// one is safe, the top-level transaction cannot commit (none of the
/// changes reach the file) and the environment works on.
#[test]
fn leaked_descendant_blocks_every_ancestor() {
    let cases: [&[bool]; 6] = [
        // The child is leaked.
        &[],
        // The grandchild is leaked, the child committed or aborted.
        &[true],
        &[false],
        // Deeper chains.
        &[true, true, true],
        &[true, false, true],
        &[false, true, false],
    ];
    for format in [Format::V09, Format::V10] {
        for ends in cases {
            let dir = tempfile::tempdir().unwrap();
            let env = open_format(dir.path(), format);
            let mut t = env.begin_rw_txn().unwrap();
            t.put(Dbi::MAIN, b"key", b"original", WriteFlags::empty())
                .unwrap();
            t.commit().unwrap();

            let mut t = env.begin_rw_txn().unwrap();
            t.put(Dbi::MAIN, b"key", b"level 0", WriteFlags::empty())
                .unwrap();
            t.create_db(Some("made at 0"), DatabaseFlags::CREATE)
                .unwrap();
            leak_innermost(&mut t, 1, ends);
            let at = format!("{format:?} {ends:?} at level 0");
            assert_blocked(&mut t, &at);
            assert_bad_txn(t.commit(), &at);

            let names: Vec<String> = (0..=ends.len() + 1)
                .map(|l| format!("made at {l}"))
                .collect();
            {
                let r = env.begin_ro_txn().unwrap();
                assert_eq!(r.get(Dbi::MAIN, b"key").unwrap(), Some(&b"original"[..]));
                assert_eq!(r.stat(Dbi::MAIN).unwrap().entries, 1, "{at}");
                for name in &names {
                    let e = r.open_db(Some(name)).unwrap_err();
                    assert!(matches!(e, Error::Mdb(MdbError::NotFound)), "{at}: {e:?}");
                }
            }

            let mut t = env.begin_rw_txn().unwrap();
            t.put(Dbi::MAIN, b"key", b"after", WriteFlags::empty())
                .unwrap();
            let mut c = t.begin_nested_txn().unwrap();
            for name in &names {
                let db = c.create_db(Some(name), DatabaseFlags::CREATE).unwrap();
                c.put(db, b"k", name.as_bytes(), WriteFlags::empty())
                    .unwrap();
            }
            c.commit().unwrap();
            t.commit().unwrap();
            {
                let r = env.begin_ro_txn().unwrap();
                assert_eq!(r.get(Dbi::MAIN, b"key").unwrap(), Some(&b"after"[..]));
                for name in &names {
                    let db = r.open_db(Some(name)).unwrap();
                    assert_eq!(r.get(db, b"k").unwrap(), Some(name.as_bytes()), "{at}");
                }
            }
            drop(env);
            if format == Format::V09 {
                if let Some(c) = common::c_tools() {
                    c.peer_ok(&["check-freelist", dir.path().to_str().unwrap()]);
                }
            }
        }
    }
}

// --- a main DB out of order --------------------------------------------------

/// `P_INVALID`: the root of an empty database.
const P_INVALID: u64 = u64::MAX;

/// The default key order with the top bit of every byte flipped: bytes from
/// 0x80 sort before the others.
fn hibit_first(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter().map(|x| x ^ 0x80).cmp(b.iter().map(|x| x ^ 0x80))
}

/// The key of database `name` in the main DB (with its NUL in LMDB 1.0).
fn name_key(format: Format, name: &str) -> Vec<u8> {
    let mut k = name.as_bytes().to_vec();
    if format == Format::V10 {
        k.push(0);
    }
    k
}

/// Begins a write transaction whose main DB is out of order, as
/// `rlmdb-capi/tests/c/capi_unsorted_main.c` leaves it: "dups" (one item),
/// "fixed", "ints" and "plain", then the key 89 8a 8b 8c put in an order
/// that sorts it first. A comparator changed within the transaction is
/// misuse C does not detect, as it cannot detect one changed between opens
/// or a damaged file. In the default order a search for "dups" turns left
/// at "fixed", then at 89 8a 8b 8c, and misses it. Returns the transaction
/// and the handle of "dups".
fn unsorted_main(env: &Environment) -> (RwTxn<'_>, Dbi) {
    let mut t = env.begin_rw_txn().unwrap();
    let dups = t.create_db(Some("dups"), DatabaseFlags::CREATE).unwrap();
    put(&mut t, dups, "k", b"v");
    for name in ["fixed", "ints", "plain"] {
        t.create_db(Some(name), DatabaseFlags::CREATE).unwrap();
    }
    t.set_compare(Dbi::MAIN, Some(hibit_first)).unwrap();
    t.put(
        Dbi::MAIN,
        &[0x89, 0x8a, 0x8b, 0x8c],
        &[b'v'; 10],
        WriteFlags::NO_OVERWRITE,
    )
    .unwrap();
    t.set_compare(Dbi::MAIN, None).unwrap();
    let key = name_key(env.format(), "dups");
    assert_eq!(
        t.get(Dbi::MAIN, &key).unwrap(),
        None,
        "a search finds dups: the main DB is in order"
    );
    (t, dups)
}

/// A main DB record: the key and, for a named database's record (48
/// bytes), its entries and root.
type MainRecord = (Vec<u8>, Option<(u64, u64)>);

/// The committed main DB's records in page order.
fn main_records(env: &Environment) -> Vec<MainRecord> {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut out = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        let u64_at = |off: usize| u64::from_ne_bytes(v[off..off + 8].try_into().unwrap());
        let db = (v.len() == 48).then(|| (u64_at(32), u64_at(40)));
        out.push((k.to_vec(), db));
    }
    out
}

/// The page count of an environment written by [`unsorted_main`]: the
/// pages of the main DB, of every database whose record is in it (read
/// from the record, as the C page audit does, so a record a search cannot
/// find counts too), of the free DB and the page numbers it lists must add
/// up to all pages but the two meta pages. A leaked page breaks it. (The C
/// page audit, which checks more, runs on the 0.9 environments here, and
/// on both formats in `rlmdb-capi/tests/capi_unsorted_main.rs`.)
fn assert_pages_accounted(env: &Environment, at: &str) {
    let txn = env.begin_ro_txn().unwrap();
    let pages = |s: rlmdb::Stat| s.branch_pages + s.leaf_pages + s.overflow_pages;
    let mut used = pages(txn.stat(Dbi::MAIN).unwrap()) + pages(txn.stat(Dbi::FREE).unwrap());
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    while let Some((_, v)) = c.next().unwrap() {
        if v.len() == 48 {
            let u64_at = |off: usize| u64::from_ne_bytes(v[off..off + 8].try_into().unwrap());
            used += (u64_at(8) + u64_at(16) + u64_at(24)) as usize;
        }
    }
    let mut f = txn.cursor(Dbi::FREE).unwrap();
    while let Some((_, v)) = f.next().unwrap() {
        used += u64::from_ne_bytes(v[..8].try_into().unwrap()) as usize;
    }
    let last = env.info().unwrap().last_pgno;
    assert_eq!(
        used,
        last - 1,
        "{at}: pages 2 to {last} are not all accounted for"
    );
}

/// One of the three runs of `capi_unsorted_main.c`: with `child`, a nested
/// transaction begins and aborts; with `reuse`, "xa" is created then; the
/// transaction commits. The main DB's records must be C's, in C's order
/// (the C program's trace against C 0.9 and C 1.0; its driver compares the
/// files byte for byte).
fn unsorted_main_run(format: Format, child: bool, reuse: bool) {
    let at = format!("{format:?}, child {child}, reuse {reuse}");
    let dir = tempfile::tempdir().unwrap();
    let env = open_format(dir.path(), format);
    let (mut t, dups) = unsorted_main(&env);
    if child {
        t.begin_nested_txn().unwrap().abort();
    }
    if reuse {
        let xa = t.create_db(Some("xa"), DatabaseFlags::CREATE).unwrap();
        // A new database takes a free slot, not the slot of an open one.
        assert_ne!(xa, dups, "{at}");
    }
    // The handle is the parent's still, with its changes.
    expect(&t, dups, "k", Some(b"v"), &at);
    t.commit().unwrap();

    let key = |name: &str| name_key(format, name);
    let empty = Some((0, P_INVALID));
    let names =
        |list: &[&str]| -> Vec<MainRecord> { list.iter().map(|n| (key(n), empty)).collect() };
    let unsorted = (vec![0x89, 0x8a, 0x8b, 0x8c], None);
    // At commit C puts the record of every database the transaction
    // changed, all with one cursor, so each search starts on the leaf the
    // last put left it on: a name the search misses gets a second record
    // where the search ended.
    let want: Vec<_> = if reuse {
        // "xa" was put at the front (C's search for it ended on the only
        // leaf, and "xa" sorts before its first key); at commit "dups" is
        // found, "fixed" goes to the front and "xa" to the end.
        let mut w = names(&["fixed", "xa"]);
        w.push(unsorted);
        w.push((key("dups"), Some((1, 3))));
        w.extend(names(&["fixed", "ints", "plain", "xa"]));
        w
    } else {
        // At commit "dups" is missed and goes to the front.
        let mut w = vec![(key("dups"), Some((1, 3))), unsorted];
        w.extend(names(&["dups", "fixed", "ints", "plain"]));
        w
    };
    assert_eq!(main_records(&env), want, "{at}");
    assert_eq!(env.info().unwrap().last_pgno, 3, "{at}");
    assert_pages_accounted(&env, &at);
    drop(env);
    if format == Format::V09 {
        if let Some(c) = common::c_tools() {
            c.peer_ok(&["check-freelist", dir.path().to_str().unwrap()]);
        }
    }
}

/// A nested transaction that aborts keeps the handles of the databases its
/// parent has a live view of, as C does (it closes only the handles of
/// databases the child created, `mdb_dbis_update`): the parent commits,
/// with its changes to them. Before, the abort looked each up in the
/// parent's main DB to close the handles of databases that do not exist
/// there (`KNOWN_C_BUGS.md` row 19), missed "dups" in the main DB out of
/// order and closed its handle: the commit failed with `MDB_BAD_DBI`, or,
/// once "xa" had taken the slot, lost the put and leaked its page.
#[test]
fn child_abort_keeps_parent_handles_in_unsorted_main() {
    for format in [Format::V09, Format::V10] {
        unsorted_main_run(format, true, false);
        unsorted_main_run(format, true, true);
    }
}

/// In an LMDB 0.9 file, emptying the main DB removes the records of the
/// named databases but keeps their handles (`KNOWN_C_BUGS.md` row 21), and
/// the commit writes back the records of those the transaction changed. A
/// nested transaction's abort keeps such a handle, as C does: the parent
/// has a live view of the database. (The abort used to look it up in the
/// parent's empty main DB and close it; the commit then failed with
/// `MDB_BAD_DBI`.)
#[test]
fn child_abort_keeps_handles_after_the_parent_emptied_the_main_db() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_format(dir.path(), Format::V09);
    let mut t = env.begin_rw_txn().unwrap();
    let x = t.create_db(Some("x"), DatabaseFlags::CREATE).unwrap();
    put(&mut t, x, "a", b"1");
    t.commit().unwrap();
    let mut t = env.begin_rw_txn().unwrap();
    put(&mut t, x, "b", b"2");
    t.clear_db(Dbi::MAIN).unwrap();
    t.begin_nested_txn().unwrap().abort();
    expect(&t, x, "b", Some(b"2"), "after the child");
    t.commit().unwrap();
    let r = env.begin_ro_txn().unwrap();
    expect(&r, x, "a", Some(b"1"), "committed");
    expect(&r, x, "b", Some(b"2"), "committed");
}

/// A new database's record goes where C's `mdb_dbi_open` puts it: with the
/// cursor its search left on the leaf, whose `mdb_cursor_set` puts a name
/// before the first key at index 0 (a new search of a main DB out of order
/// put "xa" last).
#[test]
fn new_database_record_goes_where_c_puts_it_in_unsorted_main() {
    for format in [Format::V09, Format::V10] {
        unsorted_main_run(format, false, true);
    }
}

/// What row 19's abort still does in a nested transaction: a handle the
/// child opened for a database that exists only because the child created
/// it, registered again (without `DB_NEW`) after its first handle was
/// closed and that slot reused, is closed when the child aborts. Otherwise
/// it would name nothing, and `create_db` in the parent would return it.
#[test]
fn child_abort_closes_the_handle_of_a_database_only_the_child_had() {
    for format in [Format::V09, Format::V10] {
        let dir = tempfile::tempdir().unwrap();
        let env = open_format(dir.path(), format);
        let mut t = env.begin_rw_txn().unwrap();
        t.create_db(Some("y"), DatabaseFlags::CREATE).unwrap();
        t.commit().unwrap();
        env.close_db(env.begin_ro_txn().unwrap().open_db(Some("y")).unwrap());

        let mut t = env.begin_rw_txn().unwrap();
        let mut c = t.begin_nested_txn().unwrap();
        let x1 = c.create_db(Some("x"), DatabaseFlags::CREATE).unwrap();
        env.close_db(x1);
        // "y" takes the slot, so "x" is found in the main DB and registered
        // in another, as a database that existed before.
        assert_eq!(c.open_db(Some("y")).unwrap(), x1);
        let x2 = c.open_db(Some("x")).unwrap();
        assert_ne!(x2, x1);
        c.abort();
        let e = t.open_db(Some("x")).unwrap_err();
        assert!(e.not_found(), "{format:?}: {e:?}");
        let x = t.create_db(Some("x"), DatabaseFlags::CREATE).unwrap();
        put(&mut t, x, "k", b"x");
        t.commit().unwrap();
        let r = env.begin_ro_txn().unwrap();
        let x = r.open_db(Some("x")).unwrap();
        expect(&r, x, "k", Some(b"x"), &format!("{format:?}"));
    }
}

/// What row 19's abort still does in a nested transaction for a database
/// another transaction deleted after its handle was opened (here a read
/// transaction on an older snapshot opened it, as another process could
/// have deleted it): once the child looked at the handle, its abort closes
/// it, and `create_db` in the parent creates the database afresh instead
/// of returning a handle that names nothing (as C's `mdb_dbi_open` would,
/// finding the name open). The parent has no live view of such a
/// database, so this does not depend on which transaction registered the
/// handle: looking only at the handles registered in the child would keep
/// this one open.
#[test]
fn child_abort_closes_the_handle_of_a_database_deleted_elsewhere() {
    for format in [Format::V09, Format::V10] {
        let at = format!("{format:?}");
        let dir = tempfile::tempdir().unwrap();
        let env = open_format(dir.path(), format);
        let mut t = env.begin_rw_txn().unwrap();
        let gone = t.create_db(Some("gone"), DatabaseFlags::CREATE).unwrap();
        put(&mut t, gone, "k", b"old");
        t.commit().unwrap();
        let old = env.begin_ro_txn().unwrap();
        let mut t = env.begin_rw_txn().unwrap();
        t.drop_db(gone).unwrap();
        t.commit().unwrap();
        let gone = old.open_db(Some("gone")).unwrap();
        expect(&old, gone, "k", Some(b"old"), &at);
        drop(old);

        let mut t = env.begin_rw_txn().unwrap();
        let c = t.begin_nested_txn().unwrap();
        // (The record is not found: the handle names nothing.)
        assert!(c.get(gone, b"k").is_err(), "{at}");
        c.abort();
        let db = t.create_db(Some("gone"), DatabaseFlags::CREATE).unwrap();
        put(&mut t, db, "k", b"new");
        t.commit().unwrap();
        let r = env.begin_ro_txn().unwrap();
        let db = r.open_db(Some("gone")).unwrap();
        expect(&r, db, "k", Some(b"new"), &at);
    }
}
