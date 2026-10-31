//! A data file assembled byte by byte with the `rlmdb::format` writers, read
//! by both C (`mdb_dump`, `mdb_stat`, `mdb_cursor_get`) and Rust.
//!
//! This pins down the layout knowledge (meta pages, page headers, node
//! headers, overflow runs, `F_SUBDATA` records, `P_SUBP` sub-pages, sub-DBs)
//! independently of any C-written file, ahead of the Rust write path.

mod common;

use common::*;
use rlmdb::consts::{MDB_DUPSORT, P_INVALID};
use rlmdb::format::layout::{V09, V10};
use rlmdb::format::node::{F_DUPDATA, F_SUBDATA};
use rlmdb::format::page::{NodeData, P_BRANCH, P_DIRTY, P_LEAF, P_SUBP};
use rlmdb::format::{overflow_pages_for, Db, Layout, Meta, PageMut};
use rlmdb::{Dbi, Error, MdbError, Transaction, WriteFlags};

const PS: usize = 4096;

fn kv(k: &str, v: &[u8]) -> Pair {
    (k.as_bytes().to_vec(), v.to_vec())
}

/// A sub-page holding `dups` (already sorted) as keys without data.
fn subpage(parent: u64, dups: &[&[u8]]) -> Vec<u8> {
    let size = 16
        + dups
            .iter()
            .map(|d| 2 + ((8 + d.len() + 1) & !1))
            .sum::<usize>();
    let mut buf = vec![0u8; size];
    let mut p = PageMut::new(&mut buf);
    p.init(parent, P_LEAF | P_SUBP | P_DIRTY);
    for (i, d) in dups.iter().enumerate() {
        p.add_node(i, d, NodeData::Inline(b""), 0).unwrap();
    }
    buf
}

/// Builds the file and returns it with the expected contents of the main DB
/// and of the named DB "dups".
fn build() -> (Vec<u8>, Vec<Pair>, Vec<Pair>) {
    let big: Vec<u8> = (0..5000u32).map(|i| (i * 31 % 251) as u8).collect();
    let big_pages = overflow_pages_for(big.len(), PS); // 2
    let page = |n: usize| n * PS..(n + 1) * PS;
    let last_pgno = 4 + big_pages; // pages 0..=6
    let mut file = vec![0u8; (last_pgno + 1) * PS];

    // Page 3..: the overflow run for "big".
    {
        let mut o = PageMut::new(&mut file[3 * PS..(3 + big_pages) * PS]);
        o.init_overflow(3, big_pages as u32);
        o.body_mut()[..big.len()].copy_from_slice(&big);
    }
    // Page 6: the sub-DB of key "k3" (keys = duplicate items, empty data).
    {
        let mut p = PageMut::new(&mut file[page(6)]);
        p.init(6, P_LEAF);
        p.add_node(0, b"x1", NodeData::Inline(b""), 0).unwrap();
        p.add_node(1, b"x2", NodeData::Inline(b""), 0).unwrap();
    }
    // Page 5: root leaf of the DUPSORT DB "dups".
    let k3_db = Db {
        depth: 1,
        leaf_pages: 1,
        entries: 2,
        root: 6,
        ..Db::default()
    };
    {
        let sub = subpage(5, &[b"a", b"bb", b"c"]);
        let mut p = PageMut::new(&mut file[page(5)]);
        p.init(5, P_LEAF);
        p.add_node(0, b"k1", NodeData::Inline(b"only"), 0).unwrap();
        p.add_node(1, b"k2", NodeData::Inline(&sub), F_DUPDATA)
            .unwrap();
        p.add_node(
            2,
            b"k3",
            NodeData::Inline(&k3_db.to_bytes()),
            F_DUPDATA | F_SUBDATA,
        )
        .unwrap();
    }
    // Page 2: root leaf of the main DB.
    let dups_db = Db {
        flags: MDB_DUPSORT as u16,
        depth: 1,
        leaf_pages: 2, // C counts sub-DB pages too
        entries: 6,
        root: 5,
        ..Db::default()
    };
    {
        let mut p = PageMut::new(&mut file[page(2)]);
        p.init(2, P_LEAF);
        p.add_node(0, b"alpha", NodeData::Inline(b"first value"), 0)
            .unwrap();
        p.add_node(
            1,
            b"big",
            NodeData::Overflow {
                pgno: 3,
                data_len: big.len(),
            },
            0,
        )
        .unwrap();
        p.add_node(2, b"dups", NodeData::Inline(&dups_db.to_bytes()), F_SUBDATA)
            .unwrap();
        p.add_node(3, b"zeta", NodeData::Inline(b""), 0).unwrap();
    }
    // Meta pages: page 1 holds txn 1, page 0 the empty txn 0.
    let mut m0 = Meta::new_env(PS as u32, 1 << 20, 0);
    m0.dbs[0].root = P_INVALID;
    let mut m1 = m0;
    m1.txnid = 1;
    m1.last_pgno = last_pgno as u64;
    m1.dbs[1] = Db {
        depth: 1,
        leaf_pages: 1,
        overflow_pages: big_pages as u64,
        entries: 4,
        root: 2,
        ..Db::default()
    };
    let mut metas = vec![0u8; 2 * PS];
    m0.write_initial_pages(&mut metas, PS);
    m1.write(&mut metas[PS + 16..]);
    file[..2 * PS].copy_from_slice(&metas);

    let dups_expected = [
        kv("k1", b"only"),
        kv("k2", b"a"),
        kv("k2", b"bb"),
        kv("k2", b"c"),
        kv("k3", b"x1"),
        kv("k3", b"x2"),
    ]
    .to_vec();
    let main_expected = vec![
        kv("alpha", b"first value"),
        kv("big", &big),
        kv("dups", &dups_db.to_bytes()),
        kv("zeta", b""),
    ];
    (file, main_expected, dups_expected)
}

