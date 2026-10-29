//! Long-running commands for cross-process concurrency tests (the
//! counterpart of `c/cpeer_loops.c`).
//!
//! * `hold-reader` pins a snapshot (a read transaction) for a while: MVCC.
//! * `hold-env` keeps the environment open without a read transaction.
//! * `reader-loop` repeatedly opens read transactions and checks invariants
//!   that every committed state of `writer-loop` satisfies.
//! * `writer-loop` commits many small random write transactions; it needs
//!   the write path (stage 3).
//!
//! writer-loop data (README.md), in DB `<db>` (or the main DB):
//!
//! ```text
//! "w%06u" (r)   -> "r:v:len:" + SplitMix64 filler (seeded r << 32 ^ v), len bytes
//! "#count"      -> decimal number of "w" keys
//! "#dups"       -> decimal number of values in the dups DB
//! "#commits"    -> decimal number of committed loop transactions
//! ```
//!
//! and in the DUPSORT DB `<db>.dups` (`dups` for the main DB):
//! `"d%04u" (r) -> t: u64 LE, fmix32(lo32(t) ^ hi32(t) ^ r): u32 LE`.

use std::io::BufRead;
use std::sync::mpsc;
use std::time::Duration;

use lmdb::{CursorOp, Dbi, Environment, RoTxn, Transaction};

use crate::args::{
    now_seconds, parse_seconds, parse_u64, show, sleep_seconds, strtoull10, Check, CmdResult, Fail,
    Opts, EXIT_MISMATCH,
};
use crate::data::verify_in_txn;
use crate::dataset::{fmix32, splitmix_fill};

// ---- hold-reader ---------------------------------------------------------------------

/// `wait_stdin`: wait until `deadline` (in [`now_seconds`] time), or until a
/// line or EOF arrives on stdin.
///
/// The blocking read happens on a helper thread; if the deadline passes
/// first, the thread is simply left behind (the process exits soon after).
fn wait_stdin(deadline: f64) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let _ = tx.send(());
    });
    let left = (deadline - now_seconds()).max(0.0);
    let _ = rx.recv_timeout(Duration::from_secs_f64(left));
}

/// `hold-reader <env> <seconds> [--until-stdin] [<db|-> <count> <seed>
/// [data options]]`: begin a read txn, print `READY txnid=.. pid=..`, wait,
/// then verify the data set (if given) *in the held snapshot* and print
/// `released txnid=.. held=..`.
pub fn cmd_hold_reader(o: &mut Opts) -> CmdResult {
    let secs = parse_seconds(o.pos.get(1).map(String::as_str))?;
    if o.pos.len() != 2 && o.pos.len() != 5 {
        return Err(Fail::usage(
            "hold-reader takes <env> <seconds> [<db|-> <count> <seed>]",
        ));
    }
    let ds = if o.pos.len() == 5 {
        Some(o.dataset(2)?)
    } else {
        None
    };
    let env = o.open_env(true)?;
    let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    let start = now_seconds();
    println!("READY txnid={} pid={}", txn.id(), std::process::id());
    if o.until_stdin {
        wait_stdin(start + secs);
    } else {
        sleep_seconds(secs);
    }
    // The snapshot must still show the data as of READY, no matter what
    // other processes committed meanwhile.
    let verified = match &ds {
        Some(ds) => verify_in_txn(&txn, ds, false).map(|_| ()),
        None => Ok(()),
    };
    println!(
        "released txnid={} held={:.3}",
        txn.id(),
        now_seconds() - start
    );
    verified
}

// ---- hold-env ------------------------------------------------------------------------

/// `hold-env <env> <seconds> [--until-stdin]`: keep the environment open
/// (read-only) without a read transaction, printing `READY pid=..` and then
/// `released held=..`. While this process holds its shared lock on the lock
/// file, later openers are not exclusive and do not reset the reader table,
/// so the slots of dead processes stay until a reader check clears them.
pub fn cmd_hold_env(o: &mut Opts) -> CmdResult {
    let secs = parse_seconds(o.pos.get(1).map(String::as_str))?;
    let env = o.open_env(true)?;
    let start = now_seconds();
    println!("READY pid={}", std::process::id());
    if o.until_stdin {
        wait_stdin(start + secs);
    } else {
        sleep_seconds(secs);
    }
    println!("released held={:.3}", now_seconds() - start);
    drop(env);
    Ok(())
}

// ---- writer-loop (stage 3) -----------------------------------------------------------

/// `writer-loop <env> <db|-> <seconds> <seed> [--keys N] [--max-commits N]`:
/// needs the write path.
pub fn cmd_writer_loop(o: &mut Opts) -> CmdResult {
    parse_seconds(o.pos.get(2).map(String::as_str))?;
    parse_u64(o.pos.get(3).map(String::as_str).unwrap_or_default(), "seed")?;
    Err(Fail::not_implemented("writer-loop"))
}

// ---- shared writer-loop data -----------------------------------------------------------

/// The occasional value forcing overflow pages; the longest value.
const WL_BIG: usize = 20000;

