//! Shared helpers for the `rlmdb-tools` tests:
//!
//! * building the C oracle: the C library and tools from
//!   `tests/lmdb-c/0.9` (`mdb_stat`, `mdb_dump`, `mdb_load`, `mdb_copy`,
//!   `mtest`..`mtest5`), plus `tests/c/mdb_drop.c` (LMDB's first
//!   `mdb_drop`, 0.9 API only) and `tests/c/holder.c` (keeps a reader
//!   open), with the `cc` crate at test time;
//! * running a tool of either implementation with the same `argv[0]`, so
//!   usage and `getopt` messages can be compared byte for byte;
//! * generating test environments: crafted dump files loaded with the C
//!   `mdb_load`, and the `mtest` programs.
//!
//! The C build lives in `CARGO_TARGET_TMPDIR/rlmdb-tools-c-<hash of the
//! sources and flags>`, built once and shared by all test binaries: the
//! build runs under a file lock and an atomic rename publishes it.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Which implementation of a tool to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Impl {
    /// The C tool (oracle).
    C,
    /// The Rust tool from this crate.
    Rust,
}

/// The result of running a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl Run {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    /// Standard output as text with `\n` line ends ([`text`]), for
    /// comparisons with expected text.
    pub fn stdout_text(&self) -> String {
        text(&self.stdout)
    }
    /// Standard error as text with `\n` line ends ([`text`]).
    pub fn stderr_text(&self) -> String {
        text(&self.stderr)
    }
}

/// `bytes` as text with `\n` line ends: on Windows the tools end lines
/// with `\r\n` (the C runtime's text mode, which the Rust tools emulate).
/// For comparisons with expected text; C and Rust outputs are compared as
/// bytes.
pub fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if cfg!(windows) {
        s.replace("\r\n", "\n")
    } else {
        s.into_owned()
    }
}

/// `text` with the line ends the tools write on this platform: each `\n`
/// becomes `\r\n` on Windows.
pub fn platform_lines(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        if b == b'\n' && cfg!(windows) {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `tests/lmdb-c/0.9`.
pub fn c_src() -> PathBuf {
    manifest_dir().join("../tests/lmdb-c/0.9")
}

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
    // C LMDB uses no pthreads on Windows; the tools keep the C runtime's
    // printf there, as on any Windows system (no __USE_MINGW_ANSI_STDIO).
    if !cfg!(windows) {
        b.flag_if_supported("-pthread");
    }
    b
}

/// The file `cc::Build::try_compile(name)` writes: `lib<name>.a`, or
/// `<name>.lib` with MSVC.
fn static_lib(dir: &Path, name: &str) -> PathBuf {
    if cfg!(target_env = "msvc") {
        dir.join(format!("{name}.lib"))
    } else {
        dir.join(format!("lib{name}.a"))
    }
}

/// The C compiler command that builds executable `exe` from `inputs`
/// (sources, then libraries) with the include directory `include`: `-o`
/// and `-lpthread` for GNU-style drivers, `-Fe` (objects into the
/// executable's directory, `tests/compat/msvc` for `unistd.h`, and
/// `advapi32.lib`, which C LMDB needs) for MSVC's `cl`.
fn link_program(include: &Path, exe: &Path, inputs: &[&Path]) -> Command {
    let dir = exe.parent().unwrap_or(Path::new("."));
    let compiler = cc_build(dir).get_compiler();
    let mut cmd = compiler.to_command();
    cmd.arg("-I").arg(include);
    if compiler.is_like_msvc() {
        cmd.arg("-I")
            .arg(manifest_dir().join("../tests/compat/msvc"))
            .arg(format!("-Fe{}", exe.display()))
            .arg(format!("-Fo{}\\", dir.display()))
            .args(inputs)
            .arg("advapi32.lib");
    } else {
        cmd.arg("-o").arg(exe).args(inputs);
        if !cfg!(windows) {
            cmd.arg("-lpthread");
        }
    }
    cmd
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

/// An argument from bytes: as they are on Unix, UTF-8 on Windows.
pub fn os_arg(a: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    return <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(a).to_owned();
    #[cfg(windows)]
    return String::from_utf8_lossy(a).into_owned().into();
}

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

/// The C programs built from `tests/lmdb-c/0.9`.
const C_TOOLS: &[&str] = &[
    "mdb_stat", "mdb_dump", "mdb_load", "mdb_copy", "mtest", "mtest2", "mtest3", "mtest4", "mtest5",
];
/// Our own C programs in `tests/c`.
const TEST_PROGRAMS: &[&str] = &["mdb_drop", "holder"];

/// The directory with the C programs, built on first use.
pub fn c_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| build_c().unwrap_or_else(|e| panic!("building the C oracle: {e}")))
}

