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
use rlmdb::format::Meta;
use rlmdb::Format;

/// C LMDB 0.9 (`tests/lmdb-c/0.9`), built by [`common::c_tools`].
struct C09 {
    tools: &'static CTools,
    writeops: PathBuf,
    /// With `Some(ps)`: both sides start from an empty data file of
    /// `ps`-byte pages ([`empty_data_file`]), and after a round the files
    /// must also agree page for page ([`C09::same_pages`]).
    file_page_size: Option<usize>,
}

impl C09 {
    fn new(tools: &'static CTools) -> C09 {
        C09 {
            tools,
            writeops: tools.bin("writeops"),
            file_page_size: None,
        }
    }

    /// C 0.9 on environments of `ps`-byte pages. 0.9 creates environments
    /// with the OS page size only, but opens a file of any page size
    /// (`mdb_env_open2` takes `mm_psize`), and so does rlmdb from 2 KiB on
    /// (`Layout::min_pagesize`).
    fn with_page_size(tools: &'static CTools, ps: usize) -> C09 {
        C09 {
            file_page_size: Some(ps),
            ..C09::new(tools)
        }
    }

    /// The files C and Rust committed hold the same pages:
    /// `mdb_stat -e -f -ff -a` (last page, page counts, the free pages by
    /// number) prints the same but for the reader count, and every page is
    /// the same in its [`canonical09`] form, which leaves out only the bytes
    /// C 0.9 does not initialise.
    fn same_pages(&self, c_env: &Path, rust_env: &Path, tag: &str) {
        let stat = |env: &Path| {
            self.tools
                .stat_text(env, &["-e", "-f", "-ff", "-a"])
                .lines()
                .filter(|l| !l.contains("readers used"))
                .map(|l| format!("{l}\n"))
                .collect::<String>()
        };
        assert_eq!(stat(c_env), stat(rust_env), "{tag}: mdb_stat -e -f -ff -a");
        let (c, r) = (
            std::fs::read(c_env.join("data.mdb")).unwrap(),
            std::fs::read(rust_env.join("data.mdb")).unwrap(),
        );
        let ps = self.file_page_size.unwrap_or_else(common::os_page_size);
        let page = |f: &[u8], p: usize| f.get(p * ps..(p + 1) * ps).map(|b| canonical09(b, false));
        let differ: Vec<usize> = (0..c.len().max(r.len()).div_ceil(ps))
            .filter(|&p| page(&c, p) != page(&r, p))
            .collect();
        assert!(differ.is_empty(), "{tag}: pages {differ:?} differ");
    }
}

