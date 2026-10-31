//! Building C programs for the tests, against either library:
//!
//! * [`Lib::C`]: the C LMDB 0.9.36 sources in `tests/lmdb-c/0.9`, compiled
//!   here into `liblmdbc.a` (the oracle);
//! * [`Lib::Rust`]: `liblmdb.a` built by cargo from this crate (cargo builds
//!   the crate's lib, all crate types at once, before the integration tests,
//!   because the lib has an `rlib` crate type).
//!
//! Programs that load LMDB with `dlopen` ([`Builder::dl_program`]) use the
//! shared builds instead: [`Builder::c_shared_lib`] and [`rust_cdylib`].
//!
//! No build step for the tests: the `cc` crate is used at test time, with the target
//! triple derived from the host (the tests always run on the build host).
//! Programs are built once per test process into
//! `CARGO_TARGET_TMPDIR/capi/<test binary>-<pid>-<nanos>/`, so Rust-linked
//! programs always use the current library.
//!
//! The directory is unique per test process because several test runs may
//! share one target directory at the same time: two `cargo test` commands
//! in one checkout, or `scripts/linux-test.sh` runs from different
//! worktrees, which all mount the same Docker volume as `target/` (with
//! PID namespaces, even the pids of two containers can be equal). With a
//! fixed directory, a starting run deleted and rewrote the programs and
//! environments another run was using (the cause of an intermittent
//! failure of all of `tests/concurrency.rs` on Linux). Directories of runs
//! that ended more than a day ago are removed.
//!
//! The upstream `mtest*.c` programs seed `rand()` with `time(NULL)`; the
//! tests build them with `tests/c/mtest_shim.h`, which makes the seed
//! `$MTEST_SEED` (default 20261030), so their data is deterministic.
//!
//! On Windows the libraries are `liblmdb.a` (MinGW) or `lmdb.lib` (MSVC),
//! `lmdb.dll` with its import library `liblmdb.dll.a` or `lmdb.dll.lib`;
//! programs are built with MinGW's `gcc` or MSVC's `cl` ([`Compiler`]),
//! and the test programs' POSIX calls come from `tests/compat`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

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

/// `rlmdb-capi`.
pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `tests/lmdb-c/0.9`: the C sources and tools.
pub fn c_src() -> PathBuf {
    crate_dir().join("../tests/lmdb-c/0.9")
}

/// `tests/c`: our own C test programs.
pub fn test_c_dir() -> PathBuf {
    crate_dir().join("tests/c")
}

/// `interop/c`: the C peer of the interop test bed (`cpeer`), a plain
/// `lmdb.h` client.
pub fn cpeer_dir() -> PathBuf {
    crate_dir().join("../interop/c")
}

/// The file name of static library `name` (`cc`'s `try_compile(name)`,
/// cargo's staticlib): `lib<name>.a`, `<name>.lib` with MSVC.
pub fn static_lib_name(name: &str) -> String {
    if cfg!(target_env = "msvc") {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

/// The Rust static library: next to the test executable (`deps/`), where
/// cargo puts it when building the lib for the tests, else one level up.
pub fn rust_staticlib() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("deps dir");
    let name = static_lib_name("lmdb");
    for dir in [deps, deps.parent().unwrap_or(deps)] {
        let lib = dir.join(&name);
        if lib.exists() {
            return lib;
        }
    }
    panic!(
        "{name} not found next to {}; build with `cargo test -p rlmdb-capi`",
        exe.display()
    );
}

/// The file name of a shared library `lib<name>` on this platform
/// (`<name>.dll` on Windows).
pub fn shared_lib_name(name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else if cfg!(windows) {
        format!("{name}.dll")
    } else {
        format!("lib{name}.so")
    }
}

/// The functions `lmdb.h` declares (`mdb_*`, one declaration per line,
/// starting in the first column).
pub fn lmdb_h_functions() -> Vec<String> {
    let header = fs::read_to_string(c_src().join("lmdb.h")).unwrap();
    let names: Vec<String> = header
        .lines()
        .filter(|l| l.starts_with(|c: char| c.is_ascii_alphabetic()) && !l.starts_with("typedef"))
        .filter_map(|l| {
            let name = &l[l.find("mdb_")?..];
            let name = &name[..name.find('(')?];
            name.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                .then(|| name.to_owned())
        })
        .collect();
    // 0.9.36 declares 56; a change of style would show here.
    assert!(names.len() > 50, "lmdb.h declares only {names:?}");
    names
}

/// A module-definition file (`.def`) for DLL `name` exporting `functions`.
pub fn module_definition(name: &str, functions: &[String]) -> String {
    let mut def = format!("LIBRARY {name}\nEXPORTS\n");
    for f in functions {
        def.push_str(&format!("\t{f}\n"));
    }
    def
}

/// The import library of the Rust DLL (Windows): `liblmdb.dll.a` (MinGW)
/// or `lmdb.dll.lib` (MSVC), next to `lmdb.dll`.
pub fn rust_import_lib() -> PathBuf {
    let dll = rust_cdylib();
    let name = if cfg!(target_env = "msvc") {
        "lmdb.dll.lib"
    } else {
        "liblmdb.dll.a"
    };
    dll.with_file_name(name)
}

/// The Rust shared library (`liblmdb.so` / `liblmdb.dylib`), found like
/// [`rust_staticlib`].
pub fn rust_cdylib() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let deps = exe.parent().expect("deps dir");
    let name = shared_lib_name("lmdb");
    for dir in [deps, deps.parent().unwrap_or(deps)] {
        let lib = dir.join(&name);
        if lib.exists() {
            return lib;
        }
    }
    panic!(
        "{name} not found next to {}; build with `cargo test -p rlmdb-capi`",
        exe.display()
    );
}

