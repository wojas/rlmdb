//! The thread-safety contract of the core types, checked at compile time.
//!
//! * `Environment`: `Send + Sync` (shared state is behind locks/atomics).
//! * `RoTxn`, `InactiveTxn`: `Send` (read transactions are not bound to a
//!   thread); `RoTxn` is not `Sync` (its DBI view uses a `RefCell`).
//! * `RwTxn`: neither (`!Send` because the Linux writer mutex belongs to the
//!   thread that locked it; `RwTxn` holds a `MutexGuard`).
//! * Cursors borrow their transaction and are not `Send`.

use rlmdb::{CursorId, Dbi, Environment, InactiveTxn, RoCursor, RoTxn, RwCursor, RwTxn};

fn send<T: Send>() {}
fn sync<T: Sync>() {}

/// `assert_not_impl!(T: Trait)`: compiles only if `T` does not implement
/// the auto trait (the trick of the `static_assertions` crate: with an
/// implementation, the call is ambiguous).
macro_rules! assert_not_impl {
    ($t:ty: $tr:path) => {{
        trait AmbiguousIfImpl<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
        #[allow(dead_code)]
        struct Invalid;
        impl<T: ?Sized + $tr> AmbiguousIfImpl<Invalid> for T {}
        <$t as AmbiguousIfImpl<_>>::check()
    }};
}

#[test]
fn robust_thread_safety_contract() {
    send::<Environment>();
    sync::<Environment>();
    send::<RoTxn<'static>>();
    send::<InactiveTxn<'static>>();
    sync::<InactiveTxn<'static>>();
    send::<Dbi>();
    sync::<Dbi>();
    send::<CursorId>();
    send::<rlmdb::Error>();

    assert_not_impl!(RoTxn<'static>: Sync);
    assert_not_impl!(RwTxn<'static>: Send);
    assert_not_impl!(RwTxn<'static>: Sync);
    assert_not_impl!(RoCursor<'static>: Send);
    assert_not_impl!(RwCursor<'static>: Send);
}
