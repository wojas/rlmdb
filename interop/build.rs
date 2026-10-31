//! Builds the C side of the interop test bed.
//!
//! * `liblmdb` objects from `../tests/lmdb-c/0.9/{mdb,midl}.c` (the vendored
//!   LMDB 0.9 sources, `tests/lmdb-c/README.md`), compiled
//!   like the upstream `Makefile` does (`-pthread -O2 -g -W -Wall
//!   -Wno-unused-parameter`, platform-default lock mode, no `MDB_DEBUG`), so
//!   the data and lock files are the ones a normal `make` build produces.
//! * `cpeer`: the C implementation of the peer command protocol (`c/cpeer*.c`).
//! * The cpeer objects once more, compiled against the Rust library's
//!   `lmdb.h` (`../rlmdb-capi/include`): at test time they are linked with
//!   the Rust `liblmdb.a` into `cpeer_capi`, the third peer
//!   (`Peer::COnRust`, see `src/bins.rs`). That link cannot happen here:
//!   the build script runs before cargo builds `liblmdb.a`.
//! * `layout_dump`: prints `sizeof`/`offsetof` of the internal structs; it
//!   `#include`s `mdb.c` to see the static definitions.
//! * The upstream tools (`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`) and
//!   the `mtest*` programs.
//! * `writeops`: the script runner of the write-path differential tests
//!   (`../rlmdb/tests/c/writeops.c`, see `rlmdb/tests/common/differential.rs`).
//! * The LMDB 1.0 oracle: from the vendored 1.0 sources in
//!   `../tests/lmdb-c/1.0` (or `$LMDB10_DIR`, see [`lmdb10_dir`]), the
//!   library, the tools, the `mtest*` programs, `layout_dump`, `cpeer` and
//!   `writeops` are built once more, with the same flags, under names ending
//!   in `10` (`mdb_stat10`, `mtest2_10`, see [`name10`]), and `writeops`
//!   also against a 1.0 library built with `-DNDEBUG` (`writeops10_ndebug`).
//!
//! Executables go to `$OUT_DIR/bin/` and their paths are exported to the crate
//! as `INTEROP_<NAME>` compile-time environment variables (see `src/bins.rs`).
//! The recipe for linking `cpeer_capi` (compiler, flags, objects) goes to
//! `$OUT_DIR/cpeer_capi.link`, exported as `INTEROP_CPEER_CAPI_LINK`.
//! Nothing is linked into the Rust crate itself: the C programs run as
//! separate processes, which is the point of the exercise.
//!
//! With `RLMDB_SKIP_C_BUILD=1` ([`SKIP_C_VAR`]) nothing is compiled or
//! linked and the exported paths name programs that do not exist: for
//! `cargo check` of a target without a C toolchain (CI checks the BSD
//! targets this way). The tests of such a build cannot run.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Upstream programs built from `tests/lmdb-c/0.9/<name>.c`.
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

/// Environment variable naming another directory with the LMDB 1.0 sources
/// (an absolute path, for example `libraries/liblmdb` of an upstream
/// checkout), used instead of `tests/lmdb-c/1.0` to test against another
/// 1.0 commit. The directory must hold [`LMDB10_LIB_FILES`] and the sources
/// of [`UPSTREAM10_PROGRAMS`].
const LMDB10_DIR_VAR: &str = "LMDB10_DIR";

/// Files of the LMDB 1.0 sources besides the programs: the library (1.0's
/// `liblmdb.a` also holds `module.o`, the loader of crypto modules) and
/// `chacha8.[ch]` for the encryption tests. With the sources of
/// [`UPSTREAM10_PROGRAMS`], this is the list of files that
/// `scripts/update-c-sources.sh` vendors into `tests/lmdb-c/1.0` from the
/// pinned `mdb.RE/1.0` commit (`tests/lmdb-c/README.md`).
const LMDB10_LIB_FILES: &[&str] = &[
    "mdb.c",
    "midl.c",
    "midl.h",
    "lmdb.h",
    "module.c",
    "chacha8.c",
    "chacha8.h",
];

