//! The data-file format functions of `include/rlmdb.h`:
//! `rlmdb_env_set_format`, `rlmdb_env_get_format`, `rlmdb_sniff_format`,
//! `rlmdb_set_default_format` and `rlmdb_env_set_pagesize`
//! (`DUAL_FORMAT.md` §4.4). C LMDB has none of them: a C library
//! handles one format only.
//!
//! Formats are numbered as in `rlmdb.h`: [`RLMDB_FORMAT_09`] (LMDB 0.9,
//! `MDB_DATA_VERSION 1`), [`RLMDB_FORMAT_10`] (LMDB 1.0, `MDB_DATA_VERSION
//! 3`) and [`RLMDB_FORMAT_UNKNOWN`] for "none". The decisions are the core's
//! ([`rlmdb::EnvironmentBuilder::format`], [`rlmdb::sniff_format`],
//! [`rlmdb::set_default_format`]); an existing data file always keeps its
//! format, and asking for the other one makes `mdb_env_open` fail with
//! `MDB_VERSION_MISMATCH`.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uint};

use rlmdb::consts::MDB_NOSUBDIR;
use rlmdb::{EnvironmentFlags, Format};

use crate::handles::MDB_env;
use crate::SUCCESS;

const EINVAL: c_int = libc::EINVAL;

/// `RLMDB_FORMAT_UNKNOWN`: no format (none requested, or no data file).
pub const RLMDB_FORMAT_UNKNOWN: c_uint = 0;
/// `RLMDB_FORMAT_09`: the LMDB 0.9 data file (`MDB_DATA_VERSION 1`).
pub const RLMDB_FORMAT_09: c_uint = 9;
/// `RLMDB_FORMAT_10`: the LMDB 1.0 data file (`MDB_DATA_VERSION 3`).
pub const RLMDB_FORMAT_10: c_uint = 10;

/// The smallest page size of `mdb_env_set_pagesize` (LMDB 1.0).
const MIN_SET_PAGESIZE: c_uint = 256;

/// The `rlmdb.h` number of `format`.
pub(crate) fn format_number(format: Option<Format>) -> c_uint {
    match format {
        None => RLMDB_FORMAT_UNKNOWN,
        Some(Format::V09) => RLMDB_FORMAT_09,
        Some(Format::V10) => RLMDB_FORMAT_10,
    }
}

/// The format numbered `n` in `rlmdb.h` (`None` for
/// `RLMDB_FORMAT_UNKNOWN`), or `EINVAL` for any other number.
fn format_of(n: c_uint) -> Result<Option<Format>, c_int> {
    match n {
        RLMDB_FORMAT_UNKNOWN => Ok(None),
        RLMDB_FORMAT_09 => Ok(Some(Format::V09)),
        RLMDB_FORMAT_10 => Ok(Some(Format::V10)),
        _ => Err(EINVAL),
    }
}

/// `rlmdb_env_set_format`: the format `mdb_env_open` uses for a new data
/// file and requires of an existing one; `RLMDB_FORMAT_UNKNOWN` removes the
/// request again. `EINVAL` after the open, for a NULL handle, and for
/// other values.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_set_format(env: *mut MDB_env, format: c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (rlmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if e.open_env().is_some() {
        return EINVAL;
    }
    match format_of(format) {
        Ok(f) => {
            e.config().format = f;
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `rlmdb_env_get_format`: the format of the open environment, or before
/// `mdb_env_open` the one requested with [`rlmdb_env_set_format`]
/// (`RLMDB_FORMAT_UNKNOWN` if none). `EINVAL` for NULL arguments.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_get_format(env: *mut MDB_env, format: *mut c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (rlmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    if format.is_null() {
        return EINVAL;
    }
    let value = match e.open_env() {
        Some(open) => format_number(Some(open.format())),
        None => format_number(e.config().format),
    };
    // SAFETY: format is non-NULL and writable (rlmdb.h).
    unsafe { *format = value };
    SUCCESS
}

