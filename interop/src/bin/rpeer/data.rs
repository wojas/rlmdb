//! `fill`, `verify`, `delete` and `dump` of the deterministic data set (the
//! counterpart of `c/cpeer_data.c`).
//!
//! `fill` and `delete` write, with the same LMDB calls and the same
//! transaction boundaries as the C peer, so both peers produce the same
//! sequence of commits.
//!
//! `verify` proves that a DB holds exactly the data set (as modified by a
//! prior `delete`), using every read path a peer must get right: point
//! lookups, full cursor walks in both directions, duplicate navigation,
//! `MDB_GET_MULTIPLE` for DUPFIXED, and the DB statistics. It follows the C
//! peer step by step, so that both peers exercise the same LMDB calls.

use std::io::{self, BufWriter, Write};

use rlmdb::{
    CursorOp, DatabaseFlags, Dbi, Format, RoCursor, RoTxn, RwTxn, Transaction, WriteFlags,
};

use crate::args::{found, short_hex, show, Check, CmdResult, Fail, Opts};
use crate::dataset::{delete_dup, delete_key, DataSet, Model, ModelItem};

// ---- fill ------------------------------------------------------------------------------

/// `print_db_stat`: the `filled`/`deleted`-style summary line of `dbi`, as
/// the open write transaction sees it.
fn print_db_stat(what: &str, txn: &RwTxn<'_>, dbi: Dbi, ds: &DataSet, txns: u32) -> CmdResult {
    let st = txn.stat(dbi).check("mdb_stat")?;
    println!(
        "{what} db={} keys={} entries={} txns={txns} psize={} depth={} branch_pages={} \
         leaf_pages={} overflow_pages={}",
        ds.label(),
        ds.count,
        st.entries,
        st.page_size,
        st.depth,
        st.branch_pages,
        st.leaf_pages,
        st.overflow_pages
    );
    Ok(())
}

/// `put_item`: put value `d` of key `i` with `mdb_put`, or with
/// `MDB_RESERVE` and then write the value into the space LMDB hands out.
fn put_item(
    txn: &mut RwTxn<'_>,
    dbi: Dbi,
    o: &Opts,
    ds: &DataSet,
    key: &[u8],
    i: u32,
    d: u32,
) -> CmdResult {
    let value = ds.value(i, d);
    if o.reserve {
        let space = txn
            .reserve(dbi, key, value.len(), WriteFlags::empty())
            .check("mdb_put(MDB_RESERVE)")?;
        space.copy_from_slice(&value);
    } else {
        txn.put(dbi, key, &value, WriteFlags::empty())
            .check("mdb_put")?;
    }
    Ok(())
}

/// `fill <env> <db|-> <count> <seed> [data options] [--per-txn N] [--append]
/// [--reserve]`: write the data set into `<db>`, creating it with the data
/// flags. Items in index order with `mdb_put` (random order for the
/// comparator); with `--append` in comparator order with `MDB_APPEND` /
/// `MDB_APPENDDUP` through a cursor. Commits every `N` keys with
/// `--per-txn`, exactly where the C peer does.
pub fn cmd_fill(o: &mut Opts) -> CmdResult {
    let ds = o.dataset(1)?;
    if o.reserve && ds.is_dupsort() {
        return Err(Fail::usage("--reserve cannot be used with DUPSORT"));
    }
    let env = o.open_env(false)?;
    let mut txns = 1u32;
    let mut n = 0u32;
    let mut txn = env.begin_rw_txn().check("mdb_txn_begin")?;
    let flags = DatabaseFlags::from_bits_retain(ds.flags) | DatabaseFlags::CREATE;
    let dbi = txn
        .create_db(ds.db.as_deref(), flags)
        .check("mdb_dbi_open")?;

    if o.append {
        // Insert in comparator order with MDB_APPEND (new key) and
        // MDB_APPENDDUP (further values of the same key) via a cursor. The
        // DB must not already hold keys sorting after ours.
        let m = Model::build(&ds);
        let mut cur = txn.open_cursor(dbi).check("mdb_cursor_open")?;
        for (k, it) in m.items.iter().enumerate() {
            for (j, val) in it.vals.iter().enumerate() {
                let f = if j == 0 {
                    WriteFlags::APPEND
                } else {
                    WriteFlags::APPEND_DUP
                };
                txn.cursor_put(cur, &it.key, val, f)
                    .check("mdb_cursor_put(MDB_APPEND)")?;
            }
            if o.per_txn != 0 && bump(&mut n) % o.per_txn == 0 && k + 1 < m.items.len() {
                txn.close_cursor(cur);
                txn.commit().check("mdb_txn_commit")?;
                txn = env.begin_rw_txn().check("mdb_txn_begin")?;
                txns += 1;
                cur = txn.open_cursor(dbi).check("mdb_cursor_open")?;
            }
        }
        txn.close_cursor(cur);
    } else {
        // Insert in index order, which is random order for the comparator.
        for i in 0..ds.count {
            let key = ds.key(i);
            for d in 0..ds.ndups(i) {
                if ds.present(i, d) {
                    put_item(&mut txn, dbi, o, &ds, &key, i, d)?;
                }
            }
            if o.per_txn != 0 && bump(&mut n) % o.per_txn == 0 && i + 1 < ds.count {
                txn.commit().check("mdb_txn_commit")?;
                txn = env.begin_rw_txn().check("mdb_txn_begin")?;
                txns += 1;
            }
        }
    }
    print_db_stat("filled", &txn, dbi, &ds, txns)?;
    txn.commit().check("mdb_txn_commit")
}

