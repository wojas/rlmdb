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
//! transaction it is bound to; when a read-only transaction is reset,
//! committed or aborted it first drops the Rust cursors (they borrow it, see
//! `crate::erased`), leaving the C handles alive but detached. A detached
//! cursor's operations return `EINVAL` (C: undefined behaviour); a cursor of
//! a reset transaction returns `MDB_BAD_TXN` as in C, and after
//! `mdb_txn_renew` it is re-created unpositioned on first use. Cursors of a
//! write transaction are tracked cursors of the core
//! ([`lmdb_core::CursorId`]); the transaction frees their handles when it
//! ends, as C does.
//!
//! Re-entrancy: while a write operation runs, the transaction state is
//! borrowed mutably. A C comparison function that calls back into the
//! library with the same transaction gets `EINVAL` (C would let it read
//! inconsistent state). One that frees a handle the running operation is
//! using (`mdb_txn_commit`/`mdb_txn_abort` of its transaction or an
//! ancestor, `mdb_cursor_close` of a cursor that may be in use) aborts the
//! process with a message: the alternative is a use after free, which is
//! what C does. `mdb_txn_reset` of a transaction in use does nothing and
//! `mdb_txn_renew` returns `EINVAL`. `mdb_env_close` cannot be guarded
//! this way; as in C, it must not be called while transactions are alive.

use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr::{self, NonNull};
use std::sync::atomic::AtomicPtr;
use std::sync::{Mutex, MutexGuard, OnceLock};

use lmdb_core::consts::DEFAULT_READERS;
use lmdb_core::{CursorId, Dbi, Environment, RwTxn, Transaction};

use crate::erased::{ErasedInactiveTxn, ErasedRoCursor, ErasedRoTxn, ErasedRwTxn};
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

/// Aborts the process because `what` would free a handle that an operation
/// further up the stack is using: a callback of that operation (a
/// comparison function) called back into the library. See "Re-entrancy"
/// in the module documentation.
#[cold]
fn abort_in_use(what: &str) -> ! {
    eprintln!("lmdb: {what} called on a handle that is in use (from a callback?); aborting");
    std::process::abort()
}

// ---------------------------------------------------------------------------
// MDB_txn
// ---------------------------------------------------------------------------

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
    /// An active write transaction.
    Write(ErasedRwTxn),
}

/// `MDB_BAD_TXN`.
pub(crate) fn bad_txn() -> c_int {
    lmdb_core::MdbError::BadTxn.to_err_code()
}

/// `MDB_txn`: a transaction handle (opaque to C).
pub struct MDB_txn {
    env: NonNull<MDB_env>,
    state: RefCell<TxnState>,
    /// A write transaction (not `MDB_TXN_RDONLY`).
    write: bool,
    /// `mt_txnid`: kept across `mdb_txn_reset` as C does.
    txnid: Cell<u64>,
    /// Cursors bound to this transaction (see the module documentation).
    cursors: RefCell<Vec<NonNull<MDB_cursor>>>,
    /// `mt_parent`: the parent of a nested transaction, else NULL.
    parent: *mut MDB_txn,
    /// `mt_child`: the active nested transaction, else NULL. While there
    /// is one, this transaction's state is borrowed by the child and must
    /// not be touched (`crate::erased` rule 5): every operation fails with
    /// `MDB_BAD_TXN` (C's `MDB_TXN_HAS_CHILD`).
    child: Cell<*mut MDB_txn>,
}

impl MDB_txn {
    /// A handle for the read transaction `txn` of `env` (the pointer the
    /// caller passed to `mdb_txn_begin`, kept as is because
    /// `mdb_txn_env` hands it back).
    pub(crate) fn new_read(env: NonNull<MDB_env>, txn: ErasedRoTxn) -> MDB_txn {
        let txnid = lmdb_core::Transaction::id(txn.txn());
        MDB_txn::new(env, TxnState::Read(txn), txnid, ptr::null_mut())
    }

