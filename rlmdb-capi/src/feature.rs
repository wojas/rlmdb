//! The feature and environment-variable functions of `include/rlmdb.h`:
//! `rlmdb_features`, `rlmdb_feature_default`, `rlmdb_env_enable_feature`,
//! `rlmdb_env_disable_feature`, `rlmdb_env_feature_enabled` and
//! `rlmdb_env_read_environment`. C LMDB has none of them.
//!
//! The decisions are the core's ([`rlmdb::feature`],
//! [`rlmdb::EnvironmentBuilder::read_environment`]); an `MDB_env` collects
//! the program's requests until `mdb_env_open`, which hands them to the
//! builder. Unlike the Rust API, a handle reads rlmdb's environment
//! variables unless the program turns that off: an unmodified C program
//! has no other way to control rlmdb's own behaviour. Feature names are
//! C strings; one that is not UTF-8 is a name no build knows.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::OnceLock;

use rlmdb::{Feature, FeatureDefault};

use crate::handles::MDB_env;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// `RLMDB_FEATURE_OFF`: known, off unless enabled.
pub const RLMDB_FEATURE_OFF: c_int = 0;
/// `RLMDB_FEATURE_ON`: known, on unless disabled.
pub const RLMDB_FEATURE_ON: c_int = 1;
/// `RLMDB_FEATURE_FIXED`: known, on, cannot be disabled.
pub const RLMDB_FEATURE_FIXED: c_int = 2;

/// `RLMDB_UNKNOWN_FEATURE`: a name the build does not know.
pub const RLMDB_UNKNOWN_FEATURE: c_int = rlmdb::consts::RLMDB_UNKNOWN_FEATURE;
/// `RLMDB_FEATURE_IS_FIXED`: a fixed feature cannot be disabled.
pub const RLMDB_FEATURE_IS_FIXED: c_int = rlmdb::consts::RLMDB_FEATURE_IS_FIXED;

/// The `rlmdb.h` number of a default state.
fn state_number(default: FeatureDefault) -> c_int {
    match default {
        FeatureDefault::Off => RLMDB_FEATURE_OFF,
        FeatureDefault::On => RLMDB_FEATURE_ON,
        FeatureDefault::Fixed => RLMDB_FEATURE_FIXED,
    }
}

/// The feature named by the C string `name`, `EINVAL` for NULL,
/// `RLMDB_UNKNOWN_FEATURE` for a name the build does not know.
///
/// # Safety
///
/// `name` is NULL or a NUL-terminated string.
unsafe fn feature_of(name: *const c_char) -> Result<Feature, c_int> {
    if name.is_null() {
        return Err(EINVAL);
    }
    // SAFETY: name is a NUL-terminated string (rlmdb.h).
    let name = unsafe { CStr::from_ptr(name) };
    name.to_str()
        .ok()
        .and_then(Feature::from_name)
        .ok_or(RLMDB_UNKNOWN_FEATURE)
}

/// The unopened environment behind `env`, or `EINVAL` (NULL, or open).
///
/// # Safety
///
/// `env` is NULL or a live handle.
unsafe fn unopened<'a>(env: *mut MDB_env) -> Result<&'a MDB_env, c_int> {
    // SAFETY: per this function's contract.
    match unsafe { MDB_env::from_ptr(env) } {
        Some(e) if e.open_env().is_none() => Ok(e),
        _ => Err(EINVAL),
    }
}

/// Turns `Result<(), code>` into a return code.
fn code(r: Result<(), c_int>) -> c_int {
    r.err().unwrap_or(SUCCESS)
}

/// The names of [`rlmdb::features`] as C strings followed by NULL, built
/// once and never freed or changed.
struct NameList(Vec<*const c_char>);

// SAFETY: the pointers point into the strings of `NAMES`, a static that is
// built before them and never changed or freed; they are only read.
unsafe impl Send for NameList {}
// SAFETY: as for Send: shared reads of immutable static data.
unsafe impl Sync for NameList {}

/// `rlmdb_features`: the names of the features this build knows, a static
/// array of static strings ending with NULL, in the order of
/// [`rlmdb::features`].
#[no_mangle]
pub unsafe extern "C" fn rlmdb_features() -> *const *const c_char {
    static NAMES: OnceLock<Vec<CString>> = OnceLock::new();
    static LIST: OnceLock<NameList> = OnceLock::new();
    let list = LIST.get_or_init(|| {
        let names = NAMES.get_or_init(|| {
            (rlmdb::features().iter())
                .map(|f| CString::new(f.name).unwrap_or_default())
                .collect()
        });
        let mut ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        ptrs.push(ptr::null());
        NameList(ptrs)
    });
    list.0.as_ptr()
}

