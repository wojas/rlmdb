//! Builds the C side of the interop test bed.
//!
//! * `liblmdb` objects from `../../libraries/liblmdb/{mdb,midl}.c`, compiled
//!   like the upstream `Makefile` does (`-pthread -O2 -g -W -Wall
//!   -Wno-unused-parameter`, platform-default lock mode, no `MDB_DEBUG`), so
//!   the data and lock files are the ones a normal `make` build produces.
//! * `cpeer`: the C implementation of the peer command protocol (`c/cpeer*.c`).
//! * The cpeer objects once more, compiled against the Rust library's
//!   `lmdb.h` (`../lmdb-capi/include`): at test time they are linked with
//!   the Rust `liblmdb.a` into `cpeer_capi`, the third peer
//!   (`Peer::COnRust`, see `src/bins.rs`). That link cannot happen here:
//!   the build script runs before cargo builds `liblmdb.a`.
//! * `layout_dump`: prints `sizeof`/`offsetof` of the internal structs; it
//!   `#include`s `mdb.c` to see the static definitions.
//! * The upstream tools (`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`) and
//!   the `mtest*` programs.
//!
//! Executables go to `$OUT_DIR/bin/` and their paths are exported to the crate
//! as `INTEROP_<NAME>` compile-time environment variables (see `src/bins.rs`).
//! The recipe for linking `cpeer_capi` (compiler, flags, objects) goes to
//! `$OUT_DIR/cpeer_capi.link`, exported as `INTEROP_CPEER_CAPI_LINK`.
//! Nothing is linked into the Rust crate itself: the C programs run as
//! separate processes, which is the point of the exercise.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Upstream programs built from `libraries/liblmdb/<name>.c`.
///
/// Not built: `mdb_drop` (not part of LMDB 0.9; the tool only exists on the
/// 1.0 branch) and `mtest6` (it calls `mdb_dkey()`, which exists only in
/// `MDB_DEBUG` builds; the upstream Makefile leaves it out of `PROGS` too).
const UPSTREAM_PROGRAMS: &[&str] = &[
    "mdb_stat", "mdb_dump", "mdb_load", "mdb_copy", "mtest", "mtest2", "mtest3", "mtest4", "mtest5",
];

/// Sources of the C peer, in `c/`.
const CPEER_SOURCES: &[&str] = &[
    "cpeer.c",
    "cpeer_data.c",
    "cpeer_loops.c",
    "cpeer_misc.c",
    "cpeer_pagewalk.c",
    "dataset.c",
];

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let lmdb_dir = manifest_dir.join("../../libraries/liblmdb");
    let lmdb_dir = lmdb_dir.canonicalize().unwrap_or(lmdb_dir);
    let peer_dir = manifest_dir.join("c");
    let bin_dir = out_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");

    for f in ["mdb.c", "midl.c", "lmdb.h", "midl.h"] {
        println!("cargo:rerun-if-changed={}", lmdb_dir.join(f).display());
    }
    for f in UPSTREAM_PROGRAMS {
        println!(
            "cargo:rerun-if-changed={}",
            lmdb_dir.join(format!("{f}.c")).display()
        );
    }
    println!("cargo:rerun-if-changed={}", peer_dir.display());

    // The library: mdb.o + midl.o, the equivalent of liblmdb.a.
    let midl_obj = compile(&out_dir, "midl", &lmdb_dir, &[lmdb_dir.join("midl.c")]);
    let mdb_obj = compile(&out_dir, "mdb", &lmdb_dir, &[lmdb_dir.join("mdb.c")]);
    let lib_objs: Vec<PathBuf> = mdb_obj.iter().chain(midl_obj.iter()).cloned().collect();

    let mut exported = Vec::new();

    for prog in UPSTREAM_PROGRAMS {
        let objs = compile(
            &out_dir,
            prog,
            &lmdb_dir,
            &[lmdb_dir.join(format!("{prog}.c"))],
        );
        exported.push((*prog, link(&bin_dir, prog, &objs, &lib_objs)));
    }

    let peer_srcs: Vec<PathBuf> = CPEER_SOURCES.iter().map(|f| peer_dir.join(f)).collect();
    let objs = compile(&out_dir, "cpeer", &lmdb_dir, &peer_srcs);
    exported.push(("cpeer", link(&bin_dir, "cpeer", &objs, &lib_objs)));

    // cpeer against the Rust library's header, linked at test time.
    let capi_include = manifest_dir.join("../lmdb-capi/include");
    let capi_include = capi_include.canonicalize().unwrap_or(capi_include);
    println!(
        "cargo:rerun-if-changed={}",
        capi_include.join("lmdb.h").display()
    );
    let capi_objs = compile(&out_dir, "cpeer_capi", &capi_include, &peer_srcs);
    let recipe = out_dir.join("cpeer_capi.link");
    write_link_recipe(&recipe, &bin_dir.join("cpeer_capi"), &capi_objs);
    println!(
        "cargo:rustc-env=INTEROP_CPEER_CAPI_LINK={}",
        recipe.display()
    );

    // layout_dump includes mdb.c itself, so it only needs midl.o.
    let objs = compile(
        &out_dir,
        "layout_dump",
        &lmdb_dir,
        &[peer_dir.join("layout_dump.c")],
    );
    exported.push((
        "layout_dump",
        link(&bin_dir, "layout_dump", &objs, &midl_obj),
    ));

    for (name, path) in exported {
        println!(
            "cargo:rustc-env=INTEROP_{}={}",
            name.to_uppercase(),
            path.display()
        );
    }
    println!("cargo:rustc-env=INTEROP_BIN_DIR={}", bin_dir.display());
    println!("cargo:rustc-env=INTEROP_LMDB_SRC={}", lmdb_dir.display());
}

