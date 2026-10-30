//! Regression tests for bugs found by the write-path review and the model
//! test (`tests/model.rs`).

mod common;

use std::process::Command;

use rlmdb::{CursorOp, DatabaseFlags, Dbi, Environment, EnvironmentFlags, Transaction, WriteFlags};

fn env(dir: &std::path::Path) -> Environment {
    Environment::builder()
        .map_size(64 << 20)
        .max_dbs(4)
        .flags(EnvironmentFlags::NO_SYNC)
        .open(dir)
        .unwrap()
}

/// `MDB_GET_MULTIPLE` on a key with a single item, reached by moving off a
/// key with duplicates: the sub-cursor is still initialised for the other
/// key. Rust used to fail with `MDB_CORRUPTED`; C returns the other key's
/// duplicates (KNOWN_C_BUGS.md #27). Nothing to fetch is the answer.
#[test]
fn get_multiple_on_a_single_item_after_a_key_with_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let flags = DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED;
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(Some("f"), flags).unwrap();
    for d in [b"1111", b"2222", b"3333"] {
        txn.put(db, b"a", d, WriteFlags::empty()).unwrap();
    }
    txn.put(db, b"b", b"4444", WriteFlags::empty()).unwrap();
    for d in [b"5555", b"6666"] {
        txn.put(db, b"c", d, WriteFlags::empty()).unwrap();
    }
    txn.commit().unwrap();

    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(db).unwrap();
    let multiple = |c: &mut rlmdb::RoCursor<'_>| {
        c.get(None, None, CursorOp::GetMultiple)
            .unwrap()
            .map(|(k, d)| (k.to_vec(), d.to_vec()))
    };
    let nothing = Some((b"b".to_vec(), Vec::new()));
    // NEXT_NODUP from a key with duplicates.
    c.get(Some(b"a"), None, CursorOp::Set).unwrap().unwrap();
    assert_eq!(
        c.get(None, None, CursorOp::NextNoDup).unwrap().unwrap().0,
        b"b"
    );
    assert_eq!(multiple(&mut c), nothing);
    // NEXT from the last duplicate.
    c.get(Some(b"a"), None, CursorOp::Set).unwrap().unwrap();
    c.get(None, None, CursorOp::LastDup).unwrap().unwrap();
    assert_eq!(c.get(None, None, CursorOp::Next).unwrap().unwrap().0, b"b");
    assert_eq!(multiple(&mut c), nothing);
    // PREV from the first duplicate of the next key.
    c.get(Some(b"c"), None, CursorOp::Set).unwrap().unwrap();
    assert_eq!(c.get(None, None, CursorOp::Prev).unwrap().unwrap().0, b"b");
    assert_eq!(multiple(&mut c), nothing);
    // PREV_MULTIPLE has no earlier page of duplicates of "b".
    c.get(Some(b"c"), None, CursorOp::Set).unwrap().unwrap();
    c.get(None, None, CursorOp::PrevNoDup).unwrap().unwrap();
    assert!(c.get(None, None, CursorOp::PrevMultiple).unwrap().is_none());
    // A key with duplicates still gives its page.
    c.get(Some(b"c"), None, CursorOp::Set).unwrap().unwrap();
    assert_eq!(multiple(&mut c).unwrap().1, b"55556666".to_vec());

    // C, for the record: GET_MULTIPLE on "b" returns "a"'s duplicates.
    let Some(c_tools) = common::c_tools() else {
        return;
    };
    let hex = |b: &[u8]| common::hex(b);
    let out = Command::new(c_tools.bin("cursorops"))
        .arg(dir.path())
        .arg("f")
        .arg(format!("set:{}", hex(b"a")))
        .arg("next_nodup")
        .arg("get_multiple")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(2).unwrap_or_default().to_owned();
    eprintln!("C: {line}");
    assert!(
        line.starts_with(&format!("get_multiple rc=0 d={}", hex(b"111122223333"))),
        "C's GET_MULTIPLE on a single item after a key with duplicates: {line}"
    );
}