/// C's `++n`: increment and return the new value.
fn bump(n: &mut u32) -> u32 {
    *n = n.wrapping_add(1);
    *n
}

// ---- delete ----------------------------------------------------------------------------

/// `delete <env> <db|-> <count> <seed> [--every N] [--per-txn N] [data
/// options]`: delete a deterministic subset (README.md): key `i` entirely
/// if `i % N == 0` (alternating `mdb_del(key, NULL)` and `MDB_SET` +
/// `mdb_cursor_del`), for DUPSORT the values `d % 2 == 0` of keys with
/// `i % N == 1` one by one (alternating `mdb_del(key, value)` and
/// `MDB_GET_BOTH` + `mdb_cursor_del`). Items gone per `--deleted-every` are
/// skipped; a missing item is a mismatch.
///
/// One cursor stays open for the whole transaction, as in C, so the
/// `mdb_del` calls next to it exercise the fixups of tracked cursors.
pub fn cmd_delete(o: &mut Opts) -> CmdResult {
    let ds = o.dataset(1)?;
    let every = o.every;
    if every == 0 {
        return Err(Fail::usage("--every must be > 0"));
    }
    let env = o.open_env(false)?;
    let (mut keys, mut entries) = (0usize, 0usize);
    let mut txns = 1u32;
    let mut n = 0u32;
    let mut txn = env.begin_rw_txn().check("mdb_txn_begin")?;
    let dbi = txn
        .create_db(ds.db.as_deref(), DatabaseFlags::empty())
        .check("mdb_dbi_open")?;
    let mut cur = txn.open_cursor(dbi).check("mdb_cursor_open")?;
    let whole_key = if ds.is_dupsort() {
        WriteFlags::NO_DUP_DATA
    } else {
        WriteFlags::empty()
    };

    for i in 0..ds.count {
        let key = ds.key(i);
        let nd = ds.ndups(i);
        let mut left = (0..nd).filter(|&d| ds.present(i, d)).count();
        if left == 0 {
            continue; // already deleted by a previous delete
        }

        if delete_key(every, i) {
            // Alternate between mdb_del() and a cursor delete of the whole
            // key (all its values).
            let found = if (i / every) % 2 == 0 {
                txn.del(dbi, &key, None).check("delete key")?
            } else {
                match txn
                    .cursor_get(cur, Some(&key), None, CursorOp::Set)
                    .check("delete key")?
                {
                    Some(_) => {
                        txn.cursor_del(cur, whole_key).check("delete key")?;
                        true
                    }
                    None => false,
                }
            };
            if !found {
                return Err(Fail::mismatch(format!("delete: key i={i} not found")));
            }
            keys += 1;
            entries += left;
        } else {
            for d in 0..nd {
                if !ds.present(i, d) || !delete_dup(&ds, every, i, d) {
                    continue;
                }
                let value = ds.value(i, d);
                // Alternate between mdb_del(key, data) and a cursor
                // positioned with MDB_GET_BOTH.
                let found = if (d / 2) % 2 == 0 {
                    txn.del(dbi, &key, Some(&value)).check("delete value")?
                } else {
                    match txn
                        .cursor_get(cur, Some(&key), Some(&value), CursorOp::GetBoth)
                        .check("delete value")?
                    {
                        Some(_) => {
                            txn.cursor_del(cur, WriteFlags::empty())
                                .check("delete value")?;
                            true
                        }
                        None => false,
                    }
                };
                if !found {
                    return Err(Fail::mismatch(format!(
                        "delete: value {d} of key i={i} not found"
                    )));
                }
                entries += 1;
                left -= 1;
                if left == 0 {
                    keys += 1;
                }
            }
        }
        if o.per_txn != 0 && bump(&mut n) % o.per_txn == 0 && i + 1 < ds.count {
            txn.close_cursor(cur);
            txn.commit().check("mdb_txn_commit")?;
            txn = env.begin_rw_txn().check("mdb_txn_begin")?;
            txns += 1;
            cur = txn.open_cursor(dbi).check("mdb_cursor_open")?;
        }
    }
    txn.close_cursor(cur);
    println!(
        "deleted db={} keys={keys} entries={entries} txns={txns}",
        ds.label()
    );
    txn.commit().check("mdb_txn_commit")
}

