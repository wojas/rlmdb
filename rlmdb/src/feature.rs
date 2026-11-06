//! Opt-in features, enabled by name: behaviour a caller chooses that C
//! LMDB does not have. Not in `mdb.c`; the C API's form is in `rlmdb.h`
//! (`rlmdb_features`, `rlmdb_env_enable_feature`, …).
//!
//! A feature keeps the environment compatible with C LMDB: a C peer still
//! opens, reads and writes it correctly. It may change runtime behaviour,
//! and what it writes may then differ from what C writes byte for byte (a
//! free list laid out otherwise, which C reads correctly). A feature is a
//! setting of one environment handle and is never recorded in the files:
//! it must be correct while other processes on the same environment,
//! rlmdb ones included, have it off.
//!
//! # Names and states
//!
//! Features are named by kebab-case strings ([`Feature::name`]) that are
//! never renamed, so that a program written for a later rlmdb still
//! compiles and runs against an earlier one: it gets
//! [`Error::UnknownFeature`] for a name the build does not know. Each
//! feature has a default ([`FeatureDefault`]) that is also its lifecycle:
//! [`Off`](FeatureDefault::Off) (off unless enabled),
//! [`On`](FeatureDefault::On) (on unless disabled), and
//! [`Fixed`](FeatureDefault::Fixed) (on, cannot be disabled). A feature
//! that graduates to "always on" stays listed as fixed, so a program that
//! asks for it keeps getting a true answer. [`features`] lists the
//! features of this build.
//!
//! # Where the state comes from
//!
//! At the open of an environment ([`EnvironmentBuilder::open`]) each
//! feature starts at its default; then, if the builder reads the
//! environment variables ([`EnvironmentBuilder::read_environment`], off
//! by default in Rust, on in the C API), the [`FEATURES_ENV`] variable
//! (`RLMDB_FEATURES`) applies; then what the program asked for
//! ([`EnvironmentBuilder::enable`], [`EnvironmentBuilder::enable_feature`]
//! and the `disable` forms), which wins over the variable. The variable
//! holds names separated by commas or whitespace (any mix); a leading `-`
//! disables a feature; for a name given more than once the last entry
//! wins. An unknown name, or `-` on a fixed feature, fails the open
//! ([`Error::UnknownFeature`], [`Error::FeatureIsFixed`], with the name),
//! so that a typo in a deployment is noticed.
//!
//! [`EnvironmentBuilder::open`]: crate::EnvironmentBuilder::open
//! [`EnvironmentBuilder::read_environment`]: crate::EnvironmentBuilder::read_environment
//! [`EnvironmentBuilder::enable`]: crate::EnvironmentBuilder::enable
//! [`EnvironmentBuilder::enable_feature`]: crate::EnvironmentBuilder::enable_feature

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// The environment variable with the features to enable or disable at the
/// open of an environment that reads the environment variables
/// ([`EnvironmentBuilder::read_environment`](crate::EnvironmentBuilder::read_environment)):
/// `RLMDB_FEATURES=a,b -c` (see the module documentation).
pub const FEATURES_ENV: &str = "RLMDB_FEATURES";

/// An opt-in feature of this build (see the module documentation).
/// Programs that take names from configuration use the strings
/// ([`Feature::from_name`], [`FromStr`]) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Feature {
    /// `split-freelist` (off by default): a write transaction that frees
    /// many pages saves them in several free-list records of at most one
    /// page each, instead of one big record.
    ///
    /// C LMDB saves the pages a transaction freed as one record, 8 bytes
    /// per page, which needs a contiguous run of overflow pages: deleting
    /// 4 GB of data with 4 KiB pages frees about a million pages, whose
    /// record of 8 MB needs about 2,000 pages in a row. The free list
    /// rarely has such a run, so the file grows for it, and a transaction
    /// that only deletes data can fail with `MDB_MAP_FULL`. With this
    /// feature that record is split.
    ///
    /// It is safe to turn on unless the program counts transaction IDs or
    /// rolls back commits.
    ///
    /// What changes for the program:
    ///
    /// * A transaction that splits its record commits under a higher
    ///   transaction ID than [`Transaction::id`](crate::Transaction::id)
    ///   reported while it was open. Each record takes one ID, and the
    ///   commit writes its snapshot into both meta pages under two
    ///   consecutive IDs. The ID it publishes is
    ///   [`PreparedTxn::id`](crate::PreparedTxn::id), after
    ///   [`RwTxn::prepare`](crate::RwTxn::prepare).
    /// * Such a commit cannot be undone with
    ///   [`Environment::rollback`](crate::Environment::rollback), which
    ///   refuses it. A program that needs the undo aborts the prepared
    ///   transaction when its ID rose.
    /// * Both meta page writes of such a commit are synchronous, also with
    ///   [`NO_META_SYNC`](crate::EnvironmentFlags::NO_META_SYNC) (not with
    ///   [`NO_SYNC`](crate::EnvironmentFlags::NO_SYNC)).
    ///
    /// The files keep their format: C LMDB and rlmdb without the feature
    /// read and write them. `docs/split-freelist.md` in the repository
    /// explains the feature, `docs/split-freelist-details.md` why it is
    /// safe.
    SplitFreelist,
}

