//! Every interop scenario with the Rust peer (`rpeer` on `lmdb-core`) writing and
//! the C peer (`cpeer` on the C library) reading (and, in the bidirectional scenarios,
//! writing too).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(interop::Peer::Rust, interop::Peer::C);
