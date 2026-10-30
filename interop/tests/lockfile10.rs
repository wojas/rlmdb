//! The LMDB 1.0 lock file (`MDB_LOCK_VERSION 2`) shared by rlmdb and C.
//!
//! rlmdb cannot open a 1.0 *environment* yet (the 1.0 data layout is a
//! separate stage), so these tests use the lock file alone, through
//! `rlmdb::core::reader::LockFile`: the same lock-file code an environment
//! uses, opened for a data format. The data file is always C 1.0's.
//!
//! * The descriptors of both versions equal every lock-file line of
//!   `layout_dump` and `layout_dump10`.
//! * rlmdb attaches to a table C 1.0 created: it sees C's header and
//!   reader, shares the writer lock both ways, takes a reader slot that C
//!   lists, and clears a dead C reader.
//! * C 1.0 attaches to a table rlmdb created (with `MDB_NOSUBDIR`): the
//!   same checks the other way round, and a C commit moves the `mti_txnid`
//!   rlmdb reads.
//! * Lock files of the two versions refuse each other while held, with
//!   `MDB_VERSION_MISMATCH` on both sides.
//!
//! While a `LockFile` is open, its header is read through it, never by
//! reading the file: closing another descriptor of the lock file would drop
//! every record lock this process holds on it, and the next C opener would
//! re-initialise the table under us.

#[path = "common/lock_lines.rs"]
mod lock_lines;

use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use interop::tools::{c_layout, c_layout10, run_tool_any_status, MdbStat, ReaderEntry};
use interop::{bins, Output, Peer, PeerProcess, TestEnv};
use rlmdb::consts::{DEFAULT_READERS, MDB_MAGIC};
use rlmdb::core::reader::LockFile;
use rlmdb::format::lockfile::{
    LockLayout, LockObjects, TxnInfoSnapshot, LOCK_V09, LOCK_V10, USES_NAMED_SEMAPHORES,
};
use rlmdb::{Error, Format, MdbError, ReaderInfo};

/// How long a peer that must wait for a lock is given to (wrongly) get it.
const BLOCKED: Duration = Duration::from_millis(500);

/// Every lock-file line of both C builds equals the descriptor of its
/// version.
#[test]
fn lock_file_lines_match_c() {
    for (c, layout, tool) in [
        (c_layout().expect("layout_dump"), &LOCK_V09, "layout_dump"),
        (
            c_layout10().expect("layout_dump10"),
            &LOCK_V10,
            "layout_dump10",
        ),
    ] {
        let expected_mode = if USES_NAMED_SEMAPHORES {
            "posix_sem"
        } else {
            "posix_mutex"
        };
        assert_eq!(c.lock_mode, expected_mode, "{tool}");
        let mismatches = lock_lines::lock_line_mismatches(c, layout);
        assert!(
            mismatches.is_empty(),
            "{tool} differs from {:?}:\n{}",
            layout.format,
            mismatches.join("\n")
        );
        println!(
            "{tool}: MDB_LOCK_FORMAT={:#x}, readers at {}",
            layout.lock_format, layout.readers
        );
    }
}

// --- helpers --------------------------------------------------------------

fn os(s: impl AsRef<std::ffi::OsStr>) -> OsString {
    s.as_ref().to_owned()
}

/// `cpeer10 <command> <env> <args...>` (with `--nosubdir` if needed), run
/// to completion.
fn cpeer10(env: &TestEnv, command: &str, args: &[&str]) -> Output {
    let mut all = vec![os(command), os(env.path())];
    all.extend(args.iter().map(os));
    all.extend(env.peer_options());
    run_tool_any_status(bins::cpeer10(), &all, None, None)
        .expect("run cpeer10")
        .ok()
        .expect("cpeer10 command")
}

/// `cpeer10 <command> <env> 60 --until-stdin` in the background.
fn spawn_cpeer10(env: &TestEnv, command: &str) -> PeerProcess {
    let mut all = vec![os(command), os(env.path()), os("60"), os("--until-stdin")];
    all.extend(env.peer_options());
    PeerProcess::spawn_program(bins::cpeer10(), &all).expect("start cpeer10")
}

/// The `txnid=` of a peer's `READY txnid=<n> pid=<p>` line.
fn ready_txnid(line: &str) -> u64 {
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("txnid="))
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no txnid in {line:?}"))
}

/// Run a C tool on `env` with `flags` (and `-n` if needed).
fn tool(bin: &Path, env: &TestEnv, flags: &[&str]) -> Output {
    let mut args = env.tool_options();
    args.extend(flags.iter().map(os));
    args.push(os(env.path()));
    run_tool_any_status(bin, &args, None, None).expect("run C tool")
}

