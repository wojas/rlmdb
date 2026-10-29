//! Every interop scenario with the C peer on both sides.
//!
//! This validates the harness itself: if a scenario fails here, the bug is
//! in the scenario, the C peer or the harness, not in the Rust library.
//! `c_writes_rust_reads.rs` has the same line for (C, Rust); stage 3 adds
//! (Rust, C) and (Rust, Rust).
//!
//! The tests after the macro check that the checkers are not vacuous: they
//! must reject wrong data and broken page accounting.

mod common;

use interop::scenarios;
use interop::Peer;

interop::scenario_tests!(interop::Peer::C, interop::Peer::C);

/// The scenario table and the generated tests come from the same list.
#[test]
fn scenario_table_is_complete() {
    let names: Vec<&str> = scenarios::ALL.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"mvcc_hold_reader"));
    assert!(names.contains(&"gen_vectors"));
    assert!(names.contains(&"stale_reader_cleared_by_mdb_stat"));
    assert_eq!(names.len(), 26, "{names:?}");
}

/// The C peer implements every command, including the harness's own
/// `commands` query.
#[test]
fn c_peer_supports_everything() {
    for cmd in [
        "create",
        "fill",
        "delete",
        "writer-loop",
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
