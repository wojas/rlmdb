//! Rust reimplementations of the LMDB 0.9 command line tools, built on
//! [`lmdb_core`].
//!
//! Each tool lives in its own module with a `main(args) -> exit code`
//! function; the binaries in `src/bin/` only forward `std::env::args_os()`
//! to it:
//!
//! * [`stat`]: `mdb_stat` (`libraries/liblmdb/mdb_stat.c`),
//! * [`dump`]: `mdb_dump` (`mdb_dump.c`),
//! * [`load`]: `mdb_load` (`mdb_load.c`),
//! * [`copy`]: `mdb_copy` (`mdb_copy.c`),
//! * [`drop`](mod@drop): `mdb_drop`, which 0.9.36 does not have; modelled
//!   on LMDB 1.0's `mdb_drop.c`.
//!
//! The tools behave like the C ones byte for byte: same options (parsed by
//! [`getopt`], which reproduces the platform's `getopt(3)`, including GNU
//! argument permutation on Linux), same output, same messages on standard
//! error and same exit codes. Arguments, database names and paths are
//! handled as bytes, as C does. The deliberate differences (C bugs that are
//! not reproduced) are listed in `README.md`.
//!
//! Shared helpers: [`cfmt`] (C formatting and parsing: `%p`, `isprint`,
//! `atoi`, `sscanf` numbers, `mdb_strerror`), [`output`] (buffered standard
//! output that remembers write errors), [`dumpfmt`] (the encodings of the
//! dump format) and [`common`] (version string, usage and error messages,
//! opening databases by byte name).

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all)]

pub mod cfmt;
pub mod common;
pub mod copy;
pub mod drop;
pub mod dump;
pub mod dumpfmt;
pub mod getopt;
pub mod load;
pub mod output;
pub mod stat;
