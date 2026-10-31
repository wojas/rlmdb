//! `tests/c/capi_dlopen.c`: threads that each load the library with
//! `dlopen` and open one environment `MDB_RDONLY | MDB_NOLOCK` with their
//! own handle, all at once (several embedded interpreters in one process),
//! then `dlclose`; then the library is loaded and unloaded again. The same
//! program runs against the C library and the Rust one (shared builds);
//! both must report no failures and print the same trace.
//!
//! It also reports whether `dlclose` unloaded the library (`U` lines): the
//! C library is unloaded, the Rust one stays loaded (see `build.rs`).

mod common;

use std::path::Path;
use std::process::Command;

use common::*;

fn run_capi_dlopen(lib: &Path, env: &Path) -> String {
    let exe = builder().dl_program("capi_dlopen");
    let out = Command::new(&exe).arg(lib).arg(env).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    eprintln!("--- capi_dlopen ({}) ---\n{stdout}{stderr}", lib.display());
    assert!(
        out.status.success() && stderr.is_empty(),
        "capi_dlopen with {} failed ({})",
        lib.display(),
        out.status
    );
    stdout
}

/// The lines starting with `tag`.
fn lines<'a>(out: &'a str, tag: &str) -> Vec<&'a str> {
    out.lines().filter(|l| l.starts_with(tag)).collect()
}

#[test]
fn threads_dlopen_the_library_and_open_one_environment() {
    let b = builder();
    let env = b.scratch("dlopen-env");
    make_fixture(&env);
    let c = run_capi_dlopen(&b.c_shared_lib(), &env);
    let rust = run_capi_dlopen(&rust_cdylib(), &env);
    assert_eq!(lines(&c, "T "), lines(&rust, "T "));
    assert!(lines(&rust, "T ").len() > 2 * 4);
    // C's library is unloaded by its last dlclose; ours never is (`-z
    // nodelete` on Linux, a dyld rule on macOS; see build.rs), so later
    // dlopens get the loaded copy with its process-wide state.
    let unloads = lines(&rust, "U ");
    assert_eq!(unloads.len(), 5);
    assert!(
        unloads.iter().all(|l| l.starts_with("U loaded ")),
        "{unloads:?}"
    );
}
