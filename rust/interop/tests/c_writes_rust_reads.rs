//! Every interop scenario with the C peer writing and the Rust peer
//! (`rpeer`, on the `lmdb` crate) reading: verifying, dumping, holding
//! snapshots, running reader loops, auditing page usage and listing and
//! checking reader slots, concurrently with C writers on the same
//! environment.
//!
//! Steps a scenario gives to the reader that need the write side (stage 3:
//! `copy`, `writer-loop`) run on the C peer instead, with an
//! `[interop] SKIPPED` line on stderr.

interop::scenario_tests!(interop::Peer::C, interop::Peer::Rust);
