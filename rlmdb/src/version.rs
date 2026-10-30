//! Version reporting: which rlmdb this is, what `mdb_version` reports, which
//! C LMDB release each supported format matches, and the capabilities a
//! program can test for.
//!
//! C LMDB only has `mdb_version()` (`lmdb.h`, "Version Macros"), which
//! cannot tell a program which implementation it runs on or what that
//! supports. [`version_info`] answers both. The C API exports the same data
//! as `rlmdb_version()`, `rlmdb_version_string()` and
//! `rlmdb_has_capability()` (`rlmdb-capi/include/rlmdb.h`); the design is in
//! `rust/DUAL_FORMAT.md` §4.3 and §4.4.

use std::fmt;

use crate::consts;

/// The capability tokens of this build, sorted. This is the only place
/// they are listed.
///
/// A token is a short lower-case name of one feature a program may want to
/// test for ([`VersionInfo::has`], `rlmdb_has_capability`). It is added when
/// the feature lands and never removed while the feature exists. Planned
/// (`rust/DUAL_FORMAT.md` §4.4):
///
/// * `format-0.9`: the LMDB 0.9 data and lock file formats
///   (`MDB_DATA_VERSION 1`, `MDB_LOCK_VERSION 1`). Present.
/// * `format-1.0`: the LMDB 1.0 data file format (`MDB_DATA_VERSION 3`):
///   environments that C LMDB 1.0 reads and writes, written as it writes
///   them, with 1.0's version 2 lock file (`MDB_LOCK_VERSION 2`), so a C
///   1.0 process and rlmdb can have one environment open at the same time.
///   [`VersionInfo::v10`] is reported exactly when this token is present.
/// * `split-freelist`: the free-list work of the `freelist-split` branch.
/// * `convert-copy`: a compacting copy that re-encodes the environment in
///   the other format.
/// * `prevsnapshot`: `MDB_PREVSNAPSHOT` (LMDB 1.0).
/// * `txn-prepare`: two-phase commit, `mdb_txn_prepare` (LMDB 1.0).
/// * `checksum`: page checksums (LMDB 1.0).
/// * `encrypt`: encrypted environments, `MDB_ENCRYPT` (LMDB 1.0).
/// * `default-format-env`: the experimental `RLMDB_DEFAULT_FORMAT`
///   environment variable is honoured ([`crate::DEFAULT_FORMAT_ENV`]), so a
///   program can test whether it still is. Present.
const CAPABILITIES: &[&str] = &["default-format-env", "format-0.9", "format-1.0"];

/// The C LMDB release whose data and lock file formats and API the 0.9
/// format matches (`libraries/liblmdb`).
const V09_RELEASE: (i32, i32, i32) = (0, 9, 36);

/// The C LMDB release the 1.0 format matches (`mdb.RE/1.0`); reported
/// only with capability `format-1.0`.
const V10_RELEASE: (i32, i32, i32) = (1, 0, 2);

/// What this build of rlmdb is and supports; see [`version_info`].
///
/// The C struct `rlmdb_version_info` has the same fields. Like it, this
/// struct only ever gains fields, so it is `#[non_exhaustive]`.
///
/// [`Display`](fmt::Display) prints the one line a bug report needs, which
/// is also what `rlmdb_version_string()` returns:
///
/// ```text
/// rlmdb 0.1.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36; format 1.0 = LMDB 1.0.2; caps: default-format-env format-0.9 format-1.0)
/// ```
///
/// The `format 1.0` part is there exactly with capability `format-1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct VersionInfo {
    /// The version of the rlmdb crate: `(major, minor, patch)`.
    pub rlmdb: (u32, u32, u32),
    /// What `mdb_version` reports, [`crate::lmdb_version`]: 0.9.85.
    pub lmdb: (i32, i32, i32),
    /// The C LMDB release whose format and API the 0.9 format matches:
    /// 0.9.36.
    pub v09: (i32, i32, i32),
    /// The C LMDB release whose format the 1.0 format matches; `(0, 0, 0)`
    /// while capability `format-1.0` is absent.
    pub v10: (i32, i32, i32),
    /// The capability tokens, sorted (see [`VersionInfo::has`]).
    pub capabilities: &'static [&'static str],
}

