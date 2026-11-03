//! Version information and error messages: `mdb_version` and
//! `mdb_strerror` (`lmdb.h` sections "Version Macros" and "Return Codes"),
//! and rlmdb's own `rlmdb_version` and `rlmdb_version_string`
//! (`include/rlmdb.h`), filled from [`rlmdb::version_info`].
//!
//! `mdb_version`, `rlmdb_version` and `rlmdb_version_string` report the
//! version of the library they are in ([`Profile`]): this module has them
//! as [`mdb_version_of`], [`rlmdb_version_of`] and
//! [`rlmdb_version_string_of`], which each artifact exports under the C
//! names ([`artifact_symbols!`](crate::artifact_symbols)).
//! `mdb_strerror` is the same in all of them.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint};
use std::sync::OnceLock;

use rlmdb::consts::{MDB_KEYEXIST, MDB_LAST_ERRCODE_V10};

use crate::Profile;

/// `rlmdb_version_info` of `rlmdb.h`: what `rlmdb_version` returns, the
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
    /// What `mdb_version` reports: major (0, or 1 in the LMDB 1.0 stub).
    pub lmdb_major: c_int,
    /// What `mdb_version` reports: minor (9, or 0 in the LMDB 1.0 stub).
    pub lmdb_minor: c_int,
    /// What `mdb_version` reports: patch (85).
    pub lmdb_patch: c_int,
    /// The C LMDB release the 0.9 format and API match: major.
    pub v09_major: c_int,
    /// The C LMDB release the 0.9 format and API match: minor.
    pub v09_minor: c_int,
    /// The C LMDB release the 0.9 format and API match: patch.
    pub v09_patch: c_int,
    /// The C LMDB release the 1.0 format matches: major.
    pub v10_major: c_int,
    /// The C LMDB release the 1.0 format matches: minor.
    pub v10_minor: c_int,
    /// The C LMDB release the 1.0 format matches: patch.
    pub v10_patch: c_int,
}

/// The C view of [`rlmdb::version_info`] for one [`Profile`], built once.
struct CVersion {
    /// What `rlmdb_version` returns.
    info: rlmdb_version_info,
    /// What `rlmdb_version_string` returns.
    line: CString,
    /// What `mdb_version` returns.
    lmdb_line: CString,
}

/// The version information of `profile`: [`rlmdb::version_info`] with
/// the profile's `mdb_version` ([`Profile::lmdb_version`]).
pub fn version_info(profile: Profile) -> rlmdb::VersionInfo {
    let mut v = *rlmdb::version_info();
    v.lmdb = profile.lmdb_version();
    v
}

/// The C version information of `profile`, built on first use.
fn c_version(profile: Profile) -> &'static CVersion {
    static VERSIONS: [OnceLock<CVersion>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    VERSIONS[profile.index()].get_or_init(|| {
        let v = version_info(profile);
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
        };
        CVersion {
            info,
            line: CString::new(v.to_string()).unwrap_or_default(),
            lmdb_line: CString::new(v.mdb_version_string()).unwrap_or_default(),
        }
    })
}

/// `mdb_version` of the library of `profile`: return the LMDB library
/// version information.
///
/// Stores the profile's version ([`Profile::lmdb_version`]: 0, 9, 85, or 1,
/// 0, 85 in the LMDB 1.0 stub) in the non-NULL arguments and returns
/// `"LMDB 0.9.85: (rlmdb <version>)"` (or `"LMDB 1.0.85: ..."`,
/// [`rlmdb::VersionInfo::mdb_version_string`]), a static string. LMDB
/// 0.9's `lmdb.h` says 0.9.37, the C release whose ABI and file format the
/// library implements; the patch level 85, above every C release, tells
/// rlmdb apart from C LMDB.
///
/// # Safety
///
/// Each argument is NULL or writable.
pub unsafe fn mdb_version_of(
    profile: Profile,
    major: *mut c_int,
    minor: *mut c_int,
    patch: *mut c_int,
) -> *mut c_char {
    let (ma, mi, pa) = profile.lmdb_version();
    for (out, value) in [(major, ma), (minor, mi), (patch, pa)] {
        // SAFETY: NULL or writable, per lmdb.h.
        if let Some(out) = unsafe { out.as_mut() } {
            *out = value;
        }
    }
    c_version(profile).lmdb_line.as_ptr().cast_mut()
}

/// `rlmdb_version` (`rlmdb.h`) of the library of `profile`: its version
/// information, a static structure.
pub fn rlmdb_version_of(profile: Profile) -> *const rlmdb_version_info {
    &c_version(profile).info
}

/// `rlmdb_version_string` (`rlmdb.h`) of the library of `profile`: the
/// version information as one line, the [`Display`](std::fmt::Display)
/// form of [`version_info`]; a static string.
pub fn rlmdb_version_string_of(profile: Profile) -> *const c_char {
    c_version(profile).line.as_ptr()
}

