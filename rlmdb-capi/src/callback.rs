//! Calling the program's callbacks: comparison functions (through the
//! trampolines of `cmp`, and directly in `mdb_cmp` / `mdb_dcmp`) and the
//! message function of `mdb_reader_list`.
//!
//! The callback types are `extern "C-unwind"` (see [`crate::types`]), so a
//! callback may unwind: a C++ comparison function that throws, or a Rust
//! one that panics. C LMDB lets that unwind through its own frames, with
//! locks held and state half-changed. Here the unwind stops where the
//! library called the callback, and the process aborts there, before any
//! frame of the library or the program is unwound: [`call`].
//!
//! The `extern "C"` functions the program called would abort too, but not
//! always there and not always the same way: their abort-on-unwind guard
//! is a cleanup that runs only in the unwinder's second phase, after the
//! search phase found a handler further up. A caller whose frames say the
//! call cannot unwind (a Rust caller built with optimisations: calls of
//! `extern "C"` functions are `nounwind`) ends the search first, and the
//! process then dies however the platform's unwinder ends a failed search:
//! std's "failed to initiate panic" on Unix, the C runtime's `abort()`
//! (exit code 3) inside libgcc's SEH handler with MinGW-w64.

use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Calls `f`, which calls one of the program's callbacks (`what` names
/// it). If the callback unwinds, the process aborts here: after a Rust
/// panic (whose message came first) with `rlmdb: <what> unwound into the
/// library; aborting` on stderr and `std::process::abort` (`SIGABRT`; on
/// Windows `STATUS_STACK_BUFFER_OVERRUN`, `0xC0000409`, with either
/// toolchain); for a foreign exception (C++) std aborts (`Rust cannot
/// catch foreign exceptions`).
pub(crate) fn call<R>(what: &str, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => unwound(what),
    }
}

/// Aborts the process for [`call`].
#[cold]
fn unwound(what: &str) -> ! {
    let _ = writeln!(
        std::io::stderr().lock(),
        "rlmdb: {what} unwound into the library; aborting"
    );
    std::process::abort()
}
