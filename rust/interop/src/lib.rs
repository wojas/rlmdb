//! Cross-process compatibility test bed for LMDB.
//!
//! The goal of the Rust rewrite is that a C LMDB process and a Rust LMDB
//! process can open the same environment *at the same time*: share the lock
//! file (reader table, writer lock) and read and write the same data file.
//! This crate proves it by running *peer programs* as separate processes
//! against one environment and checking what each of them sees.
//!
//! * A **peer** is a program that speaks the command protocol documented in
//!   `README.md` (`fill`, `verify`, `writer-loop`, `check-freelist`, …).
//!   [`Peer::C`] is `cpeer`, written in C against `liblmdb` (built from
//!   `../../libraries/liblmdb` by `build.rs`); [`Peer::Rust`] is `rpeer`
//!   (`src/bin/rpeer`), built on the `lmdb-core` crate; [`Peer::COnRust`]
//!   is the same C program linked against the Rust `liblmdb.a` of
//!   `lmdb-capi`. Commands a peer does not implement yet exit with status
//!   4; [`Peer::supports`] tells, and scenarios skip or substitute the C
//!   peer for them.
//! * [`TestEnv`] is an environment in a temporary directory.
//! * [`scenarios`] holds every test scenario, parameterised by a
//!   [`scenarios::Pair`] of (writer, reader) peers. [`scenario_tests!`]
//!   instantiates all of them as `#[test]`s for one pair:
//!
//!   ```ignore
//!   interop::scenario_tests!(interop::Peer::C, interop::Peer::Rust);
//!   ```
//!
//! * [`tools`] runs the upstream C tools (`mdb_stat`, `mdb_dump`, `mdb_load`,
//!   `mdb_copy`, `mtest*`) and parses their output; [`report`] parses the
//!   output of peer commands.
//! * [`bins`] has the paths of all the C programs built by `build.rs`.
//! * [`pagecmp`] compares two data files structurally (same trees on the
//!   same pages).
#![warn(missing_docs, clippy::all)]
#![forbid(unsafe_code)]

pub mod bins;
mod error;
pub mod pagecmp;
mod peer;
pub mod report;
pub mod scenarios;
mod testenv;
pub mod tools;

pub use error::{Error, Result};
pub use peer::{register_rpeer, skipped, Output, Peer, PeerCmd, PeerProcess, EXIT_NOT_IMPLEMENTED};
pub use testenv::TestEnv;
