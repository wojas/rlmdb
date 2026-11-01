//! Running peers: one-shot commands ([`PeerCmd`]) and long-running processes
//! ([`PeerProcess`]).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{bins, Error, Format, Result, TestEnv};

/// One implementation of the peer command protocol (see `README.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Peer {
    /// `cpeer`: C, linked against `liblmdb` built from `tests/lmdb-c/0.9`
    /// (LMDB 0.9; opens 0.9 environments only).
    C,
    /// `rpeer`: Rust, built on the `rlmdb` crate (`src/bin/rpeer`). Both
    /// formats.
    Rust,
    /// `cpeer_capi`: the same C program as [`Peer::C`], linked against the
    /// Rust `liblmdb.a` of `rlmdb-capi` instead of the C library (see
    /// [`bins::cpeer_capi`]). Both formats (`rlmdb_env_set_format`).
    COnRust,
    /// `cpeer10`: the same C program as [`Peer::C`], linked against the C
    /// library of LMDB 1.0 ([`bins::cpeer10`]): the reference for 1.0
    /// environments, which are the only ones it opens.
    C10,
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Peer::C => "C",
            Peer::Rust => "Rust",
            Peer::COnRust => "COnRust",
            Peer::C10 => "C10",
        })
    }
}

/// The output of a program with `\n` line ends: on Windows the C tools
/// (and the Rust tools, which do as they do) write through the C runtime's
/// text mode, which ends lines with `\r\n`. (`rlmdb-tools`' own tests
/// compare the bytes.)
fn text_lines(s: String) -> String {
    if cfg!(windows) && s.contains("\r\n") {
        s.replace("\r\n", "\n")
    } else {
        s
    }
}

/// Report something a test did not do, or only partly did, on the test's
/// real stderr (libtest captures `print!`/`eprint!`, not direct writes), so
/// that it shows up in a normal `cargo test` run: `[interop] SKIPPED ...`.
pub fn skipped(msg: impl fmt::Display) {
    let _ = writeln!(std::io::stderr().lock(), "[interop] SKIPPED {msg}");
}

/// A peer that completes a command with parts left out says so on stderr
/// with a `NOT-IMPLEMENTED` line (e.g. rpeer's `nested-test` while
/// `rlmdb` had no `mdb_drop`); pass those on as SKIPPED notes. (A
/// command that fails as not implemented is reported where it fails.)
fn report_partial(out: &Output) {
    if !out.success() {
        return;
    }
    for line in out.stderr.lines().filter(|l| l.contains("NOT-IMPLEMENTED")) {
        skipped(format_args!("partly: `{}`: {line}", out.label));
    }
}

static RPEER: OnceLock<PathBuf> = OnceLock::new();

/// Tell the harness where the `rpeer` executable is.
///
/// Cargo sets `CARGO_BIN_EXE_rpeer` when compiling the integration tests of
/// a package that has a `[[bin]] rpeer`; [`scenario_tests!`](crate::scenario_tests)
/// passes `option_env!("CARGO_BIN_EXE_rpeer")` here. `None` is ignored, and
/// so is every call after the first.
pub fn register_rpeer(path: Option<&str>) {
    if let Some(p) = path {
        let _ = RPEER.set(PathBuf::from(p));
    }
}

/// Where `rpeer` is, in order: `$INTEROP_RPEER` at run time, the path given
/// to [`register_rpeer`], `$INTEROP_RPEER` at build time, or next to the
/// running test executable (`target/<profile>/rpeer`).
fn rpeer_path() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("INTEROP_RPEER") {
        candidates.push(p.into());
    }
    if let Some(p) = RPEER.get() {
        candidates.push(p.clone());
    }
    if let Some(p) = option_env!("INTEROP_RPEER") {
        candidates.push(p.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/<profile>/deps/<test>-<hash> -> target/<profile>/rpeer
        if let Some(profile_dir) = exe.parent().and_then(|deps| deps.parent()) {
            candidates.push(profile_dir.join(format!("rpeer{}", std::env::consts::EXE_SUFFIX)));
        }
    }
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        Error::new(
            "the Rust peer `rpeer` is not built: run `cargo build -p rlmdb-interop --bin rpeer` \
             or set INTEROP_RPEER to its path",
        )
    })
}

