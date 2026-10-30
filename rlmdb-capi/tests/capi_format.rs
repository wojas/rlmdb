//! `tests/c/capi_format.c` checks the data-file format support of the C
//! API against the Rust library: `rlmdb.h`'s `rlmdb_env_set_format`,
//! `rlmdb_env_get_format`, `rlmdb_sniff_format`, `rlmdb_set_default_format`
//! (and the experimental `RLMDB_DEFAULT_FORMAT` variable, set and unset by
//! the program in its own process), `rlmdb_env_set_pagesize` and the LMDB
//! 1.0 constants; `mdb_env_get_maxkeysize` before and after the open; LMDB
//! 1.0 environments of 1, 4 and 64 KiB pages with keys up to their limit
//! (longer than 0.9's 511 bytes) and inline data larger than 16 KiB through
//! `mdb_put`, `mdb_cursor_put` (`MDB_CURRENT`, `MDB_RESERVE`,
//! `MDB_MULTIPLE`), `mdb_get`, `mdb_del` and `mdb_dbi_open`, also with
//! inputs that point into the transaction's dirty pages; a 1.0 file opened
//! without a request; `MDB_VERSION_MISMATCH` for the other format;
//! `mdb_strerror` of the 1.0 codes; `MDB_PREVSNAPSHOT` and `MDB_ENCRYPT`
//! refused. There is no C oracle (C LMDB has no `rlmdb.h`); that C 1.0
//! reads and writes these files is `interop/tests/capi10.rs`.

mod common;

use std::process::Command;

use common::*;

#[test]
fn rlmdb_h_format_functions() {
    let exe = builder().test_program("capi_format", Lib::Rust);
    let scratch = builder().scratch("capi_format");
    let out = Command::new(&exe)
        .arg(&scratch)
        .env_remove("RLMDB_DEFAULT_FORMAT")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- {} ---\n{stdout}{stderr}", exe.display());
    assert!(
        out.status.success() && stderr.is_empty(),
        "{} failed ({})",
        exe.display(),
        out.status
    );
    assert!(stdout.trim_end().ends_with(" 0 failures"), "{stdout}");
}