#[test]
fn handbuilt_file_reads_identically_in_c_and_rust() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    std::fs::create_dir(&path).unwrap();
    let (file, main_expected, dups_expected) = build();
    std::fs::write(path.join("data.mdb"), &file).unwrap();

    // Rust.
    let env = open_ro(&path, 2);
    assert_eq!(
        env.page_size(),
        PS,
        "page size comes from the meta, not the OS"
    );
    let info = env.info().unwrap();
    assert_eq!((info.last_txnid, info.last_pgno), (1, 6));
    verify_db(&env, Dbi::MAIN, &main_expected);
    let dups = env.open_db(Some("dups")).unwrap();
    verify_db(&env, dups, &dups_expected);
    {
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(dups).unwrap();
        c.set(b"k2").unwrap().unwrap();
        assert_eq!(c.count().unwrap(), 3);
        c.set(b"k3").unwrap().unwrap();
        assert_eq!(c.count().unwrap(), 2);
        c.set(b"k1").unwrap().unwrap();
        assert_eq!(c.count().unwrap(), 1);
    }

    // C.
    let Some(c) = common::c_tools() else {
        eprintln!("skipped C half: C LMDB tools unavailable");
        return;
    };
    assert_eq!(c.dump(&path, None), main_expected);
    assert_eq!(c.dump(&path, Some("dups")), dups_expected);
    let stat = parse_stat(&c.stat_text(&path, &["-e", "-a"]));
    assert_eq!(stat.env["Page size"], PS.to_string());
    assert_eq!(stat.env["Number of pages used"], "7");
    let txn = env.begin_ro_txn().unwrap();
    assert_stat_matches(
        "Main DB",
        &txn.stat(Dbi::MAIN).unwrap(),
        &stat.dbs["Main DB"],
    );
    assert_stat_matches("dups", &txn.stat(dups).unwrap(), &stat.dbs["dups"]);
    differential(c, &path, &env, None, &main_expected, 41, 4);
    differential(c, &path, &env, Some("dups"), &dups_expected, 42, 6);
}

