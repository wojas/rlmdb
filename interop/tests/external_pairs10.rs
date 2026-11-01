//! Every interop scenario on LMDB 1.0 environments with an external peer
//! ([`interop::Peer::External`]) in a pair: with C 1.0 (`cpeer10`), Rust
//! (`rpeer`) and C on rlmdb (`cpeer_capi`) both ways, and it on both
//! sides. C 1.0 verifies and audits as the reference.
//!
//! Skipped (`[interop] SKIPPED`) unless `INTEROP_PEER` names a peer that
//! opens 1.0 environments (README.md, "External peers"). Test names are
//! `<pair>::<scenario>`, e.g. `external_writes_c10_reads::mvcc_hold_reader`.

mod external_writes_c10_reads {
    interop::scenario_tests!(
        interop::Peer::External,
        interop::Peer::C10,
        interop::Format::V10
    );
}

mod c10_writes_external_reads {
    interop::scenario_tests!(
        interop::Peer::C10,
        interop::Peer::External,
        interop::Format::V10
    );
}

mod external_writes_rust_reads {
    interop::scenario_tests!(
        interop::Peer::External,
        interop::Peer::Rust,
        interop::Format::V10
    );
}

mod rust_writes_external_reads {
    interop::scenario_tests!(
        interop::Peer::Rust,
        interop::Peer::External,
        interop::Format::V10
    );
}

mod external_writes_conrust_reads {
    interop::scenario_tests!(
        interop::Peer::External,
        interop::Peer::COnRust,
        interop::Format::V10
    );
}

mod conrust_writes_external_reads {
    interop::scenario_tests!(
        interop::Peer::COnRust,
        interop::Peer::External,
        interop::Format::V10
    );
}

mod external_vs_external {
    interop::scenario_tests!(
        interop::Peer::External,
        interop::Peer::External,
        interop::Format::V10
    );
}
