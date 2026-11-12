//! [`TestEnv`]: an LMDB environment path inside a temporary directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};

use tempfile::TempDir;

use crate::{Format, Result};

/// An environment location in a fresh temporary directory, removed when the
/// last `TestEnv` sharing the directory is dropped.
///
/// By default the environment is a directory (`<path>/data.mdb`,
/// `<path>/lock.mdb`); with [`new_nosubdir`](Self::new_nosubdir) it is a file
/// (`<path>`, `<path>-lock`) and every peer command and tool gets the
/// "no subdir" option.
///
/// It also has a data-file format, LMDB 0.9 unless
/// [`with_format`](Self::with_format) says otherwise: peers create and open
/// a 1.0 environment with `--format 1.0`, and [`crate::tools`] runs the C
/// tools of the environment's release (`mdb_stat10` … for 1.0).
#[derive(Clone, Debug)]
pub struct TestEnv {
    root: Arc<TempDir>,
    path: PathBuf,
    nosubdir: bool,
    format: Format,
    page_size: Option<u32>,
    /// Released after `root` is removed: fields drop in declaration order,
    /// and every clone holds both.
    permit: Option<Arc<LargePermit>>,
}

/// The right to have one large environment in this process: one whose
/// data file may grow to a large map, such as a scenario's writer loop under
/// a held snapshot, whose freed pages cannot be reused (at 64 KiB pages
/// close to 1 GiB). Large environments wait for each other; the others run
/// in parallel as before. This keeps a test run within a small machine's
/// memory (the scratch directory may be a tmpfs, as in
/// `scripts/linux-test.sh`) without changing what any test does.
///
/// Released when dropped. A test that holds one must not wait for a second
/// (it would wait for itself).
#[derive(Debug)]
pub struct LargePermit(());

/// Whether a large environment exists in this process, and its waiters.
static LARGE_TAKEN: Mutex<bool> = Mutex::new(false);
static LARGE_FREED: Condvar = Condvar::new();

impl LargePermit {
    /// Waits until no other large environment exists in this process.
    pub fn acquire() -> LargePermit {
        // A panicking test does not leave the flag set (the permit is
        // dropped while unwinding), so a poisoned lock holds a valid flag.
        let mut taken = LARGE_TAKEN.lock().unwrap_or_else(PoisonError::into_inner);
        while *taken {
            taken = LARGE_FREED
                .wait(taken)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *taken = true;
        LargePermit(())
    }
}

impl Drop for LargePermit {
    fn drop(&mut self) {
        *LARGE_TAKEN.lock().unwrap_or_else(PoisonError::into_inner) = false;
        LARGE_FREED.notify_one();
    }
}

/// The variable that names a directory for the environments of tests
/// that need no disk: CI points it at a file system in memory (tmpfs, a
/// RAM disk), where synced commits do not wait for a disk. Unset, they go
/// to the system's temporary directory. The targets of copies never go
/// there ([`TestEnv::derived_on_disk`]).
pub const SCRATCH_VAR: &str = "RLMDB_TEST_SCRATCH";

/// `target/tmp` (`CARGO_TARGET_TMPDIR`), if a test told [`register_target_tmpdir`].
static TARGET_TMPDIR: OnceLock<PathBuf> = OnceLock::new();

/// Tells the library where `target/tmp` is (`CARGO_TARGET_TMPDIR`, which
/// only integration tests see): the directory of
/// [`TestEnv::derived_on_disk`]. [`scenario_tests!`](crate::scenario_tests)
/// calls it, tests of their own with copies too; without it, those
/// environments go to the system's temporary directory.
pub fn register_target_tmpdir(dir: Option<&str>) {
    if let Some(dir) = dir {
        let _ = TARGET_TMPDIR.set(PathBuf::from(dir));
    }
}

/// A new temporary directory `interop-*` in `dir` (`None`: the system's
/// temporary directory); kept after the test if `$INTEROP_KEEP` is set (to
/// look at the files of a failed scenario: the paths are in the failure
/// message).
fn temp_root_in(dir: Option<PathBuf>) -> Result<Arc<TempDir>> {
    let keep = std::env::var_os("INTEROP_KEEP").is_some_and(|v| !v.is_empty() && v != "0");
    let mut b = tempfile::Builder::new();
    b.prefix("interop-").disable_cleanup(keep);
    Ok(Arc::new(match dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir)?;
            b.tempdir_in(dir)?
        }
        None => b.tempdir()?,
    }))
}

/// [`temp_root_in`] `$RLMDB_TEST_SCRATCH`, if set.
pub(crate) fn temp_root() -> Result<Arc<TempDir>> {
    temp_root_in(
        std::env::var_os(SCRATCH_VAR)
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    )
}

/// [`temp_root_in`] `target/tmp`, if registered ([`register_target_tmpdir`]),
/// on the disk of the build, whatever `$RLMDB_TEST_SCRATCH` says.
fn disk_temp_root() -> Result<Arc<TempDir>> {
    temp_root_in(TARGET_TMPDIR.get().cloned())
}

impl TestEnv {
    /// An environment directory `env/` in a new temporary directory.
    pub fn new() -> Result<Self> {
        Self::with_name("env")
    }