/// The default state of a feature in a build, which is also where it is in
/// its life (see the module documentation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureDefault {
    /// Known, off unless enabled (`RLMDB_FEATURE_OFF`).
    Off,
    /// Known, on unless disabled (`RLMDB_FEATURE_ON`).
    On,
    /// Known and always on: disabling it is an error
    /// (`RLMDB_FEATURE_FIXED`).
    Fixed,
}

impl FeatureDefault {
    /// The marker the version string puts after the name of a feature with
    /// this default: none for off, `*` for on, `!` for fixed.
    pub fn marker(self) -> &'static str {
        match self {
            FeatureDefault::Off => "",
            FeatureDefault::On => "*",
            FeatureDefault::Fixed => "!",
        }
    }
}

/// A feature of this build, as [`features`] lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FeatureInfo {
    /// The feature.
    pub feature: Feature,
    /// Its name, as programs and `RLMDB_FEATURES` give it.
    pub name: &'static str,
    /// Its default state.
    pub default: FeatureDefault,
}

/// The features of this build, in the order of [`Feature`] (each at the
/// index of its discriminant).
static FEATURES: [FeatureInfo; 1] = [FeatureInfo {
    feature: Feature::SplitFreelist,
    name: "split-freelist",
    default: FeatureDefault::Off,
}];

// The states are bit sets over the features.
const _: () = assert!(FEATURES.len() <= 64);

/// The features this build knows, with their defaults.
pub fn features() -> &'static [FeatureInfo] {
    &FEATURES
}