/// `dups_db_name`: the DUPSORT DB that goes with `db`.
fn dups_db_name(db: Option<&str>) -> String {
    match db {
        None => "dups".to_owned(),
        Some(db) => format!("{db}.dups"),
    }
}

/// `wl_value`: `"r:v:len:"` followed by SplitMix64 filler (seeded with
/// `r << 32 ^ v`), `len` bytes in total. As C's `snprintf`, a header that
/// does not fit is truncated and NUL-terminated.
fn wl_value(r: u32, v: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let head = format!("{r}:{v}:{len}:");
    let mut state = (u64::from(r) << 32) ^ v;
    if head.len() < len {
        buf[..head.len()].copy_from_slice(head.as_bytes());
        splitmix_fill(&mut state, &mut buf[head.len()..]);
    } else if len > 0 {
        buf[..len - 1].copy_from_slice(&head.as_bytes()[..len - 1]);
    }
    buf
}

/// `dup_check`: the check word of a dups value.
fn dup_check(t: u64, r: u32) -> u32 {
    fmix32((t as u32) ^ ((t >> 32) as u32) ^ r)
}

/// `get_counter`: a decimal counter from the main DB, 0 if absent.
fn get_counter(txn: &RoTxn<'_>, dbi: Dbi, name: &str) -> CmdResult<u64> {
    match txn.get(dbi, name.as_bytes()).check("mdb_get(counter)")? {
        None => Ok(0),
        Some(data) if data.is_empty() || data.len() >= 32 => Err(Fail::mismatch(format!(
            "counter {name} has size {}",
            data.len()
        ))),
        Some(data) => Ok(strtoull10(data)),
    }
}

// ---- reader-loop ---------------------------------------------------------------------

/// `rl_fail`: a failed reader-loop check.
fn rl_fail(msg: impl std::fmt::Display) -> Fail {
    Fail::mismatch(format!("reader-loop: {msg}"))
}