/// System libraries a C program needs when linking Rust's `liblmdb.a`
/// (`cargo rustc -p rlmdb-capi --lib --crate-type staticlib -- --print
/// native-static-libs`; see README.md).
pub fn rust_native_libs() -> &'static [&'static str] {
    if cfg!(all(windows, target_env = "msvc")) {
        &[
            "legacy_stdio_definitions.lib",
            "kernel32.lib",
            "advapi32.lib",
            "ntdll.lib",
            "userenv.lib",
            "ws2_32.lib",
            "dbghelp.lib",
        ]
    } else if cfg!(windows) {
        &[
            "-lkernel32",
            "-lntdll",
            "-luserenv",
            "-lws2_32",
            "-ldbghelp",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
    } else if cfg!(target_os = "freebsd") {
        &[
            "-lrt",
            "-lutil",
            "-lexecinfo",
            "-lkvm",
            "-lmemstat",
            "-lprocstat",
            "-ldevstat",
            "-lpthread",
            "-lgcc_s",
            "-lc",
            "-lm",
        ]
    } else if cfg!(target_os = "netbsd") {
        &[
            "-lutil",
            "-lrt",
            "-lexecinfo",
            "-lpthread",
            "-lgcc_s",
            "-lc",
            "-lm",
        ]
    } else if cfg!(target_os = "openbsd") {
        &[
            "-lutil",
            "-lexecinfo",
            "-lpthread",
            "-lc++abi",
            "-lc",
            "-lm",
            "-lcompiler_rt",
        ]
    } else if cfg!(target_os = "dragonfly") {
        &[
            "-lrt",
            "-lutil",
            "-lexecinfo",
            "-lkvm",
            "-lpthread",
            "-lgcc_pic",
            "-lc",
            "-lm",
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
        "windows" if cfg!(target_env = "msvc") => format!("{arch}-pc-windows-msvc"),
        "windows" => format!("{arch}-pc-windows-gnu"),
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
        .warnings(false);
    // C LMDB uses no pthreads on Windows.
    if !cfg!(windows) {
        b.flag_if_supported("-pthread");
    }
    b
}

/// `tests/compat`: the Windows shims of the C test programs.
pub fn compat_dir() -> PathBuf {
    crate_dir().join("../tests/compat")
}

/// A C compiler command line, in the form the compiler takes it: a GNU
/// driver (`cc`, `gcc`, `clang`; MinGW's on Windows) or MSVC's `cl`.
pub struct Compiler {
    cmd: Command,
    msvc: bool,
}

impl Compiler {
    /// The C (or with `cpp` the C++) compiler of `cc_build(out_dir)`;
    /// objects `cl` writes go to `out_dir`.
    pub fn new(out_dir: &Path, cpp: bool) -> Result<Compiler, cc::Error> {
        let tool = cc_build(out_dir).cpp(cpp).try_get_compiler()?;
        let msvc = tool.is_like_msvc();
        let mut cmd = tool.to_command();
        if msvc {
            cmd.arg("-nologo")
                .arg(format!("-Fo{}\\", out_dir.display()))
                // The upstream tools include <unistd.h> for getopt.
                .arg("-I")
                .arg(compat_dir().join("msvc"));
        }
        Ok(Compiler { cmd, msvc })
    }

    /// `true` for MSVC's `cl`.
    pub fn is_msvc(&self) -> bool {
        self.msvc
    }

    /// An include directory.
    pub fn include(&mut self, dir: &Path) -> &mut Self {
        self.cmd.arg("-I").arg(dir);
        self
    }

    /// A header included before the source (`-include`, `/FI`).
    pub fn force_include(&mut self, header: &Path) -> &mut Self {
        if self.msvc {
            self.cmd.arg(format!("-FI{}", header.display()));
        } else {
            self.cmd.arg("-include").arg(header);
        }
        self
    }

    /// Other arguments, as they are.
    pub fn args<I: IntoIterator<Item = S>, S: AsRef<std::ffi::OsStr>>(
        &mut self,
        args: I,
    ) -> &mut Self {
        self.cmd.args(args);
        self
    }

    /// The executable to write (`-o`, `/Fe`).
    pub fn output(&mut self, exe: &Path) -> &mut Self {
        if self.msvc {
            self.cmd.arg(format!("-Fe{}", exe.display()));
        } else {
            self.cmd.arg("-o").arg(exe);
        }
        self
    }

    /// The thread library a test program links (`-lpthread`; nothing on
    /// Windows, where `tests/compat` uses Windows threads).
    pub fn pthread(&mut self) -> &mut Self {
        if !cfg!(windows) {
            self.cmd.arg("-lpthread");
        }
        self
    }

    /// C99 `printf` (`%zu`) for one of our test programs with MinGW, whose
    /// default is msvcrt's; the upstream programs keep that, as on any
    /// Windows system.
    pub fn test_program(&mut self) -> &mut Self {
        if cfg!(windows) && !self.msvc {
            self.cmd.arg("-D__USE_MINGW_ANSI_STDIO=1");
        }
        self
    }

    /// Libraries C LMDB itself needs: `advapi32.lib` with MSVC (MinGW's
    /// driver links it by default).
    pub fn lmdb_system_libs(&mut self) -> &mut Self {
        if self.msvc {
            self.cmd.arg("advapi32.lib");
        }
        self
    }

    /// The command.
    pub fn command(&mut self) -> &mut Command {
        &mut self.cmd
    }
}

/// A command running `exe` with `argv[0] = name`: `arg0` on Unix. On
/// Windows, where `argv[0]` is the program as named, `exe` (whose file name
/// must be `name.exe`) is run by its name, from its directory put first on
/// the child's `PATH`.
pub fn command_as(exe: &Path, name: &str) -> Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new(exe);
        cmd.arg0(name);
        cmd
    }
    #[cfg(windows)]
    {
        assert_eq!(exe.file_stem().and_then(|s| s.to_str()), Some(name));
        let dir = exe.parent().expect("a directory");
        let mut path = std::ffi::OsString::from(dir);
        if let Some(old) = std::env::var_os("PATH") {
            path.push(";");
            path.push(old);
        }
        let mut cmd = Command::new(name);
        cmd.env("PATH", path);
        cmd
    }
}

/// The build directory and the programs built so far.
pub struct Builder {
    pub dir: PathBuf,
    built: Mutex<HashMap<(String, Lib), PathBuf>>,
    c_lib: OnceLock<PathBuf>,
}

/// The builder for this test process.
pub fn builder() -> &'static Builder {
    static B: OnceLock<Builder> = OnceLock::new();
    B.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        // "capi_read-<hash>" -> "capi_read": one directory per test binary
        // and process.
        let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
        let stem = stem.split('-').next().unwrap_or("tests").to_owned();
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("capi");
        remove_old_runs(&base);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = base.join(format!("{stem}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Builder {
            dir,
            built: Mutex::new(HashMap::new()),
            c_lib: OnceLock::new(),
        }
    })
}

