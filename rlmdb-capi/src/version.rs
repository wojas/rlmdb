//! Version information and error messages: `mdb_version` and
//! `mdb_strerror` (`lmdb.h` sections "Version Macros" and "Return Codes"),
//! and rlmdb's own `rlmdb_version`, `rlmdb_version_string` and
//! `rlmdb_has_capability` (`include/rlmdb.h`), all filled from
//! [`rlmdb::version_info`].

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::ptr;
use std::sync::OnceLock;

use rlmdb::consts::{MDB_KEYEXIST, MDB_LAST_ERRCODE_V10};

/// `rlmdb_version_info` of `rlmdb.h`: what [`rlmdb_version`] returns, the
/// C form of [`rlmdb::VersionInfo`]. Fields are only ever appended.
#[repr(C)]
#[derive(Debug)]
pub struct rlmdb_version_info {
    /// `sizeof(rlmdb_version_info)` of the library.
    pub size: c_uint,
    /// The rlmdb version: major.
    pub rlmdb_major: c_int,
    /// The rlmdb version: minor.
    pub rlmdb_minor: c_int,
    /// The rlmdb version: patch.
    pub rlmdb_patch: c_int,
    /// What `mdb_version` reports: major (0).
    pub lmdb_major: c_int,
    /// What `mdb_version` reports: minor (9).
    pub lmdb_minor: c_int,
    /// What `mdb_version` reports: patch (85).
    pub lmdb_patch: c_int,
    /// The C LMDB release the 0.9 format and API match: major.
    pub v09_major: c_int,
    /// The C LMDB release the 0.9 format and API match: minor.
    pub v09_minor: c_int,
    /// The C LMDB release the 0.9 format and API match: patch.
    pub v09_patch: c_int,
    /// The C LMDB release the 1.0 format matches: major (0 until
    /// capability `format-1.0` exists).
    pub v10_major: c_int,
    /// The C LMDB release the 1.0 format matches: minor.
    pub v10_minor: c_int,
    /// The C LMDB release the 1.0 format matches: patch.
    pub v10_patch: c_int,
    /// The capability tokens as C strings, sorted, terminated by NULL.
    pub capabilities: *const *const c_char,
}

/// The C view of [`rlmdb::version_info`], built once.
struct CVersion {
    /// What `rlmdb_version` returns.
    info: rlmdb_version_info,
    /// What `rlmdb_version_string` returns.
    line: CString,
    /// What `mdb_version` returns.
    lmdb_line: CString,
}

// SAFETY: the only pointers in a `CVersion` are `info.capabilities` and
// the entries of the array it points to; they point to memory leaked when
// the value was built (never freed or written again), and the value itself
// lives in a static and is only read.
unsafe impl Send for CVersion {}
// SAFETY: as for `Send`: shared access is read-only.
unsafe impl Sync for CVersion {}

/// The C version information, built on first use.
fn c_version() -> &'static CVersion {
    static VERSION: OnceLock<CVersion> = OnceLock::new();
    VERSION.get_or_init(|| {
        let v = rlmdb::version_info();
        // Leaked on purpose: rlmdb.h promises static strings and arrays.
        let capabilities: Vec<*const c_char> = v
            .capabilities
            .iter()
            .map(|cap| {
                CString::new(*cap)
                    .unwrap_or_default()
                    .into_raw()
                    .cast_const()
            })
            .chain([ptr::null()])
            .collect();
        let capabilities = Box::leak(capabilities.into_boxed_slice()).as_ptr();
        let int = |n: u32| c_int::try_from(n).unwrap_or(c_int::MAX);
        let info = rlmdb_version_info {
            size: size_of::<rlmdb_version_info>() as c_uint,
            rlmdb_major: int(v.rlmdb.0),
            rlmdb_minor: int(v.rlmdb.1),
            rlmdb_patch: int(v.rlmdb.2),
            lmdb_major: v.lmdb.0,
            lmdb_minor: v.lmdb.1,
            lmdb_patch: v.lmdb.2,
            v09_major: v.v09.0,
            v09_minor: v.v09.1,
            v09_patch: v.v09.2,
            v10_major: v.v10.0,
            v10_minor: v.v10.1,
            v10_patch: v.v10.2,
            capabilities,
        };
        CVersion {
            info,
            line: CString::new(v.to_string()).unwrap_or_default(),
            lmdb_line: CString::new(v.mdb_version_string()).unwrap_or_default(),
        }
    })
}

