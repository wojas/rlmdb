//! Builds `cbench`, the C side of the benchmarks: `c/cbench.c` linked with
//! the C library from `../tests/lmdb-c/0.9/{mdb,midl}.c`, compiled with
//! the flags the interop test bed uses (the upstream `Makefile`'s: `-pthread
//! -O2 -g -W -Wall -Wno-unused-parameter`, no `MDB_DEBUG`, no `NDEBUG`).
//!
//! The executable goes to `$OUT_DIR/bin/cbench`; its path is exported to the
//! crate as the compile-time environment variable `LMDB_BENCH_CBENCH`; the
//! C compiler's and rustc's versions as `LMDB_BENCH_CC_VERSION` and
//! `LMDB_BENCH_RUSTC_VERSION`, for the report.
//! Nothing is linked into the Rust binary: the C workloads run as a separate
//! process, like the Rust ones (the driver starts one process per workload
//! group and implementation).
//!
//! `RLMDB_SKIP_C_BUILD=1` skips the C side, as in `interop/build.rs` (for
//! `cargo check` of a target without a C toolchain); `cbench` is then
//! missing and the C compiler's version is `unknown`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `path.canonicalize()`, without Windows' verbatim prefix (`\\?\C:\...`
/// becomes `C:\...`), which MinGW's `gcc` does not understand; `path`
/// itself if it cannot be resolved.
fn canonical(path: PathBuf) -> PathBuf {
    let Ok(c) = path.canonicalize() else {
        return path;
    };
    match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => c,
    }
}

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let lmdb_dir = manifest_dir.join("../tests/lmdb-c/0.9");
    let lmdb_dir = canonical(lmdb_dir);
    let src = manifest_dir.join("c/cbench.c");
    for f in ["mdb.c", "midl.c", "lmdb.h", "midl.h"] {
        println!("cargo:rerun-if-changed={}", lmdb_dir.join(f).display());
    }
    println!("cargo:rerun-if-changed={}", src.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("../tests/compat/posix_compat.h")
            .display()
    );
    println!("cargo:rerun-if-env-changed=RLMDB_SKIP_C_BUILD");
    let skip_c = env::var_os("RLMDB_SKIP_C_BUILD").is_some_and(|v| !v.is_empty() && v != "0");

    let bin_dir = out_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let exe = bin_dir.join(if target_windows() {
        "cbench.exe"
    } else {
        "cbench"
    });
    if !skip_c {
        let obj_dir = out_dir.join("obj");
        std::fs::create_dir_all(&obj_dir).expect("create obj dir");
        let objs = c_build(&lmdb_dir)
            .out_dir(&obj_dir)
            .files([lmdb_dir.join("mdb.c"), lmdb_dir.join("midl.c"), src])
            .compile_intermediates();
        link(&exe, &objs);
    }
    println!("cargo:rustc-env=LMDB_BENCH_CBENCH={}", exe.display());

    // Compiler versions, for the report (`cl` prints its banner on stderr
    // and has no `--version`).
    let cc_version = if skip_c {
        "unknown".to_string()
    } else {
        let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
        if compiler.is_like_msvc() {
            first_line_stderr(&mut compiler.to_command())
        } else {
            first_line(Command::new(compiler.path()).arg("--version"))
        }
    };
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let rustc_version = first_line(Command::new(rustc).arg("-V"));
    println!("cargo:rustc-env=LMDB_BENCH_CC_VERSION={cc_version}");
    println!("cargo:rustc-env=LMDB_BENCH_RUSTC_VERSION={rustc_version}");
}

/// The first line of a command's standard error, or `unknown`.
fn first_line_stderr(cmd: &mut Command) -> String {
    cmd.output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

/// The first line of a command's standard output, or `unknown`.
fn first_line(cmd: &mut Command) -> String {
    cmd.output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".into())
}

/// `true` when building for Windows (the build script runs on the host).
fn target_windows() -> bool {
    env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
}

/// A `cc::Build` with the upstream Makefile's flags (as `interop/build.rs`,
/// also for Windows: MinGW without `-pthread`, MSVC with `/W3`).
fn c_build(lmdb_dir: &Path) -> cc::Build {
    let mut b = cc::Build::new();
    b.include(lmdb_dir)
        .opt_level(2)
        .debug(true)
        .warnings(false)
        .extra_warnings(false)
        .cargo_metadata(false);
    if cc::Build::new()
        .cargo_metadata(false)
        .get_compiler()
        .is_like_msvc()
    {
        b.flag("-W3");
        return b;
    }
    if !target_windows() {
        b.flag("-pthread");
    }
    b.flag("-W").flag("-Wall").flag("-Wno-unused-parameter");
    if target_windows() {
        b.define("__USE_MINGW_ANSI_STDIO", "1");
    }
    b
}

/// Links `objs` into the executable `exe` with the C compiler (`exe.exe`
/// on Windows; `cl` needs `advapi32.lib` for C LMDB's mutexes).
fn link(exe: &Path, objs: &[PathBuf]) -> PathBuf {
    let compiler = cc::Build::new().cargo_metadata(false).get_compiler();
    let exe = if target_windows() {
        exe.with_extension("exe")
    } else {
        exe.to_path_buf()
    };
    let exe = exe.as_path();
    let mut cmd = compiler.to_command();
    if compiler.is_like_msvc() {
        cmd.arg("-nologo")
            .args(objs)
            .arg("advapi32.lib")
            .arg(format!("-Fe{}", exe.display()));
    } else if target_windows() {
        cmd.args(objs).arg("-o").arg(exe);
    } else {
        cmd.args(objs).arg("-pthread").arg("-o").arg(exe);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("running the linker for cbench: {e}"));
    assert!(status.success(), "linking cbench failed: {cmd:?}");
    exe.to_path_buf()
}