#[test]
fn corrupt_pages_give_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    std::fs::create_dir(&path).unwrap();
    let (good, _, _) = build();
    // Damage one structure at a time and make sure every read path
    // returns (possibly an error) without panicking.
    type Damage = Box<dyn Fn(&mut Vec<u8>)>;
    let damages: Vec<(&str, Damage)> = vec![
        (
            "root points past the end",
            Box::new(|f| f[PS + 16 + 24 + 48 + 40..][..8].copy_from_slice(&99u64.to_ne_bytes())),
        ),
        (
            "root points at a meta page",
            Box::new(|f| f[PS + 16 + 24 + 48 + 40..][..8].copy_from_slice(&1u64.to_ne_bytes())),
        ),
        (
            "node pointer out of page",
            Box::new(|f| f[2 * PS + 16..][..2].copy_from_slice(&0xfff0u16.to_ne_bytes())),
        ),
        (
            "huge mp_lower",
            Box::new(|f| f[2 * PS + 12..][..2].copy_from_slice(&0xfff0u16.to_ne_bytes())),
        ),
        (
            "overflow pgno past the end",
            Box::new(|f| {
                // "big" is node 1 on page 2; its data is the overflow pgno.
                let off = u16::from_ne_bytes([f[2 * PS + 18], f[2 * PS + 19]]) as usize;
                f[2 * PS + off + 8 + 3..][..8].copy_from_slice(&1000u64.to_ne_bytes());
            }),
        ),
        (
            "overflow page has wrong type",
            Box::new(|f| f[3 * PS + 10..][..2].copy_from_slice(&2u16.to_ne_bytes())),
        ),
        (
            "huge data size",
            Box::new(|f| {
                let off = u16::from_ne_bytes([f[2 * PS + 16], f[2 * PS + 17]]) as usize;
                f[2 * PS + off..][..4].copy_from_slice(&0xffff_fff0u32.to_ne_bytes());
            }),
        ),
        (
            "sub-DB record truncated",
            Box::new(|f| {
                let off = u16::from_ne_bytes([f[5 * PS + 20], f[5 * PS + 21]]) as usize;
                f[5 * PS + off..][..2].copy_from_slice(&3u16.to_ne_bytes());
            }),
        ),
        (
            "branch flag on a leaf",
            Box::new(|f| f[2 * PS + 10..][..2].copy_from_slice(&1u16.to_ne_bytes())),
        ),
    ];
    for (what, damage) in damages {
        let mut file = good.clone();
        damage(&mut file);
        std::fs::write(path.join("data.mdb"), &file).unwrap();
        let env = open_ro(&path, 2);
        let txn = env.begin_ro_txn().unwrap();
        let mut outcomes = Vec::new();
        for key in [&b"alpha"[..], b"big", b"dups", b"zeta", b"nope"] {
            outcomes.push(format!(
                "{:?}",
                txn.get(Dbi::MAIN, key).map(|v| v.map(<[u8]>::len))
            ));
        }
        if let Ok(mut c) = txn.cursor(Dbi::MAIN) {
            for _ in 0..10 {
                outcomes.push(format!("{:?}", c.next().map(|e| e.map(|(k, _)| k.len()))));
            }
            for _ in 0..10 {
                outcomes.push(format!("{:?}", c.prev().map(|e| e.map(|(k, _)| k.len()))));
            }
        }
        let _ = txn.open_db(Some("dups")).and_then(|db| {
            let mut c = txn.cursor(db)?;
            while c.next()?.is_some() {}
            Ok(())
        });
        eprintln!("{what}: {}", outcomes[..5].join(" "));
        drop(txn);
        drop(env);
    }
}

/// A file of `PS`-byte pages: the meta pages, then `pages` (page 2 on),
/// with the main DB record `main`.
fn file_of(pages: &[Vec<u8>], main: Db) -> Vec<u8> {
    file_with(&V09, PS, pages, main)
}

/// [`file_of`] in the format of `layout`, with pages of `ps` bytes.
fn file_with(layout: &Layout, ps: usize, pages: &[Vec<u8>], main: Db) -> Vec<u8> {
    let mut m0 = Meta::new_env_with_layout(layout, ps as u32, 1 << 20, 0);
    m0.dbs[0].root = P_INVALID;
    let mut m1 = m0;
    m1.txnid = 1;
    m1.last_pgno = 1 + pages.len() as u64;
    m1.dbs[1] = main;
    let mut file = vec![0u8; 2 * ps];
    m0.write_initial_pages_with_layout(&mut file, ps, layout);
    m1.write(&mut file[ps + layout.meta_offset()..]);
    for page in pages {
        file.extend_from_slice(page);
    }
    file
}

/// Pages `first..` of a chain: `depth - 1` branch pages with a single
/// child each, then a leaf holding `leaf` (keys with their data).
fn chain(first: u64, depth: usize, leaf: &[(&[u8], &[u8])]) -> Vec<Vec<u8>> {
    let mut pages = Vec::new();
    for level in 0..depth as u64 - 1 {
        let mut buf = vec![0u8; PS];
        let mut p = PageMut::new(&mut buf);
        p.init(first + level, P_BRANCH);
        p.add_node(0, b"", NodeData::Child(first + level + 1), 0)
            .unwrap();
        pages.push(buf);
    }
    let mut buf = vec![0u8; PS];
    let mut p = PageMut::new(&mut buf);
    p.init(first + depth as u64 - 1, P_LEAF);
    for (i, (k, v)) in leaf.iter().enumerate() {
        p.add_node(i, k, NodeData::Inline(v), 0).unwrap();
    }
    pages.push(buf);
    pages
}