fn build_c() -> Result<PathBuf, String> {
    let mut sources: Vec<PathBuf> = vec![
        c_src().join("mdb.c"),
        c_src().join("midl.c"),
        c_src().join("lmdb.h"),
        c_src().join("midl.h"),
    ];
    sources.extend(C_TOOLS.iter().map(|t| c_src().join(format!("{t}.c"))));
    sources.extend(
        TEST_PROGRAMS
            .iter()
            .map(|t| manifest_dir().join(format!("tests/c/{t}.c"))),
    );
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    // The compiler flags are set in this file (`cc_build`, `compile_c`) and
    // taken from the environment by `cc`. The toolchain is part of the
    // name: the MSVC and MinGW builds of one Windows checkout share
    // `CARGO_TARGET_TMPDIR`, and their C runtimes format differently.
    fnv(&mut h, include_bytes!("mod.rs"));
    fnv(&mut h, target_triple().as_bytes());
    for var in ["CC", "CFLAGS"] {
        fnv(&mut h, std::env::var(var).unwrap_or_default().as_bytes());
    }
    for f in &sources {
        fnv(&mut h, f.file_name().unwrap().to_string_lossy().as_bytes());
        fnv(
            &mut h,
            &fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?,
        );
    }
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    build_shared(&base, &format!("rlmdb-tools-c-{h:016x}"), compile_c)
}

