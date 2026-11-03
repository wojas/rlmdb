//! The three libraries built from this crate and what sets them apart.
//!
//! The implementation (every `mdb_*` and `rlmdb_*` function) is the same in
//! all three; they differ only in their file name and SONAME, in what
//! `mdb_version` reports and in the format of new environments that ask
//! for none:
//!
//! | Library | Package | `mdb_version` | New environments |
//! |---|---|---|---|
//! | `librlmdb` (`-lrlmdb`) | `librlmdb` | rlmdb's version | LMDB 0.9 |
//! | `liblmdb.so.0`, the 0.9 stub | `lmdb09-stub` | 0.9.85 | LMDB 0.9 |
//! | `liblmdb.so`, the 1.0 stub | `lmdb10-stub` | 1.0.85 | LMDB 1.0 |
//!
//! `librlmdb` reports rlmdb's own version because programs link against
//! it explicitly; the stubs stand in for a C release and report its
//! series.
//!
//! Each artifact package (`artifacts/*` of this crate's directory) is a
//! `cdylib` and `staticlib` that depends on this crate and expands
//! [`artifact_symbols!`](crate::artifact_symbols) with its [`Profile`]: that defines the four
//! exported functions whose behaviour depends on the profile
//! (`mdb_version`, `rlmdb_version`, `rlmdb_version_string` and
//! `mdb_env_create`, which records the format a new environment falls back
//! to). All other `#[no_mangle]` functions live in this crate and are
//! exported by every artifact as they are. The profile is fixed when the
//! artifact is linked: no code runs when a library is loaded, and there is
//! no cargo feature (a workspace build would unify it across the three).

use std::os::raw::c_int;

use rlmdb::Format;

/// Which of the three libraries this is (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// `librlmdb`, the library programs link with `-lrlmdb`: `mdb_version`
    /// reports the rlmdb version, new environments are LMDB 0.9.
    Rlmdb,
    /// The stub that stands in for C LMDB 0.9 (`liblmdb.so.0`):
    /// `mdb_version` 0.9.85, new environments LMDB 0.9.
    Lmdb09,
    /// The stub that stands in for C LMDB 1.0 (`liblmdb.so`):
    /// `mdb_version` 1.0.85, new environments LMDB 1.0.
    Lmdb10,
}

impl Profile {
    /// All profiles.
    pub const ALL: [Profile; 3] = [Profile::Rlmdb, Profile::Lmdb09, Profile::Lmdb10];

    /// What `mdb_version` reports: in `librlmdb` the rlmdb version
    /// ([`rlmdb::VersionInfo::rlmdb`], the workspace's); in a stub the C
    /// release series it stands in for, with rlmdb's patch level 85 (above
    /// every C release, so that rlmdb can be told apart): 0.9.85
    /// ([`rlmdb::lmdb_version`]) or 1.0.85.
    pub fn lmdb_version(self) -> (c_int, c_int, c_int) {
        let (major, minor, patch) = rlmdb::lmdb_version();
        match self {
            Profile::Rlmdb => {
                let int = |n: u32| c_int::try_from(n).unwrap_or(c_int::MAX);
                let (major, minor, patch) = rlmdb::version_info().rlmdb;
                (int(major), int(minor), int(patch))
            }
            Profile::Lmdb09 => (major, minor, patch),
            Profile::Lmdb10 => (1, 0, patch),
        }
    }

    /// The format of a new environment that asks for none, after the
    /// process default and the `RLMDB_DEFAULT_FORMAT` variable
    /// ([`rlmdb::EnvironmentBuilder::fallback_format`]).
    pub fn default_format(self) -> Format {
        match self {
            Profile::Rlmdb | Profile::Lmdb09 => Format::V09,
            Profile::Lmdb10 => Format::V10,
        }
    }

    /// The index of this profile in [`Profile::ALL`].
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// Defines the exported functions whose behaviour depends on the library's
/// [`Profile`]: `mdb_version`, `rlmdb_version`, `rlmdb_version_string` and
/// `mdb_env_create`. Each artifact package expands it once, at its crate
/// root, with its profile; every other function of the C API comes from
/// this crate.
#[macro_export]
macro_rules! artifact_symbols {
    ($profile:expr) => {
        /// `mdb_version`: the version this library reports
        /// (`rlmdb_capi::Profile::lmdb_version`) and its string
        /// (`rlmdb_capi::version::mdb_version_of`).
        ///
        /// # Safety
        ///
        /// Each argument is NULL or writable (`lmdb.h`).
        #[no_mangle]
        pub unsafe extern "C" fn mdb_version(
            major: *mut ::std::os::raw::c_int,
            minor: *mut ::std::os::raw::c_int,
            patch: *mut ::std::os::raw::c_int,
        ) -> *mut ::std::os::raw::c_char {
            // SAFETY: the arguments are passed on with the same contract.
            unsafe { $crate::version::mdb_version_of($profile, major, minor, patch) }
        }

        /// `rlmdb_version` (`rlmdb.h`): the version information of this
        /// library, a static structure (`rlmdb_capi::version::rlmdb_version_of`).
        ///
        /// # Safety
        ///
        /// None: the function takes no arguments.
        #[no_mangle]
        pub unsafe extern "C" fn rlmdb_version() -> *const $crate::version::rlmdb_version_info {
            $crate::version::rlmdb_version_of($profile)
        }

        /// `rlmdb_version_string` (`rlmdb.h`): the version information of
        /// this library as one line, a static string
        /// (`rlmdb_capi::version::rlmdb_version_string_of`).
        ///
        /// # Safety
        ///
        /// None: the function takes no arguments.
        #[no_mangle]
        pub unsafe extern "C" fn rlmdb_version_string() -> *const ::std::os::raw::c_char {
            $crate::version::rlmdb_version_string_of($profile)
        }

        /// `mdb_env_create`: create an environment handle whose new
        /// environments fall back to this library's format
        /// (`rlmdb_capi::env::env_create`).
        ///
        /// # Safety
        ///
        /// `env` is NULL or writable (`lmdb.h`).
        #[no_mangle]
        pub unsafe extern "C" fn mdb_env_create(
            env: *mut *mut $crate::MDB_env,
        ) -> ::std::os::raw::c_int {
            // SAFETY: the argument is passed on with the same contract.
            unsafe { $crate::env::env_create($profile, env) }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_formats() {
        let crate_version = env!("CARGO_PKG_VERSION");
        let (ma, mi, pa) = Profile::Rlmdb.lmdb_version();
        assert_eq!(format!("{ma}.{mi}.{pa}"), crate_version);
        assert_eq!(Profile::Lmdb09.lmdb_version(), (0, 9, 85));
        assert_eq!(Profile::Lmdb10.lmdb_version(), (1, 0, 85));
        assert_eq!(Profile::Rlmdb.default_format(), Format::V09);
        assert_eq!(Profile::Lmdb09.default_format(), Format::V09);
        assert_eq!(Profile::Lmdb10.default_format(), Format::V10);
        for (i, p) in Profile::ALL.into_iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }
}
