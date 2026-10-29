//! Lifetime erasure: the one place where Rust's borrow structure is
//! replaced by C's "do not use X after Y" rules.
//!
//! In [`lmdb_core::core`] a [`RoTxn<'env>`] borrows its [`Environment`] and a
//! [`RoCursor<'txn>`] borrows its transaction. In C all three are separate
//! heap objects whose lifetimes the *caller* manages. This module stores
//! the borrowing values with their lifetimes set to `'static` and states the
//! rules that make that sound:
//!
//! 1. **Environment outlives transactions.** The [`Environment`] lives
//!    inside a heap-allocated `MDB_env` that is freed only by
//!    `mdb_env_close`. `lmdb.h` requires all transactions to be ended before
//!    that (closing an environment with live transactions is undefined
//!    behaviour in C too). [`ErasedRoTxn::begin`] is `unsafe` with that
//!    contract.
//! 2. **Transactions do not move.** A read transaction is boxed and owned
//!    through a raw pointer, so cursors can hold references into it while
//!    the owning `MDB_txn` is accessed through shared references only.
//! 3. **Cursors die before their transaction ends.** [`ErasedRoTxn::cursor`]
//!    is `unsafe`: the caller (the `MDB_txn` handle) must drop every cursor
//!    it created before it resets or drops the transaction. `MDB_txn` keeps
//!    a list of its cursors for exactly this purpose
//!    (`MDB_txn::release_cursors`).
//!
//! Nothing here hands out `'static` references: values read through an
//! erased transaction or cursor borrow the erased handle itself.

use lmdb_core::{Dbi, Environment, InactiveTxn, RoCursor, RoTxn, Transaction};
use std::ptr::NonNull;

/// An active read transaction, boxed at a stable address.
pub(crate) struct ErasedRoTxn {
    txn: NonNull<RoTxn<'static>>,
}

impl ErasedRoTxn {
    /// `mdb_txn_begin(MDB_RDONLY)`.
    ///
    /// # Safety
    ///
    /// `env` must stay alive and must not move until the returned value
    /// (and anything derived from it) has been dropped.
    pub(crate) unsafe fn begin(env: &Environment) -> lmdb_core::Result<ErasedRoTxn> {
        let txn = env.begin_ro_txn()?;
        // SAFETY: only the lifetime changes; the caller guarantees that the
        // environment outlives the transaction (rule 1).
        let txn = unsafe { std::mem::transmute::<RoTxn<'_>, RoTxn<'static>>(txn) };
        Ok(ErasedRoTxn::boxed(txn))
    }

    fn boxed(txn: RoTxn<'static>) -> ErasedRoTxn {
        ErasedRoTxn {
            txn: NonNull::from(Box::leak(Box::new(txn))),
        }
    }

    /// The transaction. Slices read through it borrow `self`.
    pub(crate) fn txn(&self) -> &RoTxn<'static> {
        // SAFETY: the box is owned by self and only freed in Drop or
        // `into_inner`; it is never accessed mutably while self exists.
        unsafe { self.txn.as_ref() }
    }

    /// `mdb_cursor_open` / `mdb_cursor_renew`: a cursor over `dbi`.
    ///
    /// # Safety
    ///
    /// The returned cursor must be dropped before `self` is dropped or
    /// [`reset`](Self::reset) (rule 3).
    pub(crate) unsafe fn cursor(&self, dbi: Dbi) -> lmdb_core::Result<ErasedRoCursor> {
        let cursor = self.txn().cursor(dbi)?;
        // SAFETY: the cursor borrows the boxed transaction, which does not
        // move (rule 2) and, per the caller's guarantee, outlives it.
        let cursor = unsafe { std::mem::transmute::<RoCursor<'_>, RoCursor<'static>>(cursor) };
        Ok(ErasedRoCursor { cursor })
    }

    /// `mdb_txn_reset`: release the snapshot, keep the reader slot.
    pub(crate) fn reset(self) -> ErasedInactiveTxn {
        ErasedInactiveTxn {
            txn: self.into_inner().reset(),
        }
    }

    fn into_inner(self) -> RoTxn<'static> {
        let ptr = self.txn;
        std::mem::forget(self);
        // SAFETY: ptr came from Box::leak in `boxed`; `self` was forgotten,
        // so this is the only owner. No cursor refers to the transaction any
        // more (rule 3).
        *unsafe { Box::from_raw(ptr.as_ptr()) }
    }
}

impl Drop for ErasedRoTxn {
    /// `mdb_txn_abort`.
    fn drop(&mut self) {
        // SAFETY: as in `into_inner`; `self` is being dropped.
        drop(unsafe { Box::from_raw(self.txn.as_ptr()) });
    }
}

/// A reset read transaction (`mdb_txn_reset`): owns a reader slot only.
pub(crate) struct ErasedInactiveTxn {
    txn: InactiveTxn<'static>,
}

impl ErasedInactiveTxn {
    /// `mdb_txn_renew`. On failure the reader slot is released.
    pub(crate) fn renew(self) -> lmdb_core::Result<ErasedRoTxn> {
        // The environment is the one the transaction was begun on, still
        // alive by the contract of `ErasedRoTxn::begin`.
        self.txn.renew().map(ErasedRoTxn::boxed)
    }
}

/// A read cursor whose transaction is an [`ErasedRoTxn`].
pub(crate) struct ErasedRoCursor {
    cursor: RoCursor<'static>,
}

impl ErasedRoCursor {
    /// `mdb_cursor_get`. Returned slices borrow `self`.
    pub(crate) fn get<'a>(
        &'a mut self,
        key: Option<&[u8]>,
        data: Option<&[u8]>,
        op: lmdb_core::CursorOp,
    ) -> lmdb_core::Result<Option<(&'a [u8], &'a [u8])>> {
        self.cursor.get(key, data, op)
    }

    /// `mdb_cursor_count`.
    pub(crate) fn count(&mut self) -> lmdb_core::Result<u64> {
        self.cursor.count()
    }
}
