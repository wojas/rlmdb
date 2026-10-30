//! Every interop scenario on LMDB 1.0 environments with the C peer linked against the Rust `liblmdb.a` (`rlmdb-capi`) writing
//! and the Rust peer (`rpeer` on `rlmdb`) reading (and, in the bidirectional
//! scenarios, writing too).
//!
//! C 1.0 (`cpeer10`) verifies and audits as the reference, as C does in the
//! 0.9 matrix.
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(
    interop::Peer::COnRust,
    interop::Peer::Rust,
    interop::Format::V10
);
