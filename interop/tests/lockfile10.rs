//! The LMDB 1.0 lock file (`MDB_LOCK_VERSION 2`) shared by rlmdb and C,
//! through rlmdb's public API (`Environment`).
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
//!   `MDB_VERSION_MISMATCH` on both sides; an exclusive opener of either
//!   implementation re-initialises a stale table of the other version.
//!
//! The header is read from the file with a descriptor of our own
//! ([`Attached::header`]), opened after the environment and closed after
//! it: closing a descriptor of the lock file drops every record lock this
//! process holds on it, and the next C opener would re-initialise the
//! table under us.

#[path = "common/lock_lines.rs"]
mod lock_lines;

use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use interop::tools::{c_layout, c_layout10, run_tool_any_status, MdbStat, ReaderEntry};
use interop::{bins, Output, Peer, PeerProcess, TestEnv};
use rlmdb::consts::{DEFAULT_READERS, MDB_MAGIC};
use rlmdb::format::lockfile::{LockLayout, TxnInfoSnapshot, LOCK_V09, LOCK_V10};
use rlmdb::{Environment, EnvironmentFlags, Error, Format, MdbError, ReaderInfo, Transaction};

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
        // One mode per version: the BSDs other than FreeBSD use named
        // semaphores for 0.9 and a SysV set for 1.0.
        assert_eq!(c.lock_mode, lock_lines::lock_mode(layout), "{tool}");
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
    Peer::C10
        .on(command, env)
        .args(args)
        .run()
        .expect("cpeer10 command")
}

/// `cpeer10 <command> <env> 60 --until-stdin` in the background.
fn spawn_cpeer10(env: &TestEnv, command: &str) -> PeerProcess {
    Peer::C10
        .on(command, env)
        .args(["60", "--until-stdin"])
        .spawn()
        .expect("start cpeer10")
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
/// `interop::tools::mdb_stat`).
fn stat10(env: &TestEnv, flags: &[&str]) -> MdbStat {
    let out = tool(bins::mdb_stat10(), env, flags);
    assert!(
        out.success() || (out.code() == Some(1) && out.stderr.is_empty()),
        "{out}"
    );
    MdbStat::parse(&out.stdout).expect("parse mdb_stat10")
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

/// `env` opened by rlmdb without a format request (the data file decides)
/// with room for `max_readers` slots if we create the lock table.
fn open(env: &TestEnv, max_readers: u32) -> rlmdb::Result<Environment> {
    let mut flags = EnvironmentFlags::empty();
    flags.set(EnvironmentFlags::NO_SUB_DIR, env.is_nosubdir());
    Environment::builder()
        .max_readers(max_readers)
        .flags(flags)
        .open(env.path())
}

/// An environment rlmdb has open, and a descriptor of its lock file for
/// reading the header. Field order is drop order: the environment closes
/// first, so closing the descriptor drops no lock it still needs.
struct Attached {
    env: Environment,
    lock: File,
}

impl Attached {
    fn new(env: &TestEnv, max_readers: u32) -> Attached {
        let opened = open(env, max_readers).expect("rlmdb opens the environment");
        let lock = File::open(env.lock_file()).expect("open the lock file");
        Attached { env: opened, lock }
    }

    /// The lock file's header as stored, decoded with the layout of the
    /// environment's format.
    fn header(&self) -> TxnInfoSnapshot {
        let layout = LockLayout::of(self.env.format());
        let mut buf = vec![0; layout.readers];
        interop::os::read_exact_at(&self.lock, &mut buf, 0).expect("read lock file");
        TxnInfoSnapshot::read(&buf, layout).expect("lock file header")
    }
}

fn assert_version_mismatch(r: rlmdb::Result<Environment>) {
    match r {
        Err(Error::Mdb(MdbError::VersionMismatch)) => {}
        Err(e) => panic!("expected MDB_VERSION_MISMATCH, got {e}"),
        Ok(_) => panic!("expected MDB_VERSION_MISMATCH, the environment was opened"),
    }
}

/// The header as it is in the file, decoded with `layout`. Only while this
/// process has no environment open on it (see the module documentation).
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

/// The header is a version 2 header with `mti_txnid` `txnid`, and on macOS
/// the semaphores are named after the hash C 1.0 would have stored for
/// this lock file.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "ios")),
    allow(unused_variables)
)]
fn assert_v2_header(env: &TestEnv, a: &Attached, txnid: u64) -> TxnInfoSnapshot {
    let h = a.header();
    assert_eq!(h.magic, MDB_MAGIC);
    assert_eq!(h.format, LOCK_V10.lock_format);
    assert_eq!(h.txnid, txnid);
    assert_eq!(h.mutex_id.is_some(), LOCK_V10.objects.stores_hash());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    assert_eq!(h.mutex_id, Some(c10_mutex_id(&env.lock_file())));
    h
}