/// `get` uses short cursor stacks for shallow trees and falls back to
/// C's 32 levels: on trees deeper than the short stack, with a truthful
/// or a lying `md_depth`, and on a sub-DB of duplicates deeper than it,
/// it must find what a cursor finds, and fail beyond 32 levels with
/// `MDB_CURSOR_FULL` as C does.
#[test]
fn deep_trees_and_short_get_stacks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("env");
    std::fs::create_dir(&path).unwrap();
    let full = |r: rlmdb::Result<Option<&[u8]>>| match r {
        Err(Error::Mdb(MdbError::CursorFull)) => true,
        r => panic!("expected MDB_CURSOR_FULL, got {r:?}"),
    };

    let leaf: &[(&[u8], &[u8])] = &[(b"a", b"1"), (b"key", b"value"), (b"z", b"26")];
    for (depth, recorded) in [
        (1, 1),
        (8, 8),
        (9, 9),
        (12, 12),
        (12, 2),
        (32, 32),
        (32, 1),
        (33, 33),
        (33, 3),
    ] {
        let main = Db {
            depth: recorded,
            entries: leaf.len() as u64,
            root: 2,
            ..Db::default()
        };
        std::fs::write(path.join("data.mdb"), file_of(&chain(2, depth, leaf), main)).unwrap();
        let env = open_ro(&path, 2);
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(Dbi::MAIN).unwrap();
        if depth <= 32 {
            for (k, v) in leaf {
                assert_eq!(
                    txn.get(Dbi::MAIN, k).unwrap(),
                    Some(*v),
                    "depth {depth}/{recorded}"
                );
                assert_eq!(c.set(k).unwrap(), Some((*k, *v)));
            }
            assert_eq!(txn.get(Dbi::MAIN, b"nope").unwrap(), None);
        } else {
            assert!(full(txn.get(Dbi::MAIN, b"key")), "depth {depth}/{recorded}");
            assert!(matches!(
                c.set(b"key"),
                Err(Error::Mdb(MdbError::CursorFull))
            ));
        }
    }

    // A DUPSORT database "dups" whose key "k" has its duplicates in a
    // sub-DB of `depth` levels (pages 4..).
    for depth in [9, 32, 33] {
        let sub = Db {
            depth: depth as u16,
            entries: 2,
            root: 4,
            ..Db::default()
        };
        let dups = Db {
            flags: MDB_DUPSORT as u16,
            depth: 1,
            entries: 2,
            root: 3,
            ..Db::default()
        };
        let mut pages = vec![vec![0u8; PS], vec![0u8; PS]];
        let mut p = PageMut::new(&mut pages[0]);
        p.init(2, P_LEAF);
        p.add_node(0, b"dups", NodeData::Inline(&dups.to_bytes()), F_SUBDATA)
            .unwrap();
        let mut p = PageMut::new(&mut pages[1]);
        p.init(3, P_LEAF);
        p.add_node(
            0,
            b"k",
            NodeData::Inline(&sub.to_bytes()),
            F_DUPDATA | F_SUBDATA,
        )
        .unwrap();
        pages.extend(chain(4, depth, &[(b"d1", b""), (b"d2", b"")]));
        let main = Db {
            depth: 1,
            entries: 1,
            root: 2,
            ..Db::default()
        };
        std::fs::write(path.join("data.mdb"), file_of(&pages, main)).unwrap();
        let env = open_ro(&path, 2);
        let db = env.open_db(Some("dups")).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        let mut c = txn.cursor(db).unwrap();
        if depth <= 32 {
            assert_eq!(
                txn.get(db, b"k").unwrap(),
                Some(&b"d1"[..]),
                "sub-DB depth {depth}"
            );
            assert_eq!(c.set(b"k").unwrap(), Some((&b"k"[..], &b"d1"[..])));
            assert_eq!(c.next_dup().unwrap(), Some((&b"k"[..], &b"d2"[..])));
        } else {
            assert!(full(txn.get(db, b"k")), "sub-DB depth {depth}");
            assert!(matches!(c.set(b"k"), Err(Error::Mdb(MdbError::CursorFull))));
        }
    }
}

