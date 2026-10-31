//! The write path of the LMDB 1.0 format against C LMDB 1.0: the lock-step
//! differential harness of `rlmdb/tests/common/differential.rs` with
//! `writeops10` (`rlmdb/tests/c/writeops.c` built against the 1.0 library,
//! see `build.rs`) on the C side and `rlmdb` with `Format::V10` on the
//! other, at the page sizes 1.0 can use (1 KiB, as 1.0's own `mtest` does,
//! 4, 16 and 64 KiB).
//!
//! Every step must agree (return codes, the position of every cursor); the
//! committed environments must have the same contents (`mdb_dump10`), pass
//! the C 1.0 page audit (`cpeer10 check-freelist`), and hold the same pages
//! in the same places with the same bytes where they matter, `mp_txnid` and
//! the `MDB_ovpage` of overflow nodes included (`interop::pagecmp`).
//!
//! The two sides run as separate processes on separate environments, so
//! the lock file (still version 1 in rlmdb, `DUAL_FORMAT.md` stage 3) does
//! not meet a C 1.0 peer here.

#[path = "../../rlmdb/tests/common/differential.rs"]
mod differential;

use std::path::Path;
use std::process::Command;

use differential::{hex, splitmix64, Ops, Oracle, Profile, Round};
use interop::bins;
use rlmdb::consts::*;
use rlmdb::Format;

/// C LMDB 1.0 with environments of `page_size` bytes per page.
struct C10 {
    page_size: usize,
    /// Rust side's dirty-page limit ([`Oracle::dirty_limit`]).
    dirty_limit: Option<usize>,
    /// The C side: [`bins::writeops10`], or [`bins::writeops10_ndebug`].
    writeops: &'static Path,
}

impl C10 {
    fn new(page_size: usize) -> C10 {
        C10 {
            page_size,
            dirty_limit: None,
            writeops: bins::writeops10(),
        }
    }

    /// The same with C 1.0 built with `-DNDEBUG`, for a script on which the
    /// upstream build asserts although C's arithmetic is right (see
    /// `diff10_dupfixed_1byte_upper_wraps`).
    fn ndebug(self) -> C10 {
        C10 {
            writeops: bins::writeops10_ndebug(),
            ..self
        }
    }
}

