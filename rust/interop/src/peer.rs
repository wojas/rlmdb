//! Running peers: one-shot commands ([`PeerCmd`]) and long-running processes
//! ([`PeerProcess`]).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{bins, Error, Result, TestEnv};

/// One implementation of the peer command protocol (see `README.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Peer {
    /// `cpeer`: C, linked against `liblmdb` built from `libraries/liblmdb`.
    C,
    /// `rpeer`: Rust, built on the `lmdb` crate (added in stage 2).
    Rust,
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Peer::C => "C",
            Peer::Rust => "Rust",
        })
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
            candidates.push(profile_dir.join("rpeer"));
        }
    }
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        Error::new(
            "the Rust peer `rpeer` is not built yet (it is added in stage 2); \
             set INTEROP_RPEER to its path or add a [[bin]] rpeer to the interop crate",
        )
    })
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
    /// Both peers.
    pub const ALL: [Peer; 2] = [Peer::C, Peer::Rust];

    /// The peer executable.
    pub fn bin(&self) -> Result<PathBuf> {
        match self {
            Peer::C => Ok(bins::cpeer().to_path_buf()),
            Peer::Rust => rpeer_path(),
        }
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
    /// environment's options (e.g. `--nosubdir`) are appended last.
    pub fn on(self, command: &str, env: &TestEnv) -> PeerCmd {
        let mut cmd = self.cmd(command);
        cmd.args.push(env.path().into());
        cmd.env_args = env.peer_options();
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
        run_command(self.command()?, label, None, self.timeout)
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
    pub fn ok(self) -> Result<Self> {
        if self.success() {
            Ok(self)
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
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
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

    /// Close stdin, wait for the process to exit (killing it after the
    /// command's timeout) and return its output, whatever the exit status.
    pub fn finish(mut self) -> Result<Output> {
        let timeout = self.timeout;
        self.finish_inner(timeout)
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
