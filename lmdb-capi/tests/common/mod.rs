//! Building C programs for the tests, against either library:
//!
//! * [`Lib::C`]: the C LMDB 0.9.36 sources in `libraries/liblmdb`, compiled
//!   here into `liblmdbc.a` (the oracle);
//! * [`Lib::Rust`]: `liblmdb.a` built by cargo from this crate (cargo builds
//!   the crate's lib, all crate types at once, before the integration tests,
//!   because the lib has an `rlib` crate type).
//!
//! No build script: the `cc` crate is used at test time, with the target
//! triple derived from the host (the tests always run on the build host).
//! Programs are built once per test binary into
//! `CARGO_TARGET_TMPDIR/capi/<test binary>/`, which is rebuilt every run so
//! Rust-linked programs always use the current library.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};

/// Which LMDB a program is linked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lib {
    /// The C library (oracle).
    C,
    /// The Rust library (`liblmdb.a` from this crate).
    Rust,
}

impl Lib {
    pub fn name(self) -> &'static str {
        match self {
            Lib::C => "c",
            Lib::Rust => "rust",
        }
    }
}

/// `rust/lmdb-capi`.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `libraries/liblmdb`: the C sources and tools.
pub fn c_src() -> PathBuf {
    crate_dir().join("../../libraries/liblmdb")
}

/// `tests/c`: our own C test programs.
pub fn test_c_dir() -> PathBuf {
    crate_dir().join("tests/c")
}

/// The Rust static library: next to the test executable (`deps/`), where
/// cargo puts it when building the lib for the tests, else one level up.
pub fn rust_staticlib() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("deps dir");
    for dir in [deps, deps.parent().unwrap_or(deps)] {
        let lib = dir.join("liblmdb.a");
        if lib.exists() {
            return lib;
        }
    }
    panic!(
        "liblmdb.a not found next to {}; build with `cargo test -p lmdb-capi`",
        exe.display()
    );
}

/// System libraries a C program needs when linking Rust's `liblmdb.a`
/// (`cargo rustc -p lmdb-capi --lib --crate-type staticlib -- --print
/// native-static-libs`; see README.md).
pub fn rust_native_libs() -> &'static [&'static str] {
    if cfg!(target_os = "linux") {
        &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
    } else {
        // macOS: -lSystem -lc -lm, all implied by the compiler driver.
        &[]
    }
}

/// The host's target triple, as the `cc` crate expects it.
fn target_triple() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => format!("{arch}-apple-darwin"),
        "linux" => format!("{arch}-unknown-linux-gnu"),
        os => format!("{arch}-unknown-{os}"),
    }
}

fn cc_build(out_dir: &Path) -> cc::Build {
    let target = target_triple();
    let mut b = cc::Build::new();
    b.cargo_metadata(false)
        .target(&target)
        .host(&target)
        .opt_level(1)
        .debug(true)
        .out_dir(out_dir)
        .warnings(false)
        .flag_if_supported("-pthread");
    b
}

/// The build directory and the programs built so far.
pub struct Builder {
    pub dir: PathBuf,
    built: Mutex<HashMap<(String, Lib), PathBuf>>,
    c_lib: OnceLock<PathBuf>,
}

/// The builder for this test binary.
pub fn builder() -> &'static Builder {
    static B: OnceLock<Builder> = OnceLock::new();
    B.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        // "capi_read-<hash>" -> "capi_read": one directory per test binary.
        let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
        let stem = stem.split('-').next().unwrap_or("tests").to_owned();
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("capi")
            .join(stem);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Builder {
            dir,
            built: Mutex::new(HashMap::new()),
            c_lib: OnceLock::new(),
        }
    })
}

impl Builder {
    /// `liblmdbc.a` from `libraries/liblmdb/mdb.c` and `midl.c`.
    fn c_lib(&self) -> &Path {
        self.c_lib.get_or_init(|| {
            let out = self.dir.join("clib");
            fs::create_dir_all(&out).unwrap();
            cc_build(&out)
                .file(c_src().join("mdb.c"))
                .file(c_src().join("midl.c"))
                .include(c_src())
                .try_compile("lmdbc")
                .unwrap_or_else(|e| panic!("building the C library: {e}"));
            out.join("liblmdbc.a")
        })
    }

    /// Builds program `name` from `sources` against `lib` (once per test
    /// binary) and returns its path. The Rust-linked build is compiled with
    /// `-DLMDB_RUST` (see `GAP()` in the test programs).
    pub fn program(&self, name: &str, sources: &[PathBuf], lib: Lib) -> PathBuf {
        let key = (name.to_owned(), lib);
        let mut built = self.built.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = built.get(&key) {
            return p.clone();
        }
        let out_dir = self.dir.join(lib.name());
        fs::create_dir_all(&out_dir).unwrap();
        let exe = out_dir.join(name);
        let (include, archive) = match lib {
            Lib::C => (c_src(), self.c_lib().to_path_buf()),
            Lib::Rust => (crate_dir().join("include"), rust_staticlib()),
        };
        let mut cmd = cc_build(&out_dir).get_compiler().to_command();
        cmd.arg("-I").arg(&include).arg("-I").arg(test_c_dir());
        if lib == Lib::Rust {
            cmd.arg("-DLMDB_RUST");
        }
        cmd.arg("-o").arg(&exe).args(sources).arg(&archive);
        cmd.arg("-lpthread");
        if lib == Lib::Rust {
            cmd.args(rust_native_libs());
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("running the C compiler: {e}"));
        assert!(
            out.status.success(),
            "building {name} ({lib:?}) failed: {cmd:?}\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        built.insert(key, exe.clone());
        exe
    }

    /// One of the C tools or test programs in `libraries/liblmdb`
    /// (`mdb_stat`, `mdb_dump`, `mdb_load`, `mtest`, …).
    pub fn tool(&self, name: &str, lib: Lib) -> PathBuf {
        self.program(name, &[c_src().join(format!("{name}.c"))], lib)
    }

    /// One of our programs in `tests/c`.
    pub fn test_program(&self, name: &str, lib: Lib) -> PathBuf {
        self.program(name, &[test_c_dir().join(format!("{name}.c"))], lib)
    }

    /// A fresh scratch directory.
    pub fn scratch(&self, name: &str) -> PathBuf {
        let d = self.dir.join("scratch").join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
}

/// Runs `cmd`, panicking (with its output) unless it succeeds.
pub fn run_ok(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd:?} failed ({}):\n{}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Runs C `mtest`-style program `prog` in `dir` (they use `./testdb`).
pub fn run_mtest(prog: &str, dir: &Path) -> PathBuf {
    let env = dir.join("testdb");
    fs::create_dir_all(&env).unwrap();
    let exe = builder().tool(prog, Lib::C);
    run_ok(Command::new(exe).current_dir(dir));
    env
}

/// Writes the fixture environment of `tests/c/fixture.c` into `dir`.
pub fn make_fixture(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let exe = builder().test_program("fixture", Lib::C);
    run_ok(Command::new(exe).arg(dir));
}

/// Copies the environment files of `from` into a new directory `to`.
pub fn copy_env(from: &Path, to: &Path) {
    let _ = fs::remove_dir_all(to);
    fs::create_dir_all(to).unwrap();
    for name in ["data.mdb", "lock.mdb"] {
        if from.join(name).exists() {
            fs::copy(from.join(name), to.join(name)).unwrap();
        }
    }
}