/// Cursor operations after a failed `MDB_SET` / `MDB_GET_BOTH` on a
/// `MDB_DUPFIXED` database. A failed exact search leaves the cursor on the
/// insertion point, which is past the end of its leaf page when the key
/// would go after the page's last key. `MDB_PREV_MULTIPLE` then failed with
/// `MDB_CORRUPTED` in Rust: it read the current key first, and C's
/// `mdb_cursor_get` does not. C finds the sub-cursor uninitialised
/// (`mdb_cursor_set` clears it) and returns `MDB_NOTFOUND`. The same probe
/// runs the other operations whose dispatch does not start from the current
/// key; every result and position must be C's.
#[test]
fn cursor_ops_after_a_failed_set_on_dupfixed() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let flags = DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED;
    let key = |i: u32| format!("k{i:05}").into_bytes();
    let mut txn = env.begin_rw_txn().unwrap();
    let db = txn.create_db(Some("f"), flags).unwrap();
    // Even keys exist: some with a single item, most with a few
    // duplicates, a few with several pages of them (a sub-DB).
    for i in (0..800).step_by(2) {
        let n = match i {
            _ if i % 97 == 0 => 5000,
            _ if i % 3 == 0 => 1,
            _ => i % 5 + 2,
        };
        for d in 0..n {
            txn.put(db, &key(i), &(d * 2 + 1).to_be_bytes(), WriteFlags::empty())
                .unwrap();
        }
    }
    txn.commit().unwrap();

    // Each case: reset the cursor with MDB_FIRST (which also clears a
    // C_EOF that MDB_SET's shortcuts would keep, KNOWN_C_BUGS.md #28), fail
    // a search, then probe.
    const PROBES: &[&str] = &[
        "prev_multiple",
        "get_multiple",
        "next_multiple",
        "first_dup",
        "last_dup",
        "next_nodup",
        "prev_nodup",
    ];
    let hex = common::hex;
    let mut cases = Vec::new();
    for i in (1..801).step_by(2) {
        // A missing key; a missing key with data; an existing key with a
        // duplicate that sorts after all of its duplicates.
        cases.push(format!("set:{}", hex(&key(i))));
        cases.push(format!("get_both:{}:{}", hex(&key(i)), hex(&[0; 4])));
        cases.push(format!("get_both:{}:{}", hex(&key(i - 1)), hex(&[0xff; 4])));
        cases.push(format!(
            "get_both_range:{}:{}",
            hex(&key(i - 1)),
            hex(&[0xff; 4])
        ));
    }
    let mut ops = Vec::new();
    for case in &cases {
        for probe in PROBES {
            ops.extend(["first".to_owned(), case.clone(), (*probe).to_owned()]);
        }
    }

    let txn = env.begin_ro_txn().unwrap();
    let db = txn.open_db(Some("f")).unwrap();
    let rust = common::rust_cursorops(&txn, db, &ops);
    let corrupted = format!("rc={}", rlmdb::consts::MDB_CORRUPTED);
    for (op, line) in ops.iter().zip(&rust) {
        assert!(!line.contains(&corrupted), "{op}: {line}");
    }
    // The case of the report: past the end of a leaf page (GET_CURRENT
    // says MDB_NOTFOUND), MDB_PREV_MULTIPLE reports MDB_NOTFOUND.
    // (Only for a missing key: on an existing key GET_CURRENT also fails
    // when the sub-cursor is past its end.)
    let past_end = ops
        .chunks(3)
        .zip(rust.chunks(3))
        .filter(|(o, _)| o[2] == "prev_multiple" && o[1].split(':').count() < 3)
        .filter(|(_, l)| l[1].ends_with("| cur rc=-30798 k=- d=-"))
        .inspect(|(_, l)| assert_eq!(l[2], "prev_multiple rc=-30798 d=- | cur rc=-30798 k=- d=-"))
        .count();
    assert!(
        past_end > 0,
        "no case left the cursor past the end of a page"
    );

    // The same through a write cursor (one dispatch for both).
    let mut wtxn = env.begin_rw_txn().unwrap();
    let wdb = wtxn.open_db(Some("f")).unwrap();
    let c = wtxn.open_cursor(wdb).unwrap();
    let k = key(799);
    assert!(wtxn
        .cursor_get(c, Some(&k), None, CursorOp::Set)
        .unwrap()
        .is_none());
    assert!(wtxn
        .cursor_get(c, None, None, CursorOp::GetCurrent)
        .unwrap()
        .is_none());
    assert!(get(&mut wtxn, c, CursorOp::PrevMultiple).is_none());
    drop(wtxn);

    let Some(c_tools) = common::c_tools() else {
        return;
    };
    let out = Command::new(c_tools.bin("cursorops"))
        .arg(dir.path())
        .arg("f")
        .args(&ops)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "C cursorops: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c_lines: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(c_lines.len(), rust.len());
    for (i, (cl, rl)) in c_lines.iter().zip(&rust).enumerate() {
        // MDB_GET_MULTIPLE with nothing to fetch: C leaves `data` untouched,
        // Rust returns it empty (KNOWN_C_BUGS.md #8).
        let cl = cl.replacen("get_multiple rc=0 d=- ", "get_multiple rc=0 d= ", 1);
        // KNOWN_C_BUGS.md #4: past the end of a leaf page C's
        // mdb_cursor_next reads the pointer slot NUMKEYS from the free
        // space; a stale one naming an F_DUPDATA node makes MDB_NEXT_MULTIPLE
        // "succeed" with the stale sub-cursor's duplicates while the cursor
        // stays past the end. Rust has no item there, hence no duplicates.
        if ops[i] == "next_multiple"
            && cl.starts_with("next_multiple rc=0 d=")
            && cl.ends_with("| cur rc=-30798 k=- d=-")
        {
            assert_eq!(rl, "next_multiple rc=-30798 d=- | cur rc=-30798 k=- d=-");
            continue;
        }
        assert_eq!(&cl, rl, "op {i}: {}, after {:?}", ops[i], &ops[i - 2..i]);
    }
}