/// The writer lock is one lock for both: a C writer waits while rlmdb has
/// a write transaction open, and rlmdb's waits while a C writer has one.
fn assert_shared_writer_lock(env: &TestEnv, rust: &Environment) {
    let txn = rust
        .begin_rw_txn()
        .expect("rlmdb begins a write transaction");
    let mut writer = spawn_cpeer10(env, "hold-writer");
    assert!(
        writer.wait_for_line("READY", BLOCKED).is_err(),
        "C 1.0 began a write transaction while rlmdb had one open"
    );
    txn.abort();
    writer.wait_ready().expect("C 1.0 gets the writer lock");

    std::thread::scope(|s| {
        let (tx, rx) = mpsc::channel();
        s.spawn(move || {
            let txn = rust
                .begin_rw_txn()
                .expect("rlmdb begins a write transaction");
            tx.send(()).unwrap();
            txn.abort();
        });
        assert!(
            rx.recv_timeout(BLOCKED).is_err(),
            "rlmdb began a write transaction while C 1.0 had one open"
        );
        writer.release().unwrap();
        rx.recv_timeout(Duration::from_secs(60))
            .expect("rlmdb gets the writer lock once C 1.0 has released it");
    });
    writer.finish().unwrap().ok().expect("cpeer10 hold-writer");
}

/// A C reader killed with its slot taken is stale for rlmdb's
/// `reader_check`, which clears exactly that slot.
fn assert_dead_c_reader_cleared(env: &TestEnv, rust: &Environment) {
    let mut reader = spawn_cpeer10(env, "hold-reader");
    reader.wait_ready().expect("cpeer10 hold-reader READY");
    let pid = reader.pid() as i32;
    assert!(rust.reader_list().unwrap().iter().any(|r| r.pid == pid));
    reader.kill().unwrap();
    assert_eq!(rust.reader_check().unwrap(), 1);
    assert!(rust.reader_list().unwrap().iter().all(|r| r.pid != pid));
    assert_same_readers(&rust.reader_list().unwrap(), &c_readers(env));
}

/// Replace the data file of `env` with the one of `other` (a backup of the
/// other format restored while a process has the environment open: that
/// process keeps its map of the old file).
fn swap_data_file(env: &TestEnv, other: &TestEnv) {
    let tmp = env.data_file().with_extension("new");
    std::fs::copy(other.data_file(), &tmp).expect("copy data file");
    std::fs::rename(&tmp, env.data_file()).expect("replace data file");
}

// --- the tests ------------------------------------------------------------

/// rlmdb attaches to the version 2 table of a C 1.0 process.
#[test]
fn rust_attaches_to_c10_lock_file() {
    let env = TestEnv::new().unwrap().with_format(Format::V10);
    let last = new_env10(&env);

    let mut holder = spawn_cpeer10(&env, "hold-reader");
    let ready = holder.wait_ready().expect("cpeer10 hold-reader READY");
    assert_eq!(ready_txnid(&ready), last);
    let c_pid = holder.pid() as i32;

    let a = Attached::new(&env, 4);
    let rust = &a.env;
    assert_eq!(rust.format(), Format::V10);
    let info = rust.info().unwrap();
    assert_eq!(info.last_txnid, last as usize);
    // The table size is C's, not what we asked for: we attached to C's
    // table instead of initialising our own.
    assert_eq!(info.max_readers, DEFAULT_READERS);
    let h = assert_v2_header(&env, &a, last);
    assert_eq!(h.num_readers, info.num_readers);
    assert!(h.num_readers >= 1);

    // C's reader, as rlmdb and as C see it.
    let readers = rust.reader_list().unwrap();
    assert!(
        readers
            .iter()
            .any(|r| r.pid == c_pid && r.txnid == Some(last)),
        "{readers:?}"
    );
    assert_same_readers(&readers, &c_readers(&env));
    assert_eq!(rust.reader_check().unwrap(), 0, "C's reader is alive");

    // A reader slot of ours, listed by C.
    let txn = rust.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), last);
    let c_list = c_readers(&env);
    let me = std::process::id() as i32;
    assert!(
        c_list.iter().any(|r| r.pid == me && r.txnid == Some(last)),
        "{c_list:?}"
    );
    assert_same_readers(&rust.reader_list().unwrap(), &c_list);

    assert_shared_writer_lock(&env, rust);

    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer10 hold-reader");
    assert!(rust.reader_list().unwrap().iter().all(|r| r.pid != c_pid));
    assert_dead_c_reader_cleared(&env, rust);
    drop(txn);
    assert_eq!(c_readers(&env), []);
}

