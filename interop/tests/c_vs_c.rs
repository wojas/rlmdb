//! Every interop scenario with the C peer on both sides.
//!
//! This validates the harness itself: if a scenario fails here, the bug is
//! in the scenario, the C peer or the harness, not in the Rust library.
//! The other eight ordered pairs of {C, Rust, COnRust} have one file each
//! with the same line (`rust_writes_c_reads.rs`, ...).
//!
//! The tests after the macro check that the checkers are not vacuous: they
//! must reject wrong data and broken page accounting.

mod common;

use interop::scenarios;
use interop::{Peer, TestEnv};

interop::scenario_tests!(interop::Peer::C, interop::Peer::C);

/// The scenario table and the generated tests come from the same list.
#[test]
fn scenario_table_is_complete() {
    let names: Vec<&str> = scenarios::ALL.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"mvcc_hold_reader"));
    assert!(names.contains(&"gen_vectors"));
    assert!(names.contains(&"stale_reader_cleared_by_mdb_stat"));
    assert!(names.contains(&"killed_writer"));
    assert_eq!(names.len(), 33, "{names:?}");
}

/// The C peer implements every command, including the harness's own
/// `commands` query.
#[test]
fn c_peer_supports_everything() {
    for cmd in [
        "create",
        "fill",
        "delete",
        "drop",
        "writer-loop",
        "hold-writer",
        "nested-test",
        "copy",
        "verify",
    ] {
        assert!(Peer::C.supports(cmd), "{cmd}");
    }
    assert!(!Peer::C.supports("no-such-command"));
}

#[test]
fn verify_rejects_wrong_data() -> interop::Result<()> {
    let (env, ds) = common::filled_env()?;
    ds.verify(Peer::C, &env)?;
    for wrong in common::wrong_data_sets() {
        let out = Peer::C.on("verify", &env).args(wrong.args()).output()?;
        assert_eq!(
            out.code(),
            Some(1),
            "verify {:?} should fail: {out}",
            wrong.args()
        );
        assert!(out.stderr.contains("verify db=d"), "{out}");
    }
    Ok(())
}

#[test]
fn check_freelist_rejects_a_leaked_page() -> interop::Result<()> {
    let (env, _) = common::filled_env()?;
    scenarios::check_freelist(Peer::C, &env)?;
    common::leak_a_page(&env)?;
    let out = Peer::C.on("check-freelist", &env).output()?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stderr.contains("is leaked"), "{out}");
    Ok(())
}

#[test]
fn check_freelist_rejects_a_page_used_twice() -> interop::Result<()> {
    let (env, _) = common::filled_env()?;
    let report = scenarios::check_freelist(Peer::C, &env)?;
    assert!(report.pages_free > 0);
    common::use_a_page_twice(&env)?;
    let out = Peer::C.on("check-freelist", &env).output()?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stderr.contains("used twice"), "{out}");
    Ok(())
}

/// The structural comparison of data files (`interop::pagecmp`) sees real
/// differences (one more key, a changed value byte) and ignores bytes C
/// leaves uninitialised (a node's padding byte).
#[test]
fn pagecmp_detects_differences() -> interop::Result<()> {
    use interop::pagecmp::compare_data_files;
    let make = || -> interop::Result<TestEnv> {
        let env = TestEnv::new()?;
        scenarios::create(Peer::C, &env, 64 << 20)?;
        // Keys of 10..13 bytes, 31-byte values: odd node sizes, so leaf
        // nodes have a padding byte.
        scenarios::DataSet::new("d", 500, 1)
            .with(&["--valsize", "31"])
            .fill(Peer::C, &env, &[])?;
        Ok(env)
    };
    let (a, b) = (make()?, make()?);
    compare_data_files(&a.data_file(), &b.data_file())?;
    assert!(interop::pagecmp::reachable_pages(&a.data_file())? > 3);

    // A padding byte: find a leaf page of "d" and poke the byte after its
    // first node.
    let data = std::fs::read(b.data_file())?;
    let (_, psize) = common::newest_meta(&data);
    let leaf = (2..data.len() / psize)
        .find(|&p| {
            let pg = &data[p * psize..(p + 1) * psize];
            common::read_u16(pg, 10) == 2 && common::read_u64(pg, 0) == p as u64 && {
                // a leaf of "d": its first key is 10+ bytes of hex
                let node = common::read_u16(pg, 16);
                common::read_u16(pg, node + 6) >= 10
            }
        })
        .expect("a leaf page of d");
    let pg = leaf * psize;
    let node = pg + common::read_u16(&data, pg + 16);
    let ksize = common::read_u16(&data, node + 6);
    let dsize = common::read_u16(&data, node);
    let end = 8 + ksize + dsize;
    assert_eq!(end % 2, 1, "the node needs a padding byte");
    common::patch(&b.data_file(), (node + end) as u64, &[0xa5])?;
    compare_data_files(&a.data_file(), &b.data_file())?;

    // A byte of the value: a difference.
    common::patch(
        &b.data_file(),
        (node + end - 1) as u64,
        &[0x5a ^ data[node + end - 1]],
    )?;
    let err = compare_data_files(&a.data_file(), &b.data_file()).unwrap_err();
    assert!(
        err.message().contains(&format!("page {leaf} differs")),
        "{err}"
    );

    // One more key: other pages.
    scenarios::DataSet::new("e", 1, 2).fill(Peer::C, &a, &[])?;
    assert!(compare_data_files(&a.data_file(), &b.data_file()).is_err());
    Ok(())
}
