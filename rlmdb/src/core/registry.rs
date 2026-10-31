//! The process-wide registry of open environments.
//!
//! C lets a process open one environment any number of times, but `lmdb.h`
//! forbids it with a lock file: POSIX record locks belong to the process,
//! so closing any descriptor of the lock file drops every lock the process
//! holds on it, the other handle's too. Without a lock file (`MDB_NOLOCK`)
//! there are no record locks to lose, and programs do open one environment
//! several times (for example once per embedded interpreter, each through
//! its own copy of a binding). The registry enforces the first and makes
//! the second safe:
//!
//! * With a lock file, an environment is open at most once per process; a
//!   second open fails with `EBUSY` ([`Error::EnvAlreadyOpened`]) before it
//!   touches the lock file.
//! * Without one (`NO_LOCK`), any number of handles may open a data file.
//!   They share one [`LocalShared`]: the writer lock that serialises their
//!   write transactions and the reader table in which their read
//!   transactions pin snapshots, so a writer of any handle keeps the pages
//!   of readers of every handle (C coordinates nothing there,
//!   `KNOWN_C_BUGS.md` #35). It exists while one of them is open.
//! * A handle with a lock file and a `NO_LOCK` handle never share a file
//!   (their readers would be invisible to each other's writers): whichever
//!   comes second gets `EBUSY`.
//!
//! There are two keys. The canonical path is claimed first, before the lock
//! file is opened, as the guard for the record locks. The identity of the
//! open data file (device and inode) is claimed once it is open: it
//! recognises one file under other names (symlinks, the data file named
//! with `NO_SUB_DIR` next to its directory, hard links) and keys the shared
//! state. Both maps are empty, holding no memory, when no environment is
//! open, so a library that is unloaded then leaves nothing behind.
//!
//! The registry is per copy of the library: handles opened through another
//! copy linked into the same process (a static library in several modules,
//! `dlmopen` namespaces) do not see it, as C handles do not see each other.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::core::reader::LocalReaders;
use crate::error::{Error, Result};

/// What the handles of one data file without a lock file share in this
/// process: the in-process stand-ins for the lock file's writer mutex and
/// reader table.
#[derive(Debug, Default)]
pub(crate) struct LocalShared {
    /// Serialises the write transactions of every handle of the file (and
    /// the set-up of a new file, and the meta-page snapshot of a plain
    /// copy), where the lock file's writer mutex would.
    writer: Mutex<()>,
    /// The read transactions of every handle of the file.
    pub(crate) readers: LocalReaders,
}

impl LocalShared {
    /// Locks the writer lock, for as long as the guard lives.
    pub(crate) fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The identity of an open data file: device and inode (`st_dev`,
/// `st_ino`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId {
    dev: u64,
    ino: u64,
}

/// How a path or a file is open in this process.
#[derive(Debug)]
enum Use<S> {
    /// By one handle with a lock file (or none, but without `NO_LOCK`: a
    /// read-only environment on a read-only file system).
    Exclusive,
    /// By `handles` `NO_LOCK` handles, sharing `state`.
    Shared { handles: usize, state: S },
}

/// The open paths and files of the process.
struct Registry {
    paths: BTreeMap<PathBuf, Use<()>>,
    files: BTreeMap<FileId, Use<Arc<LocalShared>>>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    paths: BTreeMap::new(),
    files: BTreeMap::new(),
});

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Claims `key` in `map`: exclusively, or shared with other shared users
/// (`new_state` makes the state of the first). Returns the shared state.
fn claim<K: Ord, S: Clone>(
    map: &mut BTreeMap<K, Use<S>>,
    key: K,
    shared: bool,
    new_state: impl FnOnce() -> S,
) -> Result<Option<S>> {
    match map.get_mut(&key) {
        None if shared => {
            let state = new_state();
            map.insert(
                key,
                Use::Shared {
                    handles: 1,
                    state: state.clone(),
                },
            );
            Ok(Some(state))
        }
        None => {
            map.insert(key, Use::Exclusive);
            Ok(None)
        }
        Some(Use::Shared { handles, state }) if shared => {
            *handles += 1;
            Ok(Some(state.clone()))
        }
        Some(_) => Err(Error::EnvAlreadyOpened),
    }
}