/// Removes run directories in `base` not modified for a day (runs that
/// ended long ago; a live run touches its directory when it builds).
fn remove_old_runs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let day = Duration::from_secs(24 * 3600);
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > day);
        if old {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

impl Builder {
    /// `liblmdbc.a` from `tests/lmdb-c/0.9/mdb.c` and `midl.c`.
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
            out.join(static_lib_name("lmdbc"))
        })
    }

    /// Builds program `name` from `sources` against `lib` (once per test
    /// process) and returns its path. The Rust-linked build is compiled with
    /// `-DLMDB_RUST` (see `GAP()` in the test programs).
    pub fn program(&self, name: &str, sources: &[PathBuf], lib: Lib) -> PathBuf {
        self.program_with(name, sources, lib, &[])
    }

    /// As [`Builder::program`], with extra compiler arguments.
    pub fn program_with(
        &self,
        name: &str,
        sources: &[PathBuf],
        lib: Lib,
        extra: &[&str],
    ) -> PathBuf {
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
        // Where `cl` puts the objects (one directory per program, as
        // programs share source names).
        let obj_dir = out_dir.join(format!("{name}.obj"));
        if cfg!(windows) {
            fs::create_dir_all(&obj_dir).unwrap();
        }
        let mut c = Compiler::new(&obj_dir, false).unwrap();
        c.include(&include).include(&test_c_dir());
        if !sources.iter().any(|s| s.starts_with(c_src())) {
            c.test_program();
        }
        if lib == Lib::Rust {
            c.args(["-DLMDB_RUST"]);
        }
        c.args(extra)
            .output(&exe)
            .args(sources)
            .args([&archive])
            .pthread();
        if lib == Lib::Rust {
            c.args(rust_native_libs());
        } else {
            c.lmdb_system_libs();
        }
        let cmd = c.command();
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

    /// One of the C tools or test programs in `tests/lmdb-c/0.9`
    /// (`mdb_stat`, `mdb_dump`, `mdb_load`, `mtest`, …).
    pub fn tool(&self, name: &str, lib: Lib) -> PathBuf {
        self.program(name, &[c_src().join(format!("{name}.c"))], lib)
    }

    /// One of our programs in `tests/c`.
    pub fn test_program(&self, name: &str, lib: Lib) -> PathBuf {
        self.program(name, &[test_c_dir().join(format!("{name}.c"))], lib)
    }

    /// The C library as a shared library (`liblmdbc.so` / `.dylib`,
    /// `lmdbc.dll`), for programs that load LMDB with `dlopen`.
    pub fn c_shared_lib(&self) -> PathBuf {
        let mut built = self.built.lock().unwrap_or_else(|e| e.into_inner());
        let key = ("liblmdbc-shared".to_owned(), Lib::C);
        if let Some(p) = built.get(&key) {
            return p.clone();
        }
        let out_dir = self.dir.join("clib-shared");
        fs::create_dir_all(&out_dir).unwrap();
        let lib = out_dir.join(shared_lib_name("lmdbc"));
        let mut c = Compiler::new(&out_dir, false).unwrap();
        let sources = [c_src().join("mdb.c"), c_src().join("midl.c")];
        if c.is_msvc() {
            // MinGW's linker exports every function of a DLL; `cl` only
            // what a module-definition file names (lmdb.h has no
            // __declspec(dllexport)): the functions lmdb.h declares, as
            // rlmdb-capi exports them.
            let def = out_dir.join("lmdbc.def");
            fs::write(&def, module_definition("lmdbc", &lmdb_h_functions())).unwrap();
            let def_arg = format!("-DEF:{}", def.display());
            // (The linker's options come last, after -link.)
            c.args(["-LD"])
                .output(&lib)
                .include(&c_src())
                .args(sources)
                .lmdb_system_libs()
                .args(["-link", def_arg.as_str()]);
        } else {
            c.args(["-shared", "-fPIC"])
                .output(&lib)
                .args(sources)
                .include(&c_src())
                .pthread();
        }
        let cmd = c.command();
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "building the shared C library failed: {cmd:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        built.insert(key, lib.clone());
        lib
    }

    /// Program `name` from `tests/c` linked against no LMDB library: it
    /// loads one with `dlopen` (`lmdb.h` only for the types and constants).
    pub fn dl_program(&self, name: &str) -> PathBuf {
        let mut built = self.built.lock().unwrap_or_else(|e| e.into_inner());
        let key = (format!("{name}-dl"), Lib::C);
        if let Some(p) = built.get(&key) {
            return p.clone();
        }
        let out_dir = self.dir.join("dl");
        fs::create_dir_all(&out_dir).unwrap();
        let exe = out_dir.join(name);
        let mut c = Compiler::new(&out_dir, false).unwrap();
        c.include(&c_src())
            .test_program()
            .output(&exe)
            .args([test_c_dir().join(format!("{name}.c"))])
            .pthread();
        if cfg!(target_os = "linux") {
            c.args(["-ldl"]);
        }
        let cmd = c.command();
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "building {name} failed: {cmd:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        built.insert(key, exe.clone());
        exe
    }

    /// Program `name` from `tests/c/<name>.c` compiled as C++ (with the C++
    /// compiler, `-x c++`) and linked against the Rust library as
    /// `<name>_cxx`: a C++ program sees the C symbols only if the headers'
    /// `extern "C"` guards are right. The program must not need the C++
    /// runtime (the C compiler driver links it). `None`, with a note on
    /// stderr, when the C++ compiler (`CXX`, else the `cc` crate's default
    /// such as `c++`) cannot be run: a machine with only a C compiler
    /// skips the test instead of failing it.
    pub fn cxx_test_program(&self, name: &str) -> Option<PathBuf> {
        let out_dir = self.dir.join("cxx");
        fs::create_dir_all(&out_dir).unwrap();
        let obj = out_dir.join(format!("{name}.o"));
        let compiler = match cc_build(&out_dir).cpp(true).try_get_compiler() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipped: no C++ compiler ({e})");
                return None;
            }
        };
        let probe = if compiler.is_like_msvc() {
            "-?"
        } else {
            "--version"
        };
        if let Err(e) = compiler.to_command().arg(probe).output() {
            eprintln!(
                "skipped: C++ compiler {} cannot be run: {e}",
                compiler.path().display()
            );
            return None;
        }
        let mut cmd = compiler.to_command();
        cmd.arg("-I")
            .arg(crate_dir().join("include"))
            .arg("-I")
            .arg(test_c_dir())
            .arg("-DLMDB_RUST");
        if compiler.is_like_msvc() {
            // C++ without exceptions or RTTI, warnings as errors.
            cmd.args(["-nologo", "-EHs-c-", "-GR-", "-W3", "-WX", "-TP", "-c"])
                .arg(format!("-Fo{}", obj.display()));
        } else {
            cmd.args(["-fno-exceptions", "-fno-rtti", "-Wall", "-Werror"])
                .args(["-x", "c++", "-c", "-o"])
                .arg(&obj);
        }
        cmd.arg(test_c_dir().join(format!("{name}.c")));
        run_ok(&mut cmd);
        Some(self.program(&format!("{name}_cxx"), &[obj], Lib::Rust))
    }

    /// One of the upstream `mtest*.c` programs, unmodified, built with
    /// `tests/c/mtest_shim.{h,c}` (the header included first): a fixed
    /// `rand()` seed (`$MTEST_SEED`) and the `mdb_dkey()` that `mtest6.c`
    /// needs.
    pub fn mtest(&self, name: &str, lib: Lib) -> PathBuf {
        let shim_h = test_c_dir().join("mtest_shim.h");
        let include = if Compiler::new(&self.dir, false).is_ok_and(|c| c.is_msvc()) {
            vec![format!("-FI{}", shim_h.display())]
        } else {
            vec!["-include".to_owned(), shim_h.display().to_string()]
        };
        let extra: Vec<&str> = include.iter().map(String::as_str).collect();
        self.program_with(
            name,
            &[
                c_src().join(format!("{name}.c")),
                test_c_dir().join("mtest_shim.c"),
            ],
            lib,
            &extra,
        )
    }

    /// The interop test bed's C peer (`interop/c`), linked against
    /// `lib`.
    pub fn cpeer(&self, lib: Lib) -> PathBuf {
        let sources: Vec<PathBuf> = [
            "cpeer.c",
            "cpeer_data.c",
            "cpeer_loops.c",
            "cpeer_misc.c",
            "cpeer_pagewalk.c",
            "dataset.c",
        ]
        .iter()
        .map(|f| cpeer_dir().join(f))
        .collect();
        let include = cpeer_dir();
        self.program_with("cpeer", &sources, lib, &["-I", include.to_str().unwrap()])
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

/// The default seed of the `mtest*` programs (see `tests/c/mtest_shim.c`).
pub const MTEST_SEED: u32 = 20261030;

/// Runs `mtest`-style program `prog`, linked against `lib`, in `dir` (they
/// use `./testdb`) with `rand()` seeded with `seed`, and returns its
/// output (it must succeed).
pub fn run_mtest_seeded(prog: &str, lib: Lib, dir: &Path, seed: u32) -> Output {
    fs::create_dir_all(dir.join("testdb")).unwrap();
    let exe = builder().mtest(prog, lib);
    run_ok(
        Command::new(exe)
            .current_dir(dir)
            .env("MTEST_SEED", seed.to_string()),
    )
}

/// Runs C `mtest`-style program `prog` (C library, default seed) in `dir`
/// and returns the environment path.
pub fn run_mtest(prog: &str, dir: &Path) -> PathBuf {
    run_mtest_seeded(prog, Lib::C, dir, MTEST_SEED);
    dir.join("testdb")
}

/// Writes the fixture environment of `tests/c/fixture.c` into `dir`.
pub fn make_fixture(dir: &Path) {
    make_fixture_with(Lib::C, dir);
}

/// [`make_fixture`] with `fixture.c` linked against `lib`.
pub fn make_fixture_with(lib: Lib, dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let exe = builder().test_program("fixture", lib);
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
