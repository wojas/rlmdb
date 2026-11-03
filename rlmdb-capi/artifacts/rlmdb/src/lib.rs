//! `librlmdb`: the C library of rlmdb (`librlmdb.a`, `librlmdb.so`,
//! `librlmdb.dylib`; `rlmdb.dll` on Windows), which programs link with
//! `-lrlmdb`. It exports every function of LMDB 0.9.37's and LMDB 1.0.3's
//! `lmdb.h` and of `rlmdb.h`, from `rlmdb-capi`; `mdb_version` reports
//! rlmdb's own version and new environments are LMDB 0.9 unless the
//! program asks for 1.0 (`rlmdb_capi::Profile::Rlmdb`).

#![deny(missing_docs)]

rlmdb_capi::artifact_symbols!(rlmdb_capi::Profile::Rlmdb);
