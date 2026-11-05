//! Opt-in features through the Rust API (`rlmdb::feature`): the list of
//! the build, enabling and disabling by name and by [`Feature`], the state
//! of an open environment, and `RLMDB_FEATURES`, which only a builder that
//! reads the environment variables reads, which `RLMDB_IGNORE_ENV` blocks
//! and which the program's requests win over. The only feature is the
//! placeholder `test-feature` (off by default), which changes nothing but
//! its state; the states `on` and `fixed` are covered by the unit tests of
//! `rlmdb::feature` with a made-up table.
//!
//! The variables are process-wide, so every test of this file holds
//! [`SERIAL`] and clears them before it returns.

mod common;

use std::sync::{Mutex, MutexGuard};

use rlmdb::consts::{RLMDB_FEATURE_IS_FIXED, RLMDB_UNKNOWN_FEATURE};
use rlmdb::{
    features, Environment, EnvironmentBuilder, Error, Feature, FeatureDefault, FeatureRequests,
    FEATURES_ENV, IGNORE_ENV,
};

/// Serialises the tests of this file (see the module documentation).
static SERIAL: Mutex<()> = Mutex::new(());

/// Holds [`SERIAL`] with the variables cleared, also when the test fails.
struct Vars(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Vars {
    fn take() -> Vars {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        Vars(guard)
    }

    fn set(&self, name: &str, value: &str) {
        std::env::set_var(name, value);
    }
}

impl Drop for Vars {
    fn drop(&mut self) {
        clear();
    }
}

fn clear() {
    std::env::remove_var(FEATURES_ENV);
    std::env::remove_var(IGNORE_ENV);
}

const TEST: &str = "test-feature";

/// Opens a new environment with `builder` and returns whether
/// `test-feature` is on in it, by both queries.
fn test_feature_in(builder: &EnvironmentBuilder) -> rlmdb::Result<bool> {
    let dir = common::tempdir();
    let env = builder.open(dir.path())?;
    let on = env.has_feature(Feature::TestFeature);
    assert_eq!(env.feature_enabled(TEST).unwrap(), on);
    Ok(on)
}

/// A builder that reads the environment variables.
fn reading() -> EnvironmentBuilder {
    let mut b = Environment::builder();
    b.read_environment(true);
    b
}

#[test]
fn the_features_of_this_build() {
    let list = features();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].feature, Feature::TestFeature);
    assert_eq!(list[0].name, TEST);
    assert_eq!(list[0].default, FeatureDefault::Off);
    assert_eq!(Feature::TestFeature.name(), TEST);
    assert_eq!(TEST.parse::<Feature>().unwrap(), Feature::TestFeature);
    let line = rlmdb::version_info().to_string();
    assert!(line.ends_with("; features: test-feature)"), "{line}");
}

#[test]
fn enabling_by_name_and_by_value() {
    let _v = Vars::take();
    assert!(!test_feature_in(&Environment::builder()).unwrap());
    let mut b = Environment::builder();
    b.enable_feature(TEST).unwrap();
    assert_eq!(
        b.feature_requests().requested(Feature::TestFeature),
        Some(true)
    );
    assert!(test_feature_in(&b).unwrap());
    b.disable_feature(TEST).unwrap();
    assert!(!test_feature_in(&b).unwrap());
    b.enable(Feature::TestFeature);
    assert!(test_feature_in(&b).unwrap());
    b.disable(Feature::TestFeature).unwrap();
    assert!(!test_feature_in(&b).unwrap());
    // Chained, as the other options.
    let dir = common::tempdir();
    let env = Environment::builder()
        .map_size(1 << 20)
        .enable_feature(TEST)
        .unwrap()
        .max_dbs(2)
        .open(dir.path())
        .unwrap();
    assert!(env.has_feature(Feature::TestFeature));
    // The requests can be carried over.
    let mut r = FeatureRequests::new();
    r.enable(Feature::TestFeature);
    let mut b = Environment::builder();
    b.set_feature_requests(r);
    assert!(test_feature_in(&b).unwrap());
}

