//! `scripts/install-libs.sh` and the ways README.md says to use what it
//! installs (Unix; the script runs under MSYS2 on Windows, untested here).
//!
//! The script installs the libraries cargo built for the tests into a
//! `DESTDIR`. Then:
//!
//! * the layout is the documented one, and `rlmdb.pc` says
//!   `-I${includedir}/rlmdb` and `-lrlmdb`; on macOS each library's
//!   install name is its installed path;
//! * `tests/c/capi_whoami.c` compiled with the installed headers and
//!   linked with `-lrlmdb` runs on the installed `librlmdb` (0.9.85, new
//!   environments 0.9);
//! * the same program linked against a shared C LMDB 0.9 (SONAME
//!   `liblmdb.so.0`, as distributions ship it) runs on C, and on rlmdb's
//!   0.9 stub when `LD_LIBRARY_PATH` (macOS: `DYLD_LIBRARY_PATH`) names
//!   the stub's directory, or when `LD_PRELOAD` names the stub; linked
//!   against a shared C LMDB 1.0 (SONAME `liblmdb.so`) it runs on the 1.0
//!   stub, which creates LMDB 1.0 environments.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;

/// The prefix the libraries are installed under (inside the `DESTDIR`).
const PREFIX: &str = "/opt/rlmdb-test";

/// The variable of the dynamic linker's search path.
fn library_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// The file name of a shared library `lib<name>` here.
fn dylib(name: &str) -> String {
    shared_lib_name(name)
}

/// Runs the install script with `DESTDIR` `stage` on the libraries in
/// `from` and returns `stage` + [`PREFIX`].
fn install(stage: &Path, from: &Path) -> PathBuf {
    let script = crate_dir().join("../scripts/install-libs.sh");
    let mut cmd = Command::new("sh");
    cmd.arg(script)
        .arg("--from")
        .arg(from)
        .arg("--target")
        .arg(target_triple())
        .env("DESTDIR", stage)
        .env("PREFIX", PREFIX)
        .env_remove("LIBDIR")
        .env_remove("INCLUDEDIR")
        .env_remove("RLMDB_LMDB10_SONAME");
    let out = run_ok(&mut cmd);
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));
    stage.join(PREFIX.trim_start_matches('/'))
}

/// Compiles `capi_whoami.c` with the `lmdb.h` in `include` into `exe`,
/// linked with `-L lib_dir -l<lib>` (a shared library: no rpath, the tests
/// say where it is at run time).
fn whoami(exe: &Path, include: &Path, lib_dir: &Path, lib: &str) {
    let dir = exe.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    let mut c = Compiler::new(dir, false).unwrap();
    c.include(include)
        .output(exe)
        .args([test_c_dir().join("capi_whoami.c")])
        .args([
            OsArg::from("-L"),
            OsArg::from(lib_dir),
            OsArg::from(format!("-l{lib}")),
        ]);
    run_ok(c.command());
}

type OsArg = std::ffi::OsString;

/// A shared build of the C library `lib` (`Lib::C` or `Lib::C10`) named
/// as a system would install it, in a directory of its own (returned): on
/// ELF systems `liblmdb.so.0` (0.9) or `liblmdb.so` (1.0) with that
/// SONAME, and a `liblmdb.so` link for the linker; on macOS
/// `liblmdb.dylib` with its path as the install name.
fn c_shared(lib: Lib) -> PathBuf {
    let dir = builder().dir.join(format!("{}-shared-named", lib.name()));
    fs::create_dir_all(&dir).unwrap();
    let src = lib.src();
    let mut sources = vec![src.join("mdb.c"), src.join("midl.c")];
    if lib.is_v10() {
        sources.push(src.join("module.c"));
    }
    let mut c = Compiler::new(&dir, false).unwrap();
    c.include(&src).args(&sources).pthread();
    if cfg!(target_os = "macos") {
        let file = dir.join("liblmdb.dylib");
        c.args(["-dynamiclib"])
            .args([format!("-Wl,-install_name,{}", file.display())])
            .output(&file);
    } else {
        let soname = if lib.is_v10() {
            "liblmdb.so"
        } else {
            "liblmdb.so.0"
        };
        c.args(["-shared", "-fPIC"])
            .args([format!("-Wl,-soname,{soname}")])
            .output(&dir.join(soname));
        if lib.is_v10() {
            c.args(dl_libs());
        } else {
            let link = dir.join("liblmdb.so");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(soname, &link).unwrap();
        }
    }
    run_ok(c.command());
    dir
}

/// Runs `exe` in a fresh environment directory with the variables `envs`
/// and returns its first line and the format of the environment it made.
fn run(exe: &Path, name: &str, envs: &[(&str, &Path)]) -> (String, Option<rlmdb::Format>) {
    let env_dir = builder().scratch(name).join("env");
    let mut cmd = Command::new(exe);
    cmd.arg(&env_dir)
        .env_remove(rlmdb::DEFAULT_FORMAT_ENV)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("DYLD_LIBRARY_PATH");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = run_ok(&mut cmd);
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_owned();
    let format = rlmdb::sniff_format(&env_dir, rlmdb::EnvironmentFlags::empty()).unwrap();
    eprintln!("{name}: {line} ({format:?})");
    (line, format)
}

