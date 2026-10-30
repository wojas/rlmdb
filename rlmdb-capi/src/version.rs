//! `lmdb.h` sections "Version Macros" and "Return Codes": `mdb_version`
//! and `mdb_strerror`.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use rlmdb::consts::{MDB_KEYEXIST, MDB_LAST_ERRCODE};

/// `MDB_VERSION_STRING` of the shipped `lmdb.h` (0.9.36).
const VERSION_STRING: &CStr = c"LMDB 0.9.36: (Aug 6, 2026)";

/// `mdb_version`: return the LMDB library version information.
///
/// Stores 0, 9, 36 in the non-NULL arguments and returns the version
/// string; the Rust library implements the 0.9.36 ABI and file formats.
#[no_mangle]
pub unsafe extern "C" fn mdb_version(
    major: *mut c_int,
    minor: *mut c_int,
    patch: *mut c_int,
) -> *mut c_char {
    let (ma, mi, pa) = rlmdb::lmdb_version();
    for (out, value) in [(major, ma), (minor, mi), (patch, pa)] {
        // SAFETY: NULL or writable, per lmdb.h.
        if let Some(out) = unsafe { out.as_mut() } {
            *out = value;
        }
    }
    VERSION_STRING.as_ptr() as *mut c_char
}

/// The messages of LMDB's own codes, `MDB_KEYEXIST..=MDB_LAST_ERRCODE`, as
/// static C strings (texts from [`rlmdb::strerror`], identical to C's).
fn lmdb_messages() -> &'static [CString] {
    static MESSAGES: OnceLock<Vec<CString>> = OnceLock::new();
    MESSAGES.get_or_init(|| {
        (MDB_KEYEXIST..=MDB_LAST_ERRCODE)
            .map(|code| CString::new(rlmdb::strerror(code)).unwrap_or_default())
            .collect()
    })
}

/// `mdb_strerror`: return a string describing a given error code.
///
/// LMDB's codes map to static strings; other positive values are passed
/// to the C library's `strerror`, as C LMDB does on Unix.
#[no_mangle]
pub unsafe extern "C" fn mdb_strerror(err: c_int) -> *mut c_char {
    let text: &CStr = if err == 0 {
        c"Successful return: 0"
    } else if (MDB_KEYEXIST..=MDB_LAST_ERRCODE).contains(&err) {
        &lmdb_messages()[(err - MDB_KEYEXIST) as usize]
    } else if err < 0 {
        c"Invalid error code"
    } else {
        // SAFETY: strerror accepts any int and returns a NUL-terminated
        // string owned by the C library.
        return unsafe { libc::strerror(err) };
    };
    text.as_ptr() as *mut c_char
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_matches_the_shipped_header() {
        let header = include_str!("../include/lmdb.h");
        let date = header
            .lines()
            .find_map(|l| l.strip_prefix("#define MDB_VERSION_DATE\t"))
            .unwrap()
            .trim()
            .trim_matches('"');
        let (ma, mi, pa) = rlmdb::lmdb_version();
        let expected = format!("LMDB {ma}.{mi}.{pa}: ({date})");
        assert_eq!(VERSION_STRING.to_str().unwrap(), expected);
    }

    #[test]
    fn shipped_header_is_the_unmodified_c_header() {
        let ours = include_str!("../include/lmdb.h");
        let c = include_str!("../../../libraries/liblmdb/lmdb.h");
        // Our copy starts with one comment block, then the C header.
        let (prefix, rest) = ours.split_once(" */\n").unwrap();
        assert!(prefix.starts_with("/* lmdb.h for the Rust implementation"));
        assert_eq!(rest, c);
    }

    #[test]
    fn strerror_texts() {
        // SAFETY: mdb_strerror returns static NUL-terminated strings.
        let s = |e| unsafe { CStr::from_ptr(mdb_strerror(e)) }.to_str().unwrap();
        assert_eq!(s(0), "Successful return: 0");
        assert_eq!(
            s(rlmdb::consts::MDB_NOTFOUND),
            "MDB_NOTFOUND: No matching key/data pair found"
        );
        assert_eq!(
            s(MDB_LAST_ERRCODE),
            "MDB_BAD_DBI: The specified DBI handle was closed/changed unexpectedly"
        );
        // MDB_PROBLEM is LMDB 1.0 only.
        assert_eq!(s(rlmdb::consts::MDB_PROBLEM), "Invalid error code");
        assert_eq!(s(-1), "Invalid error code");
        assert_eq!(s(libc::ENOENT), "No such file or directory");
    }
}