/// A 0.9 page (or a sub-page, `sub`) without the bytes C does not
/// initialise: C 0.9 hands out a page buffer it freed earlier in the
/// transaction as it is (`mdb_page_malloc`, `me_dpages`; 1.0 clears it),
/// and leaves stale bytes where nodes were deleted or moved and in the
/// padding of odd-sized nodes. So the form is the header (without
/// `mp_pad` of an ordinary branch or leaf page, which nothing reads there),
/// the node pointers, every node's header, key and data (an overflow
/// reference's page number, a sub-page in this form), the packed keys of
/// a `P_LEAF2` page, and every byte of other pages (meta, overflow, free).
fn canonical09(page: &[u8], sub: bool) -> Vec<u8> {
    use rlmdb::format::layout::{NODE_HI, NODE_LO};
    use rlmdb::format::node::{F_BIGDATA, F_DUPDATA, F_SUBDATA};
    use rlmdb::format::page::{P_BRANCH, P_LEAF, P_LEAF2};
    let u16_at = |o: usize| page.get(o..o + 2).map(|b| u16::from_ne_bytes([b[0], b[1]]));
    let (Some(flags), Some(lower)) = (u16_at(10), u16_at(12)) else {
        return page.to_vec();
    };
    if flags & (P_BRANCH | P_LEAF) == 0 || usize::from(lower) < 16 {
        return page.to_vec();
    }
    let nkeys = (usize::from(lower) - 16) / 2;
    let mut out = page[..16].to_vec();
    if flags & P_LEAF2 != 0 {
        let ksize = usize::from(u16_at(8).unwrap_or(0));
        out.extend_from_slice(page.get(16..16 + nkeys * ksize).unwrap_or(page));
        return out;
    }
    if !sub {
        out[8..10].fill(0);
    }
    out.extend_from_slice(&page[16..usize::from(lower)]);
    for i in 0..nkeys {
        let Some(off) = u16_at(16 + 2 * i).map(usize::from) else {
            return page.to_vec();
        };
        // mn_lo and mn_hi swap places on big-endian hosts.
        let (Some(lo), Some(hi), Some(nflags), Some(ksize)) = (
            u16_at(off + NODE_LO),
            u16_at(off + NODE_HI),
            u16_at(off + 4),
            u16_at(off + 6),
        ) else {
            return page.to_vec();
        };
        let dsize = usize::from(lo) | usize::from(hi) << 16;
        let key_end = off + 8 + usize::from(ksize);
        let data_len = match () {
            _ if flags & P_BRANCH != 0 => 0,
            _ if nflags & F_BIGDATA != 0 => 8,
            _ => dsize,
        };
        let Some(node) = page.get(off..key_end + data_len) else {
            return page.to_vec();
        };
        if nflags & (F_DUPDATA | F_SUBDATA) == F_DUPDATA {
            out.extend_from_slice(&node[..8 + usize::from(ksize)]);
            out.extend_from_slice(&canonical09(&node[8 + usize::from(ksize)..], true));
        } else {
            out.extend_from_slice(node);
        }
    }
    out
}

/// A new 0.9 environment's data file with `ps`-byte pages: the two meta
/// pages `mdb_env_init_meta` writes.
fn empty_data_file(ps: usize) -> Vec<u8> {
    let mut file = vec![0u8; 2 * ps];
    Meta::new_env(ps as u32, 64 << 20, 0).write_initial_pages(&mut file, ps);
    file
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

    fn data_file(&self) -> Option<Vec<u8>> {
        self.file_page_size.map(empty_data_file)
    }

    fn after_round(&self, c_env: &Path, rust_env: &Path, tag: &str) {
        if self.file_page_size.is_some() {
            self.same_pages(c_env, rust_env, tag);
        }
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

/// `MDB_DUPFIXED` items of 1 byte, and of 2 and 3 bytes as neighbours. A
/// `P_LEAF2` page counts its items in `mp_lower`/`mp_upper`, `mp_upper`
/// dropping by `ksize - sizeof(indx_t)` per item: unsigned in C, so for
/// 1-byte items it grows past the page end. rlmdb refused the first such
/// insert with `MDB_PAGE_FULL`. With pages of 4 KiB and more the 256
/// possible 1-byte values stay on sub-pages (growing, shrinking, the C
/// audit of their sizes); 1.0's small pages also take them to sub-DBs and
/// split them (`interop/tests/write_differential10.rs`).
#[test]
fn diff_dupfixed_small_items() {
    for size in [1, 2, 3] {
        eprintln!("items of {size} bytes");
        let p = Profile {
            dupfixed_size: Some(size),
            ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 4)
        };
        run_profile(p, 0xf0 + size as u64, 6, 1200);
    }
}

/// [`differential::dupfixed_1byte_script`]: one key's sub-page grows to
/// all 256 1-byte values and shrinks again.
#[test]
fn diff_dupfixed_1byte_script() {
    let c = require_c!();
    let p = Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 0);
    let ops = differential::dupfixed_1byte_script();
    assert_eq!(run_round(c, &p, 2, Ops::List(ops), "1byte"), Round::Same);
}