// ---- drop ------------------------------------------------------------------------------

/// `drop <env> <db|-> [--delete]`: `mdb_drop`: empty the DB, or with
/// `--delete` delete it from the environment (the main DB can only be
/// emptied). Prints `dropped db=.. delete=0|1 entries=.. pages=..` (what
/// it held).
pub fn cmd_drop(o: &mut Opts) -> CmdResult {
    let db = Some(o.pos[1].as_str()).filter(|d| *d != "-");
    let label = db.unwrap_or("-");
    let env = o.open_env(false)?;
    let mut txn = env.begin_rw_txn().check("mdb_txn_begin")?;
    let dbi = match txn.create_db(db, DatabaseFlags::empty()) {
        Err(e) if e.not_found() => {
            return Err(Fail::mismatch(format!("drop: database {label} not found")))
        }
        r => r.check("mdb_dbi_open")?,
    };
    let st = txn.stat(dbi).check("mdb_stat")?;
    drop_db(&mut txn, dbi, o.delete)?;
    println!(
        "dropped db={label} delete={} entries={} pages={}",
        u8::from(o.delete),
        st.entries,
        st.branch_pages + st.leaf_pages + st.overflow_pages
    );
    txn.commit().check("mdb_txn_commit")
}

/// `mdb_drop(txn, dbi, delete)`.
fn drop_db(txn: &mut RwTxn<'_>, dbi: Dbi, delete: bool) -> CmdResult {
    if delete {
        txn.drop_db(dbi).check("mdb_drop(1)")
    } else {
        txn.clear_db(dbi).check("mdb_drop(0)")
    }
}

// ---- verify ----------------------------------------------------------------------------

/// The counts printed by a successful verify.
pub struct Verified {
    /// Keys present.
    pub keys: usize,
    /// Key/value pairs present.
    pub entries: usize,
    /// Keys checked to be absent.
    pub absent: usize,
}

/// `mismatch()`: a failed check of DB `ds`, `verify db=<db>: <message>`.
fn mismatch(ds: &DataSet, msg: impl std::fmt::Display) -> Fail {
    Fail::mismatch(format!("verify db={}: {msg}", ds.label()))
}

/// The walk directions of [`check_walk`].
#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Forward,
    Backward,
}