/// Runs a C 1.0 program, panicking on failure; returns stdout.
fn run(prog: &Path, args: &[&str]) -> String {
    let out = Command::new(prog).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{} {args:?}: {}",
        prog.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

impl Oracle for C10 {
    fn writeops(&self) -> &Path {
        self.writeops
    }

    fn format(&self) -> Format {
        Format::V10
    }

    fn page_size(&self) -> Option<usize> {
        Some(self.page_size)
    }

    fn dirty_limit(&self) -> Option<usize> {
        self.dirty_limit
    }

    // C 1.0 refuses rlmdb's version 1 lock file while rlmdb holds the
    // environment open (DUAL_FORMAT.md stage 3).
    fn shares_open_environments(&self) -> bool {
        false
    }

    fn dump(&self, env: &Path, db: &str) -> String {
        // The data lines only: the header holds the map address.
        let text = run(bins::mdb_dump10(), &["-s", db, env.to_str().unwrap()]);
        text.split_once("HEADER=END\n")
            .map_or(text.clone(), |(_, data)| data.to_owned())
    }

    fn check_freelist(&self, env: &Path) -> (i32, String, String) {
        let out = Command::new(bins::cpeer10())
            .args(["check-freelist", env.to_str().unwrap()])
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn page_usage(&self, env: &Path) -> String {
        run(bins::mdb_stat10(), &["-a", "-f", env.to_str().unwrap()])
    }

    fn after_round(&self, c_env: &Path, rust_env: &Path, tag: &str) {
        if let Err(e) = interop::pagecmp::compare_data_files(
            &c_env.join("data.mdb"),
            &rust_env.join("data.mdb"),
        ) {
            panic!("{tag}: page size {}: files differ: {e}", self.page_size);
        }
    }
}

/// The page sizes of the profile tests.
const PAGE_SIZES: [usize; 4] = [1024, 4096, 16384, 65536];

/// 1.0's `me_nodemax` for pages of `ps` bytes.
fn nodemax(ps: usize) -> usize {
    (((ps - 24) / 2) & !1) - 2
}

/// `p` with the generator kept out of `KNOWN_C_BUGS.md` #32 (see
/// `Profile::appenddup_nodemax`) for pages of `ps` bytes.
fn for_page_size(mut p: Profile, ps: usize) -> Profile {
    p.appenddup_nodemax = Some(nodemax(ps));
    p
}

/// Runs `p` (rebuilt by `p()` for each page size) at every page size.
fn run_profiles(p: impl Fn() -> Profile, seed: u64, rounds: usize, n: usize) {
    for ps in PAGE_SIZES {
        eprintln!("page size {ps}");
        let p = for_page_size(p(), ps);
        differential::run_profile(&C10::new(ps), p, seed + ps as u64, rounds, n);
    }
}

#[test]
fn diff10_plain() {
    run_profiles(
        || Profile {
            big: true,
            ..Profile::new(0, 300)
        },
        0x1d1,
        6,
        600,
    );
}

#[test]
fn diff10_plain_small_keyspace() {
    run_profiles(
        || Profile {
            big: true,
            ..Profile::new(0, 40)
        },
        0x1d2,
        6,
        400,
    );
}

#[test]
fn diff10_intkey() {
    run_profiles(|| Profile::new(MDB_INTEGERKEY, 500), 0x1d3, 4, 600);
}

#[test]
fn diff10_revkey() {
    run_profiles(
        || Profile {
            big: true,
            ..Profile::new(MDB_REVERSEKEY, 200)
        },
        0x1d8,
        3,
        600,
    );
}

#[test]
fn diff10_dupsort() {
    run_profiles(|| Profile::new(MDB_DUPSORT, 30), 0x1d4, 6, 800);
}

#[test]
fn diff10_dupsort_wide() {
    run_profiles(|| Profile::new(MDB_DUPSORT, 200), 0x1d5, 4, 800);
}

#[test]
fn diff10_dupfixed() {
    run_profiles(
        || Profile {
            dupfixed_size: Some(16),
            ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
        },
        0x1d6,
        6,
        800,
    );
}

/// `MDB_DUPFIXED` items over a quarter page, which only 1.0 allows (in 0.9
/// items are at most 511 bytes and pages at least 4 KiB): a key's second
/// item turns its single item into a sub-DB at once, because the fake
/// sub-page (two items and room for two more) is larger than a page. C's
/// unsigned offset arithmetic wraps there and still gives the root its
/// `mp_upper`; rlmdb must write the same page (it panicked on the
/// subtraction before). With 64 KiB pages and items over 16 KiB the fake
/// sub-page is larger than 64 KiB, so its 16-bit `mp_upper` wraps too, and
/// C's sum wraps back (a debug build of rlmdb panicked there).
#[test]
fn diff10_dupfixed_items_over_a_quarter_page() {
    for (ps, size) in [
        (512, 150),
        (512, 186),
        (1024, 300),
        (1024, 442),
        (65536, 20000),
        (65536, 32698),
    ] {
        eprintln!("page size {ps}, items of {size} bytes");
        let p = for_page_size(
            Profile {
                dupfixed_size: Some(size),
                ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
            },
            ps,
        );
        // Items of 20 and 32 KiB: fewer and shorter rounds.
        let (rounds, n) = if ps == 65536 { (2, 60) } else { (4, 300) };
        differential::run_profile(&C10::new(ps), p, 0x1da + size as u64, rounds, n);
    }
}

/// `MDB_DUPFIXED` items of 1 byte, and of 2 and 3 bytes as neighbours
/// (`rlmdb/tests/write_differential.rs`, `diff_dupfixed_small_items`): with
/// 1-byte items `mp_upper` of a `P_LEAF2` page grows past its end. At
/// 256 and 512 bytes per page the 256 possible 1-byte values also go to
/// sub-DBs, whose `P_LEAF2` pages split and merge when one key has most
/// of them; the larger pages keep them on sub-pages.
#[test]
fn diff10_dupfixed_small_items() {
    for ps in [256, 512].into_iter().chain(PAGE_SIZES) {
        for size in [1, 2, 3] {
            eprintln!("page size {ps}, items of {size} bytes");
            let p = for_page_size(
                Profile {
                    dupfixed_size: Some(size),
                    ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 4)
                },
                ps,
            );
            differential::run_profile(&C10::new(ps), p, 0x1f0 + size as u64 + ps as u64, 4, 1200);
        }
    }
    // One key at 256-byte pages: its 1-byte items fill the 232 slots of a
    // `P_LEAF2` page of its sub-DB, which splits (and merges again).
    eprintln!("page size 256, items of 1 byte, one key");
    let p = for_page_size(
        Profile {
            dupfixed_size: Some(1),
            ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 1)
        },
        256,
    );
    differential::run_profile(&C10::new(256), p, 0x1fe, 4, 3000);
}

