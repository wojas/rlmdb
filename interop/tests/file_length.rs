//! The length of an LMDB 0.9 data file on Windows, which a writable
//! environment makes as long as its map (`mdb_env_map`'s `SetEndOfFile`).
//! A writable opener with a smaller map than the file: C 0.9 cuts the file
//! to its map size, or fails to open while another process maps more;
//! rlmdb (the Rust peer, and the C peer on rlmdb) leaves the file as it is
//! (`KNOWN_C_BUGS.md` row 57). Elsewhere a map may reach past the end of the
//! file, and no opener changes its length.
#![cfg(windows)]

use interop::scenarios::{self, DataSet};
use interop::{Peer, TestEnv};

const BIG: u64 = 64 << 20;
const SMALL: &str = "33554432";

fn register() {
    interop::register_rpeer(option_env!("CARGO_BIN_EXE_rpeer"));
}

/// An environment with a 64 MiB map and about 8 MB of data, written by C.
fn big_env() -> interop::Result<(TestEnv, DataSet)> {
    let env = TestEnv::new()?;
    scenarios::create(Peer::C, &env, BIG)?;
    let ds = DataSet::new("d", 2000, 3).with(&["--valsize", "4000"]);
    ds.fill(Peer::C, &env, &[])?;
    Ok((env, ds))
}

fn file_len(env: &TestEnv) -> interop::Result<u64> {
    Ok(std::fs::metadata(env.data_file())?.len())
}

/// A write by `peer` with a 32 MiB map (one item in another DB).
fn small_write(peer: Peer, env: &TestEnv) -> interop::Result<interop::Output> {
    peer.on("fill", env)
        .args(["e", "1", "99", "--mapsize", SMALL])
        .output()
}

#[test]
fn smaller_map_and_no_other_view() -> interop::Result<()> {
    register();
    for peer in [Peer::C, Peer::Rust, Peer::COnRust] {
        let (env, ds) = big_env()?;
        assert_eq!(file_len(&env)?, BIG);
        small_write(peer, &env)?.ok()?;
        // C cuts the file to its map; nothing was committed past it here.
        let want = if peer == Peer::C { 32 << 20 } else { BIG };
        assert_eq!(file_len(&env)?, want, "{peer}");
        ds.verify(Peer::C, &env)?;
    }
    Ok(())
}

#[test]
fn smaller_map_next_to_a_larger_view() -> interop::Result<()> {
    register();
    for peer in [Peer::C, Peer::Rust, Peer::COnRust] {
        let (env, ds) = big_env()?;
        let mut holder = Peer::C
            .on("hold-env", &env)
            .args([scenarios::hold_secs().as_str(), "--until-stdin"])
            .spawn()?;
        holder.wait_ready()?;
        let out = small_write(peer, &env)?;
        if peer == Peer::C {
            // SetEndOfFile cannot cut what another process maps
            // (ERROR_USER_MAPPED_FILE), and the open fails.
            assert_eq!(out.code(), Some(3), "{out}");
            assert!(out.stderr.contains("failed: 1224"), "{out}");
        } else {
            out.ok()?;
        }
        assert_eq!(file_len(&env)?, BIG, "{peer}");
        holder.release()?;
        holder.finish()?.ok()?;
        ds.verify(Peer::C, &env)?;
    }
    Ok(())
}
