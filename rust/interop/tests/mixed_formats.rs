//! Environments of both formats open at once
//! (`interop::scenarios::mixed`): one rlmdb process (`rpeer formats-loop`)
//! with an LMDB 0.9 and an LMDB 1.0 environment, next to C 0.9 on the
//! first and C 1.0 on the second.

use interop::scenarios::mixed;

fn register() {
    interop::register_rpeer(option_env!("CARGO_BIN_EXE_rpeer"));
}

/// Fails the test, or reports it as skipped if a peer lacks a command.
fn run(name: &str, r: interop::Result<()>) {
    match r {
        Ok(()) => {}
        Err(e) if e.is_not_implemented() => interop::skipped(format_args!(
            "scenario {name}: {}",
            e.message().lines().next().unwrap_or_default()
        )),
        Err(e) => panic!("scenario {name} failed: {e}"),
    }
}

#[test]
fn formats_in_one_process() {
    register();
    run("formats_in_one_process", mixed::formats_in_one_process());
}