/// `mdb_version`: return the LMDB library version information.
///
/// Stores 0, 9, 85 in the non-NULL arguments and returns
/// `"LMDB 0.9.85: (rlmdb <version>)"` ([`rlmdb::lmdb_version`],
/// [`rlmdb::VersionInfo::mdb_version_string`]). The shipped `lmdb.h` says
/// 0.9.36, the C release whose ABI and file formats the library implements;
/// the higher patch level tells rlmdb apart from C LMDB.
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
    c_version().lmdb_line.as_ptr().cast_mut()
}

/// `rlmdb_version` (`rlmdb.h`): the version and capabilities of the
/// library, a static structure.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_version() -> *const rlmdb_version_info {
    &c_version().info
}

/// `rlmdb_version_string` (`rlmdb.h`): the version and capabilities as
/// one line, the [`Display`](std::fmt::Display) form of
/// [`rlmdb::VersionInfo`]; a static string.
#[no_mangle]
pub unsafe extern "C" fn rlmdb_version_string() -> *const c_char {
    c_version().line.as_ptr()
}

/// `rlmdb_has_capability` (`rlmdb.h`): 1 if the library has capability
/// `name`, else 0 (also for NULL and for names that are not UTF-8).
#[no_mangle]
pub unsafe extern "C" fn rlmdb_has_capability(name: *const c_char) -> c_int {
    if name.is_null() {
        return 0;
    }
    // SAFETY: a non-NULL `name` is a NUL-terminated string, per rlmdb.h.
    let name = unsafe { CStr::from_ptr(name) };
    c_int::from(
        name.to_str()
            .is_ok_and(|name| rlmdb::version_info().has(name)),
    )
}

/// The messages of LMDB's own codes as static C strings: those of LMDB 0.9
/// (`MDB_KEYEXIST..=MDB_BAD_DBI`) and those LMDB 1.0 adds (`MDB_PROBLEM
/// ..=MDB_ADDR_BUSY`, declared in `rlmdb.h`), with the texts of
/// [`rlmdb::strerror`], identical to C 0.9's and C 1.0's.
fn lmdb_messages() -> &'static [CString] {
    static MESSAGES: OnceLock<Vec<CString>> = OnceLock::new();
    MESSAGES.get_or_init(|| {
        (MDB_KEYEXIST..=MDB_LAST_ERRCODE_V10)
            .map(|code| CString::new(rlmdb::strerror(code)).unwrap_or_default())
            .collect()
    })
}

