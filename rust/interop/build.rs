//! Builds the C side of the interop test bed.
//!
//! * `liblmdb` objects from `../../libraries/liblmdb/{mdb,midl}.c`, compiled
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
//! * The LMDB 1.0 oracle: the 1.0 sources are extracted from git (the pinned
//!   commit [`LMDB10_REF`] of `mdb.RE/1.0`, see [`extract_lmdb10`]) into
//!   `$OUT_DIR/lmdb10/`, and the library, the tools, the `mtest*` programs,
//!   `layout_dump` and `cpeer` are built from them once more, with the same
//!   flags, under names ending in `10` (`mdb_stat10`, `mtest2_10`, see
//!   [`name10`]).
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

/// The commit of the LMDB 1.0 sources: `mdb.RE/1.0` after ITS#10642 and
/// ITS#10643, that is the `LMDB_1.0.2` release plus the fixes of the DUPSORT
/// page counts. The `LMDB_1.0.2` tag itself still has that bug, which would
/// be baked into the oracle, so do not pin the tag. Overridden by the
/// environment variable [`LMDB10_REF_VAR`].
const LMDB10_REF: &str = "4865523ff0e1514d26a675d9cfd1d3a4b2b6c4c3";

/// Environment variable that overrides [`LMDB10_REF`] with any commit-ish
/// (a hash, a tag, a branch name), for experiments.
const LMDB10_REF_VAR: &str = "RLMDB_LMDB10_REF";

/// The branch [`LMDB10_REF`] is on, for the "fetch it" error message.
const LMDB10_BRANCH: &str = "mdb.RE/1.0";

/// Files of `libraries/liblmdb` extracted at [`LMDB10_REF`]: the library
/// (1.0's `liblmdb.a` also holds `module.o`, the loader of crypto modules),
/// `chacha8.[ch]` for the encryption tests, and every program of
/// [`UPSTREAM10_PROGRAMS`].
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
        exported.push((prog.to_string(), link(&bin_dir, prog, &objs, &lib_objs)));
    }

    let peer_srcs: Vec<PathBuf> = CPEER_SOURCES.iter().map(|f| peer_dir.join(f)).collect();
    let objs = compile(&out_dir, "cpeer", &lmdb_dir, &peer_srcs);
    exported.push(("cpeer".into(), link(&bin_dir, "cpeer", &objs, &lib_objs)));

    // cpeer against the Rust library's header, linked at test time.
    let capi_include = manifest_dir.join("../rlmdb-capi/include");
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
        "layout_dump".into(),
        link(&bin_dir, "layout_dump", &objs, &midl_obj),
    ));

    let lmdb10_dir = extract_lmdb10(&manifest_dir, &out_dir);
    exported.extend(build_lmdb10(
        &out_dir,
        &lmdb10_dir,
        &peer_dir,
        &peer_srcs,
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

/// Extracts the LMDB 1.0 sources into `$OUT_DIR/lmdb10/` with `git show
/// <commit>:libraries/liblmdb/<file>` and returns that directory.
///
/// The commit is [`LMDB10_REF`] or `$RLMDB_LMDB10_REF`, resolved in the
/// repository that holds this crate (`git rev-parse --show-toplevel`, which
/// also works in linked worktrees, whose objects live in the main
/// repository). The resolved hash is recorded in `lmdb10/.commit`; when it is
/// unchanged and every file is there, nothing is extracted again. Fails the
/// build with instructions when git, the repository or the commit is
/// missing.
fn extract_lmdb10(manifest_dir: &Path, out_dir: &Path) -> PathBuf {
    println!("cargo:rerun-if-env-changed={LMDB10_REF_VAR}");
    let wanted = env::var(LMDB10_REF_VAR).unwrap_or_else(|_| LMDB10_REF.to_owned());

    let top = git(manifest_dir, &["rev-parse", "--show-toplevel"]).unwrap_or_else(|e| {
        panic!(
            "the LMDB 1.0 oracle is extracted from git, but {} is not inside a usable git \
             repository: {e}\n(in a container, the repository's git directory must be mounted; \
             rust/scripts/linux-test.sh does that)",
            manifest_dir.display()
        )
    });
    let top = PathBuf::from(String::from_utf8_lossy(&top).trim());
    let commit = git(
        &top,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{wanted}^{{commit}}"),
        ],
    )
    .unwrap_or_else(|_| {
        panic!(
            "the LMDB 1.0 sources are taken from commit {wanted} of the {LMDB10_BRANCH} branch, \
             which is not in this repository; run `git fetch origin {LMDB10_BRANCH}`{}",
            if wanted == LMDB10_REF {
                String::new()
            } else {
                format!(" (or fix {LMDB10_REF_VAR})")
            }
        )
    });
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();

    let dir = out_dir.join("lmdb10");
    let stamp = dir.join(".commit");
    let files: Vec<String> = LMDB10_LIB_FILES
        .iter()
        .map(|f| f.to_string())
        .chain(UPSTREAM10_PROGRAMS.iter().map(|p| format!("{p}.c")))
        .collect();
    let up_to_date = std::fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == commit)
        && files.iter().all(|f| dir.join(f).is_file());
    if up_to_date {
        return dir;
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("remove old lmdb10 dir");
    }
    std::fs::create_dir_all(&dir).expect("create lmdb10 dir");
    for f in &files {
        let spec = format!("{commit}:libraries/liblmdb/{f}");
        let text = git(&top, &["show", &spec])
            .unwrap_or_else(|e| panic!("extracting the LMDB 1.0 sources: {e}"));
        std::fs::write(dir.join(f), text).expect("write extracted source");
    }
    // Written last: an interrupted extraction is redone.
    std::fs::write(&stamp, format!("{commit}\n")).expect("write lmdb10/.commit");
    dir
}

/// Runs `git -C <dir> <args>`; returns its stdout, or a message with its
/// stderr.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("running git: {e} (git is needed to build the LMDB 1.0 oracle)"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
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