#[test]
fn unknown_names_are_errors() {
    let _v = Vars::take();
    let mut b = Environment::builder();
    b.enable(Feature::TestFeature);
    for name in [
        "no-such-feature",
        "",
        "Test-Feature",
        "test-feature ",
        "-test-feature",
    ] {
        for r in [b.enable_feature(name).err(), b.disable_feature(name).err()] {
            match r {
                Some(Error::UnknownFeature {
                    name: n,
                    from_variable: false,
                }) => assert_eq!(n, name),
                other => panic!("{name:?}: {other:?}"),
            }
        }
    }
    // Nothing changed.
    assert!(test_feature_in(&b).unwrap());
    let dir = common::tempdir();
    let env = Environment::builder().open(dir.path()).unwrap();
    let err = env.feature_enabled("no-such-feature").unwrap_err();
    assert_eq!(err.to_err_code(), RLMDB_UNKNOWN_FEATURE);
    assert_eq!(err.to_string(), "unknown feature \"no-such-feature\"");
    assert_eq!(
        rlmdb::strerror(RLMDB_FEATURE_IS_FIXED),
        "RLMDB_FEATURE_IS_FIXED: Feature cannot be disabled"
    );
}

#[test]
fn the_variable_is_read_only_on_request() {
    let v = Vars::take();
    v.set(FEATURES_ENV, TEST);
    assert!(!test_feature_in(&Environment::builder()).unwrap());
    assert!(test_feature_in(&reading()).unwrap());
    let mut off = reading();
    off.read_environment(false);
    assert!(!test_feature_in(&off).unwrap());
    // Read at every open.
    v.set(FEATURES_ENV, &format!("{TEST}, -{TEST}"));
    assert!(!test_feature_in(&reading()).unwrap());
    v.set(FEATURES_ENV, &format!("-{TEST}\t{TEST}"));
    assert!(test_feature_in(&reading()).unwrap());
    v.set(FEATURES_ENV, "");
    assert!(!test_feature_in(&reading()).unwrap());
    // Without the variable, nothing but the defaults.
    std::env::remove_var(FEATURES_ENV);
    assert!(!test_feature_in(&reading()).unwrap());
}

#[test]
fn the_program_wins_over_the_variable() {
    let v = Vars::take();
    v.set(FEATURES_ENV, TEST);
    let mut b = reading();
    b.disable(Feature::TestFeature).unwrap();
    assert!(!test_feature_in(&b).unwrap());
    v.set(FEATURES_ENV, &format!("-{TEST}"));
    let mut b = reading();
    b.enable(Feature::TestFeature);
    assert!(test_feature_in(&b).unwrap());
}

#[test]
fn a_bad_variable_fails_the_open_before_any_file_is_made() {
    let v = Vars::take();
    v.set(FEATURES_ENV, &format!("{TEST},tset-feature"));
    let dir = common::tempdir();
    // Also with a request for the feature: the typo is still a typo.
    let mut b = reading();
    b.enable(Feature::TestFeature);
    let err = b.open(dir.path()).unwrap_err();
    match &err {
        Error::UnknownFeature {
            name,
            from_variable: true,
        } => assert_eq!(name, "tset-feature"),
        other => panic!("{other:?}"),
    }
    assert_eq!(err.to_err_code(), RLMDB_UNKNOWN_FEATURE);
    assert_eq!(
        err.to_string(),
        "unknown feature \"tset-feature\" in RLMDB_FEATURES"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    // A builder that does not read it is not affected.
    assert!(!test_feature_in(&Environment::builder()).unwrap());
    // Not UTF-8: an unknown name too.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::env::set_var(
            FEATURES_ENV,
            std::ffi::OsStr::from_bytes(b"test-feature\xff"),
        );
        let err = test_feature_in(&reading()).unwrap_err();
        assert_eq!(err.to_err_code(), RLMDB_UNKNOWN_FEATURE, "{err}");
    }
}

#[test]
fn rlmdb_ignore_env_blocks_the_variable() {
    let v = Vars::take();
    v.set(FEATURES_ENV, TEST);
    v.set(IGNORE_ENV, "1");
    assert!(!test_feature_in(&reading()).unwrap());
    // A bad variable is not even looked at.
    v.set(FEATURES_ENV, "no-such-feature");
    assert!(!test_feature_in(&reading()).unwrap());
    // The program's requests still count.
    let mut b = reading();
    b.enable(Feature::TestFeature);
    assert!(test_feature_in(&b).unwrap());
    // Empty means unset.
    v.set(FEATURES_ENV, TEST);
    v.set(IGNORE_ENV, "");
    assert!(test_feature_in(&reading()).unwrap());
}
