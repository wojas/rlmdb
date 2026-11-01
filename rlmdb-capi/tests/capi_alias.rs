//! `tests/c/capi_alias.c`: `mdb_env_open` of an environment that the
//! process has open, under other names (hard links of both of its files,
//! and of the lock file alone, as a bind mount would show them), fails
//! with `EBUSY` and leaves the open handle as it was: its reader slot, the
//! value its read transaction holds and the txnid of its next writer.
//! Against the Rust library only: C allows the second open (`lmdb.h`
//! forbids it) and re-initialises the lock file under the open handle's
//! readers. The hard links are made here, of empty files that the program's
//! first open sets up, because the C test shims have no `link()`.

mod common;

use std::fs;
use std::process::Command;

use common::*;

#[test]
fn second_open_under_another_name_is_ebusy() {
    let b = builder();
    let exe = b.test_program("capi_alias", Lib::Rust);
    let scratch = b.scratch("capi_alias");
    let [env_a, env_b, env_c] = ["a", "b", "c"].map(|name| {
        let path = scratch.join(name);
        fs::create_dir(&path).unwrap();
        path
    });
    for file in ["data.mdb", "lock.mdb"] {
        fs::write(env_a.join(file), b"").unwrap();
    }
    if let Err(e) = fs::hard_link(env_a.join("data.mdb"), env_b.join("data.mdb")) {
        eprintln!("NOTE: skipped: the file system refuses hard links: {e}");
        return;
    }
    fs::hard_link(env_a.join("lock.mdb"), env_b.join("lock.mdb")).unwrap();
    fs::hard_link(env_a.join("lock.mdb"), env_c.join("lock.mdb")).unwrap();

    let out = Command::new(&exe).arg(&scratch).output().unwrap();
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
