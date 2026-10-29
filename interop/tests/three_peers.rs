//! Scenarios with all three peers at once: C, Rust, and C on the Rust
//! library (`interop::scenarios::all_peers`).

use interop::scenarios::all_peers;

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
fn three_writers() {
    register();
    run("three_writers", all_peers::three_writers());
}
