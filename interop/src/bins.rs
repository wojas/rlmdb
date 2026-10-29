//! Paths of the C programs that `build.rs` compiles into `$OUT_DIR/bin`.
//!
//! They are plain C executables linked against `liblmdb` built from
//! `../../libraries/liblmdb` with the upstream Makefile's flags, and are run as
//! subprocesses.

use std::path::Path;

/// The C peer (`c/cpeer*.c`), implementing the command protocol of `README.md`.
pub fn cpeer() -> &'static Path {
    Path::new(env!("INTEROP_CPEER"))
}

/// `c/layout_dump.c`: prints sizes and offsets of LMDB's internal structs.
pub fn layout_dump() -> &'static Path {
    Path::new(env!("INTEROP_LAYOUT_DUMP"))
}

/// The upstream `mdb_stat` tool.
pub fn mdb_stat() -> &'static Path {
    Path::new(env!("INTEROP_MDB_STAT"))
}

/// The upstream `mdb_dump` tool.
pub fn mdb_dump() -> &'static Path {
    Path::new(env!("INTEROP_MDB_DUMP"))
}

/// The upstream `mdb_load` tool.
pub fn mdb_load() -> &'static Path {
    Path::new(env!("INTEROP_MDB_LOAD"))
}

/// The upstream `mdb_copy` tool.
pub fn mdb_copy() -> &'static Path {
    Path::new(env!("INTEROP_MDB_COPY"))
}

/// The upstream test programs `mtest`, `mtest2` … `mtest5`, by name.
///
/// (`mtest6` needs an `MDB_DEBUG` build and is not built, as upstream.)
pub fn mtest(name: &str) -> Option<&'static Path> {
    let p = match name {
        "mtest" => env!("INTEROP_MTEST"),
        "mtest2" => env!("INTEROP_MTEST2"),
        "mtest3" => env!("INTEROP_MTEST3"),
        "mtest4" => env!("INTEROP_MTEST4"),
        "mtest5" => env!("INTEROP_MTEST5"),
        _ => return None,
    };
    Some(Path::new(p))
}

/// Names of the upstream test programs that [`mtest`] knows.
pub const MTESTS: &[&str] = &["mtest", "mtest2", "mtest3", "mtest4", "mtest5"];

/// The directory holding all of the above.
pub fn bin_dir() -> &'static Path {
    Path::new(env!("INTEROP_BIN_DIR"))
}

/// The C library sources (`libraries/liblmdb`).
pub fn lmdb_src() -> &'static Path {
    Path::new(env!("INTEROP_LMDB_SRC"))
}