/// The messages of LMDB's own codes as static C strings: those of LMDB 0.9
/// (`MDB_KEYEXIST..=MDB_BAD_DBI`) and those LMDB 1.0 adds (`MDB_PROBLEM
/// ..=MDB_ADDR_BUSY`, in LMDB 1.0's `lmdb.h`), with the texts of
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
/// LMDB's codes map to static strings, LMDB 1.0's codes (its `lmdb.h`)
/// included, where C 0.9 says "Invalid error code" (on Windows, where C
/// asks the system for any other code, the empty string); other positive values
/// are passed to the C library's `strerror`, as C LMDB does on Unix (on
/// Windows, as C does there: the C runtime's text for the `errno` values
/// LMDB returns, the system's message for other codes).
#[no_mangle]
pub unsafe extern "C" fn mdb_strerror(err: c_int) -> *mut c_char {
    let text: &CStr = if err == 0 {
        c"Successful return: 0"
    } else if (MDB_KEYEXIST..=MDB_LAST_ERRCODE_V10).contains(&err) {
        &lmdb_messages()[(err - MDB_KEYEXIST) as usize]
    } else if err < 0 && !cfg!(windows) {
        c"Invalid error code"
    } else {
        return crate::platform::system_strerror(err);
    };
    text.as_ptr() as *mut c_char
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    /// The shipped `lmdb.h` (LMDB 1.0's) and the vendored LMDB 0.9 one.
    const LMDB_H: &str = include_str!("../include/lmdb.h");
    const LMDB_09_H: &str = include_str!("../../tests/lmdb-c/0.9/lmdb.h");

    /// The `MDB_VERSION_*` numbers of `header`.
    fn header_version(header: &str) -> (i32, i32, i32) {
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
    fn mdb_version_reports_0_9_85_or_1_0_85() {
        for (profile, (ma0, mi0)) in [
            (Profile::Rlmdb, (0, 9)),
            (Profile::Lmdb09, (0, 9)),
            (Profile::Lmdb10, (1, 0)),
        ] {
            let (mut ma, mut mi, mut pa) = (-1, -1, -1);
            // SAFETY: valid outputs; the string is static and NUL-terminated.
            let s = unsafe { CStr::from_ptr(mdb_version_of(profile, &mut ma, &mut mi, &mut pa)) };
            assert_eq!((ma, mi, pa), (ma0, mi0, 85));
            let expected = format!("LMDB {ma0}.{mi0}.85: (rlmdb {})", env!("CARGO_PKG_VERSION"));
            assert_eq!(s.to_str().unwrap(), expected);
            // SAFETY: NULL outputs are allowed.
            let again = unsafe {
                mdb_version_of(profile, ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
            };
            assert_eq!(again.cast_const(), s.as_ptr());
        }
        // The series of a header, a higher patch level.
        for (profile, header) in [
            (Profile::Rlmdb, LMDB_09_H),
            (Profile::Lmdb09, LMDB_09_H),
            (Profile::Lmdb10, LMDB_H),
        ] {
            let (hma, hmi, hpa) = header_version(header);
            let (ma, mi, pa) = profile.lmdb_version();
            assert_eq!((ma, mi), (hma, hmi));
            assert!(pa > hpa);
        }
    }

    #[test]
    fn version_info_struct() {
        for profile in Profile::ALL {
            // SAFETY: rlmdb_version_of returns a pointer to a static structure.
            let info = unsafe { &*rlmdb_version_of(profile) };
            let v = rlmdb::version_info();
            assert_eq!(info.size as usize, size_of::<rlmdb_version_info>());
            assert_eq!(
                (info.rlmdb_major, info.rlmdb_minor, info.rlmdb_patch),
                (v.rlmdb.0 as c_int, v.rlmdb.1 as c_int, v.rlmdb.2 as c_int)
            );
            assert_eq!(
                (info.lmdb_major, info.lmdb_minor, info.lmdb_patch),
                profile.lmdb_version()
            );
            // The formats match the releases of the two headers.
            assert_eq!(
                (info.v09_major, info.v09_minor, info.v09_patch),
                header_version(LMDB_09_H)
            );
            assert_eq!(
                (info.v10_major, info.v10_minor, info.v10_patch),
                header_version(LMDB_H)
            );
            assert_eq!((info.v10_major, info.v10_minor, info.v10_patch), v.v10);
            // SAFETY: a static NUL-terminated string.
            let line = unsafe { CStr::from_ptr(rlmdb_version_string_of(profile)) };
            assert_eq!(line.to_str().unwrap(), version_info(profile).to_string());
        }
        // SAFETY: as above.
        let line10 = unsafe { CStr::from_ptr(rlmdb_version_string_of(Profile::Lmdb10)) };
        assert!(line10.to_str().unwrap().contains("(mdb_version 1.0.85;"));
        assert_eq!(
            version_info(Profile::Rlmdb).to_string(),
            rlmdb::version_info().to_string()
        );
    }

    #[test]
    fn shipped_header_is_the_unmodified_lmdb_1_0_header() {
        let c10 = include_str!("../../tests/lmdb-c/1.0/lmdb.h");
        // Our copy starts with one comment block (written by
        // scripts/update-c-sources.sh), then the C 1.0 header.
        let (prefix, rest) = LMDB_H.split_once(" */\n").unwrap();
        assert!(prefix.starts_with("/* lmdb.h of the rlmdb libraries"));
        assert!(prefix.contains("scripts/update-c-sources.sh"));
        assert_eq!(rest, c10);
        assert_eq!(header_version(LMDB_H), rlmdb::version_info().v10);
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
        // C on Windows has no text for these (FormatMessage fails).
        let invalid = if cfg!(windows) {
            ""
        } else {
            "Invalid error code"
        };
        assert_eq!(s(MDB_LAST_ERRCODE_V10 + 1), invalid);
        assert_eq!(s(MDB_KEYEXIST - 1), invalid);
        assert_eq!(s(-1), invalid);
        assert_eq!(s(libc::ENOENT), "No such file or directory");
        // What LMDB 1.0's unimplemented functions return (crate::v10); on
        // Windows not the system's text for 129 (crate::platform).
        if cfg!(any(windows, target_os = "linux", target_os = "macos")) {
            assert_eq!(s(libc::ENOTSUP), "Operation not supported");
        }
    }
}
