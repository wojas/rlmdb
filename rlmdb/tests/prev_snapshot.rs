//! `EnvironmentFlags::PREV_SNAPSHOT` (LMDB 1.0's `MDB_PREVSNAPSHOT`), in
//! environments of both formats: the previous snapshot until the first
//! commit, the exclusive lock until then, `MDB_ENV_BUSY` for a writer that
//! is not alone, read-only handles alone and next to other processes,
//! `NO_LOCK` handles, and copies. The other process is this test binary
//! run again ([`Peer`]). C 0.9 reads and audits the 0.9 files the tests
//! leave (C 1.0 does the same for 1.0 files in `rlmdb-capi/tests` and
//! `interop`); C 1.0 behaves differently in three places, which the tests
//! name (`KNOWN_C_BUGS.md` rows 67 to 69).

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use rlmdb::consts::MDB_ENV_BUSY;
use rlmdb::{Dbi, Environment, EnvironmentFlags, Format, Transaction, WriteFlags};

const FORMATS: [Format; 2] = [Format::V09, Format::V10];

fn open(dir: &Path, format: Format, flags: EnvironmentFlags) -> rlmdb::Result<Environment> {
    Environment::builder()
        .format(format)
        .map_size(64 << 20)
        .flags(flags | EnvironmentFlags::NO_SYNC)
        .open(dir)
}

fn put(env: &Environment, key: &str, value: &str) {
    let mut txn = env.begin_rw_txn().unwrap();
    txn.put(
        Dbi::MAIN,
        key.as_bytes(),
        value.as_bytes(),
        WriteFlags::empty(),
    )
    .unwrap();
    txn.commit().unwrap();
}

/// `key=value ...` of the main DB, and the txnid, in a read transaction.
fn contents(env: &Environment) -> (String, u64) {
    let txn = env.begin_ro_txn().unwrap();
    let mut c = txn.cursor(Dbi::MAIN).unwrap();
    let mut items = Vec::new();
    while let Some((k, v)) = c.next().unwrap() {
        items.push(format!(
            "{}={}",
            String::from_utf8_lossy(k),
            String::from_utf8_lossy(v)
        ));
    }
    (items.join(" "), txn.id())
}

/// A new environment of `format` in `dir` after two commits: `a=1` (txn
/// 1), `b=2` (txn 2).
fn two_commits(dir: &Path, format: Format) {
    let env = open(dir, format, EnvironmentFlags::empty()).unwrap();
    put(&env, "a", "1");
    put(&env, "b", "2");
}

fn has_prev(env: &Environment) -> bool {
    env.flags().contains(EnvironmentFlags::PREV_SNAPSHOT)
}

/// C 0.9 reads a 0.9 environment and its page audit passes (1.0
/// environments: `interop`, `rlmdb-capi/tests/capi_prevsnapshot.rs`).
fn c_reads(dir: &Path, format: Format, want: &str) {
    if format != Format::V09 {
        return;
    }
    let Some(c) = common::c_tools() else {
        return;
    };
    let got: Vec<String> = c
        .dump(dir, None)
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                String::from_utf8_lossy(k),
                String::from_utf8_lossy(v)
            )
        })
        .collect();
    assert_eq!(got.join(" "), want, "C 0.9's mdb_dump");
    c.peer_ok(&["check-freelist", dir.to_str().unwrap()]);
}

