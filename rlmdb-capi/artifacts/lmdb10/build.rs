//! Link options of the LMDB 1.0 stub (`liblmdb10.so`, installed under its
//! SONAME): SONAME `liblmdb.so` (upstream's Makefile sets none, and no
//! distribution ships C LMDB 1.0 yet), or the value of
//! `RLMDB_LMDB10_SONAME` at build time, for a system whose C LMDB 1.0
//! has another one (`liblmdb.so.1`, say); install name
//! `@rpath/liblmdb.dylib` (the variable is for ELF systems only);
//! `-z nodelete` (see `../link.rs`).

include!("../link.rs");

/// The variable that overrides the SONAME.
const SONAME_VAR: &str = "RLMDB_LMDB10_SONAME";

fn main() {
    println!("cargo:rerun-if-env-changed={SONAME_VAR}");
    let soname = std::env::var(SONAME_VAR).unwrap_or_else(|_| "liblmdb.so".to_owned());
    assert!(
        valid_soname(&soname),
        "{SONAME_VAR}={soname:?} is not a library file name \
         (letters, digits and . _ + - only)"
    );
    link_options(&soname, "liblmdb.dylib");
}
