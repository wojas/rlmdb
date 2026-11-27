//! What the tests do differently on Unix and Windows: positional file I/O,
//! running a program under another `argv[0]`, and naming the process that
//! has a process id.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `pread` of all of `buf` at `off` (`FileExt::read_exact_at` on Unix,
/// `seek_read` on Windows).
pub fn read_exact_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::read_exact_at(file, buf, off);
    #[cfg(windows)]
    {
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_read(file, buf, off)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// `pwrite` of all of `buf` at `off`.
pub fn write_all_at(file: &File, buf: &[u8], off: u64) -> io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::FileExt::write_all_at(file, buf, off);
    #[cfg(windows)]
    {
        let (mut buf, mut off) = (buf, off);
        while !buf.is_empty() {
            match std::os::windows::fs::FileExt::seek_write(file, buf, off)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                n => {
                    buf = &buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }
}

/// A command that runs `exe` with `argv[0] = name`, so that the program's
/// messages name it as `name` (`arg0` on Unix).
///
/// On Windows `argv[0]` is the program as named: the command runs `name`,
/// from a directory put first on the child's `PATH` that has it as
/// `name.exe`: `exe`'s own directory if that is its file name, else a
/// directory next to it with a copy of `exe` under that name (the C 1.0
/// tools are `mdb_stat10.exe` & co.).
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
        let dir = match named_copy_dir(exe, name) {
            Ok(dir) => dir,
            Err(e) => panic!("copying {} as {name}: {e}", exe.display()),
        };
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

/// The directory in which `name.exe` is `exe` (see [`command_as`]).
#[cfg(windows)]
fn named_copy_dir(exe: &Path, name: &str) -> io::Result<std::path::PathBuf> {
    let parent = exe.parent().unwrap_or(Path::new("."));
    if exe.file_stem().and_then(|s| s.to_str()) == Some(name) {
        return Ok(parent.to_path_buf());
    }
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned());
    let dir = parent.join(format!("as-{name}-{}", stem.unwrap_or_default()));
    let target = dir.join(format!("{name}.exe"));
    let source = if exe.extension().is_some() {
        exe.to_path_buf()
    } else {
        exe.with_extension("exe")
    };
    let fresh = |t: &Path| -> bool {
        let (Ok(a), Ok(b)) = (std::fs::metadata(&source), std::fs::metadata(t)) else {
            return false;
        };
        a.len() == b.len() && a.modified().ok() <= b.modified().ok()
    };
    if !fresh(&target) {
        std::fs::create_dir_all(&dir)?;
        // Copied under a temporary name and renamed, for concurrent tests
        // (threads of one test binary too, hence the counter); a copy that
        // is running cannot be replaced, and is the same file.
        static COPIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COPIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!("{name}.{}.{n}.tmp", std::process::id()));
        std::fs::copy(&source, &tmp)?;
        if std::fs::rename(&tmp, &target).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !target.exists() {
                return Err(io::Error::other("copy vanished"));
            }
        }
    }
    Ok(dir)
}

/// What the system says about process `pid` while it runs: the process
/// itself with its state, and on Windows also the processes it started,
/// any crash reporter (`WerFault`), and what each of its threads waits
/// for. For the message about a command that ran into its time limit,
/// taken before the command is killed: it tells a process that waits for
/// a lock or a pipe from one that is busy or has crashed. Empty if the
/// listing program cannot be run or does not finish within 60 seconds
/// (it may hang on the same cause as the process).
pub fn process_report(pid: u32) -> String {
    #[cfg(windows)]
    let cmd = {
        let script = format!(
            "Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -eq {pid} -or \
             $_.ParentProcessId -eq {pid} -or $_.Name -like 'WerFault*' }} | ForEach-Object {{ \
             '{{0}} (parent {{1}}) {{2}}: {{3}}' -f $_.ProcessId, $_.ParentProcessId, $_.Name, \
             $_.CommandLine }}; (Get-Process -Id {pid}).Threads | ForEach-Object {{ \
             '  thread {{0}}: {{1}} {{2}}' -f $_.Id, $_.ThreadState, $_.WaitReason }}"
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        cmd
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut cmd = Command::new("ps");
        cmd.args(["-o", "pid,ppid,state,etime,command", "-p", &pid.to_string()]);
        cmd
    };
    let out = stdout_within(cmd, Duration::from_secs(60));
    String::from_utf8_lossy(&out).trim_end().to_owned()
}

/// What `cmd` writes to stdout, if it finishes within `limit`; it is
/// killed then. Empty if it cannot be started or does not finish.
fn stdout_within(mut cmd: Command, limit: Duration) -> Vec<u8> {
    let Ok(mut child) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    // The output is complete when the pipe closes; then, or at the limit,
    // the process is stopped if it has not exited by itself.
    let deadline = Instant::now() + limit;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    kill_within(&mut child, Duration::from_secs(5));
    if reader.is_finished() {
        reader.join().unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Kill `child` (if it is still running) and reap it, waiting at most
/// `limit` for it to go: a process blocked inside the system, on a file
/// system that no longer answers for example, may not end when killed,
/// and an unbounded wait would hang the caller with it. Whether it is gone.
pub(crate) fn kill_within(child: &mut Child, limit: Duration) -> bool {
    let _ = child.kill();
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The program running as process `pid` now (`ps`; `tasklist` on
/// Windows), `None` if there is none: for messages about a reader slot
/// whose process should be gone, as Windows recycles process ids quickly.
pub fn process_name(pid: u32) -> Option<String> {
    #[cfg(windows)]
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    #[cfg(not(windows))]
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    // tasklist prints a line starting with "INFO:" (not CSV) for no match.
    (!text.is_empty() && !text.starts_with("INFO:")).then_some(text)
}
