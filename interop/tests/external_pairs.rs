//! Every interop scenario on LMDB 0.9 environments with an external peer
//! ([`interop::Peer::External`]) in a pair: it writes and C reads, C writes
//! and it reads, it and Rust both ways, and it on both sides. C (`cpeer`)
//! verifies and audits as the reference, as in every 0.9 pair.
//!
//! Skipped (`[interop] SKIPPED`) unless `INTEROP_PEER` names the peer's
//! executable (README.md, "External peers"). Test names are
//! `<pair>::<scenario>`, e.g.
//! `external_writes_c_reads::concurrent_writers_readers`.

mod external_writes_c_reads {
    interop::scenario_tests!(interop::Peer::External, interop::Peer::C);
}

mod c_writes_external_reads {
    interop::scenario_tests!(interop::Peer::C, interop::Peer::External);
}

mod external_writes_rust_reads {
    interop::scenario_tests!(interop::Peer::External, interop::Peer::Rust);
}

mod rust_writes_external_reads {
    interop::scenario_tests!(interop::Peer::Rust, interop::Peer::External);
}

mod external_vs_external {
    interop::scenario_tests!(interop::Peer::External, interop::Peer::External);
}