/// `rlmdb_sniff_format`: the format of the data file of the environment
/// at `path` (a directory, or with `MDB_NOSUBDIR` in `flags` the data
/// file; other flags are ignored), read from its first meta page without
/// opening the environment or touching its lock file.
/// `RLMDB_FORMAT_UNKNOWN` if there is no data file or it is empty (a new
/// environment). `MDB_VERSION_MISMATCH` for the version 2 files of lmdb-js
/// prerelease builds, `MDB_INVALID` for a file that is not an LMDB data
/// file, an `errno` value if it cannot be read, `EINVAL` for NULL
/// arguments. On error `*format` is not changed.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_sniff_format(
    path: *const c_char,
    flags: c_uint,
    format: *mut c_uint,
) -> c_int {
    if path.is_null() || format.is_null() {
        return EINVAL;
    }
    // SAFETY: path is a NUL-terminated string (rlmdb.h).
    let path = crate::platform::path_from_c(unsafe { CStr::from_ptr(path) });
    let flags = EnvironmentFlags::from_bits_retain(flags & MDB_NOSUBDIR);
    match rlmdb::sniff_format(path, flags) {
        Ok(found) => {
            // SAFETY: format is non-NULL and writable (rlmdb.h).
            unsafe { *format = format_number(found) };
            SUCCESS
        }
        Err(e) => e.to_err_code(),
    }
}

/// `rlmdb_set_default_format`: the format of new environments of this
/// process that do not request one ([`rlmdb::set_default_format`]; it
/// takes precedence over the experimental `RLMDB_DEFAULT_FORMAT`
/// variable). `RLMDB_FORMAT_UNKNOWN` removes the process default; `EINVAL`
/// for other values.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_set_default_format(format: c_uint) -> c_int {
    match format_of(format) {
        Ok(f) => {
            rlmdb::set_default_format(f);
            SUCCESS
        }
        Err(code) => code,
    }
}

/// `rlmdb_env_set_pagesize`: LMDB 1.0's `mdb_env_set_pagesize`, the page
/// size of a new 1.0 environment: a power of two from 256 to 65536, else
/// `EINVAL` (also after the open and for a NULL handle), as in C 1.0. An
/// existing environment keeps its page size. LMDB 0.9 has no such call:
/// `mdb_env_open` fails with `EINVAL` if the environment turns out to be
/// 0.9 ([`rlmdb::EnvironmentBuilder::page_size`]).
#[no_mangle]
pub unsafe extern "C" fn rlmdb_env_set_pagesize(env: *mut MDB_env, size: c_uint) -> c_int {
    // SAFETY: env is NULL or a live handle (rlmdb.h).
    let Some(e) = (unsafe { MDB_env::from_ptr(env) }) else {
        return EINVAL;
    };
    let max = Format::V10.layout().max_pagesize;
    if e.open_env().is_some()
        || !size.is_power_of_two()
        || size < MIN_SET_PAGESIZE
        || size as usize > max
    {
        return EINVAL;
    }
    e.config().page_size = Some(size as usize);
    SUCCESS
}

/// The page size a new environment of `format` gets with `page_size` set
/// by [`rlmdb_env_set_pagesize`]: that size, or the OS page size (C 1.0's
/// `me_os_psize`), capped at the format's `MAX_PAGESIZE`.
pub(crate) fn new_page_size(format: Format, page_size: Option<usize>) -> usize {
    page_size
        .unwrap_or_else(crate::platform::os_page_size)
        .min(format.layout().max_pagesize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        for f in [None, Some(Format::V09), Some(Format::V10)] {
            assert_eq!(format_of(format_number(f)), Ok(f));
        }
        for bad in [1, 8, 11, 90, 100, c_uint::MAX] {
            assert_eq!(format_of(bad), Err(EINVAL), "{bad}");
        }
    }

    #[test]
    fn page_size_of_a_new_environment() {
        assert_eq!(new_page_size(Format::V10, Some(1024)), 1024);
        assert_eq!(new_page_size(Format::V10, Some(65536)), 65536);
        let os = new_page_size(Format::V10, None);
        assert!(os.is_power_of_two() && os >= 4096, "{os}");
        assert!(new_page_size(Format::V09, None) <= 32768);
    }
}
