//! Robustness of the C API's handle rules, called from Rust through the
//! `lmdb` rlib (the same functions C programs link against).
//!
//! * A read cursor outlives its transaction safely: after commit/abort it
//!   is detached (`EINVAL`, `mdb_cursor_txn` NULL) until renewed or
//!   closed; after `mdb_txn_reset` it reports `MDB_BAD_TXN`.
//! * A comparison function that calls back into the library with the
//!   transaction being written gets `EINVAL`, and the operation completes.
//! * A comparison function that frees a handle the running operation uses
//!   (`mdb_txn_abort`, `mdb_txn_commit`, `mdb_cursor_close`) aborts the
//!   process with a message instead of a use after free.
//! * A comparison function that unwinds (a Rust panic here, standing in for
//!   a C++ exception: the callback types are `extern "C-unwind"`) aborts
//!   the process at the library boundary instead of unwinding into C.
//!
//! The aborting cases run in a child process: the test binary re-runs
//! itself with `ROBUST_CAPI_CHILD=<scenario>`.

use std::ffi::CString;
use std::os::raw::c_int;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicUsize, Ordering};

use lmdb::cursor::*;
use lmdb::dbi::*;
use lmdb::env::*;
use lmdb::txn::*;
use lmdb::write::*;
use lmdb::{MDB_cursor, MDB_env, MDB_txn, MDB_val};
use lmdb_core::consts::*;

fn scratch(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "robust-capi-{}-{name}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn val(bytes: &[u8]) -> MDB_val {
    MDB_val {
        mv_size: bytes.len(),
        mv_data: bytes.as_ptr() as *mut _,
    }
}

fn empty() -> MDB_val {
    MDB_val {
        mv_size: 0,
        mv_data: ptr::null_mut(),
    }
}