/// Upstream programs built from the 1.0 sources: the five tools of 1.0's
/// `IPROGS`, its `mtest`…`mtest5`, and the `mtest_*` programs that need no
/// external module.
///
/// Not built: `mtest6` (needs `MDB_DEBUG`, as in 0.9), `mtest_enc2` and
/// `mtest_enc4` (they `mdb_modload()` the libsodium module `crypto.lm`,
/// which needs libsodium), and `mplay` (not a test, it replays recorded
/// operations from a log).
const UPSTREAM10_PROGRAMS: &[&str] = &[
    "mdb_stat",
    "mdb_dump",
    "mdb_load",
    "mdb_copy",
    "mdb_drop",
    "mtest",
    "mtest2",
    "mtest3",
    "mtest4",
    "mtest5",
    // `MDB_REMAP_CHUNKS`, available because `MDB_RPAGE_CACHE` defaults to 1.
    "mtest_remap",
    // `mdb_env_set_encrypt()` with chacha8 (`chacha8.c`, no module).
    "mtest_enc",
    "mtest_enc3",
];

/// Upstream 1.0 programs that link `chacha8.o` (the upstream Makefile lists
/// it for `mtest_enc`; `mtest_enc3` calls it too).
const CHACHA8_PROGRAMS: &[&str] = &["mtest_enc", "mtest_enc3"];

/// Environment variable that skips the C build (see the crate docs): set
/// and neither empty nor `0`.
const SKIP_C_VAR: &str = "RLMDB_SKIP_C_BUILD";

/// `true` if [`SKIP_C_VAR`] asks to skip the C build.
fn skip_c() -> bool {
    env::var_os(SKIP_C_VAR).is_some_and(|v| !v.is_empty() && v != "0")
}

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let lmdb_dir = manifest_dir.join("../tests/lmdb-c/0.9");
    let lmdb_dir = lmdb_dir.canonicalize().unwrap_or(lmdb_dir);
    let peer_dir = manifest_dir.join("c");
    let bin_dir = out_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    println!("cargo:rerun-if-env-changed={SKIP_C_VAR}");

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
    let writeops_src = manifest_dir.join("../rlmdb/tests/c/writeops.c");
    let writeops_src = writeops_src.canonicalize().unwrap_or(writeops_src);
    println!("cargo:rerun-if-changed={}", writeops_src.display());

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
        exported.push((prog.to_string(), link(&bin_dir, prog, &objs, &lib_objs)));
    }

    let peer_srcs: Vec<PathBuf> = CPEER_SOURCES.iter().map(|f| peer_dir.join(f)).collect();
    let objs = compile(&out_dir, "cpeer", &lmdb_dir, &peer_srcs);
    exported.push(("cpeer".into(), link(&bin_dir, "cpeer", &objs, &lib_objs)));

    // The write-path differential tests' script runner.
    let objs = compile(
        &out_dir,
        "writeops",
        &lmdb_dir,
        std::slice::from_ref(&writeops_src),
    );
    exported.push((
        "writeops".into(),
        link(&bin_dir, "writeops", &objs, &lib_objs),
    ));

    // cpeer against the Rust library's headers, linked at test time.
    // `LMDB_RUST` lets it use `rlmdb.h` (`--format`, both page layouts).
    let capi_include = manifest_dir.join("../rlmdb-capi/include");
    let capi_include = capi_include.canonicalize().unwrap_or(capi_include);
    for h in ["lmdb.h", "rlmdb.h"] {
        println!("cargo:rerun-if-changed={}", capi_include.join(h).display());
    }
    let capi_objs = compile_with(
        &out_dir,
        "cpeer_capi",
        &capi_include,
        &peer_srcs,
        &["LMDB_RUST"],
    );
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
        "layout_dump".into(),
        link(&bin_dir, "layout_dump", &objs, &midl_obj),
    ));

    let lmdb10_dir = lmdb10_dir(&manifest_dir);
    exported.extend(build_lmdb10(
        &out_dir,
        &lmdb10_dir,
        &peer_dir,
        &peer_srcs,
        &writeops_src,
        &bin_dir,
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
    println!(
        "cargo:rustc-env=INTEROP_LMDB10_SRC={}",
        lmdb10_dir.display()
    );
}

/// The name of the 1.0 build of program `name`: `10` appended, after an
/// underscore when `name` ends in a digit (`mdb_stat10`, `mtest10`,
/// `mtest2_10`, `layout_dump10`).
fn name10(name: &str) -> String {
    if name.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{name}_10")
    } else {
        format!("{name}10")
    }
}

