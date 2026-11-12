// Link options of the three shared libraries, included by the build
// script of each artifact package (`include!("../link.rs")`).
//
// * The library's name for the dynamic linker: the SONAME on ELF systems
//   (`-Wl,-soname`), the install name on macOS (`@rpath/<file>`, which
//   `scripts/install-libs.sh` replaces with the installed path). Without
//   them a cdylib records its build path (macOS) or no name at all (ELF).
//   Windows needs none: a DLL is found by its file name.
// * `dlclose` must not unload the library: it keeps process-wide state
//   (the registry of open environments, the strings `mdb_strerror` and
//   `rlmdb_version` return, which are built on first use and promised to
//   be static), and a program may close one copy of a binding while
//   another still uses LMDB. On Linux the library is linked with `-z
//   nodelete` (`DF_1_NODELETE`, as if loaded with `RTLD_NODELETE`):
//   `dlclose` keeps it, and a later `dlopen` gets the same, already
//   initialised copy. Android, the BSDs, illumos and Solaris, also ELF,
//   get the same flag (Android's and illumos' linkers honour it; untested
//   on the BSDs and Solaris). macOS needs no flag: dyld never unloads an image with
//   thread-local variables, and Rust's standard library has them.
//   `rlmdb-capi/tests/capi_dlopen.rs` checks both. Windows needs no flag
//   either: `FreeLibrary` unloads the DLL with its last reference, as it
//   does a C `lmdb.dll`, and a later `LoadLibrary` initialises a fresh
//   copy.

/// Prints the link options of the shared library whose ELF SONAME is
/// `soname` and whose file on macOS is `dylib`.
fn link_options(soname: &str, dylib: &str) {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match os.as_str() {
        "linux" | "android" | "freebsd" | "openbsd" | "netbsd" | "dragonfly" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-soname,{soname}");
            println!("cargo:rustc-cdylib-link-arg=-Wl,-z,nodelete");
        }
        "macos" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-install_name,@rpath/{dylib}");
            // Room in the Mach-O header for the installed path, which
            // `install_name_tool -id` writes over the short `@rpath` one:
            // without it, a long prefix fails with "larger updated load
            // commands do not fit".
            println!("cargo:rustc-cdylib-link-arg=-Wl,-headerpad_max_install_names");
        }
        // The illumos and Solaris link editor takes the SONAME with -h.
        "illumos" | "solaris" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-h,{soname}");
            println!("cargo:rustc-cdylib-link-arg=-Wl,-z,nodelete");
        }
        _ => {}
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../link.rs");
}

/// `true` if `name` can be a library file name in a linker argument: not
/// empty, no path separator, nothing a shell or `-Wl,` would split on.
#[allow(dead_code)]
fn valid_soname(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}
