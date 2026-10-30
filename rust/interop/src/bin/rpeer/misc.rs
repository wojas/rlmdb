//! `create`, `stat`, `readers`, `reader-check`, `copy`, `nested-test` and
//! `gen` (the counterpart of `c/cpeer_misc.c`).

use std::io::{self, BufWriter, Write};

use rlmdb::{DatabaseFlags, Dbi, Environment, MdbError, RoTxn, RwTxn, Transaction, WriteFlags};

use crate::args::{
    parse_u32, parse_u32_opt, parse_u64, show, Check, CmdResult, Fail, Opts, EXIT_LMDB,
};
use crate::data::subdb;
use crate::dataset::splitmix_fill;

// ---- create ----------------------------------------------------------------------------

/// `create <env> <mapsize> <maxdbs>`: create (or open) the environment with
/// that map size and DB limit and print `created psize=.. mapsize=..
/// maxkeysize=.. last_txnid=..`.
pub fn cmd_create(o: &mut Opts) -> CmdResult {
    o.mapsize = parse_u64(&o.pos[1], "mapsize")?;
    o.maxdbs = parse_u32(&o.pos[2], "maxdbs")?;
    let env = o.open_env(false)?;
    let st = env.stat().check("mdb_env_stat")?;
    let info = env.info().check("mdb_env_info")?;
    println!(
        "created psize={} mapsize={} maxkeysize={} last_txnid={}",
        st.page_size,
        info.map_size,
        env.max_key_size(),
        info.last_txnid
    );
    Ok(())
}

// ---- copy ------------------------------------------------------------------------------

/// `copy <env> <dest> [--compact]`: `mdb_env_copy2`, with `MDB_CP_COMPACT`
/// for `--compact`. `<dest>` is a directory, created if missing (with
/// `--nosubdir` the new data file); the data file must not exist.
pub fn cmd_copy(o: &mut Opts) -> CmdResult {
    let dest = o.pos[1].clone();
    // The destination directory must exist (as for mdb_copy); with
    // --nosubdir `dest` is the new data file.
    if !o.nosubdir {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o775).create(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Fail::new(EXIT_LMDB, format!("mkdir {dest}: {e}"))),
        }
    }
    let env = o.open_env(true)?;
    copy_env(&env, &dest, o.compact)?;
    println!("copied compact={}", u8::from(o.compact));
    Ok(())
}

/// `mdb_env_copy2(env, dest, compact ? MDB_CP_COMPACT : 0)`.
fn copy_env(env: &Environment, dest: &str, compact: bool) -> CmdResult {
    env.copy_to_path(dest, compact).check("mdb_env_copy2")
}

// ---- stat ----------------------------------------------------------------------------