/// [`differential::dupfixed_1byte_script`] at every page size: at 256
/// bytes per page the key's sub-DB splits its `P_LEAF2` page of 1-byte
/// items, then merges and rebalances them as they are deleted.
#[test]
fn diff10_dupfixed_1byte_script() {
    let p = Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 0);
    for ps in [256, 512].into_iter().chain(PAGE_SIZES) {
        let ops = differential::dupfixed_1byte_script();
        assert_eq!(
            differential::run_round(&C10::new(ps), &p, 2, Ops::List(ops), "1byte"),
            Round::Same,
            "page size {ps}"
        );
    }
}

/// A `P_LEAF2` page whose stored `mp_upper` wraps. With 64 KiB pages a
/// key of 1.0's largest size (32,698 bytes) leaves almost no room for a
/// sub-page, so its 1-byte items soon go to a sub-DB, all of them on its
/// leaf, which starts with `mp_upper` 65,512; each item raises it by one
/// (see `diff10_dupfixed_small_items`) and the 24th wraps it to 0,
/// `mp_lower` being 48. C's `SIZELEFT` is 16-bit, so its free space stays
/// exact (65,488 bytes) and C keeps the 100 items of this script (1-byte
/// values 0..99 in order) on that one page; rlmdb took the free space for
/// 0 once `mp_upper` was below `mp_lower`, split on every later put and
/// ran out of an 8-page map (`MDB_MAP_FULL` at the 37th put; with more
/// room it wrote other pages than C). The longer
/// [`differential::dupfixed_1byte_script_for`] with the same key then
/// takes the page to all 256 values and deletes them again (merges,
/// `PAGEFILL` of the wrapped page).
///
/// C with its default (upstream) flags asserts `MP_UPPER(mp) >=
/// MP_LOWER(mp)` at the top of `mdb_node_add` once `mp_upper` wrapped, so
/// the C side here is `writeops10_ndebug`: C 1.0 built with `-DNDEBUG`, as
/// release builds are (`interop/build.rs`). No 0.9 page wraps
/// (`rlmdb/tests/write_differential.rs`, `diff_dupfixed_1byte_max_key`).
#[test]
fn diff10_dupfixed_1byte_upper_wraps() {
    const PS: usize = 65536;
    let key = vec![b'K'; nodemax(PS) - 8 - 48];
    assert_eq!(key.len(), 32698, "mdb_env_get_maxkeysize");
    let p = Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 0);
    let c = C10::new(PS).ndebug();
    for pages in [8, 1024] {
        let ops = differential::dupfixed_1byte_puts(&key, 100);
        let r = differential::run_round_sized(&c, &p, 2, Ops::List(ops), "wrap", pages * PS);
        assert_eq!(r, Round::Same, "map of {pages} pages");
    }
    let ops = differential::dupfixed_1byte_script_for(&key);
    let r = differential::run_round(&c, &p, 2, Ops::List(ops), "wrap-script");
    assert_eq!(r, Round::Same);
}

