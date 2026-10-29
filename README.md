# LMDB in Rust

A rewrite of LMDB 0.9 in Rust that stays binary compatible with the C
implementation in `../libraries/liblmdb`: the data file and lock file formats
are identical, so C and Rust processes can share one environment concurrently.

See `DESIGN.md` for the architecture, the format specification, and the plan.

Crates:

* `lmdb-core` – the library with a safe Rust API.
* `lmdb-capi` – `liblmdb` C ABI on top of it, drop-in for C programs.
* `lmdb-tools` – `mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`, `mdb_drop`.
* `interop` – cross-process compatibility tests against the C build.
