//! Differential tests of the write path against C LMDB 0.9: the same script
//! of puts, deletes and cursor moves on several cursors of one write
//! transaction runs in C (`tests/c/writeops.c`) and in Rust (the
//! handle-based cursor API), each in a fresh environment. After every step
//! the position of every cursor must agree, which checks the cursor fixups
//! after splits, merges, rebalances, sub-page growth and sub-DB promotion;
//! the committed databases must be identical and pass the C page audit.
//!
//! The harness is `common/differential.rs`; `interop` runs it against C
//! LMDB 1.0 (`interop/tests/write_differential10.rs`).

mod common;
#[path = "common/differential.rs"]
mod differential;

use std::path::{Path, PathBuf};

use common::{hex, CTools};
use differential::{splitmix64, Ops, Oracle, Profile, Round};
use rlmdb::consts::*;
use rlmdb::Format;

/// C LMDB 0.9 (`tests/lmdb-c/0.9`), built by [`common::c_tools`].
struct C09 {
    tools: &'static CTools,
    writeops: PathBuf,
}

impl C09 {
    fn new(tools: &'static CTools) -> C09 {
        C09 {
            tools,
            writeops: tools.bin("writeops"),
        }
    }
}

impl Oracle for C09 {
    fn writeops(&self) -> &Path {
        &self.writeops
    }

    fn format(&self) -> Format {
        Format::V09
    }

    fn page_size(&self) -> Option<usize> {
        None
    }

    fn dump(&self, env: &Path, db: &str) -> String {
        format!("{:?}", self.tools.dump(env, Some(db)))
    }

    fn check_freelist(&self, env: &Path) -> (i32, String, String) {
        let out = self.tools.peer(&["check-freelist", env.to_str().unwrap()]);
        (out.code, out.stdout, out.stderr)
    }

    fn page_usage(&self, env: &Path) -> String {
        let s = common::parse_stat(&self.tools.stat_text(env, &["-a", "-f"]));
        format!("{:?} free {:?}", s.dbs, s.free_pages)
    }
}

/// [`differential::run_profile`] against C 0.9.
fn run_profile(p: Profile, seed: u64, rounds: usize, n: usize) {
    let c = require_c!();
    differential::run_profile(&C09::new(c), p, seed, rounds, n);
}

/// [`differential::run_round`] against C 0.9.
fn run_round(c: &'static CTools, p: &Profile, ncur: usize, ops: Ops<'_>, tag: &str) -> Round {
    differential::run_round(&C09::new(c), p, ncur, ops, tag)
}
#[test]
fn diff_plain() {
    let p = Profile {
        big: true,
        ..Profile::new(0, 300)
    };
    run_profile(p, 0xd1, 12, 600);
}

#[test]
fn diff_plain_small_keyspace() {
    let p = Profile {
        big: true,
        ..Profile::new(0, 40)
    };
    run_profile(p, 0xd2, 12, 400);
}

#[test]
fn diff_intkey() {
    run_profile(Profile::new(MDB_INTEGERKEY, 500), 0xd3, 8, 600);
}

#[test]
fn diff_revkey() {
    let p = Profile {
        big: true,
        ..Profile::new(MDB_REVERSEKEY, 200)
    };
    run_profile(p, 0xd8, 6, 600);
}

#[test]
fn diff_dupsort() {
    run_profile(Profile::new(MDB_DUPSORT, 30), 0xd4, 12, 800);
}

#[test]
fn diff_dupsort_wide() {
    run_profile(Profile::new(MDB_DUPSORT, 200), 0xd5, 8, 800);
}

#[test]
fn diff_revdup() {
    run_profile(Profile::new(MDB_DUPSORT | MDB_REVERSEDUP, 25), 0xd9, 8, 800);
}

#[test]
fn diff_dupfixed() {
    let p = Profile {
        dupfixed_size: Some(16),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
    };
    run_profile(p, 0xd6, 12, 800);
}

#[test]
fn diff_intdup() {
    let p = Profile {
        dupfixed_size: Some(8),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP, 20)
    };
    run_profile(p, 0xd7, 10, 800);
}

#[test]
fn diff_deep_plain() {
    let p = Profile {
        big: true,
        long_keys: true,
        ..Profile::new(0, 3000)
    };
    run_profile(p, 0xda, 3, 3000);
}