#[test]
fn previous_snapshot_until_the_first_commit() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        two_commits(dir.path(), format);
        let env = open(dir.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap();
        assert!(has_prev(&env));
        assert_eq!(contents(&env), ("a=1".into(), 1));
        let info = env.info().unwrap();
        assert_eq!(info.last_txnid, 1, "{format:?}");
        assert_eq!(env.stat().unwrap().entries, 1);
        assert_eq!(env.freelist().unwrap(), 0);

        // Nothing ends the flag but a commit that writes.
        env.begin_rw_txn().unwrap().commit().unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(Dbi::MAIN, b"x", b"y", WriteFlags::empty()).unwrap();
        txn.abort();
        assert!(has_prev(&env));
        assert_eq!(contents(&env), ("a=1".into(), 1));

        // The write transaction begins from the previous snapshot (txnid
        // 2 again), and its commit discards the latest one.
        let txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), 2);
        drop(txn);
        put(&env, "c", "3");
        assert!(!has_prev(&env));
        assert_eq!(contents(&env), ("a=1 c=3".into(), 2));
        assert_eq!(env.info().unwrap().last_txnid, 2);
        // The next commit keeps it (C 1.0 overwrites it: row 67).
        put(&env, "d", "4");
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        drop(env);

        let env = open(dir.path(), format, EnvironmentFlags::empty()).unwrap();
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        drop(env);
        c_reads(dir.path(), format, "a=1 c=3 d=4");

        // Read-only, alone: the previous snapshot, until it is closed.
        let ro = EnvironmentFlags::READ_ONLY | EnvironmentFlags::PREV_SNAPSHOT;
        let env = open(dir.path(), format, ro).unwrap();
        assert!(has_prev(&env));
        assert_eq!(contents(&env), ("a=1 c=3".into(), 2));
        assert_eq!(env.info().unwrap().last_txnid, 2);
        let inactive = env.begin_ro_txn().unwrap().reset();
        assert_eq!(inactive.renew().unwrap().id(), 2);
        assert!(has_prev(&env));
    }
}

/// A new environment opened with the flag: both meta pages hold txnid 0.
#[test]
fn new_environment() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap();
        assert_eq!(env.format(), format);
        assert_eq!(contents(&env), (String::new(), 0));
        put(&env, "a", "1");
        assert!(!has_prev(&env));
        put(&env, "b", "2");
        assert_eq!(contents(&env), ("a=1 b=2".into(), 2));
        drop(env);
        c_reads(dir.path(), format, "a=1 b=2");
    }
}