/// `key_number`: the number in a key `<prefix><digits>` of exactly `len`
/// bytes. (Stricter than C's `strtoul`, which would also accept a sign or
/// leading blanks; writer-loop never writes those.)
fn key_number(key: &[u8], prefix: u8, len: usize) -> Option<u32> {
    if key.len() != len || key.first() != Some(&prefix) {
        return None;
    }
    let digits = &key[1..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// The `"%u:%llu:%zu:"` header of a `w` value, as `sscanf` reads it (the
/// trailing `:` is not needed for three conversions).
fn parse_wheader(data: &[u8]) -> Option<(u32, u64, usize)> {
    let head = &data[..data.len().min(63)];
    let mut fields = head.splitn(4, |&b| b == b':');
    let mut num = |last: bool| -> Option<u64> {
        let f = fields.next()?;
        // The last field ends at the first non-digit.
        let f = if last {
            let n = f.iter().take_while(|b| b.is_ascii_digit()).count();
            &f[..n]
        } else {
            f
        };
        if f.is_empty() || !f.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(f).ok()?.parse().ok()
    };
    let r = u32::try_from(num(false)?).ok()?;
    let v = num(false)?;
    let len = usize::try_from(num(true)?).ok()?;
    Some((r, v, len))
}

/// `check_wvalue`: one `w` record is well formed for its key.
fn check_wvalue(key: &[u8], data: &[u8]) -> CmdResult {
    let Some((r, v, len)) = parse_wheader(data) else {
        return Err(rl_fail(format!("malformed value for key {}", show(key))));
    };
    if key_number(key, b'w', 7) != Some(r) {
        return Err(rl_fail(format!("value of key {} names r={r}", show(key))));
    }
    if len != data.len() || len > WL_BIG {
        return Err(rl_fail(format!(
            "value of key {} has size {}, header says {len}",
            show(key),
            data.len()
        )));
    }
    if wl_value(r, v, len) != data {
        return Err(rl_fail(format!(
            "value of key {} (v={v}) is corrupt",
            show(key)
        )));
    }
    Ok(())
}

/// `check_snapshot`: all invariants in one snapshot; walks the main DB
/// backwards if `reverse`. Returns the snapshot's `#commits`.
fn check_snapshot(txn: &RoTxn<'_>, mdbi: Dbi, ddbi: Dbi, reverse: bool) -> CmdResult<u64> {
    let commits = get_counter(txn, mdbi, "#commits")?;
    let count = get_counter(txn, mdbi, "#count")?;
    let ndups = get_counter(txn, mdbi, "#dups")?;

    let mut seen = 0u64;
    let mut cur = txn.cursor(mdbi).check("mdb_cursor_open")?;
    let (mut op, step) = if reverse {
        (CursorOp::Last, CursorOp::Prev)
    } else {
        (CursorOp::First, CursorOp::Next)
    };
    while let Some((key, data)) = cur.get(None, None, op).check("mdb_cursor_get(main)")? {
        op = step;
        if key.len() != 7 || key[0] != b'w' {
            continue; // counters, named-DB records
        }
        check_wvalue(key, data)?;
        seen += 1;
    }
    if seen != count {
        return Err(rl_fail(format!(
            "snapshot #commits={commits}: {seen} w-keys, #count says {count}"
        )));
    }

    // Every value of every dups key, key by key: MDB_NEXT_DUP within a
    // key, MDB_NEXT_NODUP to the next key.
    let mut dseen = 0u64;
    let mut cur = txn.cursor(ddbi).check("mdb_cursor_open")?;
    let mut op = CursorOp::First;
    while let Some((key, first)) = cur.get(None, None, op).check("mdb_cursor_get(dups)")? {
        op = CursorOp::NextNoDup;
        let Some(r) = key_number(key, b'd', 5) else {
            return Err(rl_fail(format!("bad dups key {}", show(key))));
        };
        let c = cur.count().check("mdb_cursor_count")?;
        let mut k = 0u64;
        let mut data = first;
        loop {
            if data.len() != 12 {
                return Err(rl_fail(format!(
                    "dup value of size {} under d{r:04}",
                    data.len()
                )));
            }
            let t = u64::from_le_bytes(data[..8].try_into().expect("8 bytes"));
            let chk = u32::from_le_bytes(data[8..12].try_into().expect("4 bytes"));
            if chk != dup_check(t, r) {
                return Err(rl_fail(format!("corrupt dup value under d{r:04}")));
            }
            k += 1;
            match cur.next_dup().check("mdb_cursor_get(MDB_NEXT_DUP)")? {
                Some((_, next)) => data = next,
                None => break,
            }
        }
        if k != c {
            return Err(rl_fail(format!(
                "d{r:04}: mdb_cursor_count {c}, walked {k}"
            )));
        }
        dseen += k;
    }
    let entries = txn.stat(ddbi).check("mdb_stat")?.entries as u64;
    if dseen != ndups || entries != ndups {
        return Err(rl_fail(format!(
            "snapshot #commits={commits}: {dseen} dup values, mdb_stat {entries}, #dups says {ndups}"
        )));
    }
    Ok(commits)
}

/// Wait for writer-loop to create its DBs; returns their handles, which
/// stay valid in later transactions.
fn wait_for_dbs(env: &Environment, db: Option<&str>, deadline: f64) -> CmdResult<(Dbi, Dbi)> {
    let dname = dups_db_name(db);
    loop {
        let txn = env.begin_ro_txn().check("mdb_txn_begin")?;
        let rc = txn
            .open_db(db)
            .and_then(|m| txn.open_db(Some(&dname)).map(|d| (m, d)));
        match rc {
            Ok(dbis) => {
                txn.commit().check("mdb_txn_commit")?;
                return Ok(dbis);
            }
            Err(e) if e.not_found() => {}
            Err(e) => return Err(Fail::lmdb("mdb_dbi_open", &e)),
        }
        drop(txn);
        if now_seconds() > deadline {
            return Err(Fail::new(
                EXIT_MISMATCH,
                "reader-loop: writer-loop DBs never appeared",
            ));
        }
        sleep_seconds(0.01);
    }
}

/// `reader-loop <env> <db|-> <seconds>`: check snapshot after snapshot
/// (alternating reset/renew and abort/begin; every third one walks the main
/// DB backwards) until `<seconds>` pass; at least one. Prints `READY` after
/// the first, then `reads=.. first_commit=.. last_commit=..`.
pub fn cmd_reader_loop(o: &mut Opts) -> CmdResult {
    let db = Some(o.pos[1].as_str()).filter(|d| *d != "-");
    let deadline = now_seconds() + parse_seconds(o.pos.get(2).map(String::as_str))?;
    let env = o.open_env(true)?;
    let (mdbi, ddbi) = wait_for_dbs(&env, db, deadline)?;

    let mut reads = 0u64;
    let (mut first, mut last) = (0u64, 0u64);
    let mut txn = env.begin_ro_txn().check("mdb_txn_begin")?;
    loop {
        if reads > 0 {
            txn = if reads % 2 == 1 {
                txn.reset().renew().check("mdb_txn_renew")?
            } else {
                txn.abort();
                env.begin_ro_txn().check("mdb_txn_begin")?
            };
        }
        let commits = check_snapshot(&txn, mdbi, ddbi, reads % 3 == 2)?;
        if commits < last {
            return Err(rl_fail(format!(
                "snapshot went back in time: #commits {commits} after {last}"
            )));
        }
        if reads == 0 {
            first = commits;
            println!("READY");
        }
        reads += 1;
        last = commits;
        if now_seconds() >= deadline {
            break;
        }
    }
    drop(txn);
    println!("reads={reads} first_commit={first} last_commit={last}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wvalue_roundtrip() {
        let v = wl_value(42, 7, 60);
        assert!(v.starts_with(b"42:7:60:"));
        assert_eq!(parse_wheader(&v), Some((42, 7, 60)));
        assert!(check_wvalue(b"w000042", &v).is_ok());
        assert!(check_wvalue(b"w000043", &v).is_err());
        assert_eq!(key_number(b"d0063", b'd', 5), Some(63));
        assert_eq!(key_number(b"d006x", b'd', 5), None);
    }
}