/// Builds the LMDB 1.0 library, programs, `layout_dump10` and `cpeer10` from
/// the sources in `lmdb10_dir`, like `main` builds the 0.9 ones. Returns the
/// (name, executable) pairs to export.
fn build_lmdb10(
    out_dir: &Path,
    lmdb10_dir: &Path,
    peer_dir: &Path,
    peer_srcs: &[PathBuf],
    writeops_src: &Path,
    bin_dir: &Path,
) -> Vec<(String, PathBuf)> {
    let src = |f: &str| lmdb10_dir.join(f);
    // 1.0's liblmdb.a: mdb.o, midl.o, module.o; programs link `-ldl` for
    // module.o's dlopen().
    let midl_obj = compile(out_dir, "lmdb10_midl", lmdb10_dir, &[src("midl.c")]);
    let module_obj = compile(out_dir, "lmdb10_module", lmdb10_dir, &[src("module.c")]);
    let mdb_obj = compile(out_dir, "lmdb10_mdb", lmdb10_dir, &[src("mdb.c")]);
    let chacha8_obj = compile(out_dir, "lmdb10_chacha8", lmdb10_dir, &[src("chacha8.c")]);
    let lib_objs: Vec<PathBuf> = [&mdb_obj, &midl_obj, &module_obj]
        .into_iter()
        .flatten()
        .cloned()
        .collect();

    let mut exported = Vec::new();
    for prog in UPSTREAM10_PROGRAMS {
        let name = name10(prog);
        let mut objs = compile(
            out_dir,
            &format!("lmdb10_{prog}"),
            lmdb10_dir,
            &[src(&format!("{prog}.c"))],
        );
        if CHACHA8_PROGRAMS.contains(prog) {
            objs.extend(chacha8_obj.iter().cloned());
        }
        let exe = link_with(bin_dir, &name, &objs, &lib_objs, &["-ldl"]);
        exported.push((name, exe));
    }

    // The C peer against the 1.0 API (a superset of 0.9's).
    let objs = compile(out_dir, "lmdb10_cpeer", lmdb10_dir, peer_srcs);
    let name = name10("cpeer");
    let exe = link_with(bin_dir, &name, &objs, &lib_objs, &["-ldl"]);
    exported.push((name, exe));

    // writeops against 1.0 (it sets the page size with 1.0's
    // mdb_env_set_pagesize).
    let objs = compile(
        out_dir,
        "lmdb10_writeops",
        lmdb10_dir,
        &[writeops_src.to_path_buf()],
    );
    let name = name10("writeops");
    let exe = link_with(bin_dir, &name, &objs, &lib_objs, &["-ldl"]);
    exported.push((name, exe));

    // The same against a 1.0 library built with -DNDEBUG, for scripts that
    // reach an assertion C's arithmetic does not need: `mdb_node_add`
    // asserts `MP_UPPER(mp) >= MP_LOWER(mp)`, which a `P_LEAF2` page of
    // 1-byte items breaks on 64 KiB pages, where `mp_upper` wraps (the
    // page is fine; release builds of C go on, see
    // `interop/tests/write_differential10.rs`).
    let ndebug_mdb_obj = compile_with(
        out_dir,
        "lmdb10_mdb_ndebug",
        lmdb10_dir,
        &[src("mdb.c")],
        &["NDEBUG"],
    );
    let ndebug_lib_objs: Vec<PathBuf> = [&ndebug_mdb_obj, &midl_obj, &module_obj]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let name = "writeops10_ndebug".to_string();
    let exe = link_with(bin_dir, &name, &objs, &ndebug_lib_objs, &["-ldl"]);
    exported.push((name, exe));

    // layout_dump includes 1.0's mdb.c, so it only needs midl.o.
    let objs = compile(
        out_dir,
        "lmdb10_layout_dump",
        lmdb10_dir,
        &[peer_dir.join("layout_dump.c")],
    );
    let name = name10("layout_dump");
    let exe = link(bin_dir, &name, &objs, &midl_obj);
    exported.push((name, exe));
    exported
}

