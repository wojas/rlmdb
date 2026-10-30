//! Every interop scenario on LMDB 1.0 environments with the Rust peer (`rpeer` on `rlmdb`) writing
//! and the C peer on the C library of LMDB 1.0 (`cpeer10`) reading (and, in the bidirectional
//! scenarios, writing too).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(
    interop::Peer::Rust,
    interop::Peer::C10,
    interop::Format::V10
);