    /// A handle for the write transaction `txn` of `env`; `parent` is the
    /// parent's handle for a nested transaction, else NULL.
    pub(crate) fn new_write(
        env: NonNull<MDB_env>,
        txn: ErasedRwTxn,
        parent: *mut MDB_txn,
    ) -> MDB_txn {
        let txnid = lmdb_core::Transaction::id(txn.txn());
        MDB_txn::new(env, TxnState::Write(txn), txnid, parent)
    }

    fn new(env: NonNull<MDB_env>, state: TxnState, txnid: u64, parent: *mut MDB_txn) -> MDB_txn {
        MDB_txn {
            env,
            write: matches!(state, TxnState::Write(_)),
            state: RefCell::new(state),
            txnid: Cell::new(txnid),
            cursors: RefCell::new(Vec::new()),
            parent,
            child: Cell::new(ptr::null_mut()),
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

    /// The transaction state, borrowed for one call. `None` if it is
    /// borrowed mutably already (a C comparison function called back into
    /// the library during a write) or lent to an active child.
    fn state(&self) -> Option<std::cell::Ref<'_, TxnState>> {
        if self.has_child() {
            return None;
        }
        self.state.try_borrow().ok()
    }

    /// `MDB_TXN_HAS_CHILD`: a nested transaction is active.
    pub(crate) fn has_child(&self) -> bool {
        !self.child.get().is_null()
    }

    /// `mdb_txn_begin` with this transaction as the parent: `EINVAL` for a
    /// read-only parent, `MDB_BAD_TXN` if `read_only` is asked for or the
    /// parent is blocked (it has a child, or failed), as in C.
    pub(crate) fn begin_child(&self, read_only: bool) -> Result<ErasedRwTxn, c_int> {
        if !self.write {
            return Err(libc::EINVAL);
        }
        if read_only || self.has_child() {
            return Err(bad_txn());
        }
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return Err(libc::EINVAL);
        };
        match &mut *state {
            // SAFETY: the caller records the child in `self.child` before
            // anything else can reach `self`; from then on every access to
            // the parent's state is refused until the child has ended
            // (crate::erased rule 5).
            TxnState::Write(w) => unsafe { w.begin_nested() }.map_err(|e| e.to_err_code()),
            _ => Err(libc::EINVAL),
        }
    }

    /// Records the active child (`mt_child`).
    pub(crate) fn set_child(&self, child: *mut MDB_txn) {
        self.child.set(child);
    }

    /// The innermost active nested transaction below this one (NULL if it
    /// has no child). C moves an ancestor's cursors into it for its
    /// lifetime (`mdb_cursor_shadow`), so they are used through it.
    pub(crate) fn innermost_child(&self) -> *mut MDB_txn {
        let mut innermost = ptr::null_mut();
        let mut next = self.child.get();
        // SAFETY: an active child is a live handle; it clears its parent's
        // `child` before it is freed.
        while let Some(t) = unsafe { MDB_txn::from_ptr(next) } {
            innermost = next;
            next = t.child.get();
        }
        innermost
    }

    /// The active read transaction, or `err` if the transaction is reset or
    /// finished (`EINVAL` for a write transaction).
    pub(crate) fn with_read<T>(
        &self,
        err: c_int,
        f: impl FnOnce(&ErasedRoTxn) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        match self.state().as_deref() {
            Some(TxnState::Read(txn)) => f(txn),
            Some(TxnState::Reset(_) | TxnState::Finished) => Err(err),
            Some(TxnState::Write(_)) | None => Err(libc::EINVAL),
        }
    }

    /// The active transaction (read or write) for an operation that only
    /// reads, or `err` if it is a reset or finished read transaction
    /// (`MDB_BAD_TXN` for a write transaction with an active child).
    pub(crate) fn with_txn<T>(
        &self,
        err: c_int,
        f: impl FnOnce(&dyn Transaction) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        if self.has_child() {
            return Err(bad_txn());
        }
        match self.state().as_deref() {
            Some(TxnState::Read(txn)) => f(txn.txn()),
            Some(TxnState::Write(w)) => f(w.txn()),
            Some(TxnState::Reset(_) | TxnState::Finished) => Err(err),
            None => Err(libc::EINVAL),
        }
    }

