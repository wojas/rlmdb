//! [`TestEnv`]: an LMDB environment path inside a temporary directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
            format: Format::V09,
            page_size: None,
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
        })
    }

    /// This environment location in data-file format `format` (the
    /// environment is not created yet: the first peer to open it creates
    /// it in that format).
    pub fn with_format(mut self, format: Format) -> TestEnv {
        self.format = format;
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
    /// copies and loads. (`cpeer copy` creates the directory, [`crate::tools`]
    /// too.)
    pub fn derived(&self, name: &str) -> TestEnv {
        TestEnv {
            root: Arc::clone(&self.root),
            path: self.root.path().join(name),
            nosubdir: self.nosubdir,
            format: self.format,
            page_size: self.page_size,
        }
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
