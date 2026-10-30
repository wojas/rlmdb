//! The format of new environments that do not ask for one
//! (`DUAL_FORMAT.md` §4.2): the process default of
//! `rlmdb::set_default_format`, then the experimental `RLMDB_DEFAULT_FORMAT`
//! variable, then LMDB 0.9; and that none of them changes what an existing
//! file is opened as.
//!
//! The process default and the variable are process-wide, so every test of
//! this file holds [`SERIAL`] and resets both before it returns. They are
//! in a test binary of their own: no other test opens an environment while
//! one of them has the variable set.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rlmdb::{
    default_format, process_default_format, set_default_format, version_info, Environment,
    EnvironmentFlags, Error, Format, MdbError, DEFAULT_FORMAT_ENV,
};

/// Serialises the tests of this file (see the module documentation).
static SERIAL: Mutex<()> = Mutex::new(());

/// Holds [`SERIAL`] and clears the process default and the variable, also
/// when the test fails.
struct Defaults(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Defaults {
    fn take() -> Defaults {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_default_format(None);
        std::env::remove_var(DEFAULT_FORMAT_ENV);
        Defaults(guard)
    }

    fn set_var(&self, value: &str) {
        std::env::set_var(DEFAULT_FORMAT_ENV, value);
    }
}

impl Drop for Defaults {
    fn drop(&mut self) {
        set_default_format(None);
        std::env::remove_var(DEFAULT_FORMAT_ENV);
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("rlmdb-defmt-")
        .tempdir()
        .unwrap()
}

/// Creates an environment at `dir` without a format request and returns
/// its format.
fn create(dir: &Path) -> Format {
    Environment::builder().open(dir).unwrap().format()
}

#[test]
fn without_defaults_new_environments_are_0_9() {
    let _d = Defaults::take();
    assert_eq!(process_default_format(), None);
    assert_eq!(default_format(), Format::V09);
    assert_eq!(create(tmp().path()), Format::V09);
}

#[test]
fn process_default() {
    let _d = Defaults::take();
    set_default_format(Some(Format::V10));
    assert_eq!(process_default_format(), Some(Format::V10));
    assert_eq!(default_format(), Format::V10);
    assert_eq!(create(tmp().path()), Format::V10);
    // A request of the builder wins.
    let dir = tmp();
    let env = Environment::builder()
        .format(Format::V09)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.format(), Format::V09);
    drop(env);
    set_default_format(Some(Format::V09));
    assert_eq!(create(tmp().path()), Format::V09);
    set_default_format(None);
    assert_eq!(process_default_format(), None);
    assert_eq!(default_format(), Format::V09);
}

#[test]
fn environment_variable() {
    let d = Defaults::take();
    assert!(version_info().has("default-format-env"));
    d.set_var("1.0");
    assert_eq!(default_format(), Format::V10);
    assert_eq!(create(tmp().path()), Format::V10);
    d.set_var("0.9");
    assert_eq!(create(tmp().path()), Format::V09);
    // Read at every open: unset again, the next one is 0.9.
    d.set_var("1.0");
    assert_eq!(create(tmp().path()), Format::V10);
    std::env::remove_var(DEFAULT_FORMAT_ENV);
    assert_eq!(create(tmp().path()), Format::V09);
}

#[test]
fn unparseable_values_are_ignored() {
    let d = Defaults::take();
    for value in [
        "", "1", "10", "1.0 ", " 1.0", "v1.0", "1.0.2", "0.9.36", "LMDB 1.0",
    ] {
        d.set_var(value);
        assert_eq!(default_format(), Format::V09, "{value:?}");
        assert_eq!(create(tmp().path()), Format::V09, "{value:?}");
    }
    // Not UTF-8.
    std::env::set_var(
        DEFAULT_FORMAT_ENV,
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"1.0\xff"),
    );
    assert_eq!(default_format(), Format::V09);
}

#[test]
fn order_builder_then_process_default_then_variable() {
    let d = Defaults::take();
    d.set_var("1.0");
    set_default_format(Some(Format::V09));
    assert_eq!(default_format(), Format::V09);
    assert_eq!(create(tmp().path()), Format::V09);
    let dir = tmp();
    let env = Environment::builder()
        .format(Format::V10)
        .open(dir.path())
        .unwrap();
    assert_eq!(env.format(), Format::V10);
    drop(env);
    set_default_format(None);
    assert_eq!(create(tmp().path()), Format::V10);
}

#[test]
fn defaults_never_apply_to_existing_files() {
    let d = Defaults::take();
    let (old09, old10) = (tmp(), tmp());
    assert_eq!(create(old09.path()), Format::V09);
    d.set_var("1.0");
    assert_eq!(create(old10.path()), Format::V10);
    // With either default, both files open in their own format, read-only
    // too; no MDB_VERSION_MISMATCH.
    for (default, var) in [(None, "1.0"), (None, "0.9"), (Some(Format::V10), "0.9")] {
        set_default_format(default);
        d.set_var(var);
        for (dir, format) in [(&old09, Format::V09), (&old10, Format::V10)] {
            assert_eq!(create(dir.path()), format);
            let env = Environment::builder()
                .flags(EnvironmentFlags::READ_ONLY)
                .open(dir.path())
                .unwrap();
            assert_eq!(env.format(), format);
        }
    }
    // An explicit request for the other format is still refused.
    set_default_format(Some(Format::V10));
    let err = Environment::builder()
        .format(Format::V10)
        .open(old09.path())
        .unwrap_err();
    assert!(
        matches!(err, Error::Mdb(MdbError::VersionMismatch)),
        "{err:?}"
    );
}

#[test]
fn page_size_with_a_default_of_1_0() {
    let _d = Defaults::take();
    // EnvironmentBuilder::page_size is for 1.0 environments; the default
    // format decides whether a new environment is one.
    set_default_format(Some(Format::V10));
    let dir = tmp();
    let env = Environment::builder()
        .page_size(1024)
        .open(dir.path())
        .unwrap();
    assert_eq!((env.format(), env.page_size()), (Format::V10, 1024));
    drop(env);
    set_default_format(None);
    let err = Environment::builder()
        .page_size(1024)
        .open(tmp().path())
        .unwrap_err();
    assert_eq!(err.to_err_code(), libc::EINVAL, "{err:?}");
}