/// C 1.0 attaches to a version 2 table that rlmdb created.
#[test]
fn c10_attaches_to_rust_lock_file() {
    let env = TestEnv::new_nosubdir().unwrap().with_format(Format::V10);
    let last = new_env10(&env);
    let lock_path = env.lock_file();
    std::fs::remove_file(&lock_path).unwrap();

    // We are the only user: rlmdb creates and initialises the table, then
    // publishes the last txnid and lets others in.
    let a = Attached::new(&env, 50);
    let rust = &a.env;
    assert_eq!(rust.info().unwrap().max_readers, 50);
    assert_eq!(
        std::fs::metadata(&lock_path).unwrap().len() as usize,
        LOCK_V10.file_size(50)
    );
    assert_v2_header(&env, &a, last);

    // A reader slot of ours, listed by C 1.0, which had to validate the
    // header and find the reader lock to get there.
    let txn = rust.begin_ro_txn().unwrap();
    assert_eq!(txn.id(), last);
    let me = std::process::id() as i32;
    let c_list = c_readers(&env);
    assert!(
        c_list.iter().any(|r| r.pid == me && r.txnid == Some(last)),
        "{c_list:?}"
    );
    assert_same_readers(&rust.reader_list().unwrap(), &c_list);
    let info = stat10(&env, &["-e"]).env.expect("environment info");
    assert_eq!(info.max_readers, 50);
    assert_eq!(info.readers_used, 1);
    assert_eq!(info.last_txnid, last);

    // A C reader, as both see it.
    let mut holder = spawn_cpeer10(&env, "hold-reader");
    let ready = holder.wait_ready().expect("cpeer10 hold-reader READY");
    assert_eq!(ready_txnid(&ready), last);
    let c_pid = holder.pid() as i32;
    let readers = rust.reader_list().unwrap();
    assert_eq!(readers.len(), 2);
    assert!(readers
        .iter()
        .any(|r| r.pid == c_pid && r.txnid == Some(last)));
    assert_same_readers(&readers, &c_readers(&env));
    assert_eq!(rust.reader_check().unwrap(), 0);

    assert_shared_writer_lock(&env, rust);

    // A C commit publishes its txnid where rlmdb reads it, and the readers
    // keep their snapshots.
    cpeer10(&env, "fill", &["-", "10", "9"]);
    let now = a.header().txnid;
    assert!(now > last, "mti_txnid {now} after a commit");
    assert_eq!(rust.info().unwrap().last_txnid, now as usize);
    assert!(rust.begin_ro_txn().unwrap().id() == now);
    assert!(rust
        .reader_list()
        .unwrap()
        .iter()
        .all(|r| r.txnid == Some(last)));

    // A version 1 opener (C 0.9) is refused while we hold the file.
    let out = tool(bins::mdb_stat(), &env, &["-e"]);
    assert!(
        !out.success() && out.stderr.contains("MDB_VERSION_MISMATCH"),
        "{out}"
    );

    holder.release().unwrap();
    holder.finish().unwrap().ok().expect("cpeer10 hold-reader");
    assert_dead_c_reader_cleared(&env, rust);
    drop(txn);
    drop(a);

    // With rlmdb gone, C 1.0 is the exclusive opener and reads the data.
    let out = tool(bins::mdb_stat10(), &env, &[]);
    assert!(
        out.success() && out.stdout.contains("Entries: 110"),
        "{out}"
    );
}

