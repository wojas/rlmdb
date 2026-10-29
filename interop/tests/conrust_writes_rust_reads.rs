//! Every interop scenario with the C peer linked against the Rust `liblmdb.a` (`lmdb-capi`) writing and
//! the Rust peer (`rpeer` on `lmdb-core`) reading (and, in the bidirectional scenarios,
//! writing too).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(interop::Peer::COnRust, interop::Peer::Rust);