/// `check_walk`: walk the whole DB (`MDB_FIRST`/`MDB_NEXT` or
/// `MDB_LAST`/`MDB_PREV`) and compare it with the flattened model: entry
/// `(k, j)` is value `j` of model item `k`.
fn check_walk(cur: &mut RoCursor<'_>, ds: &DataSet, m: &Model, dir: Direction) -> CmdResult {
    let (first, step, name) = match dir {
        Direction::Forward => (CursorOp::First, CursorOp::Next, "forward"),
        Direction::Backward => (CursorOp::Last, CursorOp::Prev, "backward"),
    };
    // The model entries in walk order.
    let entries = m
        .items
        .iter()
        .flat_map(|it| it.vals.iter().map(move |v| (it, v)));
    let mut expected: Box<dyn Iterator<Item = (&ModelItem, &Vec<u8>)>> = match dir {
        Direction::Forward => Box::new(entries),
        Direction::Backward => Box::new(entries.rev()),
    };
    let mut seen = 0usize;
    let mut op = first;
    while let Some((key, data)) = cur.get(None, None, op).check("mdb_cursor_get(walk)")? {
        op = step;
        let Some((it, val)) = expected.next() else {
            return Err(mismatch(
                ds,
                format!("{name} walk: extra item key={}", short_hex(key)),
            ));
        };
        seen += 1;
        if key != it.key.as_slice() || data != val.as_slice() {
            return Err(mismatch(
                ds,
                format!(
                    "{name} walk: entry {} is {}={}, expected {}={} (i={})",
                    seen - 1,
                    short_hex(key),
                    short_hex(data),
                    short_hex(&it.key),
                    short_hex(val),
                    it.i
                ),
            ));
        }
    }
    if seen != m.nentries {
        return Err(mismatch(
            ds,
            format!("{name} walk saw {seen} entries, expected {}", m.nentries),
        ));
    }
    Ok(())
}

/// The DUPFIXED part of `check_dups`: `MDB_GET_MULTIPLE` returns up to a
/// page of packed values of the current key, `MDB_NEXT_MULTIPLE` the
/// following pages.
fn check_multiple(cur: &mut RoCursor<'_>, ds: &DataSet, it: &ModelItem) -> CmdResult {
    let valsize = ds.valsize as usize;
    let (_, mut data) = found(cur.set(&it.key), "mdb_cursor_get(MDB_SET)")?;
    let mut got = 0usize;
    let mut op = CursorOp::GetMultiple;
    while let Some((_, multiple)) = cur
        .get(None, None, op)
        .check("mdb_cursor_get(MDB_NEXT_MULTIPLE)")?
    {
        // C's MDB_GET_MULTIPLE on a key whose only value is stored without
        // a sub-page succeeds and leaves `data` untouched, i.e. still the
        // value MDB_SET returned. The Rust API reports that as empty data
        // (KNOWN_C_BUGS.md #8); map it back to C's behaviour.
        if !(op == CursorOp::GetMultiple && multiple.is_empty()) {
            data = multiple;
        }
        op = CursorOp::NextMultiple;
        if data.len() % valsize != 0 {
            return Err(mismatch(
                ds,
                format!(
                    "GET_MULTIPLE size {} not a multiple of {valsize}",
                    data.len()
                ),
            ));
        }
        for value in data.chunks(valsize) {
            if it.vals.get(got).map(Vec::as_slice) != Some(value) {
                return Err(mismatch(
                    ds,
                    format!("GET_MULTIPLE value {got} of i={} differs", it.i),
                ));
            }
            got += 1;
        }
    }
    if got != it.vals.len() {
        return Err(mismatch(
            ds,
            format!(
                "GET_MULTIPLE saw {got} values of i={}, expected {}",
                it.i,
                it.vals.len()
            ),
        ));
    }
    Ok(())
}

