//! Every interop scenario on LMDB 1.0 environments with the C peer on the C library of LMDB 1.0 (`cpeer10`) writing
//! and the C peer linked against the Rust `liblmdb.a` (`rlmdb-capi`) reading (and, in the bidirectional
//! scenarios, writing too).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(
    interop::Peer::C10,
    interop::Peer::COnRust,
    interop::Format::V10
);