    /// An environment directory `<name>/` in a new temporary directory
    /// (the `mtest` programs insist on `./testdb`).
    pub fn with_name(name: &str) -> Result<Self> {
        let root = temp_root()?;
        let path = root.path().join(name);
        std::fs::create_dir(&path)?;
        Ok(TestEnv {
            root,
            path,
            nosubdir: false,
            format: Format::V09,
            page_size: None,
            permit: None,
        })
    }

    /// A `MDB_NOSUBDIR` environment: data file `env.mdb`, lock file
    /// `env.mdb-lock`, in a new temporary directory.
    pub fn new_nosubdir() -> Result<Self> {
        let root = temp_root()?;
        let path = root.path().join("env.mdb");
        Ok(TestEnv {
            root,
            path,
            nosubdir: true,
            format: Format::V09,
            page_size: None,
            permit: None,
        })
    }

    /// This environment location in data-file format `format` (the
    /// environment is not created yet: the first peer to open it creates
    /// it in that format).
    pub fn with_format(mut self, format: Format) -> TestEnv {
        self.format = format;
        self
    }

    /// This environment location, holding `permit` until the temporary
    /// directory is removed (with the last `TestEnv` sharing it).
    pub fn with_permit(mut self, permit: LargePermit) -> TestEnv {
        self.permit = Some(Arc::new(permit));
        self
    }

    /// This environment location with pages of `page_size` bytes if a peer
    /// creates it (`--pagesize`; LMDB 1.0 only: `None` is the OS page size).
    pub fn with_page_size(mut self, page_size: Option<u32>) -> TestEnv {
        self.page_size = page_size;
        self
    }

    /// Another environment location `<name>` in the same temporary directory
    /// and with the same subdir mode and format, not created: a target for
    /// loads. (`cpeer copy` creates the directory, [`crate::tools`] too.)
    /// Copies go to [`derived_on_disk`](Self::derived_on_disk).
    pub fn derived(&self, name: &str) -> TestEnv {
        TestEnv {
            root: Arc::clone(&self.root),
            path: self.root.path().join(name),
            nosubdir: self.nosubdir,
            format: self.format,
            page_size: self.page_size,
            permit: self.permit.clone(),
        }
    }

    /// Another environment location `<name>` with the same subdir mode,
    /// format and page size, not created, in a new temporary directory under
    /// `target/tmp` ([`register_target_tmpdir`]), on the disk whatever
    /// `$RLMDB_TEST_SCRATCH` says: the target of a copy. The peers, C's and
    /// rlmdb's `mdb_copy` open it with `O_DIRECT` on Linux, which a disk
    /// file system honours (writes must be aligned to its logical block
    /// size; C 1.0 misses that below 4 KiB pages, `KNOWN_C_BUGS.md` row 34,
    /// which [`copy_or_skip`](crate::scenarios::copy_or_skip) expects)
    /// and tmpfs ignores (it takes any write).
    pub fn derived_on_disk(&self, name: &str) -> Result<TestEnv> {
        let root = disk_temp_root()?;
        let path = root.path().join(name);
        Ok(TestEnv {
            root,
            path,
            nosubdir: self.nosubdir,
            format: self.format,
            page_size: self.page_size,
            permit: None,
        })
    }

    /// The data-file format of this environment.
    pub fn format(&self) -> Format {
        self.format
    }

    /// The page size a peer creates this environment with (`None`: the OS
    /// page size).
    pub fn page_size(&self) -> Option<u32> {
        self.page_size
    }

    /// The environment path given to `mdb_env_open`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The temporary directory holding the environment.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Is this a `MDB_NOSUBDIR` environment?
    pub fn is_nosubdir(&self) -> bool {
        self.nosubdir
    }

    /// The data file (`data.mdb`, or the path itself with no subdir).
    pub fn data_file(&self) -> PathBuf {
        if self.nosubdir {
            self.path.clone()
        } else {
            self.path.join("data.mdb")
        }
    }

    /// The lock file (`lock.mdb`, or `<path>-lock` with no subdir).
    pub fn lock_file(&self) -> PathBuf {
        if self.nosubdir {
            let mut s = self.path.clone().into_os_string();
            s.push("-lock");
            s.into()
        } else {
            self.path.join("lock.mdb")
        }
    }

    /// Options every peer command on this environment gets: `--nosubdir`,
    /// `--format 1.0` for a 1.0 environment (none for 0.9, the default of
    /// every peer that can open it) and `--pagesize N` if set (an existing
    /// environment keeps its page size, so every command can get it).
    pub fn peer_options(&self) -> Vec<OsString> {
        let mut v = Vec::new();
        if self.nosubdir {
            v.push("--nosubdir".into());
        }
        if self.format == Format::V10 {
            v.extend(["--format".into(), "1.0".into()]);
        }
        if let Some(ps) = self.page_size {
            v.extend(["--pagesize".into(), ps.to_string().into()]);
        }
        v
    }

    /// Options every C tool (`mdb_stat`, `mdb_dump`, …) gets.
    pub fn tool_options(&self) -> Vec<OsString> {
        if self.nosubdir {
            vec!["-n".into()]
        } else {
            Vec::new()
        }
    }
}
