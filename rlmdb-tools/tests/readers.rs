//! `mdb_stat -r`/`-rr` with readers in the table: a C process holding a
//! read transaction, a Rust read transaction in this test process, and the
//! stale slot of a killed reader. The Rust tool prints exactly what the C
//! tool prints; only the exit code of `-r` alone differs (C exits 1,
//! KNOWN_C_BUGS.md #2; not on Windows, see [`C_R_EXIT`]).

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use common::*;

/// A C process with the environment open (and, with `-r`, a read
/// transaction); it exits when dropped.
struct Holder {
    child: Child,
}

impl Holder {
    fn start(env: &Path, read_txn: bool) -> Holder {
        let mut cmd = Command::new(c_dir().join("holder"));
        if read_txn {
            cmd.arg("-r");
        }
        let mut child = cmd
            .arg(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("READY"), "holder: {line:?}");
        Holder { child }
    }

    /// Kills the process (SIGKILL): its reader slot stays in the table.
    /// Dropping it closes the process handle too: Windows keeps a dead
    /// process's object while a handle to it is open, and C's reader check
    /// would count it as alive (`KNOWN_C_BUGS.md` #38).
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// The exit code of C's `mdb_stat -r` without `-a`/`-s`/`-f`: that of the
/// last `fputs` of the reader list (KNOWN_C_BUGS.md #2), which returns a
/// positive count with glibc and macOS, 0 with the Windows C runtime.
const C_R_EXIT: i32 = if cfg!(windows) { 0 } else { 1 };

/// Runs `opts` with both tools; stdout and stderr must be identical;
/// returns the exit codes (C, Rust) and Rust's output as text.
fn compare(env: &Path, opts: &str) -> (i32, i32, String) {
    let mut argv = words(opts);
    argv.push(p(env));
    let (c, r) = both("mdb_stat", &argv, b"", env.parent().unwrap());
    assert_eq!(
        (c.stdout_str(), c.stderr_str()),
        (r.stdout_str(), r.stderr_str()),
        "mdb_stat {opts}"
    );
    (c.code, r.code, r.stdout_text())
}

#[test]
fn live_readers() {
    let dir = tempdir();
    let env = env_copy("named", dir.path());
    let _holder = Holder::start(&env, true);
    let (c, r, out) = compare(&env, "-r");
    assert_eq!((c, r), (C_R_EXIT, 0));
    assert!(out.contains("    pid     thread     txnid\n"), "{out}");
    assert_eq!(compare(&env, "-rr").0, C_R_EXIT);
    for opts in ["-r -a", "-er -s plain", "-rr -f", "-e"] {
        let (c, r, _) = compare(&env, opts);
        assert_eq!((c, r), (0, 0), "{opts}");
    }

    // Add a Rust reader (this process) next to the C one.
    let renv = rlmdb::Environment::builder()
        .flags(rlmdb::EnvironmentFlags::READ_ONLY)
        .open(&env)
        .unwrap();
    let txn = renv.begin_ro_txn().unwrap();
    let (_, _, out) = compare(&env, "-r");
    assert_eq!(out.lines().count(), 4, "{out}");
    assert!(
        out.contains(&format!("{:10} ", std::process::id())),
        "{out}"
    );
    compare(&env, "-er");
    // A slot kept but not in a transaction shows "-".
    let inactive = txn.reset();
    let (_, _, out) = compare(&env, "-r");
    assert!(out.contains(" -\n"), "{out}");
    drop(inactive);
}

/// A reader killed while in a transaction leaves a stale slot; `-rr`
/// clears it. Both tools start from the same lock file state.
#[test]
fn stale_reader() {
    /// Puts the lock file back as `snapshot`. On Windows, where the file is
    /// mapped and its byte 0 locked by the keeper, in place and from the
    /// header's second word on (the magic does not change).
    fn restore(lock: &std::path::Path, snapshot: &[u8]) {
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            let f = fs::OpenOptions::new().write(true).open(lock).unwrap();
            let mut off = 4;
            while off < snapshot.len() {
                off += f.seek_write(&snapshot[off..], off as u64).unwrap();
            }
        }
        #[cfg(not(windows))]
        fs::write(lock, snapshot).unwrap();
    }
    let dir = tempdir();
    let env = env_copy("main", dir.path());
    // Keeps the environment open, so the tools are not the exclusive
    // opener (which would reset the reader table).
    let _keeper = Holder::start(&env, false);
    Holder::start(&env, true).kill();
    let lock = env.join("lock.mdb");
    let snapshot = fs::read(&lock).unwrap();
    let mut outputs = Vec::new();
    for imp in [Impl::C, Impl::Rust] {
        restore(&lock, &snapshot);
        let r = run(
            imp,
            "mdb_stat",
            &args(&[b"-rr".to_vec(), p(&env)]),
            b"",
            dir.path(),
        );
        outputs.push(r);
    }
    assert_eq!(outputs[0].stdout_str(), outputs[1].stdout_str());
    assert_eq!(outputs[0].stderr, outputs[1].stderr);
    assert_eq!((outputs[0].code, outputs[1].code), (C_R_EXIT, 0));
    let out = outputs[1].stdout_text();
    assert!(out.contains("  1 stale readers cleared.\n"), "{out}");
    assert!(out.ends_with("(no active readers)\n"), "{out}");
}