#[test]
fn installed_libraries_and_the_stubs_standing_in_for_c() {
    let b = builder();
    let base = b.scratch("install");
    let from = artifact_cdylib(Artifact::Rlmdb)
        .parent()
        .unwrap()
        .to_path_buf();
    let root = install(&base.join("stage"), &from);
    let (lib, inc) = (root.join("lib"), root.join("include/rlmdb"));
    let (v09, v10) = (lib.join("rlmdb/lmdb-0.9"), lib.join("rlmdb/lmdb-1.0"));

    // The layout.
    let mut expected = vec![
        lib.join("librlmdb.a"),
        lib.join(dylib("rlmdb")),
        lib.join("pkgconfig/rlmdb.pc"),
        inc.join("lmdb.h"),
        inc.join("rlmdb.h"),
        v09.join("liblmdb.a"),
        v09.join(dylib("lmdb")),
        v10.join("liblmdb.a"),
        v10.join(dylib("lmdb")),
    ];
    if !cfg!(target_os = "macos") {
        expected.push(v09.join("liblmdb.so.0"));
        assert_eq!(
            fs::read_link(v09.join("liblmdb.so")).unwrap(),
            Path::new("liblmdb.so.0")
        );
    }
    for f in &expected {
        assert!(f.is_file(), "{} not installed", f.display());
    }
    assert_eq!(
        fs::read(inc.join("rlmdb.h")).unwrap(),
        fs::read(crate_dir().join("include/rlmdb.h")).unwrap()
    );
    let pc = fs::read_to_string(lib.join("pkgconfig/rlmdb.pc")).unwrap();
    for line in [
        &format!("prefix={PREFIX}"),
        "libdir=${prefix}/lib",
        "includedir=${prefix}/include",
        "Cflags: -I${includedir}/rlmdb",
        "Libs: -L${libdir} -lrlmdb",
    ] {
        assert!(
            pc.lines().any(|l| l == line),
            "{line:?} not in rlmdb.pc:\n{pc}"
        );
    }
    if cfg!(target_os = "macos") {
        for (file, id) in [
            (
                lib.join("librlmdb.dylib"),
                format!("{PREFIX}/lib/librlmdb.dylib"),
            ),
            (
                v09.join("liblmdb.dylib"),
                format!("{PREFIX}/lib/rlmdb/lmdb-0.9/liblmdb.dylib"),
            ),
            (
                v10.join("liblmdb.dylib"),
                format!("{PREFIX}/lib/rlmdb/lmdb-1.0/liblmdb.dylib"),
            ),
        ] {
            let out = run_ok(Command::new("otool").arg("-D").arg(&file));
            let text = String::from_utf8_lossy(&out.stdout);
            assert_eq!(text.lines().nth(1), Some(id.as_str()), "{text}");
        }
    }

    // A program built with the installed headers and -lrlmdb.
    let path = library_path_var();
    let exe_dir = b.dir.join("install-programs");
    let rlmdb_exe = exe_dir.join("whoami-rlmdb");
    whoami(&rlmdb_exe, &inc, &lib, "rlmdb");
    let (line, format) = run(&rlmdb_exe, "install-rlmdb", &[(path, &lib)]);
    assert!(line.starts_with("0.9.85 LMDB 0.9.85: (rlmdb "), "{line}");
    assert_eq!(format, Some(rlmdb::Format::V09));

    // A program built against C LMDB 0.9, on C and on the 0.9 stub.
    let c09 = c_shared(Lib::C);
    let exe09 = exe_dir.join("whoami-c09");
    whoami(&exe09, &c_src(), &c09, "lmdb");
    let (line, format) = run(&exe09, "install-c09", &[(path, &c09)]);
    assert!(line.starts_with("0.9.37 LMDB 0.9.37"), "{line}");
    assert_eq!(format, Some(rlmdb::Format::V09));
    let (line, format) = run(&exe09, "install-c09-on-stub", &[(path, &v09)]);
    assert!(line.starts_with("0.9.85 LMDB 0.9.85: (rlmdb "), "{line}");
    assert_eq!(format, Some(rlmdb::Format::V09));
    if !cfg!(target_os = "macos") {
        let stub = v09.join("liblmdb.so.0");
        let (line, _) = run(
            &exe09,
            "install-c09-preload",
            &[(path, &c09), ("LD_PRELOAD", &stub)],
        );
        assert!(line.starts_with("0.9.85 LMDB 0.9.85: (rlmdb "), "{line}");
    }

    // A program built against C LMDB 1.0, on C and on the 1.0 stub.
    let c10 = c_shared(Lib::C10);
    let exe10 = exe_dir.join("whoami-c10");
    whoami(&exe10, &c10_src(), &c10, "lmdb");
    let (line, format) = run(&exe10, "install-c10", &[(path, &c10)]);
    assert!(line.starts_with("1.0.3 LMDB 1.0.3"), "{line}");
    assert_eq!(format, Some(rlmdb::Format::V10));
    let (line, format) = run(&exe10, "install-c10-on-stub", &[(path, &v10)]);
    assert!(line.starts_with("1.0.85 LMDB 1.0.85: (rlmdb "), "{line}");
    assert_eq!(format, Some(rlmdb::Format::V10));
}