/// `MDB_CURRENT` replacing a key's only item in an `MDB_DUPSORT` DB
/// ([`differential::current_single_item_script`]) at every page size 1.0
/// allows, with keys of 103 bytes (fewer where keys are limited to less).
/// Items that do not fit a node with the key are deleted and inserted, as
/// C's `mdb_cursor_del` + `mdb_cursor_put` do (C's `MDB_CURRENT` would put
/// them on overflow pages, `KNOWN_C_BUGS.md` #38); with 4 KiB pages that
/// includes the Go port's reproducer, an item of 1978 bytes (the maximum)
/// replacing one under a 103-byte key, then a second duplicate. The files
/// must agree page for page.
#[test]
fn diff10_current_single_item() {
    let p = Profile::new(MDB_DUPSORT, 0);
    for ps in [256, 512].into_iter().chain(PAGE_SIZES) {
        let maxkey = nodemax(ps) - 56;
        let ops =
            differential::current_single_item_script(nodemax(ps), maxkey, true, maxkey.min(103));
        assert!(ops.iter().any(|op| op.starts_with("cr:")), "page size {ps}");
        assert_eq!(
            differential::run_round(&C10::new(ps), &p, 2, Ops::List(ops), "current"),
            Round::Same,
            "page size {ps}"
        );
    }
}

/// `KNOWN_C_BUGS.md` #38 ([`differential::single_item_on_overflow_pages`]):
/// C 1.0's `MDB_CURRENT` leaves a key's only item on overflow pages (items
/// of the maximum key size, keys of 103 bytes or the maximum, the Go port's
/// shape with 4 KiB pages); rlmdb's next duplicate put reads the item
/// through its overflow pages and frees them, and its other changes of the
/// item are right too, in a `MDB_DUPSORT` and a `MDB_DUPFIXED` DB. C 1.0
/// audits and reads every result.
#[test]
fn c10_single_item_on_overflow_pages() {
    for ps in [256, 1024, 4096, 65536] {
        let maxkey = nodemax(ps) - 56;
        for flags in [MDB_DUPSORT, MDB_DUPSORT | MDB_DUPFIXED] {
            eprintln!("page size {ps}, flags {flags:#x}");
            differential::single_item_on_overflow_pages(
                &C10::new(ps),
                flags,
                maxkey.min(103),
                maxkey,
            );
        }
    }
}

#[test]
fn diff10_intdup() {
    run_profiles(
        || Profile {
            dupfixed_size: Some(8),
            ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED | MDB_INTEGERDUP, 20)
        },
        0x1d7,
        5,
        800,
    );
}

#[test]
fn diff10_deep_plain() {
    run_profiles(
        || Profile {
            big: true,
            long_keys: true,
            ..Profile::new(0, 3000)
        },
        0x1da,
        2,
        3000,
    );
}

#[test]
fn diff10_deep_dupsort() {
    run_profiles(
        || Profile {
            long_keys: true,
            ..Profile::new(MDB_DUPSORT, 600)
        },
        0x1db,
        2,
        3000,
    );
}

#[test]
fn diff10_nested_plain() {
    run_profiles(
        || Profile {
            big: true,
            nested: true,
            drops: true,
            ..Profile::new(0, 300)
        },
        0x1e1,
        5,
        800,
    );
}

#[test]
fn diff10_nested_dupsort() {
    run_profiles(
        || Profile {
            nested: true,
            drops: true,
            ..Profile::new(MDB_DUPSORT, 40)
        },
        0x1e2,
        5,
        800,
    );
}