impl Feature {
    /// What [`features`] says about this feature.
    pub fn info(self) -> &'static FeatureInfo {
        &FEATURES[self as usize]
    }

    /// The name of the feature, as programs and `RLMDB_FEATURES` give it.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// The default state of the feature in this build.
    pub fn default_state(self) -> FeatureDefault {
        self.info().default
    }

    /// The feature named `name`, `None` if this build knows none of that
    /// name.
    pub fn from_name(name: &str) -> Option<Feature> {
        FEATURES.iter().find(|f| f.name == name).map(|f| f.feature)
    }

    /// The feature named `name`, or [`Error::UnknownFeature`].
    pub(crate) fn lookup(name: &str) -> Result<Feature> {
        Feature::from_name(name).ok_or_else(|| Error::UnknownFeature {
            name: name.to_owned(),
            from_variable: false,
        })
    }

    /// The bit of the feature in a set.
    fn bit(self) -> u64 {
        1 << self as usize
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Feature {
    type Err = Error;

    /// [`Feature::from_name`], [`Error::UnknownFeature`] for a name this
    /// build does not know.
    fn from_str(name: &str) -> Result<Feature> {
        Feature::lookup(name)
    }
}

/// What a program asked for, feature by feature: enabled, disabled, or
/// nothing (the default, or `RLMDB_FEATURES`, decides). The builder keeps
/// one ([`EnvironmentBuilder::feature_requests`]); the C API keeps one in
/// each unopened `MDB_env`.
///
/// [`EnvironmentBuilder::feature_requests`]: crate::EnvironmentBuilder::feature_requests
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeatureRequests {
    enabled: u64,
    disabled: u64,
}

impl FeatureRequests {
    /// No requests.
    pub fn new() -> FeatureRequests {
        FeatureRequests::default()
    }

    /// Asks for `feature` on, replacing an earlier request to disable it.
    /// A fixed feature is on anyway.
    pub fn enable(&mut self, feature: Feature) {
        self.enabled |= feature.bit();
        self.disabled &= !feature.bit();
    }

    /// Asks for `feature` off, replacing an earlier request to enable it;
    /// [`Error::FeatureIsFixed`] (and nothing changes) for a fixed feature.
    pub fn disable(&mut self, feature: Feature) -> Result<()> {
        if feature.default_state() == FeatureDefault::Fixed {
            return Err(Error::FeatureIsFixed {
                name: feature.name().to_owned(),
                from_variable: false,
            });
        }
        self.disabled |= feature.bit();
        self.enabled &= !feature.bit();
        Ok(())
    }

    /// What was asked for `feature`: `Some(true)` enabled, `Some(false)`
    /// disabled, `None` nothing.
    pub fn requested(&self, feature: Feature) -> Option<bool> {
        if self.enabled & feature.bit() != 0 {
            Some(true)
        } else if self.disabled & feature.bit() != 0 {
            Some(false)
        } else {
            None
        }
    }

    /// Whether `feature` is on with these requests and its default, before
    /// `RLMDB_FEATURES` applies (which it may at the open).
    pub fn is_enabled(&self, feature: Feature) -> bool {
        self.requested(feature)
            .unwrap_or(feature.default_state() != FeatureDefault::Off)
    }
}

/// The features on in an open environment, as a bit set over [`Feature`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FeatureSet(u64);

impl FeatureSet {
    /// Whether `feature` is in the set.
    pub(crate) fn contains(self, feature: Feature) -> bool {
        self.0 & feature.bit() != 0
    }
}

/// The features on at an open with `requests` from the program and the
/// value of `RLMDB_FEATURES` if it is read and set (see the module
/// documentation).
pub(crate) fn resolve(requests: FeatureRequests, variable: Option<&str>) -> Result<FeatureSet> {
    let table: Vec<(&str, FeatureDefault)> = FEATURES.iter().map(|f| (f.name, f.default)).collect();
    resolve_in(&table, requests, variable).map(FeatureSet)
}

/// [`resolve`] over the features of `table` (name and default, each at the
/// index of its bit), so that the unit tests can use features this build
/// does not have.
fn resolve_in(
    table: &[(&str, FeatureDefault)],
    requests: FeatureRequests,
    variable: Option<&str>,
) -> Result<u64> {
    let bits_with = |pick: fn(FeatureDefault) -> bool| {
        (table.iter().enumerate())
            .filter(|(_, (_, d))| pick(*d))
            .fold(0u64, |set, (i, _)| set | 1 << i)
    };
    let fixed = bits_with(|d| d == FeatureDefault::Fixed);
    let mut on = bits_with(|d| d != FeatureDefault::Off);
    for (name, enable) in variable.into_iter().flat_map(entries) {
        let Some(i) = table.iter().position(|(n, _)| *n == name) else {
            return Err(Error::UnknownFeature {
                name: name.to_owned(),
                from_variable: true,
            });
        };
        let bit = 1u64 << i;
        if enable {
            on |= bit;
        } else if fixed & bit != 0 {
            return Err(Error::FeatureIsFixed {
                name: name.to_owned(),
                from_variable: true,
            });
        } else {
            on &= !bit;
        }
    }
    Ok((on | requests.enabled) & !requests.disabled | fixed)
}

/// The entries of an `RLMDB_FEATURES` value: names separated by commas or
/// whitespace, each with `true` to enable it, `false` for `-name`.
fn entries(value: &str) -> impl Iterator<Item = (&str, bool)> {
    value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|entry| !entry.is_empty())
        .map(|entry| match entry.strip_prefix('-') {
            Some(name) => (name, false),
            None => (entry, true),
        })
}