/// `mdb_stat10 <flags>`, parsed: the environment and reader sections.
/// `-r` alone exits with status 1 on success (see
/// `interop::tools::mdb_stat`). The DB sections are left out: 1.0 adds a
/// `Page size` line to them, which the 0.9 parser does not know.
fn stat10(env: &TestEnv, flags: &[&str]) -> MdbStat {
    let out = tool(bins::mdb_stat10(), env, flags);
    assert!(
        out.success() || (out.code() == Some(1) && out.stderr.is_empty()),
        "{out}"
    );
    let text: String = out
        .stdout
        .lines()
        .take_while(|l| !l.starts_with("Status of "))
        .map(|l| format!("{l}\n"))
        .collect();
    MdbStat::parse(&text).expect("parse mdb_stat10")
}

/// The reader table as `mdb_stat10 -r` (C's `mdb_reader_list`) prints it.
fn c_readers(env: &TestEnv) -> Vec<ReaderEntry> {
    stat10(env, &["-r"]).readers.expect("reader section")
}

/// rlmdb's `reader_list` and C's agree: same slots, same order, same
/// values.
fn assert_same_readers(rust: &[ReaderInfo], c: &[ReaderEntry]) {
    let rust: Vec<ReaderEntry> = rust
        .iter()
        .map(|r| ReaderEntry {
            pid: r.pid,
            thread: format!("{:x}", r.thread),
            txnid: r.txnid,
        })
        .collect();
    assert_eq!(rust, c);
}

/// A new 1.0 environment with some data, written by C 1.0; returns its
/// last txnid.
fn new_env10(env: &TestEnv) -> u64 {
    cpeer10(env, "create", &["67108864", "4"]);
    cpeer10(env, "fill", &["-", "100", "7"]);
    let info = stat10(env, &["-e"]).env.expect("environment info");
    assert!(info.last_txnid >= 1, "{info:?}");
    info.last_txnid
}

fn assert_version_mismatch(r: rlmdb::Result<LockFile>) {
    match r {
        Err(Error::Mdb(MdbError::VersionMismatch)) => {}
        Err(e) => panic!("expected MDB_VERSION_MISMATCH, got {e}"),
        Ok(_) => panic!("expected MDB_VERSION_MISMATCH, the lock file was accepted"),
    }
}

/// The header as it is in the file, decoded with `layout`. Only while this
/// process has no `LockFile` open on it (see the module documentation).
fn header(env: &TestEnv, layout: &LockLayout) -> TxnInfoSnapshot {
    let bytes = std::fs::read(env.lock_file()).expect("read lock file");
    TxnInfoSnapshot::read(&bytes, layout).expect("lock file header")
}

/// `mti_mutexid` as LMDB 1.0 computes it (`mdb_env_setup_locks`, mdb10.c
/// lines 6064-6084): 64-bit FNV-1a of `struct { dev_t dev; ino_t ino; }`
/// of the lock file, zeroed first. macOS: 4-byte `dev_t`, 4 bytes of
/// padding, 8-byte `ino_t`.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn c10_mutex_id(lock_file: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(lock_file).expect("stat lock file");
    let mut id = [0u8; 16];
    id[0..4].copy_from_slice(&(md.dev() as i32).to_ne_bytes());
    id[8..16].copy_from_slice(&md.ino().to_ne_bytes());
    id.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The header is a version 2 header, and on macOS the semaphores are
/// named after the hash C 1.0 would have stored for this lock file.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "ios")),
    allow(unused_variables)
)]
fn assert_v2_header(env: &TestEnv, lf: &LockFile, txnid: u64) -> TxnInfoSnapshot {
    let h = lf.header();
    assert_eq!(h.magic, MDB_MAGIC);
    assert_eq!(h.format, LOCK_V10.lock_format);
    assert_eq!(h.txnid, txnid);
    assert_eq!(
        h.mutex_id.is_some(),
        LOCK_V10.objects == LockObjects::SemaphoreHash
    );
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    assert_eq!(h.mutex_id, Some(c10_mutex_id(&env.lock_file())));
    h
}