#[test]
fn diff10_nested_dupfixed() {
    run_profiles(
        || Profile {
            nested: true,
            drops: true,
            dupfixed_size: Some(16),
            ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 20)
        },
        0x1e3,
        4,
        800,
    );
}

/// Rust spills (a dirty limit of 256 pages) where C does not: every step and
/// the files must still agree.
#[test]
fn diff10_spilling_rust_side() {
    for ps in PAGE_SIZES {
        for (p, seed) in [
            (
                Profile {
                    big: true,
                    nested: true,
                    ..Profile::new(0, 300)
                },
                0x1f1,
            ),
            (
                Profile {
                    nested: true,
                    ..Profile::new(MDB_DUPSORT, 40)
                },
                0x1f2,
            ),
        ] {
            let o = C10 {
                dirty_limit: Some(256),
                ..C10::new(ps)
            };
            differential::run_profile(&o, for_page_size(p, ps), seed + ps as u64, 3, 1200);
        }
    }
}

/// A scripted round: nested transactions around bulk loads, drops and
/// clears in children, and cursor moves in between (as in the 0.9 tests).
#[test]
fn diff10_nested_script() {
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
    for ps in PAGE_SIZES {
        let p = Profile::new(0, 0);
        assert_eq!(
            differential::run_round(&C10::new(ps), &p, 2, Ops::List(ops.clone()), "script"),
            Round::Same
        );
    }
}

/// In-place overwrites of overflow values (same size, smaller, larger, in a
/// nested transaction) and of sub-page duplicates, with odd key and data
/// sizes (1.0 pads both to even lengths).
#[test]
fn diff10_overwrites() {
    let k = |i: u32| hex(format!("key{i:03}{}", "z".repeat(i as usize % 3)).as_bytes());
    let v = |n: usize, b: u8| hex(&vec![b; n]);
    for ps in PAGE_SIZES {
        let big = 3 * ps + 7;
        let mut ops: Vec<String> = Vec::new();
        for i in 0..60u32 {
            ops.push(format!("p:{}:{}:0", k(i), v(big + i as usize, b'a')));
        }
        for i in 0..60u32 {
            let len = match i % 4 {
                0 => big + i as usize,   // same size
                1 => big - ps,           // fewer pages
                2 => big + 2 * ps,       // more pages
                _ => 5 + i as usize % 3, // inline again
            };
            ops.push(format!("p:{}:{}:0", k(i), v(len, b'b')));
        }
        ops.push("nb".into());
        for i in (0..60u32).step_by(3) {
            ops.push(format!("p:{}:{}:0", k(i), v(big, b'c')));
        }
        ops.push("nc".into());
        ops.push("nb".into());
        for i in (1..60u32).step_by(5) {
            ops.push(format!("d:{}", k(i)));
        }
        ops.push("na".into());
        let p = Profile::new(0, 0);
        assert_eq!(
            differential::run_round(&C10::new(ps), &p, 1, Ops::List(ops), "overwrite"),
            Round::Same,
            "page size {ps}"
        );
    }
}

/// Transactions that dirty more pages than a dirty list holds
/// (`MDB_IDL_UM_MAX`), in lock step with C 1.0, so that both spill (with
/// 1 KiB pages: about 150 MiB per side). Afterwards `mdb_stat10 -a -f`
/// and the files agree page for page, which requires the same pages to have
/// been spilled and every spilled page to carry C's `mp_txnid`.
#[test]
#[ignore]
fn diff10_spill_full_size() {
    let ps: usize = 1024;
    let len = ps - 100; // one overflow page per value
    let n = MDB_IDL_UM_MAX + 20_000;
    let key = |seed: u64, i: usize| {
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
    assert_eq!(
        differential::run_round_sized(&C10::new(ps), &p, 2, Ops::List(ops), "spill", map),
        Round::Same
    );
}
