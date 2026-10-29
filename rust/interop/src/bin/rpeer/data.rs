//! `verify` and `dump` of the deterministic data set; `fill` and `delete`
//! are stage 3 (the counterpart of `c/cpeer_data.c`).
//!
//! `verify` proves that a DB holds exactly the data set (as modified by a
//! prior `delete`), using every read path a peer must get right: point
//! lookups, full cursor walks in both directions, duplicate navigation,
//! `MDB_GET_MULTIPLE` for DUPFIXED, and the DB statistics. It follows the C
//! peer step by step, so that both peers exercise the same LMDB calls.

use std::io::{self, BufWriter, Write};

use lmdb::{CursorOp, Dbi, RoCursor, RoTxn, Transaction};

use crate::args::{found, short_hex, Check, CmdResult, Fail, Opts};
use crate::dataset::{DataSet, Model, ModelItem};

// ---- fill / delete (stage 3) ---------------------------------------------------------

/// `fill <env> <db|-> <count> <seed> [data options] [--per-txn N] [--append]
/// [--reserve]`: needs the write path.
pub fn cmd_fill(o: &mut Opts) -> CmdResult {
    let ds = o.dataset(1)?;
    if o.reserve && ds.is_dupsort() {
        return Err(Fail::usage("--reserve cannot be used with DUPSORT"));
    }
    Err(Fail::not_implemented("fill"))
}

/// `delete <env> <db|-> <count> <seed> [--every N] [data options]`: needs
/// the write path.
pub fn cmd_delete(o: &mut Opts) -> CmdResult {
    o.dataset(1)?;
    if o.every == 0 {
        return Err(Fail::usage("--every must be > 0"));
    }
    Err(Fail::not_implemented("delete"))
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
        if ds.flags & lmdb::consts::MDB_DUPFIXED != 0 {
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

/// `is_subdb`: is `key` of the main DB the name of a named DB? Decided as
/// `mdb_stat -a` does: no NUL byte, and opening it as a DB succeeds.
///
/// Limitation: `Transaction::open_db` takes `&str`, so a named DB whose
/// name is not UTF-8 cannot be opened from Rust; such records dump as
/// plain data (C would print `@db`). The data sets never produce those.
pub fn subdb_name<'k>(txn: &RoTxn<'_>, key: &'k [u8]) -> Option<&'k str> {
    if key.contains(&0) {
        return None;
    }
    let name = std::str::from_utf8(key).ok()?;
    txn.open_db(Some(name)).ok().map(|_| name)
}

/// `dump_db`: print `db=<label>`, then `<hex key>=<hex value>` per item in
/// cursor order. Named-DB records of the main DB print as `<hex key>=@db`
/// (their value, an `MDB_db` with page numbers, differs between copies);
/// their names are returned for `--all`.
fn dump_db(out: &mut impl Write, txn: &RoTxn<'_>, db: Option<&str>) -> CmdResult<Vec<String>> {
    let label = db.unwrap_or("-");
    let dbi: Dbi = match txn.open_db(db) {
        Err(e) if e.not_found() => {
            return Err(Fail::mismatch(format!("dump: database {label} not found")))
        }
        r => r.check("mdb_dbi_open")?,
    };
    writeln!(out, "db={label}")?;
    let mut names = Vec::new();
    let mut cur = txn.cursor(dbi).check("mdb_cursor_open")?;
    while let Some((key, data)) = cur.next().check("mdb_cursor_get")? {
        write_hex(out, key)?;
        if db.is_none() {
            if let Some(name) = subdb_name(txn, key) {
                out.write_all(b"=@db\n")?;
                names.push(name.to_owned());
                continue;
            }
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
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    let mut out = BufWriter::new(io::stdout().lock());
    if o.all {
        for name in dump_db(&mut out, &txn, None)? {
            dump_db(&mut out, &txn, Some(&name))?;
        }
    } else {
        let db = o.pos[1].as_str();
        dump_db(&mut out, &txn, (db != "-").then_some(db))?;
    }
    out.flush()?;
    Ok(())
}