/// The directory with the LMDB 1.0 sources: `$LMDB10_DIR` if set, else the
/// vendored `../tests/lmdb-c/1.0`. Registers every source file for
/// `rerun-if-changed`; fails the build with a message when one is missing.
fn lmdb10_dir(manifest_dir: &Path) -> PathBuf {
    println!("cargo:rerun-if-env-changed={LMDB10_DIR_VAR}");
    let dir = match env::var_os(LMDB10_DIR_VAR) {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => manifest_dir.join("../tests/lmdb-c/1.0"),
    };
    let dir = dir.canonicalize().unwrap_or(dir);
    let files = LMDB10_LIB_FILES
        .iter()
        .map(|f| f.to_string())
        .chain(UPSTREAM10_PROGRAMS.iter().map(|p| format!("{p}.c")));
    for f in files {
        let path = dir.join(&f);
        assert!(
            path.is_file(),
            "the LMDB 1.0 sources are incomplete: {} is missing (the vendored sources are \
             in tests/lmdb-c/1.0, see tests/lmdb-c/README.md; ${LMDB10_DIR_VAR} overrides \
             the directory)",
            path.display()
        );
        println!("cargo:rerun-if-changed={}", path.display());
    }
    dir
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
    compile_with(out_dir, tag, lmdb_dir, srcs, &[])
}

/// [`compile`] with preprocessor macros defined (`-D<name>`).
fn compile_with(
    out_dir: &Path,
    tag: &str,
    lmdb_dir: &Path,
    srcs: &[PathBuf],
    defines: &[&str],
) -> Vec<PathBuf> {
    if skip_c() {
        return Vec::new();
    }
    let obj_dir = out_dir.join("obj").join(tag);
    std::fs::create_dir_all(&obj_dir).expect("create obj dir");
    let mut b = c_build(lmdb_dir);
    for d in defines {
        b.define(d, None);
    }
    b.out_dir(&obj_dir).files(srcs);
    b.compile_intermediates()
}

/// Writes the recipe for linking `cpeer_capi` at test time, one item per
/// line: `out <executable>`, `cc <compiler>`, `arg <compiler argument>`
/// (the flags `cc` would use, e.g. `-arch`), `obj <object file>`.
fn write_link_recipe(recipe: &Path, exe: &Path, objs: &[PathBuf]) {
    if skip_c() {
        std::fs::write(recipe, format!("out {}\n", exe.display())).expect("write link recipe");
        return;
    }
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
    link_with(bin_dir, name, objs, libs, &[])
}

/// [`link`] with extra linker arguments after the objects (e.g. `-ldl`).
fn link_with(
    bin_dir: &Path,
    name: &str,
    objs: &[PathBuf],
    libs: &[PathBuf],
    extra: &[&str],
) -> PathBuf {
    let exe = bin_dir.join(name);
    if skip_c() {
        return exe;
    }
    let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
    let mut cmd: Command = compiler.to_command();
    cmd.args(objs)
        .args(libs)
        .arg("-pthread")
        .args(extra)
        .arg("-o")
        .arg(&exe);
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("running linker for {name}: {e}"));
    assert!(status.success(), "linking {name} failed: {cmd:?}");
    exe
}