/// Lock files of the two versions refuse each other while held. rlmdb
/// meets a held table of the other version when the data file was replaced
/// under a live process (its sniff then picks the other version): it fails
/// with `MDB_VERSION_MISMATCH` and leaves the table alone, for C 0.9's
/// version 1 table and for C 1.0's version 2 table. C 1.0 refuses rlmdb's
/// version 1 table (C 0.9 refusing rlmdb's version 2 table is in
/// `c10_attaches_to_rust_lock_file`).
#[test]
fn lock_versions_refuse_each_other() {
    let env09 = TestEnv::new().unwrap();
    Peer::C
        .on("create", &env09)
        .args(["67108864", "4"])
        .run()
        .expect("cpeer create");
    let env10 = TestEnv::new().unwrap().with_format(Format::V10);
    new_env10(&env10);
    // Windows does not let a data file be replaced while a process has it
    // open (C and rlmdb open it without FILE_SHARE_DELETE), so the held table
    // of the other version cannot arise there.
    let swaps: &[_] = if cfg!(windows) {
        interop::skipped("lock_versions_refuse_each_other: the data file of an open environment cannot be replaced on Windows");
        &[]
    } else {
        &[
            (&env09, &env10, Peer::C, &LOCK_V09),
            (&env10, &env09, Peer::C10, &LOCK_V10),
        ]
    };
    for &(env, other, c, lock) in swaps {
        // A copy of `env` that the C peer of its release holds open.
        let held = env.derived("held");
        c.on("copy", env).arg(held.path()).run().expect("copy");
        let mut holder = c
            .on("hold-env", &held)
            .args(["60", "--until-stdin"])
            .spawn()
            .expect("start hold-env");
        holder.wait_ready().expect("hold-env READY");
        swap_data_file(&held, other);
        assert_version_mismatch(open(&held, 126));
        assert_eq!(header(&held, lock).format, lock.lock_format);
        holder.release().unwrap();
        holder.finish().unwrap().ok().expect("hold-env");
    }

    // rlmdb holds a version 1 table: C 1.0 is refused.
    let a = Attached::new(&env09, 126);
    assert_eq!(a.env.format(), Format::V09);
    let out = tool(bins::mdb_stat10(), &env09, &["-e"]);
    assert!(
        !out.success() && out.stderr.contains("MDB_VERSION_MISMATCH"),
        "{out}"
    );
    assert_eq!(a.header().format, LOCK_V09.lock_format);
}

/// Nobody holds it: an exclusive opener of either implementation
/// re-initialises a stale lock file of the other version in the version of
/// its data file (a lock file left behind by the other format's
/// environment).
#[test]
fn stale_lock_files_of_the_other_version_are_reinitialised() {
    let env09 = TestEnv::new().unwrap();
    Peer::C
        .on("create", &env09)
        .args(["67108864", "4"])
        .run()
        .expect("cpeer create");
    let env10 = TestEnv::new().unwrap().with_format(Format::V10);
    new_env10(&env10);
    let (v1, v2) = (
        std::fs::read(env09.lock_file()).unwrap(),
        std::fs::read(env10.lock_file()).unwrap(),
    );
    assert_eq!(header(&env09, &LOCK_V09).format, LOCK_V09.lock_format);
    assert_eq!(header(&env10, &LOCK_V10).format, LOCK_V10.lock_format);

    // rlmdb, both ways.
    for (env, stale, layout) in [(&env09, &v2, &LOCK_V09), (&env10, &v1, &LOCK_V10)] {
        std::fs::write(env.lock_file(), stale).unwrap();
        let rust = open(env, 126).expect("rlmdb opens the environment");
        assert_eq!(rust.format(), layout.format);
        assert!(rust.begin_rw_txn().is_ok());
        drop(rust);
        assert_eq!(header(env, layout).format, layout.lock_format);
    }
    // And C, both ways.
    for (env, stale, layout, stat) in [
        (&env09, &v2, &LOCK_V09, bins::mdb_stat()),
        (&env10, &v1, &LOCK_V10, bins::mdb_stat10()),
    ] {
        std::fs::write(env.lock_file(), stale).unwrap();
        let out = tool(stat, env, &["-e"]);
        assert!(out.success(), "{out}");
        assert_eq!(header(env, layout).format, layout.lock_format);
    }
}