/// Gives back one claim of `key`, forgetting it with the last one. An empty
/// map gives back its memory.
fn release<K: Ord, S>(map: &mut BTreeMap<K, Use<S>>, key: &K) {
    let last = match map.get_mut(key) {
        Some(Use::Shared { handles, .. }) if *handles > 1 => {
            *handles -= 1;
            false
        }
        Some(_) => true,
        None => false,
    };
    if last {
        map.remove(key);
    }
    if map.is_empty() {
        *map = BTreeMap::new();
    }
}

/// An environment's place in the registry; released on drop. An
/// [`Environment`](crate::core::Environment) keeps it as its last field, so
/// that its path and file are released only after every file of the
/// environment is closed.
#[derive(Debug)]
pub(crate) struct Registration {
    path: PathBuf,
    file: Option<FileId>,
    shared: bool,
}

impl Registration {
    /// The first step of an open, before the lock file or the data file is
    /// touched: claims the canonical form of `path` (see [`canonical_key`]),
    /// shared for a `NO_LOCK` open (`shared`). [`Error::EnvAlreadyOpened`]
    /// if the process has the path open with a lock file, or at all and
    /// this open uses a lock file.
    pub(crate) fn claim_path(path: &Path, no_sub_dir: bool, shared: bool) -> Result<Registration> {
        let key = canonical_key(path, no_sub_dir)?;
        claim(&mut registry().paths, key.clone(), shared, || ())?;
        Ok(Registration {
            path: key,
            file: None,
            shared,
        })
    }

    /// The second step, once the data file is open: claims the file, as
    /// [`Registration::claim_path`] claimed the path. Returns the state the
    /// file's `NO_LOCK` handles share, `None` for an exclusive claim.
    pub(crate) fn claim_file(&mut self, data_file: &File) -> Result<Option<Arc<LocalShared>>> {
        let meta = data_file.metadata()?;
        let id = FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        let state = claim(&mut registry().files, id, self.shared, Arc::default)?;
        self.file = Some(id);
        Ok(state)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut reg = registry();
        release(&mut reg.paths, &self.path);
        if let Some(id) = &self.file {
            release(&mut reg.files, id);
        }
    }
}