/// The features of this build as the version string lists them: each
/// name with its default's marker ([`FeatureDefault::marker`]), separated
/// by spaces (`a b* c!`); empty without features.
pub(crate) fn version_list() -> String {
    let names: Vec<String> = FEATURES
        .iter()
        .map(|f| format!("{}{}", f.name, f.default.marker()))
        .collect();
    names.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table with one feature in each state: `off`, `on`, `fixed`.
    const TABLE: [(&str, FeatureDefault); 3] = [
        ("off", FeatureDefault::Off),
        ("on", FeatureDefault::On),
        ("fixed", FeatureDefault::Fixed),
    ];
    const OFF: u64 = 1;
    const ON: u64 = 2;
    const FIXED: u64 = 4;

    /// Requests of the program over [`TABLE`]'s bits.
    fn asked(enabled: u64, disabled: u64) -> FeatureRequests {
        FeatureRequests { enabled, disabled }
    }

    fn var(value: &str) -> Result<u64> {
        resolve_in(&TABLE, asked(0, 0), Some(value))
    }

    /// The name and origin of an error about a feature.
    fn refused(r: Result<u64>) -> (&'static str, String, bool) {
        match r {
            Err(Error::UnknownFeature {
                name,
                from_variable,
            }) => ("unknown", name, from_variable),
            Err(Error::FeatureIsFixed {
                name,
                from_variable,
            }) => ("fixed", name, from_variable),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_without_requests() {
        assert_eq!(resolve_in(&TABLE, asked(0, 0), None).unwrap(), ON | FIXED);
        assert_eq!(var("").unwrap(), ON | FIXED);
        assert_eq!(var(" ,, \t\n").unwrap(), ON | FIXED);
    }

    #[test]
    fn the_variable_enables_and_disables() {
        assert_eq!(var("off").unwrap(), OFF | ON | FIXED);
        assert_eq!(var("-on").unwrap(), FIXED);
        assert_eq!(var("off,-on").unwrap(), OFF | FIXED);
        // Commas and whitespace in any mix.
        assert_eq!(var(" off ,\t-on,").unwrap(), OFF | FIXED);
        assert_eq!(var("off -on").unwrap(), OFF | FIXED);
        // Enabling a fixed feature, or one that is on, changes nothing.
        assert_eq!(var("fixed,on").unwrap(), ON | FIXED);
        // The last entry for a name wins.
        assert_eq!(var("off,-off").unwrap(), ON | FIXED);
        assert_eq!(var("-off,off").unwrap(), OFF | ON | FIXED);
        assert_eq!(var("-on,on").unwrap(), ON | FIXED);
    }

    #[test]
    fn the_variable_refuses_unknown_names_and_disabling_fixed_ones() {
        let unknown = |s: &str| ("unknown", s.to_owned(), true);
        assert_eq!(refused(var("of")), unknown("of"));
        assert_eq!(refused(var("off,typo,-on")), unknown("typo"));
        // Names are case-sensitive, and only `-` is a prefix.
        assert_eq!(refused(var("OFF")), unknown("OFF"));
        assert_eq!(refused(var("+off")), unknown("+off"));
        assert_eq!(refused(var("--on")), unknown("-on"));
        assert_eq!(refused(var("-")), unknown(""));
        assert_eq!(refused(var("-typo")), unknown("typo"));
        assert_eq!(
            refused(var("off -fixed")),
            ("fixed", "fixed".to_owned(), true)
        );
        // Also when a later entry would enable it again.
        assert_eq!(
            refused(var("-fixed,fixed")),
            ("fixed", "fixed".to_owned(), true)
        );
    }

    #[test]
    fn the_program_wins_over_the_variable() {
        let with = |enabled, disabled, v: Option<&str>| {
            resolve_in(&TABLE, asked(enabled, disabled), v).unwrap()
        };
        assert_eq!(with(OFF, 0, None), OFF | ON | FIXED);
        assert_eq!(with(0, ON, None), FIXED);
        assert_eq!(with(OFF, 0, Some("-off")), OFF | ON | FIXED);
        assert_eq!(with(0, OFF, Some("off")), ON | FIXED);
        assert_eq!(with(ON, 0, Some("-on")), ON | FIXED);
        assert_eq!(with(0, ON, Some("on")), FIXED);
        // What the program left alone, the variable decides.
        assert_eq!(with(0, OFF, Some("-on")), FIXED);
        // A request does not excuse a bad variable.
        assert_eq!(
            refused(resolve_in(&TABLE, asked(OFF, 0), Some("off,x"))),
            ("unknown", "x".to_owned(), true)
        );
    }

    #[test]
    fn requests() {
        let f = Feature::SplitFreelist;
        let mut r = FeatureRequests::new();
        assert_eq!(r.requested(f), None);
        assert!(!r.is_enabled(f));
        r.enable(f);
        assert_eq!(r.requested(f), Some(true));
        assert!(r.is_enabled(f));
        r.disable(f).unwrap();
        assert_eq!(r.requested(f), Some(false));
        assert!(!r.is_enabled(f));
        r.enable(f);
        assert_eq!(r, {
            let mut e = FeatureRequests::new();
            e.enable(f);
            e
        });
    }

    #[test]
    fn this_builds_features() {
        for (i, info) in features().iter().enumerate() {
            assert_eq!(info.feature as usize, i);
            assert_eq!(info.feature.info(), info);
            assert_eq!(Feature::from_name(info.name), Some(info.feature));
            assert_eq!(info.name.parse::<Feature>().unwrap(), info.feature);
            assert_eq!(info.feature.to_string(), info.name);
            // Kebab-case: lower-case words separated by single hyphens.
            assert!(info.name.split('-').all(|w| !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())));
        }
        assert_eq!(Feature::from_name("no-such-feature"), None);
        assert!(matches!(
            "no-such-feature".parse::<Feature>(),
            Err(Error::UnknownFeature {
                from_variable: false,
                ..
            })
        ));
        assert_eq!(Feature::SplitFreelist.default_state(), FeatureDefault::Off);
        assert_eq!(version_list(), "split-freelist");
        let set = resolve(FeatureRequests::new(), Some("split-freelist")).unwrap();
        assert!(set.contains(Feature::SplitFreelist));
        let set = resolve(FeatureRequests::new(), None).unwrap();
        assert!(!set.contains(Feature::SplitFreelist));
    }
}
