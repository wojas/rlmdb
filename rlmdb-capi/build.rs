//! Link options of the shared library (`liblmdb.so`).
//!
//! `dlclose` must not unload it: the library keeps process-wide state (the
//! registry of open environments, the strings `mdb_strerror` and
//! `rlmdb_version` return, which are built on first use and promised to be
//! static), and a program may close one copy of a binding while another
//! still uses LMDB. On Linux the library is linked with `-z nodelete`
//! (`DF_1_NODELETE`, as if loaded with `RTLD_NODELETE`): `dlclose` keeps
//! it, and a later `dlopen` gets the same, already initialised copy. The
//! BSDs, also ELF, get the same flag (untested there). macOS needs no
//! flag: dyld never unloads an image with thread-local variables, and
//! Rust's standard library has them. `tests/capi_dlopen.rs` checks both.
//! Windows needs no flag either: `FreeLibrary` unloads `lmdb.dll` with its
//! last reference, as it does a C `lmdb.dll`, and a later `LoadLibrary`
//! initialises a fresh copy.

fn main() {
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux" | "freebsd" | "openbsd" | "netbsd" | "dragonfly")
    ) {
        println!("cargo:rustc-cdylib-link-arg=-Wl,-z,nodelete");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
