//! `tests/c/capi_features.c` checks the opt-in features and the
//! environment variables of the C API against the Rust library: `rlmdb.h`'s
//! `rlmdb_features`, `rlmdb_feature_default`, `rlmdb_env_enable_feature`,
//! `rlmdb_env_disable_feature`, `rlmdb_env_feature_enabled` and
//! `rlmdb_env_read_environment`; `RLMDB_FEATURES`, `RLMDB_IGNORE_ENV` and
//! `RLMDB_DEFAULT_FORMAT`, set and unset by the program in its own process,
//! read by default and not after `rlmdb_env_read_environment(env, 0)`; the
//! program's requests winning over the variables; the error codes, their
//! `mdb_strerror` texts and the features in `rlmdb_version_string`. There
//! is no C oracle: C LMDB has no `rlmdb.h`.

mod common;

use std::process::Command;

use common::*;

#[test]
fn rlmdb_h_feature_functions() {
    let exe = builder().test_program("capi_features", Lib::RustApi);
    let scratch = builder().scratch("capi_features");
    let out = Command::new(&exe)
        .arg(&scratch)
        .env_remove("RLMDB_DEFAULT_FORMAT")
        .env_remove("RLMDB_FEATURES")
        .env_remove("RLMDB_IGNORE_ENV")
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