/// `rlmdb_feature_default`: the default state of the feature `name` in
/// this build (`RLMDB_FEATURE_OFF`, `_ON` or `_FIXED`) in `*state`.
/// `RLMDB_UNKNOWN_FEATURE` for a name the build does not know, `EINVAL`
/// for NULL arguments; `*state` is not changed on failure.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_feature_default(name: *const c_char, state: *mut c_int) -> c_int {
    if state.is_null() {
        return EINVAL;
    }
    // SAFETY: name is NULL or a NUL-terminated string (rlmdb.h).
    match unsafe { feature_of(name) } {
        Ok(f) => {
            // SAFETY: state is non-NULL and writable (rlmdb.h).
            unsafe { *state = state_number(f.default_state()) };
            SUCCESS
        }
        Err(rc) => rc,
    }
}

/// `rlmdb_env_enable_feature`: enable the feature `name` at the open,
/// whatever `RLMDB_FEATURES` says; replaces an earlier disable. A no-op
/// for a fixed feature. Only before `mdb_env_open` (`EINVAL` after, and
/// for NULL arguments); `RLMDB_UNKNOWN_FEATURE` for a name the build
/// does not know.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_enable_feature(env: *mut MDB_env, name: *const c_char) -> c_int {
    code((|| {
        // SAFETY: env is NULL or a live handle (rlmdb.h).
        let e = unsafe { unopened(env) }?;
        // SAFETY: name is NULL or a NUL-terminated string (rlmdb.h).
        let f = unsafe { feature_of(name) }?;
        e.config().features.enable(f);
        Ok(())
    })())
}

/// `rlmdb_env_disable_feature`: disable the feature `name` at the open,
/// whatever `RLMDB_FEATURES` says; replaces an earlier enable. Only before
/// `mdb_env_open` (`EINVAL` after, and for NULL arguments);
/// `RLMDB_UNKNOWN_FEATURE` for a name the build does not know,
/// `RLMDB_FEATURE_IS_FIXED` for a feature that cannot be disabled.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_disable_feature(
    env: *mut MDB_env,
    name: *const c_char,
) -> c_int {
    code((|| {
        // SAFETY: env is NULL or a live handle (rlmdb.h).
        let e = unsafe { unopened(env) }?;
        // SAFETY: name is NULL or a NUL-terminated string (rlmdb.h).
        let f = unsafe { feature_of(name) }?;
        e.config()
            .features
            .disable(f)
            .map_err(|err| err.to_err_code())
    })())
}

/// `rlmdb_env_feature_enabled`: whether the feature `name` is on, in
/// `*enabled` (1 or 0). After `mdb_env_open` the state the open resolved;
/// before it, what the program's requests and the defaults give
/// (`RLMDB_FEATURES` is read at the open). `RLMDB_UNKNOWN_FEATURE` for a
/// name the build does not know, `EINVAL` for NULL arguments; `*enabled`
/// is not changed on failure.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_feature_enabled(
    env: *mut MDB_env,
    name: *const c_char,
    enabled: *mut c_int,
) -> c_int {
    // SAFETY: env is NULL or a live handle (rlmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if enabled.is_null() {
        return EINVAL;
    }
    // SAFETY: name is NULL or a NUL-terminated string (rlmdb.h).
    let f = match unsafe { feature_of(name) } {
        Ok(f) => f,
        Err(rc) => return rc,
    };
    let on = match e.open_env() {
        Some(open) => open.has_feature(f),
        None => e.config().features.is_enabled(f),
    };
    // SAFETY: enabled is non-NULL and writable (rlmdb.h).
    unsafe { *enabled = c_int::from(on) };
    SUCCESS
}

/// `rlmdb_env_read_environment`: whether `mdb_env_open` reads rlmdb's
/// environment variables (`RLMDB_FEATURES`, the experimental
/// `RLMDB_DEFAULT_FORMAT`): on unless `on` is 0. A new handle reads them;
/// `RLMDB_IGNORE_ENV` turns them off whatever this says. Only before
/// `mdb_env_open` (`EINVAL` after, and for a NULL handle).
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_read_environment(env: *mut MDB_env, on: c_int) -> c_int {
    code((|| {
        // SAFETY: env is NULL or a live handle (rlmdb.h).
        let e = unsafe { unopened(env) }?;
        e.config().read_environment = on != 0;
        Ok(())
    })())
}
