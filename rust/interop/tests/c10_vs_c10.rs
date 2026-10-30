//! Every interop scenario on LMDB 1.0 environments with the C peer on the C library of LMDB 1.0 (`cpeer10`) on both
//! sides (two processes of the same implementation sharing one environment).
//!
//! See `src/scenarios` for the scenarios and README.md for the matrix; steps
//! a peer cannot do yet print `[interop] SKIPPED` lines.

interop::scenario_tests!(interop::Peer::C10, interop::Peer::C10, interop::Format::V10);
