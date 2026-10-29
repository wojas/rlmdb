//! `stat`, `readers`, `reader-check`, `gen` and `nested-test --check`;
//! `create` and `copy` are stage 3 (the counterpart of `c/cpeer_misc.c`).

use std::io::{self, BufWriter, Write};

use lmdb_core::{Dbi, RoTxn, Transaction};

use crate::args::{parse_u32, parse_u32_opt, parse_u64, show, Check, CmdResult, Fail, Opts};
use crate::data::subdb_name;
use crate::dataset::splitmix_fill;

// ---- create / copy (stage 3) ---------------------------------------------------------

/// `create <env> <mapsize> <maxdbs>`: creating an environment writes its
/// meta pages; the write side of rpeer arrives in stage 3.
pub fn cmd_create(o: &mut Opts) -> CmdResult {
    parse_u64(&o.pos[1], "mapsize")?;
    parse_u32(&o.pos[2], "maxdbs")?;
    Err(Fail::not_implemented("create"))
}

/// `copy <env> <dest> [--compact]`: needs `Environment::copy_to_path`
/// (stage 3).
pub fn cmd_copy(_o: &mut Opts) -> CmdResult {
    Err(Fail::not_implemented("copy"))
}

// ---- stat ----------------------------------------------------------------------------

/// `print_stat_line`: `db=<label> [flags=0x..] depth=.. branch_pages=..
/// leaf_pages=.. overflow_pages=.. entries=..`. `mdb_dbi_flags` rejects the
/// free DB, so its line has no flags.
fn print_stat_line(out: &mut impl Write, txn: &RoTxn<'_>, dbi: Dbi, label: &str) -> CmdResult {
    let st = txn.stat(dbi).check("mdb_stat")?;
    write!(out, "db={label} ")?;
    if dbi != Dbi::FREE {
        let flags = txn.db_flags(dbi).check("mdb_dbi_flags")?.bits();
        write!(out, "flags={flags:#x} ")?;
    }
    writeln!(
        out,
        "depth={} branch_pages={} leaf_pages={} overflow_pages={} entries={}",
        st.depth, st.branch_pages, st.leaf_pages, st.overflow_pages, st.entries
    )?;
    Ok(())
}

/// `stat <env>`: environment info, the free pages, then the free DB, the
/// main DB and every named DB it records (as `mdb_stat -a` finds them).
pub fn cmd_stat(o: &mut Opts) -> CmdResult {
    let env = o.open_env(true)?;
    let st = env.stat().check("mdb_env_stat")?;
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    let info = env.info().check("mdb_env_info")?;
    let mut out = BufWriter::new(io::stdout().lock());
    writeln!(
        out,
        "psize={}\nmapsize={}\nlast_pgno={}\nlast_txnid={}\nmaxreaders={}\nnumreaders={}\nmaxkeysize={}",
        st.page_size,
        info.map_size,
        info.last_pgno,
        info.last_txnid,
        info.max_readers,
        info.num_readers,
        env.max_key_size()
    )?;

    // The free DB: records are txnid -> [count, pgno...].
    let mut free_pages = 0u64;
    let mut cur = txn.cursor(Dbi::FREE).check("mdb_cursor_open(free DB)")?;
    while let Some((_, data)) = cur.next().check("mdb_cursor_get(free DB)")? {
        let count = data
            .get(..8)
            .ok_or_else(|| Fail::mismatch(format!("free DB record of {} bytes", data.len())))?;
        free_pages += u64::from_ne_bytes(count.try_into().expect("8 bytes"));
    }
    writeln!(out, "free_pages={free_pages}")?;
    print_stat_line(&mut out, &txn, Dbi::FREE, "@free")?;

    // Main DB, then every named DB it records.
    let main = txn.open_db(None).check("mdb_dbi_open")?;
    print_stat_line(&mut out, &txn, main, "-")?;
    let mut cur = txn.cursor(main).check("mdb_cursor_open")?;
    while let Some((key, _)) = cur.next_nodup().check("mdb_cursor_get(main)")? {
        if let Some(name) = subdb_name(&txn, key) {
            let dbi = txn.open_db(Some(name)).check("mdb_dbi_open")?;
            print_stat_line(&mut out, &txn, dbi, name)?;
        }
    }
    out.flush()?;
    Ok(())
}

// ---- readers -------------------------------------------------------------------------

/// `readers <env>`: the reader table (`mdb_reader_list`), from a process
/// that takes no slot itself: `reader pid=.. tid=0x.. txnid=<n|->` per used
/// slot, then `readers=.. numreaders=.. maxreaders=..`.
pub fn cmd_readers(o: &mut Opts) -> CmdResult {
    let env = o.open_env(true)?;
    let info = env.info().check("mdb_env_info")?;
    let list = env.reader_list().check("mdb_reader_list")?;
    for r in &list {
        let txnid = r.txnid.map_or_else(|| "-".to_owned(), |t| t.to_string());
        println!("reader pid={} tid={:#x} txnid={txnid}", r.pid, r.thread);
    }
    println!(
        "readers={} numreaders={} maxreaders={}",
        list.len(),
        info.num_readers,
        info.max_readers
    );
    Ok(())
}

/// `reader-check <env>`: `mdb_reader_check`, clearing the slots of dead
/// processes: `reader-check cleared=<n>`.
pub fn cmd_reader_check(o: &mut Opts) -> CmdResult {
    let env = o.open_env(true)?;
    let dead = env.reader_check().check("mdb_reader_check")?;
    println!("reader-check cleared={dead}");
    Ok(())
}

// ---- gen -----------------------------------------------------------------------------

