//! The stub that stands in for C LMDB 0.9: built as `liblmdb09.*` (cargo
//! cannot name two libraries of one workspace `liblmdb`) with SONAME
//! `liblmdb.so.0`, installed as `liblmdb.so.0` in a directory of its own
//! (`scripts/install-libs.sh`), for programs built against C LMDB 0.9
//! (`LD_PRELOAD`, `LD_LIBRARY_PATH`). It exports what `librlmdb` exports;
//! `mdb_version` reports 0.9.85 and new environments are LMDB 0.9 unless
//! the program asks for 1.0 (`rlmdb_capi::Profile::Lmdb09`).

#![deny(missing_docs)]

rlmdb_capi::artifact_symbols!(rlmdb_capi::Profile::Lmdb09);
