//! The opaque handles of `lmdb.h`: `MDB_env`, `MDB_txn` and `MDB_cursor`.
//!
//! Each is allocated with `Box` by its constructor (`mdb_env_create`,
//! `mdb_txn_begin`, `mdb_cursor_open`), handed to C as a raw pointer and
//! freed by its destructor (`mdb_env_close`, `mdb_txn_commit`/`abort`,
//! `mdb_cursor_close`). Between those calls the handles are only accessed
//! through shared references; state that changes uses interior mutability.
//!
//! * [`MDB_env`] may be used from several threads at once (as in C), so its
//!   mutable parts are behind a `Mutex`, `OnceLock` or atomics.
//! * [`MDB_txn`] and [`MDB_cursor`] follow C's rule that a transaction and
//!   its cursors are used by one thread at a time; they use `Cell` and
//!   `RefCell`.
//!
//! Cursor tracking: C frees read-only transactions without touching their
//! cursors, which survive (unusable) until `mdb_cursor_close` or
//! `mdb_cursor_renew`. Here every cursor is registered with the
//! transaction it is bound to; when the transaction is reset, committed or
//! aborted it first drops the Rust cursors (they borrow it, see
//! `crate::erased`), leaving the C handles alive but detached. A detached
//! cursor's operations return `EINVAL` (C: undefined behaviour); a cursor of
//! a reset transaction returns `MDB_BAD_TXN` as in C, and after
//! `mdb_txn_renew` it is re-created unpositioned on first use. Write
//! transactions (stage 3) will free their cursors at the end, as C does.

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicPtr;
use std::sync::{Mutex, MutexGuard, OnceLock};

use lmdb::consts::DEFAULT_READERS;
use lmdb::{Dbi, Environment};

use crate::erased::{ErasedInactiveTxn, ErasedRoCursor, ErasedRoTxn};
use crate::types::{MDB_assert_func, MDB_cmp_func, MDB_dbi};

// ---------------------------------------------------------------------------
// MDB_env
// ---------------------------------------------------------------------------

/// Settings made before `mdb_env_open` (C keeps them in the `MDB_env`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct EnvConfig {
    /// `mdb_env_set_mapsize`; 0 = use the size recorded in the file.
    pub(crate) map_size: usize,
    /// `mdb_env_set_maxreaders`.
    pub(crate) max_readers: c_uint,
    /// `mdb_env_set_maxdbs` (named databases, without the two core DBs).
    pub(crate) max_dbs: MDB_dbi,
    /// Flags set with `mdb_env_set_flags` before the open.
    pub(crate) flags: c_uint,
}

/// The C comparison functions installed on one DBI slot, remembered for
/// `mdb_cmp` / `mdb_dcmp`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CComparators {
    pub(crate) key: Option<MDB_cmp_func>,
    pub(crate) dup: Option<MDB_cmp_func>,
}

/// `MDB_env`: an LMDB environment handle (opaque to C).
pub struct MDB_env {
    config: Mutex<EnvConfig>,
    env: OnceLock<Environment>,
    /// `me_path` as a C string, for `mdb_env_get_path`.
    path: OnceLock<CString>,
    /// `mdb_env_set_userctx`.
    pub(crate) userctx: AtomicPtr<c_void>,
    /// `mdb_env_set_assert`. Stored only: the Rust implementation reports
    /// errors instead of asserting, so it is never called.
    pub(crate) assert_func: Mutex<Option<MDB_assert_func>>,
    /// C comparators per DBI (`mdb_set_compare`, `mdb_set_dupsort`).
    comparators: Mutex<Vec<CComparators>>,
}

impl MDB_env {
    /// A new, unopened environment handle (`mdb_env_create`).
    pub(crate) fn new() -> MDB_env {
        MDB_env {
            config: Mutex::new(EnvConfig {
                map_size: 0,
                max_readers: DEFAULT_READERS,
                max_dbs: 0,
                flags: 0,
            }),
            env: OnceLock::new(),
            path: OnceLock::new(),
            userctx: AtomicPtr::new(ptr::null_mut()),
            assert_func: Mutex::new(None),
            comparators: Mutex::new(Vec::new()),
        }
    }

    /// The handle behind a C pointer.
    ///
    /// # Safety
    ///
    /// `p` is NULL or a handle from `mdb_env_create` not yet closed.
    pub(crate) unsafe fn from_ptr<'a>(p: *const MDB_env) -> Option<&'a MDB_env> {
        // SAFETY: per this function's contract.
        unsafe { p.as_ref() }
    }

