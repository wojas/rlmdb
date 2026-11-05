//! The scope of a test run: everything (the default, and what
//! `scripts/check-all.sh`, the Linux x86-64 CI and the nightly runs do), or
//! the platform suite of the other CI platforms (`$RLMDB_TEST_SCOPE` set to
//! `platform`).
//!
//! The platform suite keeps what depends on the platform (the reader table,
//! the lock file, writer locks, concurrency, the file I/O paths, the
//! differential tests at the OS page size, the C runtime) and leaves out
//! what only repeats on another platform what Linux x86-64 checks: the
//! generic interop scenarios beyond a smoke set
//! ([`scenarios::PLATFORM_SCENARIOS`](crate::scenarios::PLATFORM_SCENARIOS)),
//! every scenario at a page size other than the OS's (`pagesize10.rs`), the
//! differential tests of the other page sizes, the mutation and model
//! tests. A test left out passes with a `[interop] SKIPPED` note, so the
//! test counts stay the same in both scopes.

use std::fmt;
use std::sync::OnceLock;

/// The variable that selects the scope: `platform` for the platform
/// suite; unset (or anything else) for everything.
pub const SCOPE_VAR: &str = "RLMDB_TEST_SCOPE";

/// Whether this run is the platform suite (`$RLMDB_TEST_SCOPE` is
/// `platform`).
pub fn platform() -> bool {
    std::env::var_os(SCOPE_VAR).is_some_and(|v| v == "platform")
}

/// For a test of the full scope only: `true`, after a SKIPPED note naming
/// `what`, in the platform suite (the test then returns); `false`
/// otherwise.
pub fn full_scope_only(what: impl fmt::Display) -> bool {
    if platform() {
        crate::skipped(format_args!(
            "{what}: full scope only (${SCOPE_VAR}=platform)"
        ));
        return true;
    }
    false
}

/// The OS page size, which LMDB 0.9 environments and LMDB 1.0
/// environments without a page size of their own use: the page size of a
/// new environment that rlmdb creates without one.
pub fn os_page_size() -> usize {
    static PS: OnceLock<usize> = OnceLock::new();
    *PS.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temporary directory");
        rlmdb::Environment::builder()
            .open(dir.path())
            .expect("a new environment")
            .page_size()
    })
}

/// The page sizes of `sizes` that this run tests: all of them, in the
/// platform suite only the OS page size (if `sizes` has it; a SKIPPED note
/// names `what` and the sizes left out).
pub fn page_sizes(what: &str, sizes: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let sizes: Vec<usize> = sizes.into_iter().collect();
    if !platform() {
        return sizes;
    }
    let os = os_page_size();
    let (kept, left): (Vec<usize>, Vec<usize>) = sizes.into_iter().partition(|&ps| ps == os);
    if !left.is_empty() {
        crate::skipped(format_args!(
            "{what}: page sizes {left:?}: full scope only (${SCOPE_VAR}=platform)"
        ));
    }
    kept
}

/// Whether this run tests pages of `ps` bytes: always, in the platform
/// suite only at the OS page size (else after a SKIPPED note naming
/// `what`).
pub fn at_page_size(what: &str, ps: usize) -> bool {
    if !platform() || ps == os_page_size() {
        return true;
    }
    crate::skipped(format_args!(
        "{what}: page size {ps}: full scope only (${SCOPE_VAR}=platform)"
    ));
    false
}