/// A tree as deep as a cursor stack (`CURSOR_STACK`, 32 levels; a real
/// one would have more than 2^31 pages, so this file is damaged: every
/// branch page names one child several times) whose pages along the path
/// to its last key are all full: a put after that key splits every level,
/// up to the root, and the new root would be a 33rd level under the
/// cursors on the old one. C writes past the end of its cursor stacks;
/// rlmdb panicked with an index out of bounds in `mdb_page_split` (which
/// aborts a C caller). It must fail the put with `MDB_CURSOR_FULL`, which
/// fails the transaction, and leave the file as it was.
///
/// Pages of 256 bytes, in both formats: pages 2..=32 are branch pages of
/// the implicit first child and three 58-byte separators, every child
/// being the next page; page 33 is a leaf of three 58-byte keys with
/// 1-byte values. The put goes through `mdb_put` and through a cursor
/// handle (the C API's `mdb_cursor_put`), with another cursor on the leaf.
#[test]
fn root_split_beyond_the_cursor_stack_fails_the_txn() {
    const PS256: usize = 256;
    let key = |c: u8| vec![c; 58];
    for layout in [&V09, &V10] {
        let mut pages = Vec::new();
        for pgno in 2..=32 {
            let mut buf = vec![0u8; PS256];
            let mut p = PageMut::with_layout(&mut buf, layout);
            p.init(pgno, P_BRANCH);
            p.add_node(0, b"", NodeData::Child(pgno + 1), 0).unwrap();
            for (i, c) in (b'b'..=b'd').enumerate() {
                p.add_node(i + 1, &key(c), NodeData::Child(pgno + 1), 0)
                    .unwrap();
            }
            assert!(p.add_node(4, &key(b'e'), NodeData::Child(2), 0).is_err());
            pages.push(buf);
        }
        let mut buf = vec![0u8; PS256];
        let mut p = PageMut::with_layout(&mut buf, layout);
        p.init(33, P_LEAF);
        for (i, c) in (b'd'..=b'f').enumerate() {
            p.add_node(i, &key(c), NodeData::Inline(&[c]), 0).unwrap();
        }
        assert!(p
            .add_node(3, &key(b'g'), NodeData::Inline(b"g"), 0)
            .is_err());
        pages.push(buf);
        let main = Db {
            depth: 32,
            branch_pages: 31,
            leaf_pages: 1,
            entries: 3,
            root: 2,
            ..Db::default()
        };
        let file = file_with(layout, PS256, &pages, main);
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data.mdb");
        std::fs::write(&data, &file).unwrap();
        let env = open_rw(dir.path(), 2);
        let what = format!("{:?}", env.format());
        assert_eq!((env.format(), env.page_size()), (layout.format, PS256));
        {
            let txn = env.begin_ro_txn().unwrap();
            assert_eq!(txn.get(Dbi::MAIN, &key(b'e')).unwrap(), Some(&b"e"[..]));
        }
        let cursor_full = |r: rlmdb::Result<()>, how: &str| match r {
            Err(Error::Mdb(MdbError::CursorFull)) => {}
            r => panic!("{what}: {how}: expected MDB_CURSOR_FULL, got {r:?}"),
        };
        let bad_txn = |r: rlmdb::Result<()>, how: &str| match r {
            Err(Error::Mdb(MdbError::BadTxn)) => {}
            r => panic!("{what}: commit after {how}: expected MDB_BAD_TXN, got {r:?}"),
        };

        let mut txn = env.begin_rw_txn().unwrap();
        let r = txn.put(Dbi::MAIN, &key(b'g'), b"g", WriteFlags::empty());
        cursor_full(r, "put");
        bad_txn(txn.commit(), "put");

        let mut txn = env.begin_rw_txn().unwrap();
        let (c0, c1) = (
            txn.open_cursor(Dbi::MAIN).unwrap(),
            txn.open_cursor(Dbi::MAIN).unwrap(),
        );
        let on_leaf = txn.cursor_get(c1, Some(&key(b'f')), None, rlmdb::CursorOp::Set);
        assert!(matches!(on_leaf, Ok(Some(_))), "{what}");
        let r = txn.cursor_put(c0, &key(b'g'), b"g", WriteFlags::empty());
        cursor_full(r, "cursor put");
        bad_txn(txn.commit(), "cursor put");

        drop(env);
        assert!(
            std::fs::read(&data).unwrap() == file,
            "{what}: file changed"
        );
    }
}
