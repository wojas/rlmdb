//! Version reporting: which rlmdb this is, what `mdb_version` reports and
//! which C LMDB release each supported format matches.
//!
//! C LMDB only has `mdb_version()` (`lmdb.h`, "Version Macros"), which
//! cannot tell a program which implementation it runs on. [`version_info`]
//! answers that. The C API exports the same data as `rlmdb_version()` and
//! `rlmdb_version_string()` (`rlmdb-capi/include/rlmdb.h`); the design is
//! in `DUAL_FORMAT.md` §4.4.

use std::fmt;

use crate::consts;

/// The C LMDB release whose data and lock file formats and API the 0.9
/// format matches (`tests/lmdb-c/0.9`).
const V09_RELEASE: (i32, i32, i32) = (0, 9, 37);

/// The C LMDB release the 1.0 format matches (`mdb.RE/1.0`).
const V10_RELEASE: (i32, i32, i32) = (1, 0, 3);

/// What this build of rlmdb is; see [`version_info`].
///
/// The C struct `rlmdb_version_info` has the same fields. Like it, this
/// struct only ever gains fields, so it is `#[non_exhaustive]`.
///
/// [`Display`](fmt::Display) prints the one line a bug report needs, which
/// is also what `rlmdb_version_string()` returns (in the C API's LMDB 0.9
/// stub; `librlmdb` and the 1.0 stub put their own `mdb_version` in it):
///
/// ```text
/// rlmdb 0.0.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.37; format 1.0 = LMDB 1.0.3; features: split-freelist)
/// ```
///
/// The opt-in features of the build ([`crate::features`]) come last, each
/// name followed by `*` if it is on by default and `!` if it is fixed
/// ([`FeatureDefault::marker`](crate::FeatureDefault::marker)); the part is
/// left out in a build without features. The line is for humans and logs:
/// programs use [`crate::features`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VersionInfo {
    /// The version of the rlmdb crate: `(major, minor, patch)`.
    pub rlmdb: (u32, u32, u32),
    /// What `mdb_version` reports where rlmdb stands in for C LMDB 0.9,
    /// [`crate::lmdb_version`]: 0.9.85, which the tools print for `-V` and
    /// the C API's LMDB 0.9 stub reports. (The C API's `librlmdb` reports
    /// the rlmdb version, the 1.0 stub 1.0.85; their `rlmdb_version()`
    /// says so here.)
    pub lmdb: (i32, i32, i32),
    /// The C LMDB release whose format and API the 0.9 format matches:
    /// 0.9.37.
    pub v09: (i32, i32, i32),
    /// The C LMDB release whose format the 1.0 format matches: 1.0.3.
    pub v10: (i32, i32, i32),
}

impl VersionInfo {
    /// The string `mdb_version` returns, `LMDB 0.9.85: (rlmdb 0.0.0)`.
    ///
    /// It keeps the `LMDB <version>: (<text>)` shape of C's
    /// `MDB_VERSION_STRING` because programs print it verbatim; where C has
    /// its release date in the parentheses, this names rlmdb and its
    /// version. The tools print it for `-V`.
    pub fn mdb_version_string(&self) -> String {
        format!("LMDB {}: (rlmdb {})", Dotted(self.lmdb), Dotted(self.rlmdb))
    }
}

impl fmt::Display for VersionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rlmdb {} (mdb_version {}; format 0.9 = LMDB {}; format 1.0 = LMDB {}",
            Dotted(self.rlmdb),
            Dotted(self.lmdb),
            Dotted(self.v09),
            Dotted(self.v10)
        )?;
        let features = crate::feature::version_list();
        if !features.is_empty() {
            write!(f, "; features: {features}")?;
        }
        f.write_str(")")
    }
}

/// A version triple printed as `major.minor.patch`.
struct Dotted<T>((T, T, T));

impl<T: fmt::Display> fmt::Display for Dotted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (major, minor, patch) = &self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

/// This build's [`VersionInfo`].
static VERSION_INFO: VersionInfo = VersionInfo {
    rlmdb: (
        parse_u32(env!("CARGO_PKG_VERSION_MAJOR")),
        parse_u32(env!("CARGO_PKG_VERSION_MINOR")),
        parse_u32(env!("CARGO_PKG_VERSION_PATCH")),
    ),
    lmdb: (
        consts::MDB_VERSION_MAJOR,
        consts::MDB_VERSION_MINOR,
        consts::MDB_VERSION_PATCH,
    ),
    v09: V09_RELEASE,
    v10: V10_RELEASE,
};

/// The version information of this build of rlmdb (a static value).
pub fn version_info() -> &'static VersionInfo {
    &VERSION_INFO
}

/// A decimal `CARGO_PKG_VERSION_*` part as a number, at compile time (a
/// malformed one stops the build).
const fn parse_u32(s: &str) -> u32 {
    let digits = s.as_bytes();
    assert!(!digits.is_empty(), "empty version number");
    let mut n = 0u32;
    let mut i = 0;
    while i < digits.len() {
        assert!(digits[i].is_ascii_digit(), "version number not decimal");
        n = n * 10 + (digits[i] - b'0') as u32;
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        let v = version_info();
        let crate_version = format!("{}.{}.{}", v.rlmdb.0, v.rlmdb.1, v.rlmdb.2);
        assert_eq!(crate_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(v.lmdb, (0, 9, 85));
        assert_eq!(v.lmdb, crate::lmdb_version());
        assert_eq!(v.v09, (0, 9, 37));
        assert_eq!(v.v10, (1, 0, 3));
    }

    #[test]
    fn display_is_one_line() {
        let v = version_info();
        let expected = format!(
            "rlmdb {} (mdb_version 0.9.85; format 0.9 = LMDB 0.9.37; \
             format 1.0 = LMDB 1.0.3; features: split-freelist)",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(v.to_string(), expected);
        assert!(!v.to_string().contains('\n'));
    }

    #[test]
    fn display_of_other_versions() {
        let v = VersionInfo {
            rlmdb: (0, 2, 0),
            lmdb: (0, 9, 85),
            v09: (0, 9, 37),
            v10: (1, 0, 3),
        };
        assert_eq!(
            v.to_string(),
            "rlmdb 0.2.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.37; \
             format 1.0 = LMDB 1.0.3; features: split-freelist)"
        );
    }

    #[test]
    fn mdb_version_string() {
        let expected = format!("LMDB 0.9.85: (rlmdb {})", env!("CARGO_PKG_VERSION"));
        assert_eq!(version_info().mdb_version_string(), expected);
    }

    #[test]
    fn const_helpers() {
        assert_eq!(parse_u32("0"), 0);
        assert_eq!(parse_u32("4711"), 4711);
    }
}