/// Builds a small C program against the C library of the tests, or `None`
/// without C tools.
fn c_program(name: &str, source: &str) -> Option<std::path::PathBuf> {
    let tools = common::c_tools()?;
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("write_review");
    std::fs::create_dir_all(&dir).unwrap();
    // One binary per version of the source; built under a temporary name.
    let hash = source.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let bin = dir.join(format!("{name}-{hash:016x}"));
    if bin.exists() {
        return Some(bin);
    }
    let src = dir.join(format!("{name}-{}.c", std::process::id()));
    let tmp = dir.join(format!("{name}-{}", std::process::id()));
    std::fs::write(&src, source).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let out = Command::new(cc)
        .arg("-pthread")
        .arg("-I")
        .arg(&tools.dir)
        .arg("-o")
        .arg(&tmp)
        .arg(&src)
        .arg(tools.dir.join("liblmdb.a"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cc {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&src);
    std::fs::rename(&tmp, &bin).unwrap();
    Some(bin)
}

/// Scenarios run through C, printing one line per cursor result.
const C_SCENARIOS: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "lmdb.h"
#define CHK(x) do { int rc_ = (x); if (rc_) { printf("%s: %s\n", #x, mdb_strerror(rc_)); exit(1); } } while (0)
static MDB_val v(const char *s) { MDB_val x = { strlen(s), (void *)s }; return x; }
static void show(const char *what, int rc, MDB_val *k, MDB_val *d) {
	if (rc) printf("%s rc=%d\n", what, rc);
	else printf("%s %.*s %.*s\n", what, (int)k->mv_size, (char *)k->mv_data,
		(int)d->mv_size, (char *)d->mv_data);
}
static void put(MDB_txn *t, MDB_dbi db, const char *k, const char *d, unsigned f) {
	MDB_val kv = v(k), dv = v(d); CHK(mdb_put(t, db, &kv, &dv, f));
}
int main(int argc, char **argv) {
	MDB_env *env; MDB_txn *txn; MDB_dbi db; MDB_cursor *c, *c2;
	MDB_val k, d; int rc, scenario = atoi(argv[2]);
	CHK(mdb_env_create(&env)); CHK(mdb_env_set_maxdbs(env, 4));
	CHK(mdb_env_open(env, argv[1], MDB_NOSYNC, 0664));
	CHK(mdb_txn_begin(env, NULL, 0, &txn));
	CHK(mdb_dbi_open(txn, "t", MDB_CREATE | MDB_DUPSORT, &db));
	CHK(mdb_cursor_open(txn, db, &c)); CHK(mdb_cursor_open(txn, db, &c2));
	switch (scenario) {
	case 1: /* delete before the cursor on the same page */
		put(txn, db, "a", "1", 0); put(txn, db, "a", "2", 0);
		put(txn, db, "b", "1", 0); put(txn, db, "b", "2", 0);
		put(txn, db, "c", "1", 0); put(txn, db, "c", "2", 0);
		k = v("b"); CHK(mdb_cursor_get(c, &k, &d, MDB_SET));
		k = v("a"); CHK(mdb_del(txn, db, &k, NULL));
		rc = mdb_cursor_get(c, &k, &d, MDB_NEXT); show("next", rc, &k, &d);
		break;
	case 2: /* a stale C_DEL activated by another cursor's put */
		put(txn, db, "a", "1", 0); put(txn, db, "a", "2", 0);
		put(txn, db, "b", "5", 0);
		put(txn, db, "c", "1", 0); put(txn, db, "c", "2", 0);
		k = v("a"); CHK(mdb_cursor_get(c, &k, &d, MDB_SET));
		CHK(mdb_cursor_del(c, MDB_NODUPDATA));
		rc = mdb_cursor_get(c, &k, &d, MDB_GET_CURRENT); show("current", rc, &k, &d);
		k = v("b"); d = v("7"); CHK(mdb_cursor_put(c2, &k, &d, 0));
		rc = mdb_cursor_get(c, &k, &d, MDB_NEXT); show("next", rc, &k, &d);
		break;
	case 3: /* MDB_LAST, a duplicate added after it, MDB_NEXT */
		put(txn, db, "a", "1", 0); put(txn, db, "b", "1", 0); put(txn, db, "b", "2", 0);
		CHK(mdb_cursor_get(c, &k, &d, MDB_LAST));
		put(txn, db, "b", "3", 0);
		rc = mdb_cursor_get(c, &k, &d, MDB_NEXT); show("next", rc, &k, &d);
		break;
	case 4: { /* MDB_MULTIPLE with an item equal to the key's only item */
		MDB_dbi f; MDB_val m[2]; size_t n = 0;
		CHK(mdb_dbi_open(txn, "f", MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED, &f));
		put(txn, f, "x", "11", 0);
		CHK(mdb_cursor_open(txn, f, &c2));
		k = v("x"); m[0].mv_size = 2; m[0].mv_data = "112233"; m[1].mv_size = 3;
		rc = mdb_cursor_put(c2, &k, m, MDB_MULTIPLE);
		CHK(mdb_cursor_count(c2, &n));
		printf("multiple rc=%d written=%zu count=%zu\n", rc, m[1].mv_size, n);
		break;
	}
	}
	mdb_txn_abort(txn); mdb_env_close(env); return 0;
}
"#;

/// The output of C scenario `n` (see `C_SCENARIOS`), if C is available.
fn c_scenario(n: u32) -> Option<String> {
    static BIN: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let bin = BIN
        .get_or_init(|| c_program("scenarios", C_SCENARIOS))
        .as_ref()?;
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin)
        .arg(dir.path())
        .arg(n.to_string())
        .output()
        .unwrap();
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn dupsort_txn_db(txn: &mut rlmdb::RwTxn<'_>) -> rlmdb::Dbi {
    txn.create_db(Some("t"), DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT)
        .unwrap()
}

fn get(txn: &mut rlmdb::RwTxn<'_>, c: rlmdb::CursorId, op: CursorOp) -> Option<(Vec<u8>, Vec<u8>)> {
    txn.cursor_get(c, None, None, op)
        .unwrap()
        .map(|(k, d)| (k.to_vec(), d.to_vec()))
}

fn item(k: &str, d: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    Some((k.as_bytes().to_vec(), d.as_bytes().to_vec()))
}

/// After a delete earlier on its leaf page, a cursor on a key with
/// duplicates returned its current item again on `MDB_NEXT`: C's
/// `mdb_cursor_del0` marks the sub-cursor of every cursor at or after the
/// deleted node `C_DEL` (KNOWN_C_BUGS.md #29).
#[test]
fn next_after_a_delete_before_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = dupsort_txn_db(&mut txn);
    for (k, d) in [
        ("a", "1"),
        ("a", "2"),
        ("b", "1"),
        ("b", "2"),
        ("c", "1"),
        ("c", "2"),
    ] {
        txn.put(db, k.as_bytes(), d.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    let c = txn.open_cursor(db).unwrap();
    txn.cursor_get(c, Some(b"b"), None, CursorOp::Set).unwrap();
    assert!(txn.del(db, b"a", None).unwrap());
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("b", "2"));
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("c", "1"));
    if let Some(out) = c_scenario(1) {
        assert_eq!(out.trim(), "next b 1", "C");
    }
}

/// A sub-cursor marked `C_DEL` by a delete that moved its cursor onto a key
/// without duplicates: when another cursor gives that key a second item,
/// C's `mdb_xcursor_init2` only adds `C_INITIALIZED`, and the cursor's
/// next `MDB_NEXT` returned its item again (KNOWN_C_BUGS.md #29).
#[test]
fn next_after_another_cursor_adds_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = dupsort_txn_db(&mut txn);
    for (k, d) in [("a", "1"), ("a", "2"), ("b", "5"), ("c", "1"), ("c", "2")] {
        txn.put(db, k.as_bytes(), d.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    let (c, c2) = (txn.open_cursor(db).unwrap(), txn.open_cursor(db).unwrap());
    txn.cursor_get(c, Some(b"a"), None, CursorOp::Set).unwrap();
    txn.cursor_del(c, WriteFlags::NO_DUP_DATA).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::GetCurrent), item("b", "5"));
    txn.cursor_put(c2, b"b", b"7", WriteFlags::empty()).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("b", "7"));
    if let Some(out) = c_scenario(2) {
        assert_eq!(out.trim(), "current b 5\nnext b 5", "C");
    }
}