/// `print_stat_line`: `db=<label> [flags=0x..] depth=.. branch_pages=..
/// leaf_pages=.. overflow_pages=.. entries=..`. `mdb_dbi_flags` rejects the
/// free DB, so its line has no flags.
fn print_stat_line(out: &mut impl Write, txn: &RoTxn<'_>, dbi: Dbi, label: &[u8]) -> CmdResult {
    let st = txn.stat(dbi).check("mdb_stat")?;
    // The label is a DB name: bytes, printed as they are (C's "%s").
    out.write_all(b"db=")?;
    out.write_all(label)?;
    out.write_all(b" ")?;
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
    print_stat_line(&mut out, &txn, Dbi::FREE, b"@free")?;

    // Main DB, then every named DB it records.
    let main = txn.open_db(None).check("mdb_dbi_open")?;
    print_stat_line(&mut out, &txn, main, b"-")?;
    let mut cur = txn.cursor(main).check("mdb_cursor_open")?;
    while let Some((key, _)) = cur.next_nodup().check("mdb_cursor_get(main)")? {
        if let Some((name, dbi)) = subdb(&txn, env.format(), key) {
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
fn expect(where_: &str, txn: &impl Transaction, dbi: Dbi, k: &str, v: Option<&[u8]>) -> CmdResult {
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
fn expect_no_db(env: &Environment, name: &str) -> CmdResult {
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
fn nested_check(env: &Environment) -> CmdResult {
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

/// `expect_entries`: `dbi` holds `n` items.
fn expect_entries(where_: &str, txn: &impl Transaction, dbi: Dbi, n: usize) -> CmdResult {
    let entries = txn.stat(dbi).check("mdb_stat")?.entries;
    if entries != n {
        return Err(Fail::mismatch(format!(
            "nested-test {where_}: {entries} entries, expected {n}"
        )));
    }
    Ok(())
}

/// `put_s`: store `v` under `k`.
fn put_s(txn: &mut RwTxn<'_>, dbi: Dbi, k: &str, v: &[u8]) -> CmdResult {
    txn.put(dbi, k.as_bytes(), v, WriteFlags::empty())
        .check("mdb_put")
}

/// `del_s`: delete `k`, which must exist.
fn del_s(txn: &mut RwTxn<'_>, dbi: Dbi, k: &str) -> CmdResult {
    let what = format!("mdb_del({k})");
    if txn.del(dbi, k.as_bytes(), None).check(&what)? {
        Ok(())
    } else {
        Err(Fail::lmdb(&what, &MdbError::NotFound.into()))
    }
}

/// `nested_write`: the write mode of nested-test, step by step as in C
/// (README.md); every step checks its intermediate states.
fn nested_write(env: &Environment) -> CmdResult {
    let big = make_big();
    let begin = || env.begin_rw_txn().check("mdb_txn_begin");

    // 1. A committed child is visible to the parent and is persisted.
    let mut t = begin()?;
    let dbi = t
        .create_db(Some("nested"), DatabaseFlags::CREATE)
        .check("mdb_dbi_open(nested)")?;
    put_s(&mut t, dbi, "k1", b"v1")?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    expect("1/child", &c, dbi, "k1", Some(b"v1"))?;
    put_s(&mut c, dbi, "k2", b"v2")?;
    c.commit().check("mdb_txn_commit(child)")?;
    expect("1/parent", &t, dbi, "k2", Some(b"v2"))?;
    t.commit().check("mdb_txn_commit")?;

    // 2. An aborted child changes nothing, including overwrites.
    let mut t = begin()?;
    put_s(&mut t, dbi, "k3", b"v3")?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    put_s(&mut c, dbi, "k4", b"v4")?;
    put_s(&mut c, dbi, "k1", b"v1b")?;
    expect("2/child", &c, dbi, "k3", Some(b"v3"))?;
    c.abort();
    expect("2/parent", &t, dbi, "k4", None)?;
    expect("2/parent", &t, dbi, "k1", Some(b"v1"))?;
    t.commit().check("mdb_txn_commit")?;

    // 3. Overflow pages, and a committed grandchild inside an aborted child.
    let mut t = begin()?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    put_s(&mut c, dbi, "k5", &big)?;
    c.commit().check("mdb_txn_commit(child)")?;
    expect("3/parent", &t, dbi, "k5", Some(&big))?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    del_s(&mut c, dbi, "k5")?;
    put_s(&mut c, dbi, "k6", b"v6")?;
    let mut g = c.begin_nested_txn().check("mdb_txn_begin(grandchild)")?;
    put_s(&mut g, dbi, "k7", b"v7")?;
    del_s(&mut g, dbi, "k2")?;
    expect("3/grandchild", &g, dbi, "k5", None)?;
    g.commit().check("mdb_txn_commit(grandchild)")?;
    expect("3/child", &c, dbi, "k7", Some(b"v7"))?;
    expect("3/child", &c, dbi, "k2", None)?;
    c.abort();
    expect("3/parent", &t, dbi, "k5", Some(&big))?;
    expect("3/parent", &t, dbi, "k6", None)?;
    expect("3/parent", &t, dbi, "k7", None)?;
    expect("3/parent", &t, dbi, "k2", Some(b"v2"))?;
    t.commit().check("mdb_txn_commit")?;

    // 4. A DB created in a committed child of an aborted parent is gone.
    let mut t = begin()?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    let d2 = c
        .create_db(Some("nested2"), DatabaseFlags::CREATE)
        .check("mdb_dbi_open(nested2)")?;
    put_s(&mut c, d2, "x", b"1")?;
    c.commit().check("mdb_txn_commit(child)")?;
    expect("4/parent", &t, d2, "x", Some(b"1"))?;
    t.abort();
    expect_no_db(env, "nested2")?;

    // 5. Page splits: an aborted child with many keys leaves no trace; a
    //    committed child whose committed grandchild deletes half of them
    //    leaves the other half.
    let mut t = begin()?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    for u in 0..NESTED_N {
        put_s(
            &mut c,
            dbi,
            &format!("n{u:05}"),
            format!("val{u:05}").as_bytes(),
        )?;
    }
    c.abort();
    expect("5/parent", &t, dbi, "n00000", None)?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    for u in 0..NESTED_N {
        put_s(
            &mut c,
            dbi,
            &format!("n{u:05}"),
            format!("val{u:05}").as_bytes(),
        )?;
    }
    let mut g = c.begin_nested_txn().check("mdb_txn_begin(grandchild)")?;
    for u in (0..NESTED_N).step_by(2) {
        del_s(&mut g, dbi, &format!("n{u:05}"))?;
    }
    g.commit().check("mdb_txn_commit(grandchild)")?;
    c.commit().check("mdb_txn_commit(child)")?;
    expect_entries("5/parent", &t, dbi, 4 + (NESTED_N / 2) as usize)?;
    t.commit().check("mdb_txn_commit")?;

    // 6. mdb_drop in children: emptying in an aborted child, deleting in a
    //    committed one.
    nested_drop(env)
}

/// Step 6 of `nested_write`: `mdb_drop` in children: emptying in an
/// aborted child, deleting in a committed one.
fn nested_drop(env: &Environment) -> CmdResult {
    let mut t = env.begin_rw_txn().check("mdb_txn_begin")?;
    let d3 = t
        .create_db(Some("nested3"), DatabaseFlags::CREATE)
        .check("mdb_dbi_open(nested3)")?;
    for u in 0..10 {
        put_s(&mut t, d3, &format!("z{u}"), b"z")?;
    }
    t.commit().check("mdb_txn_commit")?;
    let mut t = env.begin_rw_txn().check("mdb_txn_begin")?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    c.clear_db(d3).check("mdb_drop(0)")?;
    expect_entries("6/child", &c, d3, 0)?;
    c.abort();
    expect_entries("6/parent", &t, d3, 10)?;
    let mut c = t.begin_nested_txn().check("mdb_txn_begin(child)")?;
    c.drop_db(d3).check("mdb_drop(1)")?;
    c.commit().check("mdb_txn_commit(child)")?;
    t.commit().check("mdb_txn_commit")?;
    expect_no_db(env, "nested3")
}

/// `nested-test <env> [--check]`: nested write transactions (the write
/// mode runs `nested_write` first), then the final state is checked; with
/// `--check` read-only.
pub fn cmd_nested_test(o: &mut Opts) -> CmdResult {
    let env = o.open_env(o.check)?;
    if !o.check {
        nested_write(&env)?;
    }
    nested_check(&env)?;
    println!("nested ok mode={}", if o.check { "check" } else { "write" });
    Ok(())
}