/// An open environment with a named database holding `keys`.
struct Fixture {
    env: *mut MDB_env,
    dbi: u32,
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, keys: &[&[u8]]) -> Fixture {
        let dir = scratch(name);
        let path = CString::new(dir.to_str().unwrap()).unwrap();
        let mut env = ptr::null_mut();
        let mut txn = ptr::null_mut();
        let mut dbi = 0;
        // SAFETY: the lmdb.h contract: valid output pointers, live handles.
        unsafe {
            assert_eq!(mdb_env_create(&mut env), 0);
            assert_eq!(mdb_env_set_maxdbs(env, 4), 0);
            assert_eq!(mdb_env_open(env, path.as_ptr(), MDB_NOSYNC, 0o644), 0);
            assert_eq!(mdb_txn_begin(env, ptr::null_mut(), 0, &mut txn), 0);
            let name = CString::new("db").unwrap();
            assert_eq!(mdb_dbi_open(txn, name.as_ptr(), MDB_CREATE, &mut dbi), 0);
            for k in keys {
                let (mut kv, mut dv) = (val(k), val(b"data"));
                assert_eq!(mdb_put(txn, dbi, &mut kv, &mut dv, 0), 0);
            }
            assert_eq!(mdb_txn_commit(txn), 0);
        }
        Fixture { env, dbi, dir }
    }

    fn begin(&self, flags: u32) -> *mut MDB_txn {
        let mut txn = ptr::null_mut();
        // SAFETY: live environment, valid output pointer.
        assert_eq!(
            unsafe { mdb_txn_begin(self.env, ptr::null_mut(), flags, &mut txn) },
            0
        );
        txn
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: every transaction of the fixture has ended.
        unsafe { mdb_env_close(self.env) };
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The handles the comparison functions below act on (one scenario per
/// process at a time for the aborting ones; the others use their own).
static TXN: AtomicPtr<MDB_txn> = AtomicPtr::new(ptr::null_mut());
static CURSOR: AtomicPtr<MDB_cursor> = AtomicPtr::new(ptr::null_mut());
static REENTRANT_RC: AtomicI32 = AtomicI32::new(0);

fn order(a: *const MDB_val, b: *const MDB_val) -> c_int {
    // SAFETY: the library passes two valid values.
    let (a, b) = unsafe {
        (
            std::slice::from_raw_parts((*a).mv_data as *const u8, (*a).mv_size),
            std::slice::from_raw_parts((*b).mv_data as *const u8, (*b).mv_size),
        )
    };
    a.cmp(b) as c_int
}

unsafe extern "C-unwind" fn cmp_reads_back(a: *const MDB_val, b: *const MDB_val) -> c_int {
    let txn = TXN.load(Ordering::SeqCst);
    let (mut k, mut d) = (val(b"k1"), empty());
    // SAFETY: TXN is a live handle while the test runs.
    let rc = unsafe { mdb_get(txn, 2, &mut k, &mut d) };
    REENTRANT_RC.store(rc, Ordering::SeqCst);
    order(a, b)
}

unsafe extern "C-unwind" fn cmp_aborts_txn(a: *const MDB_val, b: *const MDB_val) -> c_int {
    // SAFETY: deliberately wrong: frees the transaction being written.
    unsafe { mdb_txn_abort(TXN.load(Ordering::SeqCst)) };
    order(a, b)
}

unsafe extern "C-unwind" fn cmp_commits_txn(a: *const MDB_val, b: *const MDB_val) -> c_int {
    // SAFETY: deliberately wrong: frees the transaction being read.
    unsafe { mdb_txn_commit(TXN.load(Ordering::SeqCst)) };
    order(a, b)
}

unsafe extern "C-unwind" fn cmp_closes_cursor(a: *const MDB_val, b: *const MDB_val) -> c_int {
    // SAFETY: deliberately wrong: frees the cursor being used.
    unsafe { mdb_cursor_close(CURSOR.load(Ordering::SeqCst)) };
    order(a, b)
}

unsafe extern "C-unwind" fn cmp_resets_txn(a: *const MDB_val, b: *const MDB_val) -> c_int {
    // SAFETY: TXN is live; resetting a transaction in use does nothing.
    unsafe { mdb_txn_reset(TXN.load(Ordering::SeqCst)) };
    order(a, b)
}

unsafe extern "C-unwind" fn cmp_panics(_: *const MDB_val, _: *const MDB_val) -> c_int {
    panic!("comparator unwinds");
}

#[test]
fn robust_read_cursor_outlives_its_transaction() {
    let f = Fixture::new("cursor", &[b"a", b"b", b"c"]);
    let (mut k, mut d) = (empty(), empty());
    // SAFETY: lmdb.h contract; the cursor is used after its transaction
    // ended on purpose (allowed for read cursors: renew or close).
    unsafe {
        let txn = f.begin(MDB_RDONLY);
        let mut c = ptr::null_mut();
        assert_eq!(mdb_cursor_open(txn, f.dbi, &mut c), 0);
        assert_eq!(mdb_cursor_get(c, &mut k, &mut d, MDB_FIRST), 0);
        // Reset: the cursor reports MDB_BAD_TXN.
        mdb_txn_reset(txn);
        assert_eq!(mdb_cursor_get(c, &mut k, &mut d, MDB_NEXT), MDB_BAD_TXN);
        // Renewed: the cursor starts over.
        assert_eq!(mdb_txn_renew(txn), 0);
        assert_eq!(mdb_cursor_get(c, &mut k, &mut d, MDB_FIRST), 0);
        // Ended: detached, EINVAL everywhere, no transaction.
        assert_eq!(mdb_txn_commit(txn), 0);
        assert_eq!(mdb_cursor_get(c, &mut k, &mut d, MDB_NEXT), libc::EINVAL);
        let mut n = 0usize;
        assert_eq!(
            mdb_cursor_count(c, &mut n),
            lmdb_core::MdbError::Incompatible.to_err_code()
        );
        assert!(mdb_cursor_txn(c).is_null());
        // Renew onto a new transaction, then abort that one and close.
        let txn2 = f.begin(MDB_RDONLY);
        assert_eq!(mdb_cursor_renew(txn2, c), 0);
        let mut key = val(b"b");
        assert_eq!(mdb_cursor_get(c, &mut key, &mut d, MDB_SET_KEY), 0);
        assert_eq!(
            std::slice::from_raw_parts(key.mv_data as *const u8, key.mv_size),
            b"b"
        );
        mdb_txn_abort(txn2);
        assert_eq!(
            mdb_cursor_get(c, &mut k, &mut d, MDB_GET_CURRENT),
            libc::EINVAL
        );
        mdb_cursor_close(c);
    }
}

#[test]
fn robust_reentrant_read_gets_einval() {
    let f = Fixture::new("reentrant", &[b"k1", b"k3"]);
    // SAFETY: lmdb.h contract.
    unsafe {
        let txn = f.begin(0);
        TXN.store(txn, Ordering::SeqCst);
        assert_eq!(f.dbi, 2);
        assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_reads_back)), 0);
        let (mut k, mut d) = (val(b"k2"), val(b"v"));
        assert_eq!(mdb_put(txn, f.dbi, &mut k, &mut d, 0), 0);
        assert_eq!(REENTRANT_RC.load(Ordering::SeqCst), libc::EINVAL);
        // Reset of a write transaction from a callback: ignored.
        assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_resets_txn)), 0);
        let (mut k, mut d) = (val(b"k4"), val(b"v"));
        assert_eq!(mdb_put(txn, f.dbi, &mut k, &mut d, 0), 0);
        assert_eq!(mdb_txn_commit(txn), 0);
        TXN.store(ptr::null_mut(), Ordering::SeqCst);
    }
}

