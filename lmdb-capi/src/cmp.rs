//! Adapting C comparison functions (`MDB_cmp_func`) to the Rust core's
//! comparators.
//!
//! The core takes a plain `fn(&[u8], &[u8]) -> Ordering` ([`CmpFn`]), which
//! cannot carry a C function pointer. Design: a fixed table of
//! [`SLOTS`] trampolines, each a distinct Rust `fn` that calls the C
//! function stored in its own slot of a global table. `mdb_set_compare`
//! looks the C function up in the table (or claims a free slot for it) and
//! installs the matching trampoline with `Environment::set_compare`.
//!
//! The table is keyed by the C function pointer, not by environment and
//! DBI: a program has a handful of distinct comparison functions, the same
//! function serves any number of databases and environments, and a slot
//! never needs to be freed (code addresses stay valid). No thread-local
//! state is involved, so comparisons work on any thread. More than
//! [`SLOTS`] distinct functions in one process make `mdb_set_compare` fail
//! with `ENOMEM`.

use std::cmp::Ordering;
use std::sync::OnceLock;

use lmdb::CmpFn;

use crate::types::{MDB_cmp_func, MDB_val};

/// Number of distinct C comparison functions a process can install.
pub(crate) const SLOTS: usize = 64;

/// The C function of each trampoline.
static C_FUNCTIONS: [OnceLock<MDB_cmp_func>; SLOTS] = [const { OnceLock::new() }; SLOTS];

/// Calls the C function of slot `slot` on `a` and `b`.
fn call_slot(slot: usize, a: &[u8], b: &[u8]) -> Ordering {
    // A trampoline is handed out only after its slot is set.
    let Some(f) = C_FUNCTIONS[slot].get() else {
        return Ordering::Equal;
    };
    let a = MDB_val::from_slice(a);
    let b = MDB_val::from_slice(b);
    // SAFETY: `f` is a comparison function the C program installed with
    // mdb_set_compare/mdb_set_dupsort; it gets two valid values that live
    // for the duration of the call and must not modify them (lmdb.h).
    let r = unsafe { f(&a, &b) };
    r.cmp(&0)
}

macro_rules! trampolines {
    ($($slot:literal)*) => {
        /// One Rust comparator per slot.
        static TRAMPOLINES: [CmpFn; SLOTS] = [$({
            fn trampoline(a: &[u8], b: &[u8]) -> Ordering {
                call_slot($slot, a, b)
            }
            trampoline
        }),*];
    };
}

trampolines!(
    0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
    32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61
    62 63
);

/// The Rust comparator that calls `f`, claiming a table slot for `f` on
/// first use. `None` if all slots hold other functions.
pub(crate) fn trampoline_for(f: MDB_cmp_func) -> Option<CmpFn> {
    let same = |g: &MDB_cmp_func| *g as usize == f as usize;
    for (slot, cell) in C_FUNCTIONS.iter().enumerate() {
        // Either the slot already holds `f`, or we claim it for `f`; if
        // another thread claimed it first, it may have installed `f` too.
        if cell.get().is_none() {
            let _ = cell.set(f);
        }
        if cell.get().is_some_and(same) {
            return Some(TRAMPOLINES[slot]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::raw::c_int;

    unsafe extern "C" fn by_length(a: *const MDB_val, b: *const MDB_val) -> c_int {
        // SAFETY: called by call_slot with valid values.
        let (a, b) = unsafe { (&*a, &*b) };
        a.mv_size as c_int - b.mv_size as c_int
    }

    #[test]
    fn trampoline_calls_the_c_function() {
        let t = trampoline_for(by_length).unwrap();
        assert_eq!(t(b"aaa", b"b"), Ordering::Greater);
        assert_eq!(t(b"a", b"bbbb"), Ordering::Less);
        assert_eq!(t(b"ab", b"cd"), Ordering::Equal);
        // The same function gets the same slot.
        assert_eq!(
            trampoline_for(by_length).unwrap() as usize,
            t as usize,
            "slot reused"
        );
    }
}