/// `mdb_strerror`: return a string describing a given error code.
///
/// LMDB's codes map to static strings, LMDB 1.0's codes (`rlmdb.h`)
/// included, where C 0.9 says "Invalid error code"; other positive values
/// are passed to the C library's `strerror`, as C LMDB does on Unix.
#[no_mangle]
pub unsafe extern "C" fn mdb_strerror(err: c_int) -> *mut c_char {
    let text: &CStr = if err == 0 {
        c"Successful return: 0"
    } else if (MDB_KEYEXIST..=MDB_LAST_ERRCODE_V10).contains(&err) {
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

    /// The `MDB_VERSION_*` numbers of the shipped `lmdb.h`.
    fn header_version() -> (i32, i32, i32) {
        let header = include_str!("../include/lmdb.h");
        let field = |name: &str| -> i32 {
            header
                .lines()
                .find_map(|l| l.strip_prefix(&format!("#define {name}\t")))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        (
            field("MDB_VERSION_MAJOR"),
            field("MDB_VERSION_MINOR"),
            field("MDB_VERSION_PATCH"),
        )
    }

    #[test]
    fn mdb_version_reports_0_9_85() {
        let (mut ma, mut mi, mut pa) = (-1, -1, -1);
        // SAFETY: valid outputs; the string is static and NUL-terminated.
        let s = unsafe { CStr::from_ptr(mdb_version(&mut ma, &mut mi, &mut pa)) };
        assert_eq!((ma, mi, pa), (0, 9, 85));
        let expected = format!("LMDB 0.9.85: (rlmdb {})", env!("CARGO_PKG_VERSION"));
        assert_eq!(s.to_str().unwrap(), expected);
        // SAFETY: NULL outputs are allowed.
        let again = unsafe { mdb_version(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) };
        assert_eq!(again.cast_const(), s.as_ptr());
        // Same series as the header, a higher patch level.
        let (hma, hmi, hpa) = header_version();
        assert_eq!((ma, mi), (hma, hmi));
        assert!(pa > hpa);
    }

    #[test]
    fn version_info_struct() {
        // SAFETY: rlmdb_version returns a pointer to a static structure.
        let info = unsafe { &*rlmdb_version() };
        let v = rlmdb::version_info();
        assert_eq!(info.size as usize, size_of::<rlmdb_version_info>());
        assert_eq!(
            (info.rlmdb_major, info.rlmdb_minor, info.rlmdb_patch),
            (v.rlmdb.0 as c_int, v.rlmdb.1 as c_int, v.rlmdb.2 as c_int)
        );
        assert_eq!(
            (info.lmdb_major, info.lmdb_minor, info.lmdb_patch),
            rlmdb::lmdb_version()
        );
        // The 0.9 format matches the release of the shipped header.
        assert_eq!(
            (info.v09_major, info.v09_minor, info.v09_patch),
            header_version()
        );
        assert_eq!((info.v10_major, info.v10_minor, info.v10_patch), v.v10);
        let mut caps = Vec::new();
        let mut p = info.capabilities;
        // SAFETY: the array is NULL-terminated and read up to the NULL; its
        // other entries are static C strings.
        unsafe {
            while !(*p).is_null() {
                caps.push(CStr::from_ptr(*p).to_str().unwrap());
                p = p.add(1);
            }
        }
        assert_eq!(caps, v.capabilities);
        // SAFETY: a static NUL-terminated string.
        let line = unsafe { CStr::from_ptr(rlmdb_version_string()) };
        assert_eq!(line.to_str().unwrap(), v.to_string());
    }

    #[test]
    fn has_capability() {
        // SAFETY: NULL or NUL-terminated strings.
        let has = |name: Option<&CStr>| unsafe {
            rlmdb_has_capability(name.map_or(ptr::null(), CStr::as_ptr))
        };
        assert_eq!(has(Some(c"format-0.9")), 1);
        assert_eq!(has(Some(c"nope")), 0);
        assert_eq!(has(Some(c"")), 0);
        assert_eq!(has(Some(c"format-0.9\xff")), 0);
        assert_eq!(has(None), 0);
    }

    #[test]
    fn shipped_header_is_the_unmodified_c_header() {
        let ours = include_str!("../include/lmdb.h");
        let c = include_str!("../../tests/lmdb-c/0.9/lmdb.h");
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
            s(rlmdb::consts::MDB_LAST_ERRCODE),
            "MDB_BAD_DBI: The specified DBI handle was closed/changed unexpectedly"
        );
        // LMDB 1.0's codes, with 1.0's texts.
        assert_eq!(
            s(rlmdb::consts::MDB_PROBLEM),
            "MDB_PROBLEM: Unexpected problem - txn should abort"
        );
        assert_eq!(
            s(rlmdb::consts::MDB_ENV_ENCRYPTION),
            "MDB_ENV_ENCRYPTION: Environment encryption mismatch"
        );
        assert_eq!(
            s(MDB_LAST_ERRCODE_V10),
            "MDB_ADDR_BUSY: Requested map address is unavailable"
        );
        assert_eq!(s(MDB_LAST_ERRCODE_V10 + 1), "Invalid error code");
        assert_eq!(s(MDB_KEYEXIST - 1), "Invalid error code");
        assert_eq!(s(-1), "Invalid error code");
        assert_eq!(s(libc::ENOENT), "No such file or directory");
    }
}