/// Exit status of a peer that does not implement a command (yet): `rpeer`
/// for a command that needs a part of `rlmdb` that does not exist yet.
pub const EXIT_NOT_IMPLEMENTED: i32 = 4;

/// `strerror(ENOSYS)` on macOS and glibc: what `cpeer` prints when a call
/// into a not yet implemented part of the Rust C API (`rlmdb-capi`) fails.
const ENOSYS_TEXT: &str = "Function not implemented";

/// The answer of `<peer> commands`: `(command, implemented)` pairs.
type CommandTable = Option<Vec<(String, bool)>>;

/// Ask `peer` which commands it implements (`<peer> commands` prints
/// `command=<name> implemented=<0|1>` lines). `None` if it cannot be asked.
fn query_commands(peer: Peer) -> CommandTable {
    let out = peer.cmd("commands").run().ok()?;
    let table = out
        .lines()
        .filter_map(|line| {
            let kv = crate::report::Kv::from_lines([line]);
            Some((kv.get("command")?.to_owned(), kv.get("implemented")? == "1"))
        })
        .collect();
    Some(table)
}

/// Seconds a peer command may run before it is killed: `$INTEROP_TIMEOUT_SECS`
/// or 120.
pub(crate) fn default_timeout() -> Duration {
    let secs = std::env::var("INTEROP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(120.0);
    Duration::from_secs_f64(secs)
}

impl Peer {
    /// The three peers of LMDB 0.9 environments ([`Peer::all`] of 0.9).
    pub const ALL: [Peer; 3] = [Peer::C, Peer::Rust, Peer::COnRust];

    /// The three peers that open environments of `format`: the C peer of
    /// that release ([`Peer::reference`]), Rust and COnRust.
    pub fn all(format: Format) -> [Peer; 3] {
        [Peer::reference(format), Peer::Rust, Peer::COnRust]
    }

    /// The C peer of `format`'s release, the oracle of scenarios on such
    /// environments: [`Peer::C`] for 0.9, [`Peer::C10`] for 1.0.
    pub fn reference(format: Format) -> Peer {
        match format {
            Format::V09 => Peer::C,
            Format::V10 => Peer::C10,
        }
    }

    /// Can this peer open environments of `format`? The C peers only open
    /// their own release's; the peers on rlmdb open both.
    pub fn handles(&self, format: Format) -> bool {
        match self {
            Peer::C => format == Format::V09,
            Peer::C10 => format == Format::V10,
            Peer::Rust | Peer::COnRust => true,
        }
    }

    /// Does this peer implement `command`? Asks the peer once per process
    /// (`<peer> commands`) and caches the answer.
    ///
    /// If the peer cannot be asked (not built, crashed), the answer is
    /// `true`: the real command then runs and reports the real problem,
    /// instead of a scenario silently skipping work.
    pub fn supports(&self, command: &str) -> bool {
        static C: OnceLock<CommandTable> = OnceLock::new();
        static RUST: OnceLock<CommandTable> = OnceLock::new();
        static CONRUST: OnceLock<CommandTable> = OnceLock::new();
        static C10: OnceLock<CommandTable> = OnceLock::new();
        let cell = match self {
            Peer::C => &C,
            Peer::Rust => &RUST,
            Peer::COnRust => &CONRUST,
            Peer::C10 => &C10,
        };
        match cell.get_or_init(|| query_commands(*self)) {
            None => true,
            Some(table) => table.iter().any(|(name, yes)| name == command && *yes),
        }
    }

    /// The peer executable.
    pub fn bin(&self) -> Result<PathBuf> {
        match self {
            Peer::C => Ok(bins::cpeer().to_path_buf()),
            Peer::Rust => rpeer_path(),
            Peer::COnRust => Ok(bins::cpeer_capi()?.to_path_buf()),
            Peer::C10 => Ok(bins::cpeer10().to_path_buf()),
        }
    }

    /// Is this peer built on the Rust library (`rpeer`, or `cpeer` on
    /// `rlmdb-capi`)?
    pub fn is_rust(&self) -> bool {
        matches!(self, Peer::Rust | Peer::COnRust)
    }

    /// A command without an environment argument (e.g. `gen`).
    pub fn cmd(self, command: &str) -> PeerCmd {
        PeerCmd {
            peer: self,
            args: vec![command.into()],
            env_args: Vec::new(),
            timeout: default_timeout(),
        }
    }

    /// `<command> <env path> ... <env options>`: a command on `env`. The
    /// environment's options (e.g. `--nosubdir`, `--format 1.0`) are
    /// appended last. [`Peer::C10`] on a 0.9 environment gets `--format
    /// 0.9`, which it refuses with a usage error (exit 2) instead of
    /// re-initialising the environment's lock file for 1.0.
    pub fn on(self, command: &str, env: &TestEnv) -> PeerCmd {
        let mut cmd = self.cmd(command);
        cmd.args.push(env.path().into());
        cmd.env_args = env.peer_options();
        if self == Peer::C10 && env.format() == Format::V09 {
            cmd.env_args.extend(["--format".into(), "0.9".into()]);
        }
        cmd
    }
}

/// A peer command line being built; run it with [`run`](Self::run),
/// [`output`](Self::output) or [`spawn`](Self::spawn).
#[derive(Clone, Debug)]
pub struct PeerCmd {
    peer: Peer,
    args: Vec<OsString>,
    env_args: Vec<OsString>,
    timeout: Duration,
}

impl PeerCmd {
    /// Append an argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Append arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    /// Kill the peer if it runs longer than this (default: see
    /// `INTEROP_TIMEOUT_SECS` in `README.md`).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The peer this command runs on.
    pub fn peer(&self) -> Peer {
        self.peer
    }

    /// `"<peer> <args...>"`, for messages.
    pub fn label(&self) -> String {
        let mut s = self.peer.to_string();
        for a in self.args.iter().chain(&self.env_args) {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }

    fn command(&self) -> Result<Command> {
        let mut cmd = Command::new(self.peer.bin()?);
        cmd.args(&self.args).args(&self.env_args);
        Ok(cmd)
    }

    /// Run to completion and require exit status 0.
    pub fn run(self) -> Result<Output> {
        self.output()?.ok()
    }

    /// Run to completion, whatever the exit status.
    pub fn output(self) -> Result<Output> {
        let label = self.label();
        let out = run_command(self.command()?, label, None, self.timeout)?;
        report_partial(&out);
        Ok(out)
    }

    /// Start the peer in the background (for `hold-reader`, `writer-loop`,
    /// `reader-loop`). Its stdin is a pipe; [`PeerProcess::release`] writes
    /// a line to it.
    pub fn spawn(self) -> Result<PeerProcess> {
        PeerProcess::spawn(self.command()?, self.label(), self.timeout)
    }
}

/// The result of a finished process.
#[derive(Debug)]
pub struct Output {
    /// The command line, for messages.
    pub label: String,
    /// Exit status.
    pub status: ExitStatus,
    /// Everything written to stdout.
    pub stdout: String,
    /// Everything written to stderr.
    pub stderr: String,
    /// Wall-clock run time.
    pub elapsed: Duration,
}

impl Output {
    /// Did the process exit with status 0?
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// The exit code (`None` if killed by a signal).
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// `Ok(self)` if the process succeeded, else an error describing it.
    /// Exit status 4 ([`EXIT_NOT_IMPLEMENTED`]), and an LMDB call failing
    /// with `ENOSYS` (a part of the Rust C API that is not implemented
    /// yet, seen through [`Peer::COnRust`]), give an error that
    /// [`is_not_implemented`](Error::is_not_implemented).
    pub fn ok(self) -> Result<Self> {
        if self.success() {
            Ok(self)
        } else if self.code() == Some(EXIT_NOT_IMPLEMENTED) {
            Err(Error::not_implemented(format!(
                "NOT-IMPLEMENTED command not implemented by the peer: {self}"
            )))
        } else if self.code() == Some(3) && self.stderr.contains(ENOSYS_TEXT) {
            Err(Error::not_implemented(format!(
                "NOT-IMPLEMENTED an LMDB call returned ENOSYS (not implemented in rlmdb-capi \
                 yet): {self}"
            )))
        } else {
            Err(Error::new(format!("command failed: {self}")))
        }
    }

    /// The stdout lines.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines()
    }
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` exited with {} after {:.2?}",
            self.label, self.status, self.elapsed
        )?;
        write!(f, "\n--- stdout ---\n{}", tail(&self.stdout, 60))?;
        write!(f, "--- stderr ---\n{}", tail(&self.stderr, 60))
    }
}

/// The last `n` lines of `s`.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let skip = lines.len().saturating_sub(n);
    let mut out = String::new();
    if skip > 0 {
        out.push_str(&format!("[... {skip} lines ...]\n"));
    }
    for l in &lines[skip..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn reader_thread<R: Read + Send + 'static>(mut r: R) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Wait for `child` to exit, killing it after `timeout`.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Run `cmd` to completion with optional stdin data and a timeout.
pub(crate) fn run_command(
    mut cmd: Command,
    label: String,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    let start = Instant::now();
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::new(format!("cannot start `{label}`: {e}")))?;
    let out = reader_thread(child.stdout.take().expect("piped stdout"));
    let err = reader_thread(child.stderr.take().expect("piped stderr"));
    if let Some(data) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let data = data.to_vec();
        // A writer thread, so a child that does not read cannot block us.
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
        });
    }
    let status = wait_with_timeout(&mut child, timeout)?;
    let stdout = text_lines(out.join().unwrap_or_default());
    let stderr = text_lines(err.join().unwrap_or_default());
    match status {
        Some(status) => Ok(Output {
            label,
            status,
            stdout,
            stderr,
            elapsed: start.elapsed(),
        }),
        None => Err(Error::new(format!(
            "`{label}` timed out after {timeout:?} and was killed\n--- stdout ---\n{}--- stderr ---\n{}",
            tail(&stdout, 40),
            tail(&stderr, 40)
        ))),
    }
}

/// A peer running in the background. Dropping it kills the process.
pub struct PeerProcess {
    label: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    seen: Vec<String>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<String>>,
    timeout: Duration,
    started: Instant,
    finished: bool,
}

impl PeerProcess {
    /// Start `bin args...` in the background like [`PeerCmd::spawn`], for
    /// a program that speaks the peer protocol but is not one of the
    /// [`Peer`]s, such as the LMDB 1.0 C peer [`bins::cpeer10`].
    pub fn spawn_program<S: AsRef<OsStr>>(bin: &Path, args: &[S]) -> Result<Self> {
        let mut cmd = Command::new(bin);
        cmd.args(args);
        let name = bin.file_name().unwrap_or_default().to_string_lossy();
        let label = std::iter::once(name.into_owned())
            .chain(
                args.iter()
                    .map(|a| a.as_ref().to_string_lossy().into_owned()),
            )
            .collect::<Vec<_>>()
            .join(" ");
        Self::spawn(cmd, label, default_timeout())
    }

    fn spawn(mut cmd: Command, label: String, timeout: Duration) -> Result<Self> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| Error::new(format!("cannot start `{label}`: {e}")))?;
        let (tx, rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr_thread = reader_thread(child.stderr.take().expect("piped stderr"));
        Ok(PeerProcess {
            label,
            stdin: child.stdin.take(),
            child,
            lines: rx,
            seen: Vec::new(),
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            timeout,
            started: Instant::now(),
            finished: false,
        })
    }

    /// The process id (as recorded in the reader table).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The command line.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Wait until the peer prints a line starting with `prefix`; return it.
    pub fn wait_for_line(&mut self, prefix: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.starts_with(prefix) {
                        return Ok(line);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::new(format!(
                        "`{}` did not print {prefix:?} within {timeout:?}; stdout so far:\n{}",
                        self.label,
                        self.seen.join("\n")
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // stdout closed: the process ended. Report how.
                    let label = self.label.clone();
                    let out = self.finish_inner(Duration::from_secs(10))?;
                    return Err(Error::new(format!(
                        "`{label}` ended before printing {prefix:?}: {out}"
                    )));
                }
            }
        }
    }

    /// Wait for the `READY` line (default timeout 60 s); return it.
    pub fn wait_ready(&mut self) -> Result<String> {
        self.wait_for_line("READY", Duration::from_secs(60))
    }

    /// Write a newline to the peer's stdin and close it: ends the wait of
    /// `hold-reader --until-stdin`.
    pub fn release(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            // The peer may already have exited; that is not our concern here.
            let _ = stdin.write_all(b"\n");
        }
        Ok(())
    }

    /// Kill the process with `SIGKILL` (no chance to clean up: its reader
    /// slot stays in the lock file) and wait until it is gone.
    pub fn kill(mut self) -> Result<()> {
        self.child.kill()?;
        self.child.wait()?;
        self.finished = true;
        Ok(())
    }

    /// [`kill`](Self::kill), but on Windows keep a handle to the dead
    /// process until the returned value is dropped (see [`DeadProcess`]).
    pub fn kill_keeping_pid(self) -> Result<DeadProcess> {
        #[cfg(windows)]
        let handle = {
            use std::os::windows::io::AsHandle;
            self.child.as_handle().try_clone_to_owned()?
        };
        let pid = self.pid();
        self.kill()?;
        Ok(DeadProcess {
            pid,
            #[cfg(windows)]
            _handle: handle,
        })
    }

    /// Whether the process is still running.
    pub fn running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }

    /// Close stdin, wait for the process to exit (killing it after the
    /// command's timeout) and return its output, whatever the exit status.
    pub fn finish(mut self) -> Result<Output> {
        let timeout = self.timeout;
        let out = self.finish_inner(timeout)?;
        report_partial(&out);
        Ok(out)
    }

    fn finish_inner(&mut self, timeout: Duration) -> Result<Output> {
        self.stdin = None;
        let status = wait_with_timeout(&mut self.child, timeout)?;
        self.finished = true;
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        self.seen.extend(self.lines.try_iter());
        let mut stdout = self.seen.join("\n");
        if !stdout.is_empty() {
            stdout.push('\n');
        }
        let stderr = self
            .stderr_thread
            .take()
            .map(|t| t.join().unwrap_or_default())
            .unwrap_or_default();
        let stderr = text_lines(stderr);
        match status {
            Some(status) => Ok(Output {
                label: self.label.clone(),
                status,
                stdout,
                stderr,
                elapsed: self.started.elapsed(),
            }),
            None => Err(Error::new(format!(
                "`{}` timed out after {timeout:?} and was killed\n--- stdout ---\n{}--- stderr ---\n{}",
                self.label,
                tail(&stdout, 40),
                tail(&stderr, 40)
            ))),
        }
    }
}

impl Drop for PeerProcess {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A peer killed by [`PeerProcess::kill_keeping_pid`]. On Windows it holds a
/// handle to the dead process: the process id stays taken until the handle
/// is closed (Windows gives ids to new processes soon after), and C's
/// reader check counts the process as alive meanwhile (`KNOWN_C_BUGS.md`
/// #42). Elsewhere the process is reaped and only its id is kept.
pub struct DeadProcess {
    pid: u32,
    #[cfg(windows)]
    _handle: std::os::windows::io::OwnedHandle,
}

impl DeadProcess {
    /// The process id the dead process had.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Close the handle (as dropping the value does): the process id is
    /// free for new processes, and C's reader check sees the process gone.
    pub fn close(self) {}
}