/// The 0.9 side of `diff10_dupfixed_1byte_upper_wraps`
/// (`interop/tests/write_differential10.rs`, where a `P_LEAF2` page's
/// stored `mp_upper` wraps past 2^16 and C's 16-bit `SIZELEFT` stays
/// exact): its script, 1-byte items 0..99 and then
/// [`differential::dupfixed_1byte_script_for`], with a key of 0.9's
/// largest size (511 bytes). No 0.9 page can wrap: `mp_upper` of a
/// `P_LEAF2` page starts at the page size, at most 32 KiB, and grows by
/// one per 1-byte item, of which a page holds at most `page size - 16`,
/// so it stays at most 65,520; and at the OS page sizes this oracle uses
/// (4 and 16 KiB) the 256 possible values even stay on a sub-page.
#[test]
fn diff_dupfixed_1byte_max_key() {
    let c = require_c!();
    let key = vec![b'K'; 511];
    let p = Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 0);
    let ops = differential::dupfixed_1byte_puts(&key, 100);
    assert_eq!(
        run_round(c, &p, 2, Ops::List(ops), "1byte-puts"),
        Round::Same
    );
    let ops = differential::dupfixed_1byte_script_for(&key);
    assert_eq!(
        run_round(c, &p, 2, Ops::List(ops), "1byte-max-key"),
        Round::Same
    );
}

/// [`diff_dupfixed_small_items`] with 1-byte items in nested transactions
/// and with drops.
#[test]
fn diff_nested_dupfixed_1byte() {
    let p = Profile {
        nested: true,
        drops: true,
        dupfixed_size: Some(1),
        ..Profile::new(MDB_DUPSORT | MDB_DUPFIXED, 4)
    };
    run_profile(p, 0xf4, 6, 1200);
}

/// 0.9's `me_nodemax` for pages of `ps` bytes.
fn nodemax09(ps: usize) -> usize {
    (((ps - 16) / 2) & !1) - 2
}

/// `MDB_CURRENT` replacing a key's only item in an `MDB_DUPSORT` DB
/// ([`differential::current_single_item_script`], keys of 500 bytes). At
/// the OS page size every 0.9 item fits a node with any key (both are at
/// most 511 bytes, `me_nodemax` is at least 2038): both sides replace in
/// place. With 2 KiB pages, a file C 0.9 does not create but opens, the
/// larger items do not fit: C's `MDB_CURRENT` would store them on overflow
/// pages, rlmdb's deletes and inserts (`KNOWN_C_BUGS.md` #38) and must
/// leave the file C's `mdb_cursor_del` + `mdb_cursor_put` leave, page for
/// page but for the bytes C 0.9 leaves uninitialised
/// ([`C09::same_pages`]).
#[test]
fn diff_current_single_item() {
    let c = require_c!();
    let p = Profile::new(MDB_DUPSORT, 0);
    let ps = common::os_page_size();
    let ops = differential::current_single_item_script(nodemax09(ps), MDB_MAXKEYSIZE, false, 500);
    assert!(!ops.iter().any(|op| op.starts_with("cr:")));
    assert_eq!(run_round(c, &p, 2, Ops::List(ops), "current"), Round::Same);
    let o = C09::with_page_size(c, 2048);
    let ops = differential::current_single_item_script(nodemax09(2048), MDB_MAXKEYSIZE, false, 500);
    assert!(ops.iter().any(|op| op.starts_with("cr:")));
    let r = differential::run_round(&o, &p, 2, Ops::List(ops), "current-2k");
    assert_eq!(r, Round::Same);
}

/// The 0.9 side of `KNOWN_C_BUGS.md` #38
/// ([`differential::single_item_on_overflow_pages`]): with 2 KiB pages C
/// 0.9's `MDB_CURRENT` leaves a key's only item (511 bytes, the key 500)
/// on overflow pages; rlmdb converts, replaces and deletes it correctly,
/// in a `MDB_DUPSORT` and a `MDB_DUPFIXED` DB.
#[test]
fn c_single_item_on_overflow_pages() {
    let c = require_c!();
    let o = C09::with_page_size(c, 2048);
    for flags in [MDB_DUPSORT, MDB_DUPSORT | MDB_DUPFIXED] {
        differential::single_item_on_overflow_pages(&o, flags, 500, MDB_MAXKEYSIZE);
    }
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