    /// The write transaction, or `EACCES` for a read-only one (C checks
    /// `MDB_TXN_RDONLY` first, so a reset read transaction gets `EACCES`
    /// too), or `MDB_BAD_TXN` while a nested transaction is active.
    pub(crate) fn with_write<T>(
        &self,
        f: impl FnOnce(&mut RwTxn<'static>) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        if !self.write {
            return Err(libc::EACCES);
        }
        if self.has_child() {
            return Err(bad_txn());
        }
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return Err(libc::EINVAL);
        };
        match &mut *state {
            TxnState::Write(w) => f(w.txn_mut()),
            TxnState::Read(_) | TxnState::Reset(_) | TxnState::Finished => Err(libc::EACCES),
        }
    }

    /// `MDB_TXN_RDONLY`.
    pub(crate) fn is_read_only(&self) -> bool {
        !self.write
    }

    /// `mdb_txn_reset`: drop the cursors' Rust state (they stay bound to
    /// this transaction) and release the snapshot. Write transactions are
    /// left alone, as in C.
    pub(crate) fn reset(&self) {
        if self.write {
            return;
        }
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return;
        };
        if let TxnState::Read(_) = &*state {
            self.release_cursors(false);
            if let TxnState::Read(txn) = std::mem::replace(&mut *state, TxnState::Finished) {
                *state = TxnState::Reset(txn.reset());
            }
        }
    }

    /// `mdb_txn_renew` of a reset or finished read transaction (`EINVAL`
    /// for anything else, as in C).
    pub(crate) fn renew(&self) -> c_int {
        if self.write {
            return libc::EINVAL;
        }
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return libc::EINVAL;
        };
        let renewed = match std::mem::replace(&mut *state, TxnState::Finished) {
            TxnState::Reset(inactive) => inactive.renew(),
            TxnState::Finished => match self.env().open_env() {
                // SAFETY: the environment outlives its transactions
                // (crate::erased rule 1).
                Some(env) => unsafe { ErasedRoTxn::begin(env) },
                None => return libc::EINVAL,
            },
            active @ (TxnState::Read(_) | TxnState::Write(_)) => {
                *state = active;
                return libc::EINVAL;
            }
        };
        match renewed {
            Ok(txn) => {
                self.txnid.set(lmdb_core::Transaction::id(txn.txn()));
                *state = TxnState::Read(txn);
                crate::SUCCESS
            }
            Err(e) => e.to_err_code(),
        }
    }

    /// `mdb_txn_commit`: commits a write transaction, ends a read
    /// transaction, and frees the handle either way (C frees it also when
    /// the commit fails: the transaction is aborted then). An active child
    /// is committed first; if that fails, this transaction is aborted.
    ///
    /// # Safety
    ///
    /// `p` is a live handle from `mdb_txn_begin`; it is invalid afterwards.
    pub(crate) unsafe fn commit_and_free(p: *mut MDB_txn) -> c_int {
        // SAFETY: per this function's contract.
        let txn = unsafe { &*p };
        txn.assert_not_in_use("mdb_txn_commit");
        let child = txn.child.get();
        if !child.is_null() {
            // SAFETY: the active child is a live handle.
            let rc = unsafe { MDB_txn::commit_and_free(child) };
            if rc != crate::SUCCESS {
                // SAFETY: per this function's contract.
                unsafe { MDB_txn::end_and_free(p) };
                return rc;
            }
        }
        txn.release_cursors(true);
        // SAFETY: p came from Box::into_raw in mdb_txn_begin; no reference
        // derived from it outlives this call (cursors are freed or
        // detached, the child has ended).
        let handle = unsafe { Box::from_raw(p) };
        let parent = handle.parent;
        let rc = match handle.state.into_inner() {
            TxnState::Write(w) => crate::rc(w.commit()),
            TxnState::Read(_) | TxnState::Reset(_) | TxnState::Finished => crate::SUCCESS,
        };
        // SAFETY: a parent outlives its child; the child's borrow of it
        // ended with the commit above.
        if let Some(parent) = unsafe { MDB_txn::from_ptr(parent) } {
            parent.set_child(ptr::null_mut());
        }
        rc
    }

    /// Ends the transaction (`mdb_txn_abort`), after its active child, and
    /// frees the handle.
    ///
    /// # Safety
    ///
    /// `p` is a live handle from `mdb_txn_begin`; it is invalid afterwards.
    pub(crate) unsafe fn end_and_free(p: *mut MDB_txn) {
        // SAFETY: per this function's contract.
        let txn = unsafe { &*p };
        txn.assert_not_in_use("mdb_txn_abort");
        let child = txn.child.get();
        if !child.is_null() {
            // SAFETY: the active child is a live handle.
            unsafe { MDB_txn::end_and_free(child) };
        }
        txn.release_cursors(true);
        // SAFETY: as in commit_and_free. Dropping the state aborts.
        let handle = unsafe { Box::from_raw(p) };
        let parent = handle.parent;
        drop(handle);
        // SAFETY: as in commit_and_free.
        if let Some(parent) = unsafe { MDB_txn::from_ptr(parent) } {
            parent.set_child(ptr::null_mut());
        }
    }

    /// Aborts the process if an operation further up the stack is using
    /// this transaction, i.e. `what` (which frees the handle) was called
    /// from a callback of that operation, such as a comparison function.
    /// Freeing the handle would leave the running operation with dangling
    /// references (C: undefined behaviour).
    fn assert_not_in_use(&self, what: &str) {
        if self.state.try_borrow_mut().is_err() {
            abort_in_use(what);
        }
    }

    /// Registers a cursor bound to this transaction.
    pub(crate) fn register_cursor(&self, c: NonNull<MDB_cursor>) {
        self.cursors.borrow_mut().push(c);
    }

    /// Forgets a cursor (closed or renewed onto another transaction).
    pub(crate) fn unregister_cursor(&self, c: NonNull<MDB_cursor>) {
        self.cursors.borrow_mut().retain(|&x| x != c);
    }

    /// Drops the Rust state of every bound read cursor; with `end` (the
    /// transaction is about to be freed), also unbinds them, and frees the
    /// handles of write cursors, which C closes with their transaction.
    fn release_cursors(&self, end: bool) {
        let mut cursors = self.cursors.borrow_mut();
        for c in cursors.iter() {
            // SAFETY: registered cursors are live: mdb_cursor_close
            // unregisters a cursor before freeing it.
            let cursor = unsafe { c.as_ref() };
            let inner = cursor.inner.borrow_mut().take();
            if end {
                cursor.txn.set(ptr::null_mut());
                if let Some(CursorInner::Write(_)) = inner {
                    // SAFETY: the handle came from Box::into_raw in
                    // mdb_cursor_open; lmdb.h forbids using a write cursor
                    // after its transaction ended (C frees it too).
                    drop(unsafe { Box::from_raw(c.as_ptr()) });
                }
            }
        }
        if end {
            cursors.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// MDB_cursor
// ---------------------------------------------------------------------------

/// The Rust side of a cursor handle.
// Lives inside the boxed MDB_cursor; boxing the read cursor (its fixed
// cursor stacks make it large) would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub(crate) enum CursorInner {
    /// A cursor of a read-only transaction.
    Read(ErasedRoCursor),
    /// A tracked cursor of a write transaction (a handle into it).
    Write(CursorId),
}

/// What an operation on a cursor gets to work with.
pub(crate) enum CursorAccess<'a> {
    /// A cursor of a read-only transaction.
    Read(&'a mut ErasedRoCursor),
    /// A cursor of a write transaction: the transaction and the handle.
    Write(&'a mut RwTxn<'static>, CursorId),
}

/// `MDB_cursor`: a cursor handle (opaque to C).
pub struct MDB_cursor {
    /// The transaction this cursor is bound to; NULL once that read-only
    /// transaction was committed or aborted.
    txn: Cell<*mut MDB_txn>,
    dbi: MDB_dbi,
    /// The database's persistent flags when the cursor was opened (C checks
    /// `MDB_DUPSORT`/`MDB_DUPFIXED` before the transaction state in a few
    /// places).
    db_flags: c_uint,
    /// The Rust cursor; `None` while the read transaction is reset or ended.
    inner: RefCell<Option<CursorInner>>,
}

impl MDB_cursor {
    /// A new cursor handle bound to `txn` (not yet registered with it).
    pub(crate) fn new(
        txn: *mut MDB_txn,
        dbi: MDB_dbi,
        db_flags: c_uint,
        inner: CursorInner,
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

    /// The bound transaction pointer (NULL if detached). For a cursor of a
    /// write transaction with an active nested transaction, the innermost
    /// one, which the cursor works through (C: `mdb_cursor_shadow` sets
    /// `mc_txn` to the child).
    pub(crate) fn txn_ptr(&self) -> *mut MDB_txn {
        if let (Some(txn), Some(_)) = (self.txn(), self.write_id()) {
            let child = txn.innermost_child();
            if !child.is_null() {
                return child;
            }
        }
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

    /// The write-transaction cursor handle, if this is one (`C_UNTRACK`).
    pub(crate) fn write_id(&self) -> Option<CursorId> {
        match self.inner.try_borrow().ok()?.as_ref()? {
            CursorInner::Write(id) => Some(*id),
            CursorInner::Read(_) => None,
        }
    }

    /// Runs `f` on the cursor. A read cursor is (re)created if its
    /// transaction was renewed since. `EINVAL` for a detached cursor,
    /// `MDB_BAD_TXN` if the read transaction is reset or finished (C
    /// checks `MDB_TXN_BLOCKED`).
    pub(crate) fn with_cursor<T>(
        &self,
        f: impl FnOnce(CursorAccess<'_>) -> Result<T, c_int>,
    ) -> Result<T, c_int> {
        let txn = self.txn().ok_or(libc::EINVAL)?;
        if let Some(id) = self.write_id() {
            // A cursor of an ancestor works through the active child, as
            // in C; the core accepts ancestors' handles there.
            // SAFETY: the innermost child is NULL or a live handle (see
            // MDB_txn::innermost_child).
            let active = unsafe { MDB_txn::from_ptr(txn.innermost_child()) }.unwrap_or(txn);
            return active.with_write(|w| f(CursorAccess::Write(w, id)));
        }
        txn.with_read(bad_txn(), |rt| {
            let mut inner = self.inner.try_borrow_mut().map_err(|_| libc::EINVAL)?;
            if inner.is_none() {
                // SAFETY: this cursor is registered with `txn`, which drops
                // `inner` before it resets or ends the read transaction.
                let cursor =
                    unsafe { rt.cursor(Dbi::from_raw(self.dbi)) }.map_err(|e| e.to_err_code())?;
                *inner = Some(CursorInner::Read(cursor));
            }
            match inner.as_mut() {
                Some(CursorInner::Read(cursor)) => f(CursorAccess::Read(cursor)),
                _ => Err(libc::EINVAL),
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
        *c.inner.borrow_mut() = Some(CursorInner::Read(inner));
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
        if c.inner.try_borrow_mut().is_err() {
            // A read operation on this cursor is running (we were called
            // from one of its callbacks).
            abort_in_use("mdb_cursor_close");
        }
        if let Some(txn) = c.txn() {
            if let Some(id) = c.write_id() {
                if txn.has_child() {
                    // C: the cursor is shadowed by the child (mc_backup),
                    // and mdb_cursor_close ignores it.
                    return;
                }
                // An operation on the transaction may be using this cursor.
                txn.assert_not_in_use("mdb_cursor_close");
                let _ = txn.with_write(|w| {
                    w.close_cursor(id);
                    Ok(())
                });
            }
            txn.unregister_cursor(p);
        }
        c.inner.borrow_mut().take();
        // SAFETY: p came from Box::into_raw in mdb_cursor_open and is no
        // longer registered anywhere.
        drop(unsafe { Box::from_raw(p.as_ptr()) });
    }
}
