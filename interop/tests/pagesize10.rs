//! Every interop scenario on LMDB 1.0 environments of other page sizes than
//! the OS page size the other pair files of 1.0 use: 256 bytes (the
//! smallest 1.0 allows: three or four nodes per page, a main DB of two
//! levels for a few named DBs), 1 KiB (what C 1.0's own `mtest` uses: deep
//! trees, many overflow pages, sub-pages that become sub-DBs early) and 64
//! KiB (the largest: offsets near the 16-bit limit of `mp_lower` and
//! `mp_upper`, values of 20 KB inline), with C 1.0 (`cpeer10`) and the Rust
//! peer (`rpeer`) in both roles. And 16 KiB, the OS page size of macOS,
//! where the CI runs only the platform suite (`interop::scope`): the full
//! set of scenarios at that size runs here, on Linux.
//!
//! Below 4 KiB the scenarios scale their item counts down with the page
//! size (`Pair::scaled`): they write about as many pages as at 4 KiB, not
//! 4 or 16 times as many, in trees at least as deep, with the same
//! sub-DBs, sub-pages and overflow values and as many transactions.
//!
//! Test names are `<pair>::<scenario>`, e.g.
//! `c10_writes_rust_reads_1k::concurrent_writers_readers`. In the platform
//! suite (`$RLMDB_TEST_SCOPE=platform`) every test here is SKIPPED.

mod c10_writes_rust_reads_256 {
    interop::scenario_tests!(
        interop::Peer::C10,
        interop::Peer::Rust,
        interop::Format::V10,
        256
    );
}

mod rust_writes_c10_reads_256 {
    interop::scenario_tests!(
        interop::Peer::Rust,
        interop::Peer::C10,
        interop::Format::V10,
        256
    );
}

mod c10_writes_rust_reads_1k {
    interop::scenario_tests!(
        interop::Peer::C10,
        interop::Peer::Rust,
        interop::Format::V10,
        1024
    );
}

mod rust_writes_c10_reads_1k {
    interop::scenario_tests!(
        interop::Peer::Rust,
        interop::Peer::C10,
        interop::Format::V10,
        1024
    );
}

mod c10_writes_rust_reads_16k {
    interop::scenario_tests!(
        interop::Peer::C10,
        interop::Peer::Rust,
        interop::Format::V10,
        16384
    );
}

mod rust_writes_c10_reads_16k {
    interop::scenario_tests!(
        interop::Peer::Rust,
        interop::Peer::C10,
        interop::Format::V10,
        16384
    );
}

mod c10_writes_rust_reads_64k {
    interop::scenario_tests!(
        interop::Peer::C10,
        interop::Peer::Rust,
        interop::Format::V10,
        65536
    );
}

mod rust_writes_c10_reads_64k {
    interop::scenario_tests!(
        interop::Peer::Rust,
        interop::Peer::C10,
        interop::Format::V10,
        65536
    );
}