/// The previous snapshot is the older of the two meta pages, also after a
/// commit that grew the data file: the write transaction allocates from
/// its end of the file, and its commit leaves a file the C peers audit.
#[test]
fn commit_on_a_previous_snapshot_with_more_pages() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), format, EnvironmentFlags::empty()).unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..100u32 {
            let k = format!("k{i:04}");
            txn.put(Dbi::MAIN, k.as_bytes(), &[1u8; 600], WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 100..400u32 {
            let k = format!("k{i:04}");
            txn.put(Dbi::MAIN, k.as_bytes(), &[2u8; 600], WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
        let grown = env.info().unwrap().last_pgno;
        drop(env);

        let env = open(dir.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap();
        let before = env.info().unwrap().last_pgno;
        assert!(before < grown, "{before} {grown}");
        assert_eq!(env.stat().unwrap().entries, 100);
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 50..60u32 {
            txn.del(Dbi::MAIN, format!("k{i:04}").as_bytes(), None)
                .unwrap();
        }
        txn.put(Dbi::MAIN, b"z", b"z", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();
        assert_eq!(env.stat().unwrap().entries, 91);
        assert!(env.info().unwrap().last_pgno < grown);
        drop(env);
        if format == Format::V09 {
            if let Some(c) = common::c_tools() {
                assert_eq!(c.dump(dir.path(), None).len(), 91);
                c.peer_ok(&["check-freelist", dir.path().to_str().unwrap()]);
            }
        }
    }
}

/// A plain copy of an environment opened with the flag is a copy of the
/// previous snapshot, as a compacting copy is: both meta pages name it
/// (C 1.0's plain copy names the latest commit: row 69).
#[test]
fn copies_hold_the_previous_snapshot() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let env = open(dir.path(), format, EnvironmentFlags::empty()).unwrap();
        put(&env, "a", "1");
        let mut txn = env.begin_rw_txn().unwrap();
        for i in 0..300u32 {
            let k = format!("k{i:04}");
            txn.put(Dbi::MAIN, k.as_bytes(), &[3u8; 700], WriteFlags::empty())
                .unwrap();
        }
        txn.commit().unwrap();
        drop(env);
        for flags in [
            EnvironmentFlags::PREV_SNAPSHOT,
            EnvironmentFlags::PREV_SNAPSHOT | EnvironmentFlags::READ_ONLY,
            EnvironmentFlags::PREV_SNAPSHOT | EnvironmentFlags::NO_LOCK,
        ] {
            let env = open(dir.path(), format, flags).unwrap();
            for compact in [false, true] {
                let copy = tempfile::tempdir().unwrap();
                env.copy_to_path(copy.path(), compact).unwrap();
                let c = open(copy.path(), format, EnvironmentFlags::empty()).unwrap();
                assert_eq!(contents(&c).0, "a=1", "{format:?} {flags:?} {compact}");
                if !compact {
                    assert_eq!(c.info().unwrap().last_txnid, 1);
                    // Both meta pages: the previous snapshot of the copy
                    // is the same one.
                    drop(c);
                    let c = open(copy.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap();
                    assert_eq!(contents(&c), ("a=1".into(), 1));
                }
            }
        }
    }
}

/// `NO_LOCK`: the handle has the data file to itself in this process until
/// its first commit.
#[test]
fn no_lock_handle_holds_the_file_until_its_first_commit() {
    let ebusy = libc::EBUSY;
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        two_commits(dir.path(), format);
        let nolock = EnvironmentFlags::NO_LOCK;
        let prev = nolock | EnvironmentFlags::PREV_SNAPSHOT;
        let env = open(dir.path(), format, prev).unwrap();
        assert_eq!(contents(&env), ("a=1".into(), 1));
        for flags in [nolock, nolock | EnvironmentFlags::READ_ONLY, prev] {
            let e = open(dir.path(), format, flags).unwrap_err();
            assert_eq!(e.to_err_code(), ebusy, "{flags:?}");
        }
        assert_eq!(
            open(dir.path(), format, EnvironmentFlags::empty())
                .unwrap_err()
                .to_err_code(),
            ebusy
        );
        put(&env, "c", "3");
        assert!(!has_prev(&env));
        // Now an ordinary NO_LOCK handle: others share the file.
        let other = open(dir.path(), format, nolock).unwrap();
        assert_eq!(contents(&other), ("a=1 c=3".into(), 2));
        put(&other, "d", "4");
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        put(&env, "e", "5");
        assert_eq!(contents(&other), ("a=1 c=3 d=4 e=5".into(), 4));
        // While another NO_LOCK handle has it, no PREV_SNAPSHOT handle.
        let e = open(dir.path(), format, prev).unwrap_err();
        assert_eq!(e.to_err_code(), ebusy);
        drop((env, other));

        // Read-only: until it is closed.
        let ro = prev | EnvironmentFlags::READ_ONLY;
        let env = open(dir.path(), format, ro).unwrap();
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        let e = open(dir.path(), format, nolock).unwrap_err();
        assert_eq!(e.to_err_code(), ebusy);
        drop(env);
        let env = open(dir.path(), format, nolock).unwrap();
        assert_eq!(contents(&env), ("a=1 c=3 d=4 e=5".into(), 4));
        drop(env);
        c_reads(dir.path(), format, "a=1 c=3 d=4 e=5");
    }
}

#[test]
fn flag_cannot_be_set_on_an_open_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = open(dir.path(), Format::V09, EnvironmentFlags::empty()).unwrap();
    let e = env
        .set_flags(EnvironmentFlags::PREV_SNAPSHOT, true)
        .unwrap_err();
    assert_eq!(e.to_err_code(), libc::EINVAL);
}

// ---------------------------------------------------------------------------
// Another process
// ---------------------------------------------------------------------------

/// The variable that makes [`peer_process`] act: `<format> <dir>`.
const PEER_VAR: &str = "RLMDB_PREV_SNAPSHOT_PEER";

/// Not a test: the body of the other process of the tests below (this
/// binary run again with [`PEER_VAR`] set). It reads commands from stdin
/// and answers each with one line `PEER <answer>`:
///
/// * `open <flags>`: open the environment with these flag bits (`ok` or
///   `err <code>`); `close`;
/// * `get`: `<contents> @<txnid>` of a read transaction;
/// * `put <key> <value>`: commit it (`ok`).
#[test]
#[ignore = "helper process of the tests in this file"]
fn peer_process() {
    let Ok(arg) = std::env::var(PEER_VAR) else {
        return;
    };
    let (format, dir) = arg.split_once(' ').unwrap();
    let format = Format::from_name(format.as_bytes()).unwrap();
    let mut env = None;
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        let words: Vec<&str> = line.split(' ').collect();
        let answer = match words[..] {
            ["open", flags] => {
                let flags = EnvironmentFlags::from_bits_retain(flags.parse().unwrap());
                match Environment::builder()
                    .format(format)
                    .flags(flags | EnvironmentFlags::NO_SYNC)
                    .open(dir)
                {
                    Ok(e) => {
                        env = Some(e);
                        "ok".to_owned()
                    }
                    Err(e) => format!("err {}", e.to_err_code()),
                }
            }
            ["close"] => {
                env = None;
                "ok".to_owned()
            }
            ["get"] => {
                let (items, txnid) = contents(env.as_ref().unwrap());
                format!("{items} @{txnid}")
            }
            ["put", k, v] => {
                put(env.as_ref().unwrap(), k, v);
                "ok".to_owned()
            }
            _ => panic!("bad command {line}"),
        };
        println!("PEER {answer}");
    }
}

