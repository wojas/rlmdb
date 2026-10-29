//! Fixtures shared by the integration tests: a filled environment and
//! helpers to corrupt its data file on purpose.

#![allow(dead_code)] // each test file uses a subset

use interop::scenarios::{self, DataSet};
use interop::{Peer, TestEnv};

/// An environment with a filled DB "d" (written by C) and some free pages;
/// returns the data set as it is after the deletion.
pub fn filled_env() -> interop::Result<(TestEnv, DataSet)> {
    let env = TestEnv::new()?;
    scenarios::create(Peer::C, &env, 64 << 20)?;
    let ds = DataSet::new("d", 2000, 1);
    ds.fill(Peer::C, &env, &[])?;
    ds.delete(Peer::C, &env, 2, &[])?;
    Ok((env, ds.after_delete(2)))
}

/// Overwrite `bytes` at `offset` of a file.
pub fn patch(path: &std::path::Path, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(bytes)
}

pub fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(data[offset..offset + 8].try_into().unwrap())
}

pub fn read_u16(data: &[u8], offset: usize) -> usize {
    u16::from_ne_bytes(data[offset..offset + 2].try_into().unwrap()).into()
}

/// The meta page (0 or 1) with the higher txnid, and the page size.
pub fn newest_meta(data: &[u8]) -> (usize, usize) {
    let psize = u32::from_ne_bytes(data[16 + 24..16 + 28].try_into().unwrap()) as usize;
    let txnid = |m: usize| read_u64(data, m * psize + 16 + 128);
    (if txnid(1) > txnid(0) { 1 } else { 0 }, psize)
}

/// Claim one more page than is accounted for: `mm_last_pg += 1` in the
/// current meta page (offset 16 + 120).
pub fn leak_a_page(env: &TestEnv) -> std::io::Result<()> {
    let data = std::fs::read(env.data_file())?;
    let (meta, psize) = newest_meta(&data);
    let off = meta * psize + 16 + 120;
    patch(
        &env.data_file(),
        off as u64,
        &(read_u64(&data, off) + 1).to_ne_bytes(),
    )
}

/// List the main DB's root page in the first free DB record: `mm_dbs[0]`
/// (free DB) is at 16 + 24 in the meta page, `md_root` at +40; a one-level
/// free DB's root is its leaf. Its first node: `mp_ptrs[0]` at 16, node
/// header 8 bytes, key (8-byte txnid), then `[count, pgno...]`.
pub fn use_a_page_twice(env: &TestEnv) -> std::io::Result<()> {
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
    )
}

/// Data sets that `verify` must reject on the environment of
/// [`filled_env`] (`d`, 2000 keys, seed 1, every second key deleted).
pub fn wrong_data_sets() -> Vec<DataSet> {
    vec![
        DataSet::new("d", 2000, 2).with(&["--deleted-every", "2"]), // other seed
        DataSet::new("d", 1999, 1).with(&["--deleted-every", "2"]), // one key fewer
        DataSet::new("d", 2000, 1),                                 // deletions not expected
        DataSet::new("d", 2000, 1).with(&["--deleted-every", "2", "--valsize", "31"]),
        DataSet::new("d", 2000, 1).with(&["--deleted-every", "2", "--dupsort"]),
    ]
}