/// `check_dups`: duplicate navigation: count, `MDB_LAST_DUP`,
/// `MDB_FIRST_DUP`, `MDB_GET_BOTH` of every value, for DUPFIXED
/// `MDB_GET_MULTIPLE`/`MDB_NEXT_MULTIPLE`, and an `MDB_NEXT_NODUP` walk.
fn check_dups(cur: &mut RoCursor<'_>, ds: &DataSet, m: &Model) -> CmdResult {
    for it in &m.items {
        found(cur.set(&it.key), "mdb_cursor_get(MDB_SET)")?;
        let count = cur.count().check("mdb_cursor_count")?;
        if count != it.vals.len() as u64 {
            return Err(mismatch(
                ds,
                format!(
                    "key i={} has {count} values, expected {}",
                    it.i,
                    it.vals.len()
                ),
            ));
        }
        let (_, last) = found(cur.last_dup(), "mdb_cursor_get(MDB_LAST_DUP)")?;
        if Some(last) != it.vals.last().map(Vec::as_slice) {
            return Err(mismatch(
                ds,
                format!("MDB_LAST_DUP of i={} is {}", it.i, short_hex(last)),
            ));
        }
        let (_, first) = found(cur.first_dup(), "mdb_cursor_get(MDB_FIRST_DUP)")?;
        if first != it.vals[0].as_slice() {
            return Err(mismatch(
                ds,
                format!("MDB_FIRST_DUP of i={} is {}", it.i, short_hex(first)),
            ));
        }
        for (j, val) in it.vals.iter().enumerate() {
            let Some((_, data)) = cur
                .get_both(&it.key, val)
                .check("mdb_cursor_get(MDB_GET_BOTH)")?
            else {
                return Err(mismatch(
                    ds,
                    format!("MDB_GET_BOTH: value {j} of i={} not found", it.i),
                ));
            };
            if data != val.as_slice() {
                return Err(mismatch(
                    ds,
                    format!("MDB_GET_BOTH of i={} returned {}", it.i, short_hex(data)),
                ));
            }
        }
        if ds.flags & rlmdb::consts::MDB_DUPFIXED != 0 {
            check_multiple(cur, ds, it)?;
        }
    }
    let mut nkeys = 0usize;
    let mut op = CursorOp::First;
    while cur
        .get(None, None, op)
        .check("mdb_cursor_get(MDB_NEXT_NODUP)")?
        .is_some()
    {
        nkeys += 1;
        op = CursorOp::NextNoDup;
    }
    if nkeys != m.items.len() {
        return Err(mismatch(
            ds,
            format!(
                "MDB_NEXT_NODUP saw {nkeys} keys, expected {}",
                m.items.len()
            ),
        ));
    }
    Ok(())
}

/// `verify_in_txn`: check that `ds` is exactly what `txn` sees (shared by
/// `verify` and `hold-reader`). Prints the `verified` line unless `quiet`.
pub fn verify_in_txn(txn: &RoTxn<'_>, ds: &DataSet, quiet: bool) -> CmdResult<Verified> {
    let dbi = match txn.open_db(ds.db.as_deref()) {
        Err(e) if e.not_found() => return Err(mismatch(ds, "database not found")),
        r => r.check("mdb_dbi_open")?,
    };
    let flags = txn.db_flags(dbi).check("mdb_dbi_flags")?.bits();
    if flags != ds.flags {
        return Err(mismatch(
            ds,
            format!("DB flags are {flags:#x}, expected {:#x}", ds.flags),
        ));
    }

    let m = Model::build(ds);
    let st = txn.stat(dbi).check("mdb_stat")?;
    if st.entries != m.nentries {
        return Err(mismatch(
            ds,
            format!("mdb_stat entries {}, expected {}", st.entries, m.nentries),
        ));
    }

    // Point lookups: present keys return their first (smallest) value.
    for it in &m.items {
        match txn.get(dbi, &it.key).check("mdb_get")? {
            None => return Err(mismatch(ds, format!("mdb_get: key i={} not found", it.i))),
            Some(data) if data != it.vals[0].as_slice() => {
                return Err(mismatch(
                    ds,
                    format!("mdb_get: key i={} has value {}", it.i, short_hex(data)),
                ))
            }
            Some(_) => {}
        }
    }
    // Deleted keys are gone.
    for &i in &m.absent {
        let rc = match txn.get(dbi, &ds.key(i)) {
            Ok(None) => continue,
            Ok(Some(_)) => 0,
            Err(e) => e.to_err_code(),
        };
        return Err(mismatch(
            ds,
            format!("mdb_get: deleted key i={i} is present (rc={rc})"),
        ));
    }

    let mut cur = txn.cursor(dbi).check("mdb_cursor_open")?;
    check_walk(&mut cur, ds, &m, Direction::Forward)?;
    check_walk(&mut cur, ds, &m, Direction::Backward)?;
    if ds.is_dupsort() {
        check_dups(&mut cur, ds, &m)?;
    }
    // MDB_SET_RANGE on an existing key lands on that key.
    for it in m.items.iter().step_by(7) {
        let (key, _) = found(cur.set_range(&it.key), "mdb_cursor_get(MDB_SET_RANGE)")?;
        if key != it.key.as_slice() {
            return Err(mismatch(
                ds,
                format!("MDB_SET_RANGE(i={}) landed on {}", it.i, short_hex(key)),
            ));
        }
    }
    let v = Verified {
        keys: m.items.len(),
        entries: m.nentries,
        absent: m.absent.len(),
    };
    if !quiet {
        println!(
            "verified db={} keys={} entries={} absent={}",
            ds.label(),
            v.keys,
            v.entries,
            v.absent
        );
    }
    Ok(v)
}

