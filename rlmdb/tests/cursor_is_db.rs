//! `is_db` of the read and write cursors ([`RoCursor::is_db`],
//! [`RwCursor::is_db`]): LMDB 1.0's `mdb_cursor_is_db`, in environments of
//! both formats.
//!
//! The expectations are C 1.0's, as `rlmdb-capi`'s `capi_v10` traces them
//! against C LMDB 1.0 on 1.0 files: `true` on the record of a named
//! database in the main DB, `false` on any other item (a plain value of 48
//! bytes, the size of such a record, included) and on a cursor that is not
//! on a page; a cursor that a failed move left on a page (past either end,
//! a failed `MDB_SET`) reports the node it is still on.

use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Environment, Format, RoCursor, RwCursor, RwTxn, Transaction,
    WriteFlags,
};

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-is-db-")
        .tempdir()
        .unwrap()
}

/// The main DB's key of named database `name`: the name, followed by its
/// NUL in an LMDB 1.0 environment (C 1.0's `mdb_dbi_open` stores it so).
fn db_key(format: Format, name: &str) -> Vec<u8> {
    let mut k = name.as_bytes().to_vec();
    if format == Format::V10 {
        k.push(0);
    }
    k
}

/// The keys of the main DB in order, each with what `is_db` says on it:
/// named databases, and the plain items `k1` and `n` (whose value has the
/// 48 bytes of a database record).
fn main_keys(format: Format) -> Vec<(Vec<u8>, bool)> {
    let db = |name| (db_key(format, name), true);
    let plain = |k: &str| (k.as_bytes().to_vec(), false);
    vec![
        db("a"),
        db("dups"),
        db("empty"),
        plain("k1"),
        db("m"),
        plain("n"),
        db("zzz"),
    ]
}

/// The databases of the fixture.
struct Dbs {
    a: Dbi,
    dups: Dbi,
    empty: Dbi,
}

/// Fills a new environment: plain items in the main DB between and after
/// named databases, a named database `a`, an empty one, and a `DUP_SORT`
/// one with a key of a few duplicates (a sub-page), one with enough for a
/// sub-database and one with a single item.
fn fill(txn: &mut RwTxn<'_>) -> Dbs {
    let none = WriteFlags::empty();
    txn.put(Dbi::MAIN, b"k1", b"v1", none).unwrap();
    txn.put(Dbi::MAIN, b"n", &[0xa5; 48], none).unwrap();
    let a = txn.create_db(Some("a"), DatabaseFlags::CREATE).unwrap();
    txn.put(a, b"x", b"1", none).unwrap();
    txn.put(a, b"y", b"2", none).unwrap();
    let empty = txn.create_db(Some("empty"), DatabaseFlags::CREATE).unwrap();
    let m = txn.create_db(Some("m"), DatabaseFlags::CREATE).unwrap();
    txn.put(m, b"only", b"1", none).unwrap();
    let dups = txn
        .create_db(
            Some("dups"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT,
        )
        .unwrap();
    txn.put(dups, b"few", b"1", none).unwrap();
    txn.put(dups, b"few", b"2", none).unwrap();
    // 1000 items of 23 bytes: a sub-database on pages up to 16 KiB.
    for i in 0..1000 {
        let item = format!("dup{i:04}-padding-padding");
        txn.put(dups, b"many", item.as_bytes(), none).unwrap();
    }
    txn.put(dups, b"one", b"1", none).unwrap();
    let zzz = txn.create_db(Some("zzz"), DatabaseFlags::CREATE).unwrap();
    txn.put(zzz, b"z", b"1", none).unwrap();
    Dbs { a, dups, empty }
}

/// A read or a write cursor, as the checks use them.
trait Cursor {
    /// `mdb_cursor_get` with `op`: the key it returns, `None` for
    /// `MDB_NOTFOUND`.
    fn op(&mut self, key: Option<&[u8]>, op: CursorOp) -> Option<Vec<u8>>;
    /// The method under test.
    fn on_db(&self) -> bool;
}

impl Cursor for RoCursor<'_> {
    fn op(&mut self, key: Option<&[u8]>, op: CursorOp) -> Option<Vec<u8>> {
        self.get(key, None, op).unwrap().map(|(k, _)| k.to_vec())
    }

    fn on_db(&self) -> bool {
        self.is_db()
    }
}