/// The writer lock is one lock for both: a C writer waits while rlmdb
/// holds it, and rlmdb waits while a C writer holds it.
fn assert_shared_writer_lock(env: &TestEnv, lf: &LockFile) {
    lf.lock_writer().expect("rlmdb takes the writer lock");
    let mut writer = spawn_cpeer10(env, "hold-writer");
    assert!(
        writer.wait_for_line("READY", BLOCKED).is_err(),
        "C 1.0 began a write transaction while rlmdb held the writer lock"
    );
    lf.unlock_writer();
    writer.wait_ready().expect("C 1.0 gets the writer lock");

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        s.spawn(move || {
            // On Linux the mutex must be unlocked by the thread that
            // locked it.
            lf.lock_writer().expect("rlmdb takes the writer lock");
            tx.send(()).unwrap();
            lf.unlock_writer();
        });
        assert!(
            rx.recv_timeout(BLOCKED).is_err(),
            "rlmdb got the writer lock while C 1.0 held it"
        );
        writer.release().unwrap();
        rx.recv_timeout(Duration::from_secs(60))
            .expect("rlmdb gets the writer lock once C 1.0 has released it");
    });
    writer.finish().unwrap().ok().expect("cpeer10 hold-writer");
}

/// A C reader killed with its slot taken is stale for rlmdb's
/// `reader_check`, which clears exactly that slot.
fn assert_dead_c_reader_cleared(env: &TestEnv, lf: &LockFile) {
    let mut reader = spawn_cpeer10(env, "hold-reader");
    reader.wait_ready().expect("cpeer10 hold-reader READY");
    let pid = reader.pid() as i32;
    assert!(lf.reader_list().iter().any(|r| r.pid == pid));
    reader.kill().unwrap();
    assert_eq!(lf.reader_check().unwrap(), 1);
    assert!(lf.reader_list().iter().all(|r| r.pid != pid));
    assert_same_readers(&lf.reader_list(), &c_readers(env));
}

// --- the tests ------------------------------------------------------------

/// rlmdb attaches to the version 2 table of a C 1.0 process.
#[test]
fn rust_attaches_to_c10_lock_file() {
    let env = TestEnv::new().unwrap();
    let last = new_env10(&env);
    let lock_path = env.lock_file();

    let mut holder = spawn_cpeer10(&env, "hold-reader");
    let ready = holder.wait_ready().expect("cpeer10 hold-reader READY");
    assert_eq!(ready_txnid(&ready), last);
    let c_pid = holder.pid() as i32;

    // A version 1 opener is refused while C 1.0 holds the file. (Before
    // our own open: closing a descriptor of the file would drop every lock
    // this process holds on it.)
    assert_version_mismatch(LockFile::open(&lock_path, Format::V09, 126));

    let lf = LockFile::open(&lock_path, Format::V10, 4).expect("attach to C 1.0's lock file");
    assert!(!lf.is_exclusive());
    assert_eq!(lf.layout(), &LOCK_V10);
    assert_eq!(lf.lock_format(), LOCK_V10.lock_format);
    assert_eq!(lf.txnid(), last);
    // The table size is C's, not what we asked for.
    assert_eq!(lf.max_readers(), DEFAULT_READERS);
    let h = assert_v2_header(&env, &lf, last);
    assert_eq!(h.num_readers, lf.num_readers());
    assert!(h.num_readers >= 1);

    // C's reader, as rlmdb and as C see it.
    let readers = lf.reader_list();
    assert!(
        readers
            .iter()
            .any(|r| r.pid == c_pid && r.txnid == Some(last)),
        "{readers:?}"
    );
    assert_same_readers(&readers, &c_readers(&env));
    assert_eq!(lf.reader_check().unwrap(), 0, "C's reader is alive");

    // A reader slot of ours, listed by C.
    let (slot, txnid) = lf.begin_read().unwrap();
    assert_eq!(txnid, last);
    let c_list = c_readers(&env);
    let me = std::process::id() as i32;
    assert!(
        c_list.iter().any(|r| r.pid == me && r.txnid == Some(last)),
        "{c_list:?}"
    );
    assert_same_readers(&lf.reader_list(), &c_list);

    assert_shared_writer_lock(&env, &lf);

    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer10 hold-reader");
    assert!(lf.reader_list().iter().all(|r| r.pid != c_pid));
    assert_dead_c_reader_cleared(&env, &lf);
    lf.end_read(slot);
    assert_eq!(c_readers(&env), []);
}