#[test]
fn diff_deep_dupsort() {
    let p = Profile {
        long_keys: true,
        ..Profile::new(MDB_DUPSORT, 600)
    };
    run_profile(p, 0xdb, 3, 3000);
}

#[test]
fn diff_deep_dupfixed() {
    let p = Profile {
        long_keys: true,
        dupfixed_size: Some(24),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 400)
    };
    run_profile(p, 0xdc, 3, 3000);
}

/// Replays a dumped script (`WRITEOPS_REPLAY=<file>`, written with
/// `WRITEOPS_DUMP=<dir>`). `WRITEOPS_DEBUG=1` also prints every step where
/// C's and Rust's duplicate counts differ (C's go stale, KNOWN_C_BUGS.md
/// #14).
#[test]
#[ignore]
fn replay() {
    let c = require_c!();
    let Ok(file) = std::env::var("WRITEOPS_REPLAY") else {
        return;
    };
    let name = Path::new(&file)
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let parts: Vec<&str> = name.rsplitn(3, '-').collect();
    let ncur: usize = parts[0].parse().unwrap();
    let dbflags: u32 = parts[1].parse().unwrap();
    let ops: Vec<String> = std::fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let p = Profile::new(dbflags, 0);
    let r = run_round(c, &p, ncur, Ops::List(ops), "replay");
    eprintln!("{r:?}");
}

/// Nested transactions (begin, commit, abort, up to 3 deep) mixed into
/// the random ops: every cursor of the current transaction must agree with
/// C after every step, including a parent's cursors after its child
/// commits (moved along by the child's changes) or aborts (restored).
#[test]
fn diff_nested_plain() {
    let p = Profile {
        big: true,
        nested: true,
        drops: true,
        ..Profile::new(0, 300)
    };
    run_profile(p, 0xe1, 10, 800);
}

#[test]
fn diff_nested_dupsort() {
    let p = Profile {
        nested: true,
        drops: true,
        ..Profile::new(MDB_DUPSORT, 40)
    };
    run_profile(p, 0xe2, 10, 800);
}

#[test]
fn diff_nested_dupfixed() {
    let p = Profile {
        nested: true,
        drops: true,
        dupfixed_size: Some(16),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
    };
    run_profile(p, 0xe3, 8, 800);
}

#[test]
fn diff_nested_deep() {
    let p = Profile {
        big: true,
        long_keys: true,
        nested: true,
        drops: true,
        ..Profile::new(0, 2000)
    };
    run_profile(p, 0xe4, 3, 3000);
}

/// A parent's cursors used through its children (cursors 2 and 3 in the
/// child are the parent's 0 and 1, in the grandchild 4 and 5): moves,
/// puts and deletes through them, then commit or abort; after that the
/// parent's cursors must be where C has them.
#[test]
fn diff_parent_cursors_in_children() {
    let c = require_c!();
    let k = |i: u32| hex(format!("key{i:04}").as_bytes());
    let mut ops: Vec<String> = vec!["b:600:11:60".into()];
    for i in 0..200u32 {
        ops.push(format!("p:{}:{}:0", k(i), hex(&[b'v'; 300])));
    }
    ops.extend([
        format!("g:0:{MDB_SET}:{}", k(50)),
        format!("g:1:{MDB_LAST}"),
        // Child: use the parent's cursors, then abort.
        "nb".into(),
        format!("g:2:{MDB_NEXT}"),
        format!("cp:2:{}:{}:{MDB_CURRENT}", k(51), hex(b"child")),
        "cd:3:0".into(),
        format!("g:3:{MDB_PREV}"),
        format!("g:0:{MDB_SET_RANGE}:{}", k(100)),
        "cd:0:0".into(),
        "na".into(),
        // Child: use them again, a grandchild too, then commit.
        "nb".into(),
        format!("g:2:{MDB_NEXT}"),
        format!("cp:2:{}:{}:{MDB_CURRENT}", k(51), hex(b"child")),
        format!("g:3:{MDB_FIRST}"),
        "cd:3:0".into(),
        "nb".into(),
        format!("g:4:{MDB_NEXT}"),
        "cd:4:0".into(),
        format!("g:5:{MDB_NEXT}"),
        format!("cp:5:{}:{}:0", k(1), hex(&[b'w'; 2000])),
        format!("g:2:{MDB_SET}:{}", k(150)),
        "nc".into(),
        format!("g:3:{MDB_NEXT}"),
        "dr:0".into(),
        format!("g:2:{MDB_FIRST}"),
        "na".into(),
        "nb".into(),
        "dr:0".into(),
        format!("p:{}:{}:0", k(7), hex(b"x")),
        format!("g:2:{MDB_FIRST}"),
        "nc".into(),
        format!("g:0:{MDB_NEXT}"),
        format!("g:1:{MDB_PREV}"),
    ]);
    let p = Profile::new(0, 0);
    assert_eq!(
        run_round(c, &p, 2, Ops::List(ops), "parent-cursors"),
        Round::Same
    );
}