/// `verify <env> <db|-> <count> <seed> [data options]`: in one read txn.
pub fn cmd_verify(o: &mut Opts) -> CmdResult {
    let ds = o.dataset(1)?;
    let env = o.open_env(true)?;
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    verify_in_txn(&txn, &ds, false)?;
    Ok(())
}

// ---- dump ----------------------------------------------------------------------------

/// `subdb_name`: the name of the named DB whose record in the main DB of
/// an environment of `format` has key `key`, if it can be one: names have
/// no NUL byte, and in LMDB 1.0 the key is the name with its terminating
/// NUL (0.9: without).
pub fn subdb_name(format: Format, key: &[u8]) -> Option<&[u8]> {
    let name = match format {
        Format::V09 => key,
        Format::V10 => key.strip_suffix(b"\0")?,
    };
    (!name.contains(&0)).then_some(name)
}

/// `is_subdb`: is `key` of the main DB the name of a named DB? Decided as
/// `mdb_stat -a` does: a name ([`subdb_name`]), and opening it as a DB
/// succeeds (names are bytes, as in C, not necessarily UTF-8). Returns the
/// name and the handle.
pub fn subdb<'k>(txn: &RoTxn<'_>, format: Format, key: &'k [u8]) -> Option<(&'k [u8], Dbi)> {
    let name = subdb_name(format, key)?;
    Some((name, txn.open_db_bytes(Some(name)).ok()?))
}

/// `dump_db`: print `db=<label>`, then `<hex key>=<hex value>` per item in
/// cursor order. Named-DB records of the main DB print as `<hex key>=@db`
/// (their value, an `MDB_db` with page numbers, differs between copies);
/// their names are returned for `--all`.
fn dump_db(
    out: &mut impl Write,
    txn: &RoTxn<'_>,
    format: Format,
    db: Option<&[u8]>,
) -> CmdResult<Vec<Vec<u8>>> {
    let label = db.unwrap_or(b"-");
    let dbi: Dbi = match txn.open_db_bytes(db) {
        Err(e) if e.not_found() => {
            return Err(Fail::mismatch(format!(
                "dump: database {} not found",
                show(label)
            )))
        }
        r => r.check("mdb_dbi_open")?,
    };
    out.write_all(b"db=")?;
    out.write_all(label)?;
    out.write_all(b"\n")?;
    let mut names = Vec::new();
    let mut cur = txn.cursor(dbi).check("mdb_cursor_open")?;
    while let Some((key, data)) = cur.next().check("mdb_cursor_get")? {
        write_hex(out, key)?;
        if let Some((name, _)) = db.is_none().then(|| subdb(txn, format, key)).flatten() {
            out.write_all(b"=@db\n")?;
            names.push(name.to_vec());
            continue;
        }
        out.write_all(b"=")?;
        write_hex(out, data)?;
        out.write_all(b"\n")?;
    }
    Ok(names)
}

/// `print_hex`.
fn write_hex(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = Vec::with_capacity(2 * data.len());
    for b in data {
        buf.push(DIGITS[usize::from(b >> 4)]);
        buf.push(DIGITS[usize::from(b & 15)]);
    }
    out.write_all(&buf)
}

/// `dump <env> <db|->` or `dump <env> --all` (main DB, then every named DB
/// in main-DB order).
pub fn cmd_dump(o: &mut Opts) -> CmdResult {
    if o.all == (o.pos.len() == 2) {
        return Err(Fail::usage(
            "usage: rpeer dump <env> <db|-> | rpeer dump <env> --all",
        ));
    }
    let env = o.open_env(true)?;
    let format = env.format();
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    let mut out = BufWriter::new(io::stdout().lock());
    if o.all {
        for name in dump_db(&mut out, &txn, format, None)? {
            dump_db(&mut out, &txn, format, Some(&name))?;
        }
    } else {
        let db = o.pos[1].as_bytes();
        dump_db(&mut out, &txn, format, (db != b"-").then_some(db))?;
    }
    out.flush()?;
    Ok(())
}