/// A `cc::Build` with the upstream Makefile's flags.
fn c_build(lmdb_dir: &Path) -> cc::Build {
    let mut b = cc::Build::new();
    b.include(lmdb_dir)
        .opt_level(2)
        .debug(true)
        // We pick the warning flags ourselves (the Makefile's `W`), instead of
        // cc's default `-Wall -Wextra`.
        .warnings(false)
        .extra_warnings(false)
        .flag("-pthread")
        .flag("-W")
        .flag("-Wall")
        .flag("-Wno-unused-parameter")
        .flag_if_supported("-Wbad-function-cast")
        .flag_if_supported("-Wuninitialized")
        .cargo_metadata(false);
    b
}

/// Compiles `srcs` into object files under `$OUT_DIR/obj/<tag>`.
fn compile(out_dir: &Path, tag: &str, lmdb_dir: &Path, srcs: &[PathBuf]) -> Vec<PathBuf> {
    let obj_dir = out_dir.join("obj").join(tag);
    std::fs::create_dir_all(&obj_dir).expect("create obj dir");
    let mut b = c_build(lmdb_dir);
    b.out_dir(&obj_dir).files(srcs);
    b.compile_intermediates()
}

/// Writes the recipe for linking `cpeer_capi` at test time, one item per
/// line: `out <executable>`, `cc <compiler>`, `arg <compiler argument>`
/// (the flags `cc` would use, e.g. `-arch`), `obj <object file>`.
fn write_link_recipe(recipe: &Path, exe: &Path, objs: &[PathBuf]) {
    let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
    let mut text = format!("out {}\ncc {}\n", exe.display(), compiler.path().display());
    for a in compiler.args() {
        text.push_str(&format!("arg {}\n", a.to_string_lossy()));
    }
    for o in objs {
        text.push_str(&format!("obj {}\n", o.display()));
    }
    std::fs::write(recipe, text).expect("write link recipe");
}

/// Links `objs` + `libs` into the executable `bin_dir/name` with the C compiler.
fn link(bin_dir: &Path, name: &str, objs: &[PathBuf], libs: &[PathBuf]) -> PathBuf {
    let exe = bin_dir.join(name);
    let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
    let mut cmd: Command = compiler.to_command();
    cmd.args(objs)
        .args(libs)
        .arg("-pthread")
        .arg("-o")
        .arg(&exe);
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("running linker for {name}: {e}"));
    assert!(status.success(), "linking {name} failed: {cmd:?}");
    exe
}