/// The canonical path identifying an environment: the directory, or with
/// `NO_SUB_DIR` the data file (which may not exist yet).
fn canonical_key(path: &Path, no_sub_dir: bool) -> Result<PathBuf> {
    if !no_sub_dir {
        return Ok(path.canonicalize()?);
    }
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(_) => {
            let file = path.file_name().ok_or_else(Error::einval)?;
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.canonicalize()?,
                _ => std::env::current_dir()?,
            };
            Ok(parent.join(file))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Dbi, Environment, Transaction};
    use crate::flags::{EnvironmentFlags, WriteFlags};

    /// The number of shared claims, 0 for an exclusive one.
    fn handles<S>(u: &Use<S>) -> usize {
        match u {
            Use::Exclusive => 0,
            Use::Shared { handles, .. } => *handles,
        }
    }

    /// How many handles have `dir`'s data file open, if any: (path claims,
    /// file claims; 0 for an exclusive claim).
    fn claims(dir: &Path) -> Option<(usize, usize)> {
        let reg = registry();
        let path = reg.paths.get(&dir.canonicalize().unwrap())?;
        let meta = std::fs::metadata(dir.join("data.mdb")).unwrap();
        let id = FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        };
        let file = reg.files.get(&id)?;
        Some((handles(path), handles(file)))
    }

    #[test]
    fn no_lock_handles_share_the_state_while_one_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let open = || {
            Environment::builder()
                .flags(EnvironmentFlags::NO_LOCK)
                .open(dir.path())
                .unwrap()
        };
        let a = open();
        let b = open();
        assert_eq!(claims(dir.path()), Some((2, 2)));
        let (Some(la), Some(lb)) = (a.local_shared(), b.local_shared()) else {
            panic!("NO_LOCK handles without the shared state");
        };
        assert!(std::ptr::eq(la, lb));
        let txn = a.begin_ro_txn().unwrap();
        assert_eq!(b.in_memory_readers(), Some(1));
        drop(txn);
        drop(a);
        assert_eq!(claims(dir.path()), Some((1, 1)));
        drop(b);
        assert_eq!(claims(dir.path()), None);
        // A later handle starts afresh.
        let c = open();
        assert_eq!(claims(dir.path()), Some((1, 1)));
        assert_eq!(c.in_memory_readers(), Some(0));
    }

    #[test]
    fn the_file_is_recognised_under_other_names() {
        let dir = tempfile::tempdir().unwrap();
        let env_dir = dir.path().join("env");
        std::fs::create_dir(&env_dir).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&env_dir, &link).unwrap();
        let no_lock = EnvironmentFlags::NO_LOCK;
        let no_sub_dir = EnvironmentFlags::NO_SUB_DIR;
        let a = Environment::builder()
            .flags(no_lock)
            .open(&env_dir)
            .unwrap();
        // Through a symlink (the same canonical path), as the data file
        // itself and through a hard link of it (other paths, the same file).
        let hard = dir.path().join("hard.mdb");
        std::fs::hard_link(env_dir.join("data.mdb"), &hard).unwrap();
        let b = Environment::builder().flags(no_lock).open(&link).unwrap();
        let c = Environment::builder()
            .flags(no_lock | no_sub_dir)
            .open(env_dir.join("data.mdb"))
            .unwrap();
        let d = Environment::builder()
            .flags(no_lock | no_sub_dir)
            .open(&hard)
            .unwrap();
        assert_eq!(claims(&env_dir), Some((2, 4)));
        assert!(std::ptr::eq(
            a.local_shared().unwrap(),
            d.local_shared().unwrap()
        ));
        let txn = d.begin_ro_txn().unwrap();
        assert_eq!(a.in_memory_readers(), Some(1));
        // A handle with a lock file is refused: before it creates a lock
        // file where the path is claimed, after it opened the data file
        // where only the file is (a hard link that no handle uses).
        let e = Environment::builder().open(&env_dir).unwrap_err();
        assert!(matches!(e, Error::EnvAlreadyOpened));
        assert!(!env_dir.join("lock.mdb").exists());
        let hard2 = dir.path().join("hard2.mdb");
        std::fs::hard_link(&hard, &hard2).unwrap();
        let e = Environment::builder()
            .flags(no_sub_dir)
            .open(&hard2)
            .unwrap_err();
        assert!(matches!(e, Error::EnvAlreadyOpened));
        drop(txn);
        drop((a, b, c, d));
        assert_eq!(claims(&env_dir), None);
        // And the other way round: a handle with a lock file keeps NO_LOCK
        // handles away under any name.
        let locked = Environment::builder().open(&env_dir).unwrap();
        for (path, flags) in [
            (&env_dir, no_lock),
            (&hard, no_lock | no_sub_dir),
            (&hard2, no_lock | no_sub_dir),
        ] {
            let e = Environment::builder().flags(flags).open(path).unwrap_err();
            assert!(matches!(e, Error::EnvAlreadyOpened), "{path:?}");
        }
        drop(locked);
        let canon = dir.path().canonicalize().unwrap();
        assert!(!registry().paths.keys().any(|p| p.starts_with(&canon)));
    }

    /// Handles that open a new file at the same time set it up once: none
    /// finds a half-written header (the file grows while the meta pages are
    /// written), and none rewrites the meta pages after another committed.
    #[test]
    fn handles_open_a_new_file_at_once() {
        for _ in 0..20 {
            open_a_new_file_at_once(8);
        }
    }

    fn open_a_new_file_at_once(threads: u8) {
        let dir = tempfile::tempdir().unwrap();
        let start = std::sync::Barrier::new(threads as usize);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (dir, start) = (dir.path(), &start);
                s.spawn(move || {
                    start.wait();
                    let env = Environment::builder()
                        .flags(EnvironmentFlags::NO_LOCK | EnvironmentFlags::NO_SYNC)
                        .open(dir)
                        .unwrap();
                    let mut txn = env.begin_rw_txn().unwrap();
                    txn.put(Dbi::MAIN, &[t], &[t], WriteFlags::empty()).unwrap();
                    txn.commit().unwrap();
                });
            }
        });
        let env = Environment::builder()
            .flags(EnvironmentFlags::NO_LOCK)
            .open(dir.path())
            .unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), u64::from(threads));
        for t in 0..threads {
            assert_eq!(txn.get(Dbi::MAIN, &[t]).unwrap(), Some(&[t][..]));
        }
        drop(txn);
        crate::core::audit::audit(&env);
    }
}