/// C 1.0 attaches to a version 2 table that rlmdb created.
#[test]
fn c10_attaches_to_rust_lock_file() {
    let env = TestEnv::new_nosubdir().unwrap();
    let last = new_env10(&env);
    let lock_path = env.lock_file();
    std::fs::remove_file(&lock_path).unwrap();

    let lf = LockFile::open(&lock_path, Format::V10, 50).expect("create the lock file");
    assert!(lf.is_exclusive());
    assert_eq!(
        std::fs::metadata(&lock_path).unwrap().len() as usize,
        LOCK_V10.file_size(50)
    );
    // What an environment does at the end of its open: publish the last
    // txnid and let others in.
    lf.downgrade(last).unwrap();
    assert_v2_header(&env, &lf, last);

    // A reader slot of ours, listed by C 1.0, which had to validate the
    // header and find the reader lock to get there.
    let (slot, txnid) = lf.begin_read().unwrap();
    assert_eq!(txnid, last);
    let me = std::process::id() as i32;
    let c_list = c_readers(&env);
    assert!(
        c_list.iter().any(|r| r.pid == me && r.txnid == Some(last)),
        "{c_list:?}"
    );
    assert_same_readers(&lf.reader_list(), &c_list);
    let info = stat10(&env, &["-e"]).env.expect("environment info");
    assert_eq!(info.max_readers, 50);
    assert_eq!(info.readers_used, 1);
    assert_eq!(info.last_txnid, last);

    // A C reader, as both see it.
    let mut holder = spawn_cpeer10(&env, "hold-reader");
    let ready = holder.wait_ready().expect("cpeer10 hold-reader READY");
    assert_eq!(ready_txnid(&ready), last);
    let c_pid = holder.pid() as i32;
    let readers = lf.reader_list();
    assert_eq!(readers.len(), 2);
    assert!(readers
        .iter()
        .any(|r| r.pid == c_pid && r.txnid == Some(last)));
    assert_same_readers(&readers, &c_readers(&env));
    assert_eq!(lf.reader_check().unwrap(), 0);

    assert_shared_writer_lock(&env, &lf);

    // A C commit publishes its txnid where rlmdb reads it, and the readers
    // keep their snapshots.
    cpeer10(&env, "fill", &["-", "10", "9"]);
    assert!(lf.txnid() > last, "mti_txnid {} after a commit", lf.txnid());
    assert_eq!(lf.header().txnid, lf.txnid());
    assert!(lf.reader_list().iter().all(|r| r.txnid == Some(last)));

    // A version 1 opener (C 0.9) is refused while we hold the file.
    let out = tool(bins::mdb_stat(), &env, &["-e"]);
    assert!(
        !out.success() && out.stderr.contains("MDB_VERSION_MISMATCH"),
        "{out}"
    );

    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer10 hold-reader");
    assert_dead_c_reader_cleared(&env, &lf);
    lf.end_read(slot);
    drop(lf);

    // With rlmdb gone, C 1.0 is the exclusive opener and reads the data.
    let out = tool(bins::mdb_stat10(), &env, &[]);
    assert!(
        out.success() && out.stdout.contains("Entries: 110"),
        "{out}"
    );
}

/// Lock files of the two versions refuse each other while held: rlmdb
/// refuses C 0.9's version 1 table as version 2, and C 1.0 refuses
/// rlmdb's version 1 table. (rlmdb refusing C 1.0's table as version 1 and
/// C 0.9 refusing rlmdb's version 2 table are checked above.)
#[test]
fn lock_versions_refuse_each_other() {
    let env = TestEnv::new().unwrap();
    Peer::C
        .on("create", &env)
        .args(["67108864", "4"])
        .run()
        .expect("cpeer create");
    let mut holder = Peer::C
        .on("hold-env", &env)
        .args(["60", "--until-stdin"])
        .spawn()
        .expect("start cpeer hold-env");
    holder.wait_ready().expect("cpeer hold-env READY");
    assert_eq!(header(&env, &LOCK_V09).format, LOCK_V09.lock_format);
    assert_version_mismatch(LockFile::open(&env.lock_file(), Format::V10, 126));
    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer hold-env");

    // rlmdb holds a version 1 table: C 1.0 is refused.
    let lf = LockFile::open(&env.lock_file(), Format::V09, 126).unwrap();
    assert!(lf.is_exclusive());
    lf.downgrade(1).unwrap();
    let out = tool(bins::mdb_stat10(), &env, &["-e"]);
    assert!(
        !out.success() && out.stderr.contains("MDB_VERSION_MISMATCH"),
        "{out}"
    );
    assert_eq!(lf.header().format, LOCK_V09.lock_format);
    drop(lf);

    // Nobody holds it: an exclusive opener of either version re-initialises
    // it in its own (C 1.0 then fails on the 0.9 data file).
    let lf = LockFile::open(&env.lock_file(), Format::V10, 126).unwrap();
    assert!(lf.is_exclusive());
    assert_eq!(lf.header().format, LOCK_V10.lock_format);
    drop(lf);
    assert_eq!(header(&env, &LOCK_V10).format, LOCK_V10.lock_format);
    let out = tool(bins::mdb_stat(), &env, &["-e"]);
    assert!(out.success(), "{out}");
    assert_eq!(header(&env, &LOCK_V09).format, LOCK_V09.lock_format);
}