/// The aborting scenarios, run in a child process.
fn child_scenario(name: &str) {
    let f = Fixture::new(name, &[b"a", b"c"]);
    // SAFETY: lmdb.h contract, except for what the comparator does.
    unsafe {
        let (mut k, mut d) = (val(b"b"), val(b"v"));
        match name {
            "txn_abort" => {
                let txn = f.begin(0);
                TXN.store(txn, Ordering::SeqCst);
                assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_aborts_txn)), 0);
                mdb_put(txn, f.dbi, &mut k, &mut d, 0);
            }
            "txn_commit_read" => {
                let txn = f.begin(MDB_RDONLY);
                TXN.store(txn, Ordering::SeqCst);
                assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_commits_txn)), 0);
                mdb_get(txn, f.dbi, &mut k, &mut d);
            }
            "write_cursor_close" => {
                let txn = f.begin(0);
                let mut c = ptr::null_mut();
                assert_eq!(mdb_cursor_open(txn, f.dbi, &mut c), 0);
                CURSOR.store(c, Ordering::SeqCst);
                assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_closes_cursor)), 0);
                mdb_cursor_put(c, &mut k, &mut d, 0);
            }
            "read_cursor_close" => {
                let txn = f.begin(MDB_RDONLY);
                // Cursors keep the order they were opened with: set it first.
                assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_closes_cursor)), 0);
                let mut c = ptr::null_mut();
                assert_eq!(mdb_cursor_open(txn, f.dbi, &mut c), 0);
                CURSOR.store(c, Ordering::SeqCst);
                mdb_cursor_get(c, &mut k, &mut d, MDB_SET);
            }
            "panic" => {
                let txn = f.begin(0);
                assert_eq!(mdb_set_compare(txn, f.dbi, Some(cmp_panics)), 0);
                mdb_put(txn, f.dbi, &mut k, &mut d, 0);
            }
            _ => unreachable!(),
        }
    }
    // Reaching this line is the failure: the operation should have aborted.
    eprintln!("ROBUST_CAPI_CHILD survived");
    std::process::exit(3);
}

/// Entry point of the child process (does nothing in a normal run).
#[test]
fn robust_child_entry() {
    if let Ok(name) = std::env::var("ROBUST_CAPI_CHILD") {
        child_scenario(&name);
    }
}

fn run_child(name: &str) -> (Option<i32>, String) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "robust_child_entry",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("ROBUST_CAPI_CHILD", name)
        .output()
        .unwrap();
    (
        out.status.signal(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn robust_callback_freeing_its_handle_aborts() {
    for name in [
        "txn_abort",
        "txn_commit_read",
        "write_cursor_close",
        "read_cursor_close",
    ] {
        let (signal, stderr) = run_child(name);
        assert_eq!(signal, Some(libc::SIGABRT), "{name}: {stderr}");
        assert!(
            stderr.contains("called on a handle that is in use"),
            "{name}: {stderr}"
        );
    }
}

#[test]
fn robust_unwinding_callback_aborts_at_the_boundary() {
    let (signal, stderr) = run_child("panic");
    assert_eq!(signal, Some(libc::SIGABRT), "{stderr}");
    assert!(stderr.contains("comparator unwinds"), "{stderr}");
}