impl Cursor for RwCursor<'_> {
    fn op(&mut self, key: Option<&[u8]>, op: CursorOp) -> Option<Vec<u8>> {
        self.get(key, None, op).unwrap().map(|(k, _)| k.to_vec())
    }

    fn on_db(&self) -> bool {
        self.is_db()
    }
}

/// What a walk over the keys of a database saw.
#[derive(Debug, PartialEq, Eq)]
struct Walk {
    /// Each key from `MDB_FIRST` on with `MDB_NEXT_NODUP`, with `is_db`.
    forward: Vec<(Vec<u8>, bool)>,
    /// `is_db` once `MDB_NEXT_NODUP` ran past the last key.
    after_last: bool,
    /// Each key from `MDB_LAST` on with `MDB_PREV_NODUP`, with `is_db`.
    backward: Vec<(Vec<u8>, bool)>,
    /// `is_db` once `MDB_PREV_NODUP` ran past the first key.
    before_first: bool,
}

/// Walks the database of `c` forwards and backwards; `c` must not have
/// been positioned (it is checked to say `false`).
fn walk(c: &mut dyn Cursor, what: &str) -> Walk {
    assert!(!c.on_db(), "{what}: unpositioned");
    let mut forward = Vec::new();
    let mut k = c.op(None, CursorOp::First);
    while let Some(key) = k {
        forward.push((key, c.on_db()));
        k = c.op(None, CursorOp::NextNoDup);
    }
    let after_last = c.on_db();
    let mut backward = Vec::new();
    let mut k = c.op(None, CursorOp::Last);
    while let Some(key) = k {
        backward.push((key, c.on_db()));
        k = c.op(None, CursorOp::PrevNoDup);
    }
    Walk {
        forward,
        after_last,
        backward,
        before_first: c.on_db(),
    }
}

/// The walk of a database whose items all say `is_db` == `false`.
fn plain_walk(keys: &[&str]) -> Walk {
    let items: Vec<(Vec<u8>, bool)> = keys
        .iter()
        .map(|k| (k.as_bytes().to_vec(), false))
        .collect();
    Walk {
        backward: items.iter().rev().cloned().collect(),
        forward: items,
        after_last: false,
        before_first: false,
    }
}

/// The main DB: its walk, then positioning by key.
fn check_main(format: Format, c: &mut dyn Cursor, what: &str) {
    let keys = main_keys(format);
    let w = walk(c, what);
    assert_eq!(w.forward, keys, "{what}");
    assert_eq!(w.backward, keys.iter().rev().cloned().collect::<Vec<_>>());
    // Past either end the cursor is still on the last (first) record, a
    // named database's.
    assert!(w.after_last && w.before_first, "{what}: {w:?}");

    let n = c.op(Some(b"n"), CursorOp::Set);
    assert_eq!(n.as_deref(), Some(&b"n"[..]));
    assert!(!c.on_db(), "{what}: n has a 48-byte plain value");
    let m = db_key(format, "m");
    assert_eq!(c.op(Some(&m), CursorOp::Set), Some(m.clone()));
    assert!(c.on_db(), "{what}: set m");
    assert_eq!(
        c.op(Some(b"b"), CursorOp::SetRange),
        Some(db_key(format, "dups"))
    );
    assert!(c.on_db(), "{what}: set_range b");
    assert_eq!(c.op(Some(b"mz"), CursorOp::SetRange), Some(b"n".to_vec()));
    assert!(!c.on_db(), "{what}: set_range mz");
    // A failed MDB_SET leaves the cursor where the key would be: on m's
    // record before "l", on the plain n before "mz".
    assert_eq!(c.op(Some(b"l"), CursorOp::Set), None);
    assert!(c.on_db(), "{what}: set l");
    assert_eq!(c.op(Some(b"mz"), CursorOp::Set), None);
    assert!(!c.on_db(), "{what}: set mz");
}