/// `gen <seed> <i> [data options]`: print item `i` of the data set: test
/// vectors for both implementations of the generator.
pub fn cmd_gen(o: &mut Opts) -> CmdResult {
    let seed = parse_u32_opt(o.pos.first().map(String::as_str), "seed")?;
    let i = parse_u32_opt(o.pos.get(1).map(String::as_str), "i")?;
    if i == u32::MAX {
        return Err(Fail::usage("i must be < 2^32 - 1"));
    }
    let ds = &mut o.ds;
    ds.seed = seed;
    ds.count = i + 1;
    ds.finish().map_err(Fail::usage)?;
    let mut out = String::new();
    out.push_str(&format!(
        "key={}\nndups={}\n",
        crate::args::hex(&ds.key(i)),
        ds.ndups(i)
    ));
    for d in 0..ds.ndups(i) {
        out.push_str(&format!("value{d}={}\n", crate::args::hex(&ds.value(i, d))));
    }
    print!("{out}");
    Ok(())
}

// ---- nested-test ---------------------------------------------------------------------

/// Keys `n%05u` written by nested-test (the odd ones survive).
const NESTED_N: u32 = 2000;
/// Size of the overflow value `k5`.
const BIG_LEN: usize = 40000;

/// `make_big`: the 40000-byte value of `k5`, SplitMix64 words
/// (little-endian) from state 5.
fn make_big() -> Vec<u8> {
    let mut big = vec![0u8; BIG_LEN];
    splitmix_fill(&mut 5, &mut big);
    big
}

/// `expect`: key `k` holds `v`, or is absent if `v` is `None`.
fn expect(where_: &str, txn: &RoTxn<'_>, dbi: Dbi, k: &str, v: Option<&[u8]>) -> CmdResult {
    let got = txn.get(dbi, k.as_bytes());
    match (v, got) {
        (None, Ok(None)) => Ok(()),
        (None, other) => {
            let rc = match other {
                Ok(_) => 0,
                Err(e) => e.to_err_code(),
            };
            Err(Fail::mismatch(format!(
                "nested-test {where_}: {k} should be absent (rc={rc})"
            )))
        }
        (Some(_), Ok(None)) => Err(Fail::mismatch(format!(
            "nested-test {where_}: {k} not found"
        ))),
        (Some(_), Err(e)) => Err(Fail::lmdb("mdb_get", &e)),
        (Some(want), Ok(Some(data))) if data != want => Err(Fail::mismatch(format!(
            "nested-test {where_}: {k} has the wrong value ({} bytes)",
            data.len()
        ))),
        (Some(_), Ok(Some(_))) => Ok(()),
    }
}

/// `expect_no_db`: DB `name` does not exist (checked in a fresh read txn).
fn expect_no_db(env: &lmdb_core::Environment, name: &str) -> CmdResult {
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    match txn.open_db(Some(name)) {
        Err(e) if e.not_found() => Ok(()),
        other => {
            let rc = match other {
                Ok(_) => 0,
                Err(e) => e.to_err_code(),
            };
            Err(Fail::mismatch(format!(
                "nested-test: DB {name} should not exist (rc={rc})"
            )))
        }
    }
}

/// `nested_check`: the final state of nested-test (README.md): DB `nested`
/// is `k1=v1 k2=v2 k3=v3 k5=<big>` and `n%05u = val%05u` for odd `u`, in
/// that cursor order; DBs `nested2` and `nested3` do not exist.
fn nested_check(env: &lmdb_core::Environment) -> CmdResult {
    let small = [("k1", "v1"), ("k2", "v2"), ("k3", "v3")];
    let big = make_big();
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    let dbi = match txn.open_db(Some("nested")) {
        Err(e) if e.not_found() => return Err(Fail::mismatch("nested-test: DB nested not found")),
        r => r.check("mdb_dbi_open(nested)")?,
    };
    for (k, v) in small {
        expect("check", &txn, dbi, k, Some(v.as_bytes()))?;
    }
    expect("check", &txn, dbi, "k5", Some(&big))?;
    let entries = txn.stat(dbi).check("mdb_stat")?.entries;
    let want = 4 + (NESTED_N / 2) as usize;
    if entries != want {
        return Err(Fail::mismatch(format!(
            "nested-test check: {entries} entries, expected {want}"
        )));
    }

    // Cursor order: k1 k2 k3 k5 n00001 n00003 ...
    let mut cur = txn.cursor(dbi).check("mdb_cursor_open")?;
    let (mut n, mut u) = (0usize, 1u32);
    while let Some((key, data)) = cur.next().check("mdb_cursor_get")? {
        let want = if n < 4 {
            if n < 3 { small[n].0 } else { "k5" }.to_owned()
        } else {
            let (k, v) = (format!("n{u:05}"), format!("val{u:05}"));
            if data != v.as_bytes() {
                return Err(Fail::mismatch(format!(
                    "nested-test check: {k} has the wrong value"
                )));
            }
            u += 2;
            k
        };
        if key != want.as_bytes() {
            return Err(Fail::mismatch(format!(
                "nested-test check: entry {n} is {}, expected {want}",
                show(key)
            )));
        }
        n += 1;
    }
    // End the snapshot before the "does not exist" checks, which take
    // fresh ones (as C does).
    drop(txn);
    expect_no_db(env, "nested2")?;
    expect_no_db(env, "nested3")
}

/// `nested-test <env> [--check]`: only `--check` (read-only) is
/// implemented; the write mode needs nested write transactions (stage 3).
pub fn cmd_nested_test(o: &mut Opts) -> CmdResult {
    if !o.check {
        return Err(Fail::not_implemented("nested-test (write mode)"));
    }
    let env = o.open_env(true)?;
    nested_check(&env)?;
    println!("nested ok mode=check");
    Ok(())
}
