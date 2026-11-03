//! The stub that stands in for C LMDB 1.0: built as `liblmdb10.*` with
//! SONAME `liblmdb.so` (`RLMDB_LMDB10_SONAME` at build time changes it),
//! installed under that name in a directory of its own
//! (`scripts/install-libs.sh`), for programs built against C LMDB 1.0. It
//! exports what `librlmdb` exports; `mdb_version` reports 1.0.85 and new
//! environments are LMDB 1.0 unless the program or `RLMDB_DEFAULT_FORMAT`
//! asks for 0.9 (`rlmdb_capi::Profile::Lmdb10`).

#![deny(missing_docs)]

rlmdb_capi::artifact_symbols!(rlmdb_capi::Profile::Lmdb10);