/// The other process, run by [`peer_process`].
struct Peer {
    child: Child,
    stdin: ChildStdin,
    answers: Receiver<String>,
}

impl Peer {
    fn start(dir: &Path, format: Format) -> Peer {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "peer_process",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PEER_VAR, format!("{} {}", format.name(), dir.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, answers) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest prints "test peer_process ... " before it.
                if let Some(at) = line.find("PEER ") {
                    let _ = tx.send(line[at + 5..].to_owned());
                }
            }
        });
        Peer {
            child,
            stdin,
            answers,
        }
    }

    fn send(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn wait(&self) -> String {
        match self.answers.recv_timeout(Duration::from_secs(60)) {
            Ok(a) => a,
            Err(e) => panic!("peer: {e}"),
        }
    }

    fn ask(&mut self, command: &str) -> String {
        self.send(command);
        self.wait()
    }

    /// No answer within a while: the peer waits (for a lock).
    fn waits(&self) -> bool {
        matches!(
            self.answers.recv_timeout(Duration::from_millis(500)),
            Err(RecvTimeoutError::Timeout)
        )
    }

    fn open(&mut self, flags: EnvironmentFlags) -> String {
        self.ask(&format!("open {}", flags.bits()))
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A writer on the previous snapshot must be alone: `MDB_ENV_BUSY` while
/// another process has the environment open. A read-only handle may share
/// it: it reads the previous snapshot, and leaves the lock file as it is,
/// so the other process's next commit builds on the latest one (C 1.0
/// publishes the previous one in the lock file instead, and that commit
/// overwrites the latest one: row 68).
#[test]
fn writer_must_be_alone_reader_need_not() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        two_commits(dir.path(), format);
        let mut peer = Peer::start(dir.path(), format);
        assert_eq!(peer.open(EnvironmentFlags::empty()), "ok");

        let e = open(dir.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap_err();
        assert_eq!(e.to_err_code(), MDB_ENV_BUSY, "{format:?}");
        let ro = EnvironmentFlags::READ_ONLY | EnvironmentFlags::PREV_SNAPSHOT;
        let env = open(dir.path(), format, ro).unwrap();
        assert!(has_prev(&env));
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), 1);
        assert_eq!(txn.get(Dbi::MAIN, b"b").unwrap(), None);
        // The slot pins the snapshot read.
        let pinned: Vec<_> = env
            .reader_list()
            .unwrap()
            .into_iter()
            .filter_map(|r| r.txnid)
            .collect();
        assert_eq!(pinned, [1]);
        assert_eq!(env.info().unwrap().last_txnid, 1);

        assert_eq!(peer.ask("get"), "a=1 b=2 @2");
        assert_eq!(peer.ask("put c 3"), "ok");
        assert_eq!(peer.ask("get"), "a=1 b=2 c=3 @3");
        // Still pinned: snapshot 1 is intact.
        assert_eq!(txn.get(Dbi::MAIN, b"a").unwrap(), Some(&b"1"[..]));
        drop(txn);
        // Each read transaction takes the previous snapshot of the moment.
        assert_eq!(contents(&env), ("a=1 b=2".into(), 2));
        let copy = tempfile::tempdir().unwrap();
        env.copy_to_path(copy.path(), false).unwrap();
        drop(env);
        let c = open(copy.path(), format, EnvironmentFlags::empty()).unwrap();
        assert_eq!(contents(&c), ("a=1 b=2".into(), 2));
        drop(c);
        assert_eq!(peer.ask("put d 4"), "ok");
        assert_eq!(peer.ask("close"), "ok");
        drop(peer);
        let env = open(dir.path(), format, EnvironmentFlags::empty()).unwrap();
        assert_eq!(contents(&env), ("a=1 b=2 c=3 d=4".into(), 4));
        drop(env);
        c_reads(dir.path(), format, "a=1 b=2 c=3 d=4");
    }
}