/// Runs a check on a new cursor over a database (the cursor ends with
/// the check).
type WithCursor<'a> = dyn FnMut(Dbi, &mut dyn FnMut(&mut dyn Cursor)) + 'a;

/// The other databases, each through a new cursor from `with`: no item of
/// theirs is a database record.
fn check_others(dbs: &Dbs, with: &mut WithCursor<'_>, what: &str) {
    with(dbs.a, &mut |c| {
        assert_eq!(walk(c, what), plain_walk(&["x", "y"]), "{what}: a");
    });
    with(dbs.dups, &mut |c| {
        let expected = plain_walk(&["few", "many", "one"]);
        assert_eq!(walk(c, what), expected, "{what}: dups");
    });
    // The last duplicate of a key whose duplicates are a sub-database (a
    // node with both flags).
    with(dbs.dups, &mut |c| {
        assert!(c.op(Some(b"many"), CursorOp::Set).is_some());
        assert!(c.op(None, CursorOp::LastDup).is_some());
        assert!(!c.on_db(), "{what}: last dup of many");
    });
    // An empty database: the cursor never gets onto a page.
    with(dbs.empty, &mut |c| {
        assert_eq!(walk(c, what), plain_walk(&[]), "{what}: empty");
    });
}

fn check_format(format: Format) {
    let dir = tmp();
    let env = Environment::builder()
        .format(format)
        .max_dbs(8)
        .map_size(64 << 20)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.format(), format);
    let what = |s: &str| format!("{} {s}", format.name());

    // The transaction that creates everything: write cursors and read
    // cursors of the write transaction.
    let mut txn = env.begin_rw_txn().unwrap();
    let dbs = fill(&mut txn);
    check_main(
        format,
        &mut txn.cursor_mut(Dbi::MAIN).unwrap(),
        &what("write txn, main"),
    );
    check_others(
        &dbs,
        &mut |db, f| f(&mut txn.cursor_mut(db).unwrap()),
        &what("write txn"),
    );
    let w = &what("write txn, read cursor");
    check_main(format, &mut txn.cursor(Dbi::MAIN).unwrap(), w);
    check_others(&dbs, &mut |db, f| f(&mut txn.cursor(db).unwrap()), w);
    txn.commit().unwrap();

    // A read transaction.
    let txn = env.begin_ro_txn().unwrap();
    let w = &what("read txn");
    check_main(format, &mut txn.cursor(Dbi::MAIN).unwrap(), w);
    check_others(&dbs, &mut |db, f| f(&mut txn.cursor(db).unwrap()), w);
    drop(txn);

    // A nested transaction's write cursor, on a named database it adds.
    let mut txn = env.begin_rw_txn().unwrap();
    {
        let mut child = txn.begin_nested_txn().unwrap();
        child.create_db(Some("new"), DatabaseFlags::CREATE).unwrap();
        let mut c = child.cursor_mut(Dbi::MAIN).unwrap();
        let new = db_key(format, "new");
        assert_eq!(c.op(Some(&new), CursorOp::Set), Some(new));
        assert!(c.on_db(), "{}", what("child: set new"));
        assert_eq!(c.op(None, CursorOp::Prev), Some(b"n".to_vec()));
        assert!(!c.on_db(), "{}", what("child: prev n"));
    }
    let mut c = txn.cursor_mut(Dbi::MAIN).unwrap();
    assert_eq!(c.op(None, CursorOp::Last), Some(db_key(format, "zzz")));
    assert!(c.on_db(), "{}", what("parent after the child: last"));
    drop(c);
    txn.abort();
}

#[test]
fn is_db_in_lmdb_0_9_environments() {
    check_format(Format::V09);
}

#[test]
fn is_db_in_lmdb_1_0_environments() {
    check_format(Format::V10);
}
