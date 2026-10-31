//! Cross-implementation concurrency through the C API: the interop test
//! bed's C peer (`interop/c`, a plain `lmdb.h` client) is built twice,
//! against the C library and against the Rust `liblmdb.a`, and both builds
//! run against one environment at the same time:
//!
//! * a writer loop of each (small random write transactions with nested
//!   transactions and aborts, `MDB_NOOVERWRITE`/`MDB_NODUPDATA` puts,
//!   deletes by key and by cursor) — the shared writer lock must serialise
//!   them: the `#commits` counter ends at exactly 1 + the sum of their
//!   commits (no lost update);
//! * a reader loop of each, checking in every snapshot that the counters
//!   match the data and every value is well formed.
//!
//! Afterwards the C peer audits every page (`check-freelist`), and a final
//! reader pass of each build checks the end state.

mod common;

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use common::*;

/// Seconds the loops run.
const SECS: f64 = 2.0;

/// A running peer command.
struct Running {
    label: String,
    child: Child,
    stdout: BufReader<ChildStdout>,
    lines: Vec<String>,
}

impl Running {
    fn start(lib: Lib, args: &[&str]) -> Running {
        let exe = builder().cpeer(lib);
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Running {
            label: format!("cpeer ({lib:?}) {}", args.join(" ")),
            child,
            stdout,
            lines: Vec::new(),
        }
    }

    /// Waits for the "READY" line.
    fn wait_ready(&mut self) {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "{} exited before READY: {:?}",
                self.label,
                self.lines
            );
            let ready = line.starts_with("READY");
            self.lines.push(line);
            if ready {
                return;
            }
        }
    }

    /// Waits for a successful exit and returns the `key=value` numbers of
    /// the last line.
    fn finish(mut self) -> Vec<(String, u64)> {
        let mut rest = String::new();
        self.stdout.read_to_string(&mut rest).unwrap();
        self.lines.extend(rest.lines().map(str::to_owned));
        let status = self.child.wait().unwrap();
        assert!(
            status.success(),
            "{} failed ({status}): {:?}",
            self.label,
            self.lines
        );
        let last = self.lines.last().cloned().unwrap_or_default();
        eprintln!("{}: {last}", self.label);
        last.split_whitespace()
            .filter_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                Some((k.to_owned(), v.parse().ok()?))
            })
            .collect()
    }
}

fn value(report: &[(String, u64)], key: &str) -> u64 {
    report
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| *v)
        .unwrap_or_else(|| panic!("no {key} in {report:?}"))
}

fn run_ok_cpeer(lib: Lib, args: &[&str]) {
    run_ok(Command::new(builder().cpeer(lib)).args(args));
}

fn env_str(env: &Path) -> &str {
    env.to_str().unwrap()
}

#[test]
fn c_and_rust_writers_and_readers_at_once() {
    let env = builder().scratch("cross");
    let e = env_str(&env);
    run_ok_cpeer(Lib::C, &["create", e, "536870912", "8"]);
    // Initialise the loop's databases (one commit).
    run_ok_cpeer(Lib::Rust, &["writer-loop", e, "loop", "0", "1"]);

    let reader_secs = (SECS + 0.5).to_string();
    let mut readers = Vec::new();
    for lib in [Lib::C, Lib::Rust] {
        let mut r = Running::start(lib, &["reader-loop", e, "loop", &reader_secs]);
        r.wait_ready();
        readers.push(r);
    }
    let secs = SECS.to_string();
    let writers: Vec<Running> = [(Lib::C, "11"), (Lib::Rust, "12")]
        .into_iter()
        .map(|(lib, seed)| Running::start(lib, &["writer-loop", e, "loop", &secs, seed]))
        .collect();
    let reports: Vec<_> = writers.into_iter().map(Running::finish).collect();
    for r in readers {
        let report = r.finish();
        assert!(value(&report, "reads") > 1, "{report:?}");
    }
    let total: u64 = reports.iter().map(|r| value(r, "commits")).sum();
    let last = reports
        .iter()
        .map(|r| value(r, "last_commit"))
        .max()
        .unwrap();
    assert!(
        reports.iter().all(|r| value(r, "commits") > 0),
        "a writer did not commit: {reports:?}"
    );
    assert_eq!(last, 1 + total, "lost update: {reports:?}");
    let nested: u64 = reports.iter().map(|r| value(r, "nested_commits")).sum();
    eprintln!("{total} commits ({nested} nested commits) by C and Rust writers");

    // The end state: the C page audit, and a reader pass of each build.
    run_ok_cpeer(Lib::C, &["check-freelist", e]);
    for lib in [Lib::C, Lib::Rust] {
        run_ok_cpeer(lib, &["reader-loop", e, "loop", "0"]);
    }
}

/// The C peer's nested-transaction test written through the Rust library
/// and checked by the C library, and the other way round.
#[test]
fn nested_transactions_across_libraries() {
    for (writer, checker) in [(Lib::Rust, Lib::C), (Lib::C, Lib::Rust)] {
        let env = builder().scratch(&format!("nested-{}", writer.name()));
        let e = env_str(&env);
        run_ok_cpeer(Lib::C, &["create", e, "67108864", "8"]);
        run_ok_cpeer(writer, &["nested-test", e]);
        run_ok_cpeer(checker, &["nested-test", e, "--check"]);
        run_ok_cpeer(Lib::C, &["check-freelist", e]);
    }
}
