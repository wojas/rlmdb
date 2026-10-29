//! Test 3: a C program linked against the Rust library holds read
//! transactions while C writers (`mtest` and `tests/c/fixture.c`, built
//! against the C library) commit into the same environment; its snapshots
//! must stay stable through the C API (`tests/c/capi_snapshot.c`). The same
//! program linked against the C library runs the same scenario as the
//! oracle.
//!
//! (The second writer is not `mtest2`: while the reader pins snapshot 0 no
//! freed page can be reused, and `mtest2`'s hundreds of small transactions
//! overflow its fixed 10 MiB map with 16 KiB pages, in C as in Rust.)

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use common::*;

/// A running `capi_snapshot`.
struct Reader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    log: Vec<String>,
}

impl Reader {
    fn start(lib: Lib, args: &[&Path]) -> Reader {
        let exe = builder().test_program("capi_snapshot", lib);
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Reader {
            child,
            stdin,
            stdout,
            log: Vec::new(),
        }
    }

    /// The next line starting with `tag`, as whitespace-separated numbers.
    fn expect(&mut self, tag: &str) -> Vec<u64> {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(
                n > 0,
                "reader exited before {tag:?}; output: {:?}",
                self.log
            );
            self.log.push(line.clone());
            if let Some(rest) = line.strip_prefix(tag) {
                return rest
                    .split_whitespace()
                    .map(|x| x.parse().unwrap())
                    .collect();
            }
        }
    }

    fn step(&mut self) {
        self.stdin.write_all(b"step\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Waits for "DONE <checks> <failures>" and a successful exit.
    fn finish(mut self) {
        let done = self.expect("DONE");
        let status = self.child.wait().unwrap();
        eprintln!("reader output: {:?}", self.log);
        assert_eq!(done[1], 0, "reader reported failures: {:?}", self.log);
        assert!(status.success(), "reader failed: {status}");
    }
}

/// `(pid, txnid)` of the reader table entries printed by `mdb_stat -r`.
fn readers(env: &Path) -> Vec<(u32, Option<u64>)> {
    let exe = builder().tool("mdb_stat", Lib::C);
    let out = Command::new(exe).arg("-r").arg(env).output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            match f[..] {
                [pid, tid, txnid] if u64::from_str_radix(tid, 16).is_ok() => {
                    Some((pid.parse().ok()?, txnid.parse().ok()))
                }
                _ => None,
            }
        })
        .collect()
}

/// Key/data pairs of the main DB according to the C `mdb_dump`, as
/// "hexkey hexdata" lines.
fn c_dump(env: &Path) -> Vec<String> {
    let exe = builder().tool("mdb_dump", Lib::C);
    let out = run_ok(Command::new(exe).arg(env));
    let text = String::from_utf8(out.stdout).unwrap();
    let data: Vec<&str> = text
        .lines()
        .skip_while(|l| *l != "HEADER=END")
        .skip(1)
        .take_while(|l| *l != "DATA=END")
        .map(str::trim)
        .collect();
    data.chunks(2).map(|kv| kv.join(" ")).collect()
}

fn snapshot_stability(lib: Lib) {
    let dir = builder().scratch(&format!("stability-{}", lib.name()));
    let env = dir.join("testdb");
    std::fs::create_dir_all(&env).unwrap();
    let dump = dir.join("t1.dump");
    let mut reader = Reader::start(lib, &[Path::new("stability"), &env, &dump]);
    let pid = reader.child.id();
    assert_eq!(reader.expect("READY"), vec![0, 0]);

    // A C writer fills the environment while T0 pins the empty snapshot.
    run_mtest("mtest", &dir);
    reader.step();
    let t1 = reader.expect("T1");
    let (id1, n1) = (t1[0], t1[1]);
    assert!(id1 > 0 && n1 > 0);
    // C sees both of the reader's slots, with their snapshots.
    let mut ours: Vec<_> = readers(&env)
        .into_iter()
        .filter(|(p, _)| *p == pid)
        .map(|(_, t)| t)
        .collect();
    ours.sort();
    assert_eq!(ours, vec![Some(0), Some(id1)]);
    // T1 reads what C's mdb_dump reads.
    let ours = std::fs::read_to_string(&dump).unwrap();
    assert_eq!(ours.lines().collect::<Vec<_>>(), c_dump(&env));

    // Another C writer rewrites the main DB's pages (copy on write); the
    // reader verifies T0 and T1 byte for byte, then renews T1 onto the
    // latest snapshot.
    make_fixture(&env);
    reader.step();
    let t1r = reader.expect("T1R");
    assert!(t1r[0] > id1, "{t1r:?}");
    assert_eq!(t1r[1], n1 + 9, "the fixture adds 8 named DBs and a key");
    reader.finish();
}

fn map_resized(lib: Lib) {
    let dir = builder().scratch(&format!("resize-{}", lib.name()));
    let env = dir.join("testdb");
    std::fs::create_dir_all(&env).unwrap();
    let mut reader = Reader::start(lib, &[Path::new("resize"), &env]);
    reader.expect("READY");
    // mtest maps 10 MiB and grows the file beyond the reader's map.
    run_mtest("mtest", &dir);
    reader.step();
    reader.finish();
}

#[test]
fn snapshot_stability_c_oracle() {
    snapshot_stability(Lib::C);
}

#[test]
fn snapshot_stability_rust() {
    snapshot_stability(Lib::Rust);
}

#[test]
fn map_resized_c_oracle() {
    map_resized(Lib::C);
}

#[test]
fn map_resized_rust() {
    map_resized(Lib::Rust);
}
