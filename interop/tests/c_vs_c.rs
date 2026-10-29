//! Every interop scenario with the C peer on both sides.
//!
//! This validates the harness itself: if a scenario fails here, the bug is
//! in the scenario, the C peer or the harness, not in the Rust library.
//! Stage 2/3 add the same line for (C, Rust), (Rust, C) and (Rust, Rust).
//!
//! The tests after the macro check that the checkers are not vacuous: they
//! must reject wrong data and broken page accounting.

use interop::scenarios::{self, DataSet};
use interop::{Peer, TestEnv};

interop::scenario_tests!(interop::Peer::C, interop::Peer::C);

/// The Rust peer is not there yet; asking for it is a clear error.
#[test]
fn rust_peer_reports_not_built() {
    if std::env::var_os("INTEROP_RPEER").is_some() {
        return;
    }
    match Peer::Rust.bin() {
        Ok(path) => assert!(path.is_file()),
        Err(e) => assert!(e.message().contains("not built yet"), "{e}"),
    }
}

/// The scenario table and the generated tests come from the same list.
#[test]
fn scenario_table_is_complete() {
    let names: Vec<&str> = scenarios::ALL.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"mvcc_hold_reader"));
    assert!(names.contains(&"gen_vectors"));
    assert_eq!(names.len(), 21, "{names:?}");
}

/// An environment with a filled DB "d" and some free pages.
fn filled_env() -> interop::Result<(TestEnv, DataSet)> {
    let env = TestEnv::new()?;
    scenarios::create(Peer::C, &env, 64 << 20)?;
    let ds = DataSet::new("d", 2000, 1);
    ds.fill(Peer::C, &env, &[])?;
    ds.delete(Peer::C, &env, 2, &[])?;
    Ok((env, ds.after_delete(2)))
}

#[test]
fn verify_rejects_wrong_data() -> interop::Result<()> {
    let (env, ds) = filled_env()?;
    ds.verify(Peer::C, &env)?;
    for wrong in [
        DataSet::new("d", 2000, 2).with(&["--deleted-every", "2"]), // other seed
        DataSet::new("d", 1999, 1).with(&["--deleted-every", "2"]), // one key fewer
        DataSet::new("d", 2000, 1),                                 // deletions not expected
        DataSet::new("d", 2000, 1).with(&["--deleted-every", "2", "--valsize", "31"]),
        DataSet::new("d", 2000, 1).with(&["--deleted-every", "2", "--dupsort"]),
    ] {
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

/// Overwrite `bytes` at `offset` of a file.
fn patch(path: &std::path::Path, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(bytes)
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read_u16(data: &[u8], offset: usize) -> usize {
    u16::from_ne_bytes(data[offset..offset + 2].try_into().unwrap()).into()
}

/// The meta page (0 or 1) with the higher txnid, and the page size.
fn newest_meta(data: &[u8]) -> (usize, usize) {
    let psize = u32::from_ne_bytes(data[16 + 24..16 + 28].try_into().unwrap()) as usize;
    let txnid = |m: usize| read_u64(data, m * psize + 16 + 128);
    (if txnid(1) > txnid(0) { 1 } else { 0 }, psize)
}

#[test]
fn check_freelist_rejects_a_leaked_page() -> interop::Result<()> {
    let (env, _) = filled_env()?;
    scenarios::check_freelist(Peer::C, &env)?;
    // Claim one more page than is accounted for: mm_last_pg += 1 in the
    // current meta page (offset 16 + 120).
    let data = std::fs::read(env.data_file())?;
    let (meta, psize) = newest_meta(&data);
    let off = meta * psize + 16 + 120;
    patch(
        &env.data_file(),
        off as u64,
        &(read_u64(&data, off) + 1).to_ne_bytes(),
    )?;
    let out = Peer::C.on("check-freelist", &env).output()?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stderr.contains("is leaked"), "{out}");
    Ok(())
}

#[test]
fn check_freelist_rejects_a_page_used_twice() -> interop::Result<()> {
    let (env, _) = filled_env()?;
    let report = scenarios::check_freelist(Peer::C, &env)?;
    assert!(report.pages_free > 0);
    // List the main DB's root page in the first free DB record: mm_dbs[0]
    // (free DB) is at 16 + 24 in the meta page, md_root at +40; a one-level
    // free DB's root is its leaf. Its first node: mp_ptrs[0] at 16, node
    // header 8 bytes, key (8-byte txnid), then [count, pgno...].
    let data = std::fs::read(env.data_file())?;
    let (meta, psize) = newest_meta(&data);
    let m = meta * psize + 16;
    let free_root = read_u64(&data, m + 24 + 40) as usize;
    let main_root = read_u64(&data, m + 72 + 40);
    assert_eq!(read_u16(&data, m + 24 + 6), 1, "free DB depth");
    let leaf = free_root * psize;
    let node = leaf + read_u16(&data, leaf + 16);
    let ksize = read_u16(&data, node + 6);
    let first_pgno = node + 8 + ksize + 8;
    patch(
        &env.data_file(),
        first_pgno as u64,
        &main_root.to_ne_bytes(),
    )?;
    let out = Peer::C.on("check-freelist", &env).output()?;
    assert_eq!(out.code(), Some(1), "{out}");
    assert!(out.stderr.contains("used twice"), "{out}");
    Ok(())
}
