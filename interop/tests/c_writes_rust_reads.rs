//! Every interop scenario with the C peer writing and the Rust peer
//! (`rpeer`, on `lmdb-core`) reading: verifying, dumping, holding
//! snapshots, running reader loops, auditing page usage and listing and
//! checking reader slots, concurrently with C writers on the same
//! environment (and, in the bidirectional scenarios, writing too).
//!
//! Steps a peer cannot do yet run on the C peer instead, or the scenario
//! is skipped, with an `[interop] SKIPPED` line on stderr.

interop::scenario_tests!(interop::Peer::C, interop::Peer::Rust);