/// A scripted round: nested transactions around bulk loads, drops and
/// clears in children, and cursor moves in between.
#[test]
fn diff_nested_script() {
    let c = require_c!();
    let ops: Vec<String> = [
        "b:300:1:40",
        "g:0:0",
        "g:1:6",
        "nb",
        "b:500:2:100",
        "g:0:0",
        "nb",
        "b:200:3:3000",
        "g:1:6",
        "dr:0",
        "g:0:0",
        "na",
        "b:100:4:9000",
        "nc",
        "g:0:8",
        "g:1:8",
        "nb",
        "dr:0",
        "na",
        "g:0:8",
        "nb",
        "dr:0",
        "b:50:5:20",
        "nc",
        "g:0:0",
        "dr:1",
        "b:400:6:500",
        "nb",
        "nb",
        "b:400:7:5000",
        "nc",
        "g:0:6",
        "nc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let p = Profile::new(0, 0);
    assert_eq!(run_round(c, &p, 2, Ops::List(ops), "script"), Round::Same);
}

/// Transactions that dirty more pages than a dirty list holds
/// (`MDB_IDL_UM_MAX`), in lock step with C, so that both spill: a bulk
/// load of one-page overflow values in the top-level transaction, a nested
/// transaction that changes some of the (spilled) pages, overwrites and
/// deletes. Both must agree after every step, and afterwards page for
/// page: `mdb_stat -a -f` (page counts, free pages) must be equal, which
/// requires the same pages to have been spilled.
///
/// Ignored by default: each side writes about 0.7 GiB with 4 KiB pages
/// and 2.6 GiB with 16 KiB pages (and holds up to 0.5 / 2 GiB of dirty
/// pages in memory, as C does). Runs in about a minute.
#[test]
#[ignore]
fn diff_spill_full_size() {
    let c = require_c!();
    let ps = common::os_page_size();
    let len = ps - 100; // one overflow page per value
    let n = MDB_IDL_UM_MAX + 20_000;
    let key = |seed: u64, i: usize| {
        // The i-th key bulk_put draws for `seed` (one word per key, then
        // ceil(len / 8) words per value).
        let mut st = seed;
        let words_per_item = 1 + len.div_ceil(8);
        for _ in 0..i * words_per_item {
            splitmix64(&mut st);
        }
        hex(&splitmix64(&mut st).to_le_bytes())
    };
    let mut ops: Vec<String> = vec![format!("b:{n}:1:{len}"), "g:0:0".into(), "g:1:6".into()];
    ops.push("nb".into());
    ops.push(format!("b:300:2:{len}"));
    for i in [3usize, 77, 1000, 50_000] {
        ops.push(format!("p:{}:{}:0", key(1, i), hex(b"small")));
    }
    ops.push(format!("g:2:{MDB_SET}:{}", key(1, 1000)));
    ops.push("nc".into());
    for i in [5usize, 6, 9000, 120_000] {
        ops.push(format!("d:{}", key(1, i)));
    }
    ops.push(format!("b:20000:3:{len}"));
    ops.push("g:0:8".into());
    let p = Profile::new(0, 0);
    let map = (n + 30_000) * ps * 3 / 2;
    let started = std::time::Instant::now();
    assert_eq!(
        differential::run_round_sized(&C09::new(c), &p, 2, Ops::List(ops), "spill", map),
        Round::Same
    );
    eprintln!("spill differential took {:?}", started.elapsed());
}