/// A writer on the previous snapshot keeps the exclusive lock until its
/// first commit: another process that opens the environment waits until
/// then, and then sees that commit (C 1.0 publishes the previous
/// snapshot's txnid at that point, so it sees the snapshot before it, and
/// its next commit overwrites it: row 67).
#[test]
fn others_wait_for_the_first_commit() {
    for format in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        two_commits(dir.path(), format);
        let env = open(dir.path(), format, EnvironmentFlags::PREV_SNAPSHOT).unwrap();
        let mut peer = Peer::start(dir.path(), format);
        peer.send(&format!("open {}", EnvironmentFlags::empty().bits()));
        assert!(
            peer.waits(),
            "{format:?}: the peer opened before the commit"
        );
        // Neither an empty commit nor an abort lets it in.
        env.begin_rw_txn().unwrap().commit().unwrap();
        env.begin_rw_txn().unwrap().abort();
        assert!(peer.waits());
        put(&env, "c", "3");
        assert_eq!(peer.wait(), "ok");
        assert_eq!(peer.ask("get"), "a=1 c=3 @2");
        assert_eq!(peer.ask("put d 4"), "ok");
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        put(&env, "e", "5");
        assert_eq!(peer.ask("get"), "a=1 c=3 d=4 e=5 @4");
        assert_eq!(peer.ask("close"), "ok");
        drop(peer);
        drop(env);
        c_reads(dir.path(), format, "a=1 c=3 d=4 e=5");

        // A read-only handle that is alone keeps it until it is closed.
        let ro = EnvironmentFlags::READ_ONLY | EnvironmentFlags::PREV_SNAPSHOT;
        let env = open(dir.path(), format, ro).unwrap();
        let mut peer = Peer::start(dir.path(), format);
        peer.send(&format!("open {}", EnvironmentFlags::READ_ONLY.bits()));
        assert!(peer.waits());
        assert_eq!(contents(&env), ("a=1 c=3 d=4".into(), 3));
        drop(env);
        assert_eq!(peer.wait(), "ok");
        assert_eq!(peer.ask("get"), "a=1 c=3 d=4 e=5 @4");
    }
}