impl VersionInfo {
    /// Whether capability `cap` (for example `"format-0.9"`) is present.
    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.contains(&cap)
    }

    /// The string `mdb_version` returns, `LMDB 0.9.85: (rlmdb 0.1.0)`.
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
            "rlmdb {} (mdb_version {}; format 0.9 = LMDB {}",
            Dotted(self.rlmdb),
            Dotted(self.lmdb),
            Dotted(self.v09)
        )?;
        if self.has("format-1.0") {
            write!(f, "; format 1.0 = LMDB {}", Dotted(self.v10))?;
        }
        f.write_str("; caps:")?;
        for cap in self.capabilities {
            write!(f, " {cap}")?;
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
    // Derived from the list, so that the struct never claims a format the
    // build does not have.
    v10: if contains(CAPABILITIES, "format-1.0") {
        V10_RELEASE
    } else {
        (0, 0, 0)
    },
    capabilities: CAPABILITIES,
};

/// The version and capabilities of this build of rlmdb (a static value).
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

/// `list.contains(&s)` at compile time.
const fn contains(list: &[&str], s: &str) -> bool {
    let mut i = 0;
    while i < list.len() {
        if bytes_eq(list[i].as_bytes(), s.as_bytes()) {
            return true;
        }
        i += 1;
    }
    false
}

/// `a == b` at compile time.
const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
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
        assert_eq!(v.v09, (0, 9, 36));
        // The 1.0 data format is there: v10 names the C release it matches.
        assert!(v.has("format-1.0"));
        assert_eq!(v.v10, (1, 0, 2));
    }

    #[test]
    fn display_is_one_line() {
        let v = version_info();
        let expected = format!(
            "rlmdb {} (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36; \
             format 1.0 = LMDB 1.0.2; caps: default-format-env format-0.9 format-1.0)",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(v.to_string(), expected);
        assert!(!v.to_string().contains('\n'));
    }

    #[test]
    fn display_with_format_1_0() {
        let v = VersionInfo {
            rlmdb: (0, 2, 0),
            lmdb: (0, 9, 85),
            v09: (0, 9, 36),
            v10: (1, 0, 2),
            capabilities: &["convert-copy", "format-0.9", "format-1.0", "split-freelist"],
        };
        assert_eq!(
            v.to_string(),
            "rlmdb 0.2.0 (mdb_version 0.9.85; format 0.9 = LMDB 0.9.36; \
             format 1.0 = LMDB 1.0.2; caps: convert-copy format-0.9 format-1.0 split-freelist)"
        );
    }

    #[test]
    fn mdb_version_string() {
        let expected = format!("LMDB 0.9.85: (rlmdb {})", env!("CARGO_PKG_VERSION"));
        assert_eq!(version_info().mdb_version_string(), expected);
    }

    #[test]
    fn has() {
        let v = version_info();
        assert!(v.has("format-0.9"));
        assert!(v.has("format-1.0"));
        assert!(v.has("default-format-env"));
        for absent in [
            "",
            "nope",
            "format",
            "format-0.",
            "FORMAT-0.9",
            "format-0.9 ",
        ] {
            assert!(!v.has(absent), "{absent:?}");
        }
    }

    #[test]
    fn capabilities_are_sorted_short_lower_case_tokens() {
        let caps = version_info().capabilities;
        assert!(caps.contains(&"format-0.9"));
        assert!(caps.windows(2).all(|w| w[0] < w[1]), "{caps:?}");
        for cap in caps {
            assert!(
                !cap.is_empty()
                    && cap.len() <= 32
                    && cap.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"-.".contains(&b)),
                "{cap:?}"
            );
        }
    }

    #[test]
    fn const_helpers() {
        assert_eq!(parse_u32("0"), 0);
        assert_eq!(parse_u32("4711"), 4711);
        assert!(contains(&["a", "bc"], "bc"));
        assert!(!contains(&["a", "bc"], "b"));
        assert!(!contains(&[], "a"));
    }
}