/// Compiles the C library and programs into `tmp`.
fn compile_c(tmp: &Path) -> Result<(), String> {
    cc_build(tmp)
        .file(c_src().join("mdb.c"))
        .file(c_src().join("midl.c"))
        .include(c_src())
        .try_compile("lmdbc")
        .map_err(|e| e.to_string())?;
    let lib = static_lib(tmp, "lmdbc");
    let programs = C_TOOLS
        .iter()
        .map(|t| (*t, c_src().join(format!("{t}.c"))))
        .chain(
            TEST_PROGRAMS
                .iter()
                .map(|t| (*t, manifest_dir().join(format!("tests/c/{t}.c")))),
        );
    for (name, src) in programs {
        let mut cmd = link_program(&c_src(), &tmp.join(name), &[&src, &lib]);
        let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "{cmd:?} failed:\n{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    Ok(())
}

/// Returns `<base>/<name>`, filled by `build` once for all test processes.
///
/// A directory with `.ok` is complete and reused as is. Otherwise the build
/// runs under an advisory lock on `<base>/<name>.lock` (other processes wait
/// for it and then reuse its result) in `<name>.tmp<pid>`, gets its `.ok`
/// and is renamed into place. A directory without `.ok` is what a killed
/// build or a partially restored CI cache left behind: it is removed first,
/// as `rename` cannot replace a non-empty directory.
fn build_shared(
    base: &Path,
    name: &str,
    build: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let dir = base.join(name);
    if dir.join(".ok").exists() {
        return Ok(dir);
    }
    fs::create_dir_all(base).map_err(|e| format!("{}: {e}", base.display()))?;
    let lock_path = base.join(format!("{name}.lock"));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    lock_exclusive(&lock).map_err(|e| format!("locking {}: {e}", lock_path.display()))?;
    if dir.join(".ok").exists() {
        // Another process built it while we waited.
        return Ok(dir);
    }
    if dir.symlink_metadata().is_ok() {
        fs::remove_dir_all(&dir)
            .or_else(|_| fs::remove_file(&dir))
            .map_err(|e| format!("removing stale {}: {e}", dir.display()))?;
    }
    let tmp = base.join(format!("{name}.tmp{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    build(&tmp)?;
    fs::write(tmp.join(".ok"), b"ok").map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(&tmp, &dir) {
        let _ = fs::remove_dir_all(&tmp);
        // Only possible where `lock_exclusive` does not lock: a concurrent
        // build may have won.
        if !dir.join(".ok").exists() {
            return Err(format!(
                "renaming {} to {}: {e}",
                tmp.display(),
                dir.display()
            ));
        }
    }
    // Returning drops `lock`, which closes the file and releases the lock.
    Ok(dir)
}

/// Takes an exclusive advisory lock on `file`, waiting for other holders;
/// it is released when the file is closed.
#[cfg(unix)]
fn lock_exclusive(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: `flock` takes a descriptor, which `file` keeps open for
        // the duration of the call, and accesses no memory of ours.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Elsewhere the build is not locked (`File::lock`, Rust 1.89, replaces
/// this once the MSRV allows); concurrent builds then race for the rename.
#[cfg(not(unix))]
fn lock_exclusive(_file: &fs::File) -> std::io::Result<()> {
    Ok(())
}

/// The executable of `tool` for `imp`.
pub fn bin(imp: Impl, tool: &str) -> PathBuf {
    match imp {
        Impl::C => c_dir().join(tool),
        Impl::Rust => PathBuf::from(match tool {
            "mdb_stat" => env!("CARGO_BIN_EXE_mdb_stat"),
            "mdb_dump" => env!("CARGO_BIN_EXE_mdb_dump"),
            "mdb_load" => env!("CARGO_BIN_EXE_mdb_load"),
            "mdb_copy" => env!("CARGO_BIN_EXE_mdb_copy"),
            "mdb_drop" => env!("CARGO_BIN_EXE_mdb_drop"),
            other => panic!("no Rust tool {other}"),
        }),
    }
}

/// An argument list from strings or bytes.
pub fn args<S: AsRef<[u8]>>(a: &[S]) -> Vec<Vec<u8>> {
    a.iter().map(|s| s.as_ref().to_vec()).collect()
}

/// Splits a command line at spaces (no quoting).
pub fn words(s: &str) -> Vec<Vec<u8>> {
    s.split_whitespace()
        .map(|w| w.as_bytes().to_vec())
        .collect()
}

/// Runs `tool` of `imp` in `cwd` with `argv[0] = tool`, feeding `stdin`.
pub fn run(imp: Impl, tool: &str, argv: &[Vec<u8>], stdin: &[u8], cwd: &Path) -> Run {
    run_env(imp, tool, argv, stdin, cwd, &[])
}

/// As [`run`], with extra environment variables.
pub fn run_env(
    imp: Impl,
    tool: &str,
    argv: &[Vec<u8>],
    stdin: &[u8],
    cwd: &Path,
    env: &[(&str, &str)],
) -> Run {
    let mut cmd = command_as(&bin(imp, tool), tool);
    cmd.args(argv.iter().map(|a| os_arg(a)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("spawning {tool} ({imp:?}): {e}"));
    let mut child_stdin = child.stdin.take().unwrap();
    let input = stdin.to_vec();
    // Feed stdin from a thread: the tool may exit without reading it.
    let feeder = std::thread::spawn(move || {
        let _ = child_stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    let _ = feeder.join();
    Run {
        stdout: out.stdout,
        stderr: out.stderr,
        code: out.status.code().unwrap_or(-1),
    }
}

/// Runs the C and the Rust `tool` with the same arguments (C first) and
/// returns both results.
pub fn both(tool: &str, argv: &[Vec<u8>], stdin: &[u8], cwd: &Path) -> (Run, Run) {
    let c = run(Impl::C, tool, argv, stdin, cwd);
    let r = run(Impl::Rust, tool, argv, stdin, cwd);
    (c, r)
}

/// Asserts that the C and the Rust tool give identical stdout, stderr and
/// exit code (apart from the version line, see [`assert_runs_equal`]);
/// returns the result.
pub fn assert_same(tool: &str, argv: &[Vec<u8>], stdin: &[u8], cwd: &Path) -> Run {
    let (c, r) = both(tool, argv, stdin, cwd);
    assert_runs_equal(tool, argv, &c, &r);
    r
}

/// The line `-V` prints. The one intended difference between the tools:
/// the C tools print `MDB_VERSION_STRING` of `tests/lmdb-c/0.9/lmdb.h`
/// (`LMDB 0.9.36: (<date>)`), the Rust tools the string rlmdb's
/// `mdb_version` returns (`LMDB 0.9.85: (rlmdb <version>)`).
pub fn version_line(imp: Impl) -> Vec<u8> {
    let line = match imp {
        Impl::C => {
            let h = fs::read_to_string(c_src().join("lmdb.h")).unwrap();
            let field = |name: &str| {
                h.lines()
                    .find_map(|l| l.strip_prefix(&format!("#define {name}")))
                    .unwrap()
                    .trim()
                    .trim_matches('"')
                    .to_owned()
            };
            format!(
                "LMDB {}.{}.{}: ({})",
                field("MDB_VERSION_MAJOR"),
                field("MDB_VERSION_MINOR"),
                field("MDB_VERSION_PATCH"),
                field("MDB_VERSION_DATE")
            )
        }
        Impl::Rust => rlmdb::version_info().mdb_version_string(),
    };
    platform_lines(format!("{line}\n").as_bytes())
}

/// The usage synopsis of `mdb_load` as C 0.9 prints it, and as the Rust
/// tool does, which also has `-F 0.9|1.0` (the format of a new
/// environment; C 0.9 knows one format only). With `\n` line ends
/// ([`platform_lines`] gives what the tools write).
pub const LOAD_USAGE_C: &[u8] = b"[-V] [-a] [-f input] [-n] [-s name] [-N] [-T] dbpath\n";
pub const LOAD_USAGE_RUST: &[u8] =
    b"[-V] [-a] [-f input] [-F 0.9|1.0] [-n] [-s name] [-N] [-T] dbpath\n";

/// `c` as the Rust `tool` should print it: each standard output line that
/// is the C version line becomes the Rust one ([`version_line`]), and
/// `mdb_load`'s usage line names `-F` ([`LOAD_USAGE_RUST`]).
fn expected_of_rust(tool: &str, c: &Run) -> Run {
    let (c_line, rust_line) = (version_line(Impl::C), version_line(Impl::Rust));
    let mut stdout = Vec::with_capacity(c.stdout.len());
    for line in c.stdout.split_inclusive(|&b| b == b'\n') {
        stdout.extend_from_slice(if line == c_line { &rust_line } else { line });
    }
    let mut stderr = c.stderr.clone();
    if tool == "mdb_load" {
        if let Some(rest) = stderr
            .strip_suffix(platform_lines(LOAD_USAGE_C).as_slice())
            .filter(|head| head.ends_with(b"usage: mdb_load "))
        {
            stderr = [rest, &platform_lines(LOAD_USAGE_RUST)].concat();
        }
    }
    Run {
        stdout,
        stderr,
        ..c.clone()
    }
}

/// Asserts that two runs are identical, with a readable diff. The version
/// line is the exception: where C prints its version line, Rust must print
/// its own ([`version_line`]); so is `mdb_load`'s usage line, which also
/// names `-F`.
pub fn assert_runs_equal(tool: &str, argv: &[Vec<u8>], c: &Run, r: &Run) {
    if expected_of_rust(tool, c) != *r {
        let cmd: Vec<String> = argv
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        panic!(
            "{tool} {}: C and Rust differ\n--- C (exit {}) stdout:\n{}\n--- C stderr:\n{}\n\
             --- Rust (exit {}) stdout:\n{}\n--- Rust stderr:\n{}",
            cmd.join(" "),
            c.code,
            c.stdout_str(),
            c.stderr_str(),
            r.code,
            r.stdout_str(),
            r.stderr_str()
        );
    }
}

/// Runs a C tool and asserts it succeeded; returns its stdout.
pub fn c_ok(tool: &str, argv: &[Vec<u8>], cwd: &Path) -> Vec<u8> {
    let r = run(Impl::C, tool, argv, b"", cwd);
    assert_eq!(
        r.code,
        0,
        "C {tool} {:?} failed: {}",
        argv.iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect::<Vec<_>>(),
        r.stderr_str()
    );
    r.stdout
}

/// A fresh temporary directory for one test.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-tools-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// The OS page size, which is LMDB's default page size.
pub fn page_size() -> usize {
    if cfg!(windows) {
        // GetSystemInfo's dwPageSize on x86-64 Windows.
        return 4096;
    }
    let out = Command::new("getconf").arg("PAGESIZE").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Copies an environment directory (its data file only; the lock file is
/// recreated by the next opener) to `dst`.
pub fn copy_env(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    fs::copy(src.join("data.mdb"), dst.join("data.mdb")).unwrap();
}

// ---------------------------------------------------------------------------
// Crafted databases
// ---------------------------------------------------------------------------

/// One database of a crafted dump.
#[derive(Debug, Clone)]
pub struct DbSpec {
    /// `None` = the main DB.
    pub name: Option<Vec<u8>>,
    /// Flag names as in the dump header (`"dupsort"`, …).
    pub flags: Vec<&'static str>,
    pub records: Vec<(Vec<u8>, Vec<u8>)>,
}

impl DbSpec {
    pub fn new(name: Option<&[u8]>, flags: &[&'static str]) -> DbSpec {
        DbSpec {
            name: name.map(<[u8]>::to_vec),
            flags: flags.to_vec(),
            records: Vec::new(),
        }
    }
    pub fn rec(mut self, k: impl AsRef<[u8]>, v: impl AsRef<[u8]>) -> DbSpec {
        self.records
            .push((k.as_ref().to_vec(), v.as_ref().to_vec()));
        self
    }
}

/// Appends a dump header (in mdb_dump's layout) to `out`.
pub fn header(out: &mut Vec<u8>, db: &DbSpec, print: bool, mapsize: usize) {
    out.extend_from_slice(b"VERSION=3\n");
    out.extend_from_slice(if print {
        b"format=print\n"
    } else {
        b"format=bytevalue\n"
    });
    if let Some(name) = &db.name {
        out.extend_from_slice(b"database=");
        out.extend_from_slice(name);
        out.push(b'\n');
    }
    out.extend_from_slice(format!("type=btree\nmapsize={mapsize}\nmaxreaders=126\n").as_bytes());
    if db.flags.contains(&"dupsort") {
        out.extend_from_slice(b"duplicates=1\n");
    }
    for f in &db.flags {
        out.extend_from_slice(format!("{f}=1\n").as_bytes());
    }
    out.extend_from_slice(format!("db_pagesize={}\nHEADER=END\n", page_size()).as_bytes());
}

/// Appends an encoded data line.
pub fn data_line(out: &mut Vec<u8>, v: &[u8], print: bool) {
    out.push(b' ');
    for &c in v {
        if print && (0x20..=0x7e).contains(&c) {
            if c == b'\\' {
                out.push(b'\\');
            }
            out.push(c);
        } else {
            if print {
                out.push(b'\\');
            }
            out.extend_from_slice(format!("{c:02x}").as_bytes());
        }
    }
    out.push(b'\n');
}

/// The dump text of `dbs`.
pub fn dump_text(dbs: &[DbSpec], print: bool, mapsize: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for db in dbs {
        header(&mut out, db, print, mapsize);
        for (k, v) in &db.records {
            data_line(&mut out, k, print);
            data_line(&mut out, v, print);
        }
        out.extend_from_slice(b"DATA=END\n");
    }
    out
}

/// The dump text for the C `mdb_load`, which drops the header flags of
/// the first database (KNOWN_C_BUGS.md #1): an empty main-DB block goes
/// first, so every real database keeps its flags.
pub fn dump_text_for_c(dbs: &[DbSpec], print: bool, mapsize: usize) -> Vec<u8> {
    let mut out = dump_text(&[DbSpec::new(None, &[])], print, mapsize);
    out.extend(dump_text(dbs, print, mapsize));
    out
}

/// An empty database block with the environment settings (`mapsize=`,
/// `maxreaders=`, `format=`, …) of the first header of `dump`, for the
/// database `name` (`None` = the main DB). Put in front of a dump for the
/// C `mdb_load` so that it does not drop the first real database's flags
/// (KNOWN_C_BUGS.md #1).
pub fn empty_block_like(dump: &[u8], name: Option<&[u8]>) -> Vec<u8> {
    const FLAGS: &[&[u8]] = &[
        b"database=",
        b"duplicates=",
        b"reversekey=",
        b"dupsort=",
        b"integerkey=",
        b"dupfixed=",
        b"integerdup=",
        b"reversedup=",
    ];
    let mut out = Vec::new();
    for line in dump.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b"HEADER=END") {
            if let Some(name) = name {
                out.extend_from_slice(b"database=");
                out.extend_from_slice(name);
                out.push(b'\n');
            }
            out.extend_from_slice(b"HEADER=END\nDATA=END\n");
            return out;
        }
        if !FLAGS.iter().any(|f| line.starts_with(f)) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Loads `dbs` into a new environment at `dir` with the C `mdb_load`.
pub fn c_load(dir: &Path, dbs: &[DbSpec], mapsize: usize, extra_args: &[&str]) {
    fs::create_dir_all(dir).unwrap();
    let mut argv = args(extra_args);
    argv.push(dir.as_os_str().as_encoded_bytes().to_vec());
    let r = run(
        Impl::C,
        "mdb_load",
        &argv,
        &dump_text_for_c(dbs, false, mapsize),
        dir.parent().unwrap(),
    );
    assert_eq!(r.code, 0, "C mdb_load failed: {}", r.stderr_str());
}

fn u64_key(n: u64) -> Vec<u8> {
    n.to_ne_bytes().to_vec()
}

/// Bytes that need escaping in print mode, and some that do not.
pub fn awkward_bytes() -> Vec<Vec<u8>> {
    vec![
        b"\x00".to_vec(),
        b"\x00\x01".to_vec(),
        b"\\".to_vec(),
        b"\\\\x".to_vec(),
        b"a b".to_vec(),
        b"\x7f\x80\xff".to_vec(),
        b"~".to_vec(),
        b"\n".to_vec(),
        b"\r\n\t".to_vec(),
        b" leading space".to_vec(),
        b"DATA=END".to_vec(),
        (0..=255u8).collect(),
    ]
}

/// The main-DB-only environment: many records (several levels), keys and
/// values with every kind of byte, empty and overflow values.
pub fn spec_main() -> Vec<DbSpec> {
    let ps = page_size();
    let mut db = DbSpec::new(None, &[]);
    for i in 0..3000u32 {
        db = db.rec(
            format!("key{i:05}"),
            format!("value {i} {}", "x".repeat((i % 50) as usize)),
        );
    }
    for (i, b) in awkward_bytes().into_iter().enumerate() {
        db = db.rec(&b, &b);
        db = db.rec(format!("awkward-value-{i:02}"), &b);
    }
    db = db.rec("empty-value", b"");
    for (i, n) in [ps, 2 * ps + 1, 5 * ps + 100].into_iter().enumerate() {
        let v: Vec<u8> = (0..n).map(|j| (j * 7 + i) as u8).collect();
        db = db.rec(format!("overflow{i}"), v);
    }
    vec![db]
}

/// Named databases of every flag combination, plus main-DB records that
/// are not databases (a plain value, a key with a NUL byte).
pub fn spec_named() -> Vec<DbSpec> {
    let ps = page_size();
    let mut main = DbSpec::new(None, &[])
        .rec("zz-plain-value", "not a database")
        .rec(b"a\x00b", "key with NUL");
    main = main.rec("aa-plain", "x");
    let mut plain = DbSpec::new(Some(b"plain"), &[]);
    for i in 0..500u32 {
        plain = plain.rec(format!("p{i:04}"), format!("{i}"));
    }
    plain = plain.rec("big", vec![0x5au8; 3 * ps]);
    let mut dupsort = DbSpec::new(Some(b"dupsort"), &["dupsort"]);
    for i in 0..600u32 {
        dupsort = dupsort.rec("many", format!("dup{i:04}"));
    }
    for k in 0..20u32 {
        for d in 0..3u32 {
            dupsort = dupsort.rec(format!("few{k:02}"), format!("d{d}"));
        }
    }
    dupsort = dupsort.rec("one", "single").rec("awk", b"\x00\\\n\xff");
    let mut dupfixed = DbSpec::new(Some(b"dupfixed"), &["dupsort", "dupfixed"]);
    for i in 0..1000u64 {
        dupfixed = dupfixed.rec("many", format!("{i:08}"));
    }
    dupfixed = dupfixed.rec("two", "AAAAAAAA").rec("two", "BBBBBBBB");
    let mut intkey = DbSpec::new(Some(b"intkey"), &["integerkey"]);
    for i in (1..300u64).rev() {
        intkey = intkey.rec(u64_key(i * 1000), format!("n{i}"));
    }
    let mut intdup = DbSpec::new(Some(b"intdup"), &["dupsort", "dupfixed", "integerdup"]);
    for i in 0..400u32 {
        intdup = intdup.rec("k", (i * 3).to_ne_bytes());
    }
    let mut revkey = DbSpec::new(Some(b"revkey"), &["reversekey"]);
    for i in 0..200u32 {
        revkey = revkey.rec(format!("{i}-rev"), format!("{i}"));
    }
    let mut revdup = DbSpec::new(Some(b"revdup"), &["dupsort", "reversedup"]);
    for i in 0..50u32 {
        revdup = revdup.rec("k", format!("{i}x"));
    }
    let mut combo = DbSpec::new(
        Some(b"combo"),
        &["reversekey", "dupsort", "dupfixed", "reversedup"],
    );
    for i in 0..30u32 {
        combo = combo.rec(format!("k{}", i % 3), format!("{i:04}"));
    }
    let empty = DbSpec::new(Some(b"empty"), &[]);
    let odd_name = DbSpec::new(Some(b"sp ace=eq"), &[]).rec("k", "v");
    let non_utf8 = DbSpec::new(Some(b"n\xffme"), &["dupsort"])
        .rec("k", "1")
        .rec("k", "2");
    vec![
        main, plain, dupsort, dupfixed, intkey, intdup, revkey, revdup, combo, empty, odd_name,
        non_utf8,
    ]
}

/// A main DB with `DUP_SORT`.
pub fn spec_dupmain() -> Vec<DbSpec> {
    let mut db = DbSpec::new(None, &["dupsort"]);
    for k in 0..40u32 {
        for d in 0..(k % 7 + 1) {
            db = db.rec(format!("k{k:03}"), format!("v{d}"));
        }
    }
    vec![db]
}

/// The test environments, created once per test binary in a shared
/// directory. Tests must not modify them: use [`env_copy`].
pub struct Envs {
    pub root: PathBuf,
    /// Name -> environment directory (or file, for `nosubdir`).
    pub envs: BTreeMap<&'static str, PathBuf>,
}

/// Names of the environments in [`envs`] that are directories.
pub const ENV_NAMES: &[&str] = &[
    "main", "named", "dupmain", "empty", "mtest", "mtest2", "mtest3", "mtest4", "mtest5",
];

/// The shared test environments (see [`Envs`]).
pub fn envs() -> &'static Envs {
    static ENVS: OnceLock<Envs> = OnceLock::new();
    ENVS.get_or_init(|| {
        // One directory per test binary ("stat-<hash>" -> "stat").
        let exe = std::env::current_exe().unwrap();
        let stem = exe.file_stem().unwrap().to_string_lossy().into_owned();
        let stem = stem.split('-').next().unwrap_or("tests").to_owned();
        let root =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rlmdb-tools-envs-{stem}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut envs = BTreeMap::new();
        let mapsize = 32 << 20;
        for (name, spec) in [
            ("main", spec_main()),
            ("named", spec_named()),
            ("dupmain", spec_dupmain()),
            ("empty", vec![DbSpec::new(None, &[])]),
        ] {
            let dir = root.join(name);
            c_load(&dir, &spec, mapsize, &[]);
            envs.insert(name, dir);
        }
        // A data file without a directory (MDB_NOSUBDIR).
        let nosub = root.join("nosubdir.mdb");
        let r = run(
            Impl::C,
            "mdb_load",
            &args(&["-n".as_bytes(), nosub.as_os_str().as_encoded_bytes()]),
            &dump_text_for_c(&spec_dupmain(), false, mapsize),
            &root,
        );
        assert_eq!(r.code, 0, "{}", r.stderr_str());
        envs.insert("nosubdir", nosub);
        for prog in ["mtest", "mtest2", "mtest3", "mtest4", "mtest5"] {
            let dir = root.join(format!("{prog}-run"));
            fs::create_dir_all(dir.join("testdb")).unwrap();
            let out = Command::new(c_dir().join(prog))
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{prog}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            envs.insert(prog, dir.join("testdb"));
        }
        Envs { root, envs }
    })
}

/// A private copy of test environment `name` in `dir` (for tests that
/// write, or that need the reader table to themselves).
pub fn env_copy(name: &str, dir: &Path) -> PathBuf {
    let src = &envs().envs[name];
    if name == "nosubdir" {
        let dst = dir.join("nosubdir.mdb");
        fs::copy(src, &dst).unwrap();
        dst
    } else {
        let dst = dir.join(name);
        copy_env(src, &dst);
        dst
    }
}

/// The C `mdb_dump -a` plus the main DB dump of `env`, with `mapaddr=`
/// lines removed (they record where a `MDB_FIXEDMAP` creator mapped the
/// file, which differs between runs): a complete picture of the contents.
pub fn c_dump_all(env: &Path, extra: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for mode in [&[][..], &["-a"][..]] {
        let mut argv = args(extra);
        argv.extend(args(mode));
        argv.push(env.as_os_str().as_encoded_bytes().to_vec());
        let r = run(Impl::C, "mdb_dump", &argv, b"", env.parent().unwrap());
        out.extend_from_slice(&r.stdout);
        out.extend_from_slice(&r.stderr);
    }
    strip_mapaddr(&out)
}

/// `text` without `mapaddr=` lines.
pub fn strip_mapaddr(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.split_inclusive(|&b| b == b'\n') {
        if !line.starts_with(b"mapaddr=") {
            out.extend_from_slice(line);
        }
    }
    out
}

/// An absolute path as an argument.
pub fn p(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}
