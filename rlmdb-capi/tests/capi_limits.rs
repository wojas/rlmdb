//! `tests/c/capi_limits.c` checks the limits the Rust library has where C
//! LMDB has none (`mdb_env_set_maxdbs` beyond `MAX_DBS_LIMIT` is `EINVAL`,
//! `KNOWN_C_BUGS.md` #54), against the Rust library only.

mod common;

use std::process::Command;

use common::*;

#[test]
fn limits_c_lmdb_lacks() {
    let exe = builder().test_program("capi_limits", Lib::Rust);
    let dir = builder().scratch("capi_limits");
    let env = dir.join("env");
    std::fs::create_dir_all(&env).unwrap();
    let out = Command::new(&exe).arg(&env).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
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
