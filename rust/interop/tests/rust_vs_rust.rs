//! Every interop scenario with the Rust peer (`rpeer` on `rlmdb`) on both sides
//! (two processes of the same implementation sharing one environment).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(interop::Peer::Rust, interop::Peer::Rust);