    /// The settings made before the open.
    pub(crate) fn config(&self) -> MutexGuard<'_, EnvConfig> {
        lock(&self.config)
    }

    /// The open environment, or `None` before a successful `mdb_env_open`.
    pub(crate) fn open_env(&self) -> Option<&Environment> {
        self.env.get()
    }

    /// Records a successful `mdb_env_open`.
    pub(crate) fn set_opened(&self, env: Environment, path: CString) {
        // mdb_env_open refuses an open handle, so both are still unset.
        let _ = self.env.set(env);
        let _ = self.path.set(path);
    }

    /// `me_path`: the path given to `mdb_env_open`, if open.
    pub(crate) fn path(&self) -> Option<&CString> {
        self.path.get()
    }

    /// The C comparators installed on `dbi`.
    pub(crate) fn c_comparators(&self, dbi: MDB_dbi) -> CComparators {
        lock(&self.comparators)
            .get(dbi as usize)
            .copied()
            .unwrap_or_default()
    }

    /// Updates the C comparators remembered for `dbi` (a valid handle).
    pub(crate) fn update_c_comparators(&self, dbi: MDB_dbi, f: impl FnOnce(&mut CComparators)) {
        let mut all = lock(&self.comparators);
        let i = dbi as usize;
        if all.len() <= i {
            all.resize(i + 1, CComparators::default());
        }
        f(&mut all[i]);
    }

    /// Forgets the C comparators of `dbi` (`mdb_dbi_close`).
    pub(crate) fn forget_c_comparators(&self, dbi: MDB_dbi) {
        if let Some(c) = lock(&self.comparators).get_mut(dbi as usize) {
            *c = CComparators::default();
        }
    }
}

/// Locks `m`, ignoring poisoning (a panic aborts in an `extern "C"` call
/// anyway, so a poisoned lock cannot be observed in practice).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// MDB_txn
// ---------------------------------------------------------------------------

/// Placeholder for the write transaction of stage 3. Uninhabited, so every
/// `match` on [`TxnState`] already has the arm the write path will fill in.
pub(crate) enum WriteTxn {}

/// What an `MDB_txn` holds.
pub(crate) enum TxnState {
    /// An active read-only transaction.
    Read(ErasedRoTxn),
    /// A read-only transaction after `mdb_txn_reset`: it keeps its reader
    /// slot for `mdb_txn_renew`.
    Reset(ErasedInactiveTxn),
    /// A read-only transaction whose `mdb_txn_renew` failed: it holds
    /// nothing; the next renew begins afresh.
    Finished,
    /// A write transaction (stage 3).
    #[allow(dead_code)] // constructed by the write path (stage 3)
    Write(WriteTxn),
}

/// `MDB_txn`: a transaction handle (opaque to C).
pub struct MDB_txn {
    env: NonNull<MDB_env>,
    state: RefCell<TxnState>,
    /// `mt_txnid`: kept across `mdb_txn_reset` as C does.
    txnid: Cell<u64>,
    /// Cursors bound to this transaction (see the module documentation).
    cursors: RefCell<Vec<NonNull<MDB_cursor>>>,
}

impl MDB_txn {
    /// A handle for the read transaction `txn` of `env` (the pointer the
    /// caller passed to `mdb_txn_begin`, kept as is because
    /// `mdb_txn_env` hands it back).
    pub(crate) fn new_read(env: NonNull<MDB_env>, txn: ErasedRoTxn) -> MDB_txn {
        let txnid = lmdb::Transaction::id(txn.txn());
        MDB_txn {
            env,
            state: RefCell::new(TxnState::Read(txn)),
            txnid: Cell::new(txnid),
            cursors: RefCell::new(Vec::new()),
        }
    }

    /// The handle behind a C pointer.
    ///
    /// # Safety
    ///
    /// `p` is NULL or a live handle from `mdb_txn_begin`.
    pub(crate) unsafe fn from_ptr<'a>(p: *const MDB_txn) -> Option<&'a MDB_txn> {
        // SAFETY: per this function's contract.
        unsafe { p.as_ref() }
    }

    /// The environment handle (`mdb_txn_env`).
    pub(crate) fn env_ptr(&self) -> *mut MDB_env {
        self.env.as_ptr()
    }

    /// The environment handle.
    pub(crate) fn env(&self) -> &MDB_env {
        // SAFETY: a transaction's environment outlives it (lmdb.h contract,
        // see crate::erased).
        unsafe { self.env.as_ref() }
    }