/// `MDB_LAST` leaves `C_EOF` set; C's `mdb_cursor_next` then returns
/// `MDB_NOTFOUND` without looking, even after items were added after the
/// cursor (KNOWN_C_BUGS.md #28): a duplicate of the last key, or a key
/// that `MDB_APPEND` put alone on a new page.
#[test]
fn next_after_last_sees_items_added_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = dupsort_txn_db(&mut txn);
    for (k, d) in [("a", "1"), ("b", "1"), ("b", "2")] {
        txn.put(db, k.as_bytes(), d.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    let c = txn.open_cursor(db).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::Last), item("b", "2"));
    txn.put(db, b"b", b"3", WriteFlags::empty()).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("b", "3"));
    assert_eq!(get(&mut txn, c, CursorOp::Next), None);
    if let Some(out) = c_scenario(3) {
        assert_eq!(out.trim(), "next rc=-30798", "C");
    }

    // Without duplicates: two values fill a page, a third one appended
    // goes alone to a new page.
    let plain = txn.create_db(Some("p"), DatabaseFlags::CREATE).unwrap();
    let v = vec![7u8; env.page_size() / 2 - 100];
    txn.put(plain, b"k1", &v, WriteFlags::empty()).unwrap();
    txn.put(plain, b"k2", &v, WriteFlags::empty()).unwrap();
    let p = txn.open_cursor(plain).unwrap();
    assert_eq!(get(&mut txn, p, CursorOp::Last).unwrap().0, b"k2");
    txn.put(plain, b"k3", &v, WriteFlags::APPEND).unwrap();
    assert_eq!(txn.stat(plain).unwrap().leaf_pages, 2);
    assert_eq!(get(&mut txn, p, CursorOp::Next).unwrap().0, b"k3");

    // MDB_GET_MULTIPLE on the last duplicate after MDB_LAST (C: nothing).
    let f = txn
        .create_db(
            Some("f"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    for d in [b"11", b"22", b"33"] {
        txn.put(f, b"x", d, WriteFlags::empty()).unwrap();
    }
    let m = txn.open_cursor(f).unwrap();
    assert_eq!(get(&mut txn, m, CursorOp::Last), item("x", "33"));
    assert_eq!(get(&mut txn, m, CursorOp::GetMultiple), item("x", "112233"));
    // C's writeops shows C's MDB_NEXT stopping after an MDB_APPEND split.
    let Some(tools) = common::c_tools() else {
        return;
    };
    let h = |b: &[u8]| common::hex(b);
    let script = [
        format!("p:{}:{}:0", h(b"k1"), h(&v)),
        format!("p:{}:{}:0", h(b"k2"), h(&v)),
        format!("g:0:{}", rlmdb::consts::MDB_LAST),
        format!("p:{}:{}:{}", h(b"k3"), h(&v), rlmdb::consts::MDB_APPEND),
        format!("g:0:{}", rlmdb::consts::MDB_NEXT),
    ];
    let cdir = tempfile::tempdir().unwrap();
    let mut child = Command::new(tools.bin("writeops"))
        .arg(cdir.path())
        .args(["t", "0", "1"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        for op in &script {
            writeln!(stdin, "{op}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().nth(4).unwrap_or_default();
    assert!(last.starts_with("g rc=-30798 "), "C: {last}");
}

/// A cursor that hit the end (`C_EOF`), then wrote a new first key (C's
/// `mdb_cursor_set` leaves `C_EOF` when the key is before the page's first
/// one) and deleted it: `mdb_cursor_del0` skipped setting up the
/// sub-cursor for the key with duplicates the cursor moved onto, because
/// of the stale `C_EOF`, so `MDB_GET_CURRENT` failed with `EINVAL`
/// (KNOWN_C_BUGS.md #28; found by the model test, seed 28).
#[test]
fn delete_by_a_cursor_with_a_stale_eof() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = dupsort_txn_db(&mut txn);
    for (k, d) in [("k1", "1"), ("k1", "2"), ("k2", "1"), ("k2", "2")] {
        txn.put(db, k.as_bytes(), d.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    let c = txn.open_cursor(db).unwrap();
    txn.cursor_get(c, Some(b"k2"), None, CursorOp::Set).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::NextNoDup), None);
    txn.cursor_put(c, b"k0", b"x", WriteFlags::empty()).unwrap();
    txn.cursor_del(c, WriteFlags::empty()).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::GetCurrent), item("k1", "1"));
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("k1", "2"));
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("k2", "1"));
    // Without looking first, the next item is the deleted one's successor.
    txn.cursor_put(c, b"k0", b"x", WriteFlags::empty()).unwrap();
    txn.cursor_del(c, WriteFlags::empty()).unwrap();
    assert_eq!(get(&mut txn, c, CursorOp::Next), item("k1", "1"));
}

