//! [`TestEnv`]: an LMDB environment path inside a temporary directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;

use crate::Result;

/// An environment location in a fresh temporary directory, removed when the
/// last `TestEnv` sharing the directory is dropped.
///
/// By default the environment is a directory (`<path>/data.mdb`,
/// `<path>/lock.mdb`); with [`new_nosubdir`](Self::new_nosubdir) it is a file
/// (`<path>`, `<path>-lock`) and every peer command and tool gets the
/// "no subdir" option.
#[derive(Clone, Debug)]
pub struct TestEnv {
    root: Arc<TempDir>,
    path: PathBuf,
    nosubdir: bool,
}

/// A new temporary directory `interop-*`; kept after the test if
/// `$INTEROP_KEEP` is set (to look at the files of a failed scenario: the
/// paths are in the failure message).
fn temp_root() -> Result<Arc<TempDir>> {
    let keep = std::env::var_os("INTEROP_KEEP").is_some_and(|v| !v.is_empty() && v != "0");
    Ok(Arc::new(
        tempfile::Builder::new()
            .prefix("interop-")
            .disable_cleanup(keep)
            .tempdir()?,
    ))
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
        })
    }

    /// Another environment location `<name>` in the same temporary directory
    /// and with the same subdir mode, not created: a target for copies and
    /// loads. (`cpeer copy` creates the directory, [`crate::tools`] too.)
    pub fn derived(&self, name: &str) -> TestEnv {
        TestEnv {
            root: Arc::clone(&self.root),
            path: self.root.path().join(name),
            nosubdir: self.nosubdir,
        }
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

    /// Options every peer command on this environment gets.
    pub fn peer_options(&self) -> Vec<OsString> {
        if self.nosubdir {
            vec!["--nosubdir".into()]
        } else {
            Vec::new()
        }
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