    /// `mt_txnid`.
    pub(crate) fn id(&self) -> u64 {
        self.txnid.get()
    }

    /// The transaction state, borrowed for one call.
    pub(crate) fn state(&self) -> std::cell::Ref<'_, TxnState> {
        self.state.borrow()
    }

    /// The active read transaction, or `err` if the transaction is reset or
    /// finished. (Write transactions do not exist yet.)
    pub(crate) fn with_read<T>(
        &self,
        err: c_int,
        f: impl FnOnce(&ErasedRoTxn) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        match &*self.state() {
            TxnState::Read(txn) => f(txn),
            TxnState::Reset(_) | TxnState::Finished => Err(err),
            TxnState::Write(w) => match *w {},
        }
    }

    /// `MDB_TXN_RDONLY`.
    pub(crate) fn is_read_only(&self) -> bool {
        match &*self.state() {
            TxnState::Read(_) | TxnState::Reset(_) | TxnState::Finished => true,
            TxnState::Write(w) => match *w {},
        }
    }

    /// `mdb_txn_reset`: drop the cursors' Rust state (they stay bound to
    /// this transaction) and release the snapshot.
    pub(crate) fn reset(&self) {
        let mut state = self.state.borrow_mut();
        if let TxnState::Read(_) = &*state {
            self.release_cursors(false);
            if let TxnState::Read(txn) = std::mem::replace(&mut *state, TxnState::Finished) {
                *state = TxnState::Reset(txn.reset());
            }
        }
    }

    /// `mdb_txn_renew` of a reset or finished read transaction.
    pub(crate) fn renew(&self) -> c_int {
        let mut state = self.state.borrow_mut();
        let renewed = match std::mem::replace(&mut *state, TxnState::Finished) {
            TxnState::Reset(inactive) => inactive.renew(),
            TxnState::Finished => match self.env().open_env() {
                // SAFETY: the environment outlives its transactions
                // (crate::erased rule 1).
                Some(env) => unsafe { ErasedRoTxn::begin(env) },
                None => return libc::EINVAL,
            },
            active @ TxnState::Read(_) => {
                *state = active;
                return libc::EINVAL;
            }
            TxnState::Write(w) => match w {},
        };
        match renewed {
            Ok(txn) => {
                self.txnid.set(lmdb::Transaction::id(txn.txn()));
                *state = TxnState::Read(txn);
                crate::SUCCESS
            }
            Err(e) => e.to_err_code(),
        }
    }

    /// Ends the transaction (`mdb_txn_abort`, `mdb_txn_commit` of a read
    /// transaction) and frees the handle.
    ///
    /// # Safety
    ///
    /// `p` is a live handle from `mdb_txn_begin`; it is invalid afterwards.
    pub(crate) unsafe fn end_and_free(p: *mut MDB_txn) {
        // SAFETY: per this function's contract.
        let txn = unsafe { &*p };
        txn.release_cursors(true);
        // SAFETY: p came from Box::into_raw in mdb_txn_begin; no reference
        // derived from it outlives this call (cursors are detached).
        drop(unsafe { Box::from_raw(p) });
    }

    /// Registers a cursor bound to this transaction.
    pub(crate) fn register_cursor(&self, c: NonNull<MDB_cursor>) {
        self.cursors.borrow_mut().push(c);
    }

    /// Forgets a cursor (closed or renewed onto another transaction).
    pub(crate) fn unregister_cursor(&self, c: NonNull<MDB_cursor>) {
        self.cursors.borrow_mut().retain(|&x| x != c);
    }

    /// Drops the Rust state of every bound cursor; with `detach`, also
    /// unbinds them (the transaction is about to be freed).
    fn release_cursors(&self, detach: bool) {
        let mut cursors = self.cursors.borrow_mut();
        for c in cursors.iter() {
            // SAFETY: registered cursors are live: mdb_cursor_close
            // unregisters a cursor before freeing it.
            let cursor = unsafe { c.as_ref() };
            cursor.inner.borrow_mut().take();
            if detach {
                cursor.txn.set(ptr::null_mut());
            }
        }
        if detach {
            cursors.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// MDB_cursor
// ---------------------------------------------------------------------------

/// `MDB_cursor`: a cursor handle (opaque to C).
pub struct MDB_cursor {
    /// The transaction this cursor is bound to; NULL once that transaction
    /// was committed or aborted.
    txn: Cell<*mut MDB_txn>,
    dbi: MDB_dbi,
    /// The database's persistent flags when the cursor was opened (C checks
    /// `MDB_DUPSORT`/`MDB_DUPFIXED` before the transaction state in a few
    /// places).
    db_flags: c_uint,
    /// The Rust cursor; `None` while the transaction is reset or ended.
    inner: RefCell<Option<ErasedRoCursor>>,
}

impl MDB_cursor {
    /// A new cursor handle bound to `txn` (not yet registered with it).
    pub(crate) fn new(
        txn: *mut MDB_txn,
        dbi: MDB_dbi,
        db_flags: c_uint,
        inner: ErasedRoCursor,
    ) -> MDB_cursor {
        MDB_cursor {
            txn: Cell::new(txn),
            dbi,
            db_flags,
            inner: RefCell::new(Some(inner)),
        }
    }

    /// The handle behind a C pointer.
    ///
    /// # Safety
    ///
    /// `p` is NULL or a live handle from `mdb_cursor_open`.
    pub(crate) unsafe fn from_ptr<'a>(p: *const MDB_cursor) -> Option<&'a MDB_cursor> {
        // SAFETY: per this function's contract.
        unsafe { p.as_ref() }
    }

    /// The bound transaction pointer (NULL if detached).
    pub(crate) fn txn_ptr(&self) -> *mut MDB_txn {
        self.txn.get()
    }

    /// The bound transaction, if any.
    pub(crate) fn txn(&self) -> Option<&MDB_txn> {
        // SAFETY: a bound transaction is live: it detaches its cursors
        // before it is freed.
        unsafe { MDB_txn::from_ptr(self.txn.get()) }
    }

    /// `mc_dbi`.
    pub(crate) fn dbi(&self) -> MDB_dbi {
        self.dbi
    }

    /// The database flags captured at open.
    pub(crate) fn db_flags(&self) -> c_uint {
        self.db_flags
    }

    /// Runs `f` on the Rust cursor, (re)creating it if the transaction was
    /// renewed since. `EINVAL` for a detached cursor, `MDB_BAD_TXN` if the
    /// transaction is reset or finished (C checks `MDB_TXN_BLOCKED`).
    pub(crate) fn with_cursor<T>(
        &self,
        f: impl FnOnce(&mut ErasedRoCursor) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        let txn = self.txn().ok_or(libc::EINVAL)?;
        txn.with_read(lmdb::MdbError::BadTxn.to_err_code(), |rt| {
            let mut inner = self.inner.borrow_mut();
            if inner.is_none() {
                // SAFETY: this cursor is registered with `txn`, which drops
                // `inner` before it resets or ends the read transaction.
                *inner = Some(
                    unsafe { rt.cursor(Dbi::from_raw(self.dbi)) }.map_err(|e| e.to_err_code())?,
                );
            }
            match inner.as_mut() {
                Some(cursor) => f(cursor),
                None => Err(libc::EINVAL),
            }
        })
    }

    /// Binds this cursor to `txn` with a fresh Rust cursor
    /// (`mdb_cursor_renew`).
    ///
    /// # Safety
    ///
    /// `this` is a live cursor handle and `txn` a live transaction handle.
    pub(crate) unsafe fn rebind(
        this: NonNull<MDB_cursor>,
        txn: *mut MDB_txn,
        inner: ErasedRoCursor,
    ) {
        // SAFETY: per this function's contract.
        let c = unsafe { this.as_ref() };
        if let Some(old) = c.txn() {
            old.unregister_cursor(this);
        }
        // The old Rust cursor (if any) goes before the new one is stored.
        c.inner.borrow_mut().take();
        c.txn.set(txn);
        *c.inner.borrow_mut() = Some(inner);
        // SAFETY: per this function's contract.
        unsafe { &*txn }.register_cursor(this);
    }

    /// Frees a cursor handle (`mdb_cursor_close`).
    ///
    /// # Safety
    ///
    /// `p` is a live handle from `mdb_cursor_open`; it is invalid afterwards.
    pub(crate) unsafe fn close(p: NonNull<MDB_cursor>) {
        // SAFETY: per this function's contract.
        let c = unsafe { p.as_ref() };
        if let Some(txn) = c.txn() {
            txn.unregister_cursor(p);
        }
        c.inner.borrow_mut().take();
        // SAFETY: p came from Box::into_raw in mdb_cursor_open and is no
        // longer registered anywhere.
        drop(unsafe { Box::from_raw(p.as_ptr()) });
    }
}