/// A put that fails part-way on a corrupt page leaves the transaction
/// unusable: commit fails with `MDB_BAD_TXN` instead of writing a
/// half-changed tree (C's `MDB_TXN_ERROR`).
#[test]
fn a_write_that_hits_a_corrupt_page_cannot_be_committed() {
    let dir = tempfile::tempdir().unwrap();
    let ps;
    {
        let env = env(dir.path());
        ps = env.page_size();
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..50u32 {
            txn.put(Dbi::MAIN, &i.to_be_bytes(), b"v", WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    // The main DB's only page is page 2: point its first node outside it.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("data.mdb"))
            .unwrap();
        f.seek(SeekFrom::Start(2 * ps as u64 + 16)).unwrap();
        f.write_all(&[0xff, 0xff]).unwrap();
    }
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let e = txn
        .put(Dbi::MAIN, &0u32.to_be_bytes(), b"w", WriteFlags::empty())
        .unwrap_err();
    assert!(
        matches!(e, rlmdb::Error::Mdb(rlmdb::MdbError::Corrupted)),
        "{e:?}"
    );
    assert!(matches!(
        txn.commit(),
        Err(rlmdb::Error::Mdb(rlmdb::MdbError::BadTxn))
    ));
}

/// Calls of `panicky_cmp` left before it panics.
static CMP_CALLS_LEFT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A duplicate comparator that panics on its `CMP_CALLS_LEFT`-th call.
fn panicky_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::sync::atomic::Ordering::SeqCst;
    if CMP_CALLS_LEFT.fetch_sub(1, SeqCst) == 1 {
        panic!("comparator panics (on purpose)");
    }
    a.cmp(b)
}

/// A user comparator that panics part-way through a put, delete or drop
/// (the panic caught with `catch_unwind`) leaves the transaction failed:
/// commit returns `MDB_BAD_TXN` and nothing changes on disk.
#[test]
fn a_panicking_comparator_fails_the_transaction() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::Ordering::SeqCst;
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let db = dupsort_txn_db(&mut txn);
    CMP_CALLS_LEFT.store(usize::MAX, SeqCst);
    txn.set_dupsort(db, Some(panicky_cmp)).unwrap();
    for (k, d) in [("a", "1"), ("k", "a"), ("z", "1"), ("z", "2")] {
        txn.put(db, k.as_bytes(), d.as_bytes(), WriteFlags::empty())
            .unwrap();
    }
    txn.commit().unwrap();
    // (Handles closed by drop_db stay closed after the abort, as in C.)
    let contents = |env: &Environment| {
        let txn = env.begin_ro_txn().unwrap();
        common::walk_forward(&txn, txn.open_db(Some("t")).unwrap())
    };
    let before = contents(&env);
    let mut panics = 0;
    for op in 0..3 {
        // The n-th comparison panics: the first ones happen before
        // anything changes, later ones in the middle (a put of a second
        // item converts the key's single item into duplicates first).
        for n in 1..=4 {
            let mut txn = env.begin_rw_txn().unwrap();
            let db = txn.open_db(Some("t")).unwrap();
            txn.set_dupsort(db, Some(panicky_cmp)).unwrap();
            CMP_CALLS_LEFT.store(n, SeqCst);
            let r = catch_unwind(AssertUnwindSafe(|| match op {
                0 => txn.put(db, b"k", b"b", WriteFlags::empty()).map(drop),
                1 => txn.del(db, b"z", Some(b"2")).map(drop),
                _ => txn.drop_db(db),
            }));
            CMP_CALLS_LEFT.store(usize::MAX, SeqCst);
            match r {
                Ok(rc) => {
                    rc.unwrap();
                    txn.abort();
                }
                Err(_) => {
                    panics += 1;
                    assert!(
                        matches!(
                            txn.commit(),
                            Err(rlmdb::Error::Mdb(rlmdb::MdbError::BadTxn))
                        ),
                        "op {op}, panic at comparison {n}"
                    );
                }
            }
            assert_eq!(contents(&env), before, "op {op}, comparison {n}");
        }
    }
    assert!(panics >= 2, "{panics} panics");
}

/// `MDB_MULTIPLE` with an item equal to the key's only item (stored
/// without a sub-page): C stops there, so the later items are never
/// written and the count tells the caller none were (KNOWN_C_BUGS.md #30).
#[test]
fn put_multiple_does_not_stop_at_an_existing_single_item() {
    let dir = tempfile::tempdir().unwrap();
    let env = env(dir.path());
    let mut txn = env.begin_rw_txn().unwrap();
    let f = txn
        .create_db(
            Some("f"),
            DatabaseFlags::CREATE | DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .unwrap();
    txn.put(f, b"x", b"11", WriteFlags::empty()).unwrap();
    let c = txn.open_cursor(f).unwrap();
    let n = txn
        .cursor_put_multiple(c, b"x", b"112233", 2, WriteFlags::empty())
        .unwrap();
    assert_eq!(n, 3);
    assert_eq!(txn.cursor_count(c).unwrap(), 3);
    assert_eq!(txn.stat(f).unwrap().entries, 3);
    if let Some(out) = c_scenario(4) {
        assert_eq!(out.trim(), "multiple rc=0 written=0 count=1", "C");
    }
}
