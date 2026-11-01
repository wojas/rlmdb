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
//! * With a lock file, an environment is open at most once per process,
//!   under any name; a second open fails with `EBUSY`
//!   ([`Error::EnvAlreadyOpened`]) before it touches the lock file.
//! * Without one (`NO_LOCK`), any number of handles may open a data file.
//!   They share one [`LocalShared`]: the writer lock that serialises their
//!   write transactions, the last committed txnid and the reader table in
//!   which their read transactions pin snapshots, so a writer of any handle
//!   keeps the pages of readers of every handle (C coordinates nothing
//!   there, `KNOWN_C_BUGS.md` #35). It exists while one of them is open.
//! * A handle with a lock file and a `NO_LOCK` handle never share a file
//!   (their readers would be invisible to each other's writers): whichever
//!   comes second gets `EBUSY`.
//!
//! There are three keys, each claimed before the step it guards. The
//! canonical path is claimed first, before any file is opened. The identity
//! of the data file (device and inode) is claimed as soon as it is open,
//! before the lock file is touched: it recognises one file under other
//! names (symlinks, the data file named with `NO_SUB_DIR` next to its
//! directory, hard links, bind mounts) and keys the shared state. The
//! identity of the lock file is claimed once it is open, before any record
//! lock is taken on it, so that a lock file reachable under another name is
//! refused too. Setting up a lock file this process holds again would be
//! the worst outcome: record locks never conflict within one process, so
//! the exclusive lock would be granted and the table re-initialised under
//! the readers of the open handle (on Windows locks belong to the handle;
//! the claims apply there as well).
//!
//! A refused open must not close a descriptor of a lock file in use either:
//! closing any descriptor of a file drops the process's record locks on it.
//! The registry keeps such descriptors open ("parked") until the handle
//! that holds the lock file is gone, and [`dispose`] does the same for a
//! file opened by name for another purpose ([`sniff_format`]). Descriptors
//! of any other file are closed. All maps are empty, holding no memory, when
//! no environment is open, so a library that is unloaded then leaves
//! nothing behind.
//!
//! The registry is per copy of the library: handles opened through another
//! copy linked into the same process (a static library in several modules,
//! `dlmopen` namespaces) do not see it, as C handles do not see each other.
//!
//! [`sniff_format`]: crate::core::sniff_format

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{fence, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use crate::core::reader::{LocalReaders, LocalSlot};
use crate::error::{Error, Result};

/// What the handles of one data file without a lock file share in this
/// process: the in-process stand-ins for the lock file's writer mutex,
/// `mti_txnid` and reader table.
#[derive(Debug)]
pub(crate) struct LocalShared {
    /// Serialises the write transactions of every handle of the file (and
    /// the set-up of a new file, and the meta-page snapshot of a plain
    /// copy), where the lock file's writer mutex would. Holding it, nobody
    /// in this process writes a meta page.
    writer: Mutex<()>,
    /// The last committed transaction (`mti_txnid`), which read
    /// transactions publish and pin as they would the lock file's (see
    /// `RoTxn::take_snapshot`). [`TXNID_UNKNOWN`] until the first handle
    /// has read the file's header ([`LocalShared::init_txnid`]); from then
    /// on changed only under `writer`, and never back.
    txnid: AtomicU64,
    /// The read transactions of every handle of the file.
    pub(crate) readers: LocalReaders,
}

/// The committed txnid of a [`LocalShared`] before a handle has read the
/// file's header (the txnid of an idle reader slot, `(txnid_t)-1`).
const TXNID_UNKNOWN: u64 = !0;

impl Default for LocalShared {
    fn default() -> Self {
        LocalShared {
            writer: Mutex::new(()),
            txnid: AtomicU64::new(TXNID_UNKNOWN),
            readers: LocalReaders::default(),
        }
    }
}

impl LocalShared {
    /// Locks the writer lock, for as long as the guard lives.
    pub(crate) fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks the writer lock if nobody holds it (this thread included).
    pub(crate) fn try_lock_writer(&self) -> Option<MutexGuard<'_, ()>> {
        match self.writer.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// The last committed transaction of the file (`mti_txnid`).
    pub(crate) fn txnid(&self) -> u64 {
        self.txnid.load(Ordering::SeqCst)
    }

    /// Publishes the last committed txnid in `slot` and returns it,
    /// re-reading until both agree, as
    /// [`LockRegion::publish_current_txnid`](crate::core::reader::LockRegion::publish_current_txnid)
    /// does with `mti_txnid`.
    pub(crate) fn publish_current_txnid(&self, slot: &LocalSlot) -> u64 {
        loop {
            let t = self.txnid();
            slot.publish(t);
            if self.txnid() == t {
                return t;
            }
        }
    }

    /// Sets the last committed transaction: by a writer, holding the writer
    /// lock, as `mdb_env_write_meta` sets `mti_txnid`. Ordered before the
    /// caller's later writes, so that a reader that sees any of them (a
    /// meta page being rewritten) sees the new txnid when it re-checks.
    pub(crate) fn set_txnid(&self, txnid: u64) {
        self.txnid.store(txnid, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// Adopts `newest`, the txnid of the newest meta page, read while
    /// holding the writer lock (`_writer`), if it is newer than what this
    /// process knows: a commit of another process (`NO_LOCK` leaves keeping
    /// processes apart to the caller, as in C, so such a commit happened
    /// while no transaction of ours ran). Returns whether it was newer.
    pub(crate) fn adopt_txnid(&self, _writer: &MutexGuard<'_, ()>, newest: u64) -> bool {
        self.txnid.fetch_max(newest, Ordering::SeqCst) < newest
    }

    /// Sets the last committed transaction from the newest meta page of the
    /// file's header (`newest`) unless it is known: every handle calls this
    /// once it has read the header and before it begins a transaction, so
    /// the first one to get here sets it, while no writer can have
    /// committed (no handle has finished opening), and later ones leave it.
    pub(crate) fn init_txnid(&self, newest: u64) {
        let order = Ordering::SeqCst;
        let _ = self
            .txnid
            .compare_exchange(TXNID_UNKNOWN, newest, order, order);
    }
}

/// The identity of an open data file: device and inode (`st_dev`,
/// `st_ino`; on Windows the volume serial number and the file index,
/// `os::file_id`).
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
    /// As the lock file of one handle, which holds (or is about to take)
    /// record locks on it: no other descriptor of the file may be closed.
    Lock,
}

/// The open paths and files of the process.
struct Registry {
    paths: BTreeMap<PathBuf, Use<()>>,
    files: BTreeMap<FileId, Use<Arc<LocalShared>>>,
    /// Descriptors of lock files in use ([`Use::Lock`]) that a refused open
    /// or [`dispose`] could not close, kept until the file's claim is given
    /// back (after the handle closed its own descriptor).
    parked: BTreeMap<FileId, Vec<File>>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    paths: BTreeMap::new(),
    files: BTreeMap::new(),
    parked: BTreeMap::new(),
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

/// The identity of `file`, which is handed back. If it cannot be read, the
/// descriptor is leaked rather than closed: it may be a lock file in use.
fn identify(file: File) -> Result<(File, FileId)> {
    match crate::os::file_id(&file) {
        Ok((dev, ino)) => Ok((file, FileId { dev, ino })),
        Err(e) => {
            std::mem::forget(file);
            Err(e.into())
        }
    }
}

/// Gets rid of `file`, a descriptor of the file `id` that the caller may
/// not keep: parks it if the file is a lock file in use, else closes it.
/// Either happens under the registry lock, so the file cannot become a lock
/// file in use (which takes a claim first) before it is closed.
fn park_or_close(reg: &mut Registry, id: FileId, file: File) {
    if matches!(reg.files.get(&id), Some(Use::Lock)) {
        reg.parked.entry(id).or_default().push(file);
    } else {
        drop(file);
    }
}

/// Closes `file`, which the caller opened by name, unless it is the lock
/// file of an environment open in this process: closing it would drop that
/// environment's record locks, so it is parked until the environment is
/// gone.
pub(crate) fn dispose(file: File) {
    if let Ok((file, id)) = identify(file) {
        park_or_close(&mut registry(), id, file);
    }
}

/// An environment's place in the registry; released on drop. An
/// [`Environment`](crate::core::Environment) keeps it as its last field, so
/// that its path and files are released only after every file of the
/// environment is closed.
#[derive(Debug)]
pub(crate) struct Registration {
    path: PathBuf,
    /// The data file; normally one, a second one if its name led to another
    /// file when it was opened again.
    data: Vec<FileId>,
    lock: Option<FileId>,
    shared: bool,
}

impl Registration {
    /// The first step of an open, before any file is touched: claims the
    /// canonical form of `path` (see [`canonical_key`]), shared for a
    /// `NO_LOCK` open (`shared`). [`Error::EnvAlreadyOpened`] if the process
    /// has the path open with a lock file, or at all and this open uses a
    /// lock file.
    pub(crate) fn claim_path(path: &Path, no_sub_dir: bool, shared: bool) -> Result<Registration> {
        let key = canonical_key(path, no_sub_dir)?;
        claim(&mut registry().paths, key.clone(), shared, || ())?;
        Ok(Registration {
            path: key,
            data: Vec::new(),
            lock: None,
            shared,
        })
    }

    /// Claims `data_file`, just opened, as [`Registration::claim_path`]
    /// claimed the path, and hands it back with the state the file's
    /// `NO_LOCK` handles share (`None` for an exclusive claim). A file this
    /// registration holds already (the data file opened again) is not
    /// claimed twice. On [`Error::EnvAlreadyOpened`] the descriptor is
    /// closed, or parked if it is a lock file in use.
    pub(crate) fn claim_data_file(
        &mut self,
        data_file: File,
    ) -> Result<(File, Option<Arc<LocalShared>>)> {
        let (data_file, id) = identify(data_file)?;
        let mut reg = registry();
        if self.data.contains(&id) {
            let state = match reg.files.get(&id) {
                Some(Use::Shared { state, .. }) => Some(Arc::clone(state)),
                _ => None,
            };
            return Ok((data_file, state));
        }
        match claim(&mut reg.files, id, self.shared, Arc::default) {
            Ok(state) => {
                self.data.push(id);
                Ok((data_file, state))
            }
            Err(e) => {
                park_or_close(&mut reg, id, data_file);
                Err(e)
            }
        }
    }

    /// Claims `lock_file`, just opened and not locked yet, as the
    /// environment's lock file, and hands it back. [`Error::EnvAlreadyOpened`]
    /// if the process has the file open in any role, the descriptor then
    /// being closed, or parked if it is a lock file in use.
    pub(crate) fn claim_lock_file(&mut self, lock_file: File) -> Result<File> {
        let (lock_file, id) = identify(lock_file)?;
        let mut reg = registry();
        if reg.files.contains_key(&id) {
            park_or_close(&mut reg, id, lock_file);
            return Err(Error::EnvAlreadyOpened);
        }
        reg.files.insert(id, Use::Lock);
        self.lock = Some(id);
        Ok(lock_file)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut reg = registry();
        let reg = &mut *reg;
        release(&mut reg.paths, &self.path);
        for id in self.data.iter().chain(&self.lock) {
            release(&mut reg.files, id);
            if !reg.files.contains_key(id) {
                // Nobody has the file open any more (a lock file's handle
                // closed its descriptor before it got here): close what
                // refused opens left.
                reg.parked.remove(id);
            }
        }
        if reg.parked.is_empty() {
            reg.parked = BTreeMap::new();
        }
    }
}

/// The canonical path identifying an environment: the directory, or with
/// `NO_SUB_DIR` the data file (which may not exist yet).
fn canonical_key(path: &Path, no_sub_dir: bool) -> Result<PathBuf> {
    if !no_sub_dir {
        return resolve(path);
    }
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(_) => {
            let file = path.file_name().ok_or_else(Error::einval)?;
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => resolve(p)?,
                _ => std::env::current_dir()?,
            };
            Ok(parent.join(file))
        }
    }
}

/// The canonical form of the directory `path`. On Windows a directory that
/// cannot be resolved is keyed by its absolute path instead: the open that
/// follows then fails with the error C gets from opening a file in it
/// (`ERROR_PATH_NOT_FOUND` for a missing directory, where resolving it says
/// `ERROR_FILE_NOT_FOUND`). On Unix both are `ENOENT`.
fn resolve(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(p) => Ok(p),
        // The files of "" are opened in the current directory.
        #[cfg(windows)]
        Err(_) if path.as_os_str().is_empty() => Ok(std::env::current_dir()?),
        #[cfg(windows)]
        Err(e) => std::path::absolute(path).map_err(|_| e.into()),
        #[cfg(not(windows))]
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Dbi, Environment, Transaction};
    use crate::error::MdbError;
    use crate::flags::{EnvironmentFlags, WriteFlags};
    use crate::format::layout::Format;

    /// The number of shared claims, 0 for an exclusive one.
    fn handles<S>(u: &Use<S>) -> usize {
        match u {
            Use::Exclusive | Use::Lock => 0,
            Use::Shared { handles, .. } => *handles,
        }
    }

    /// How many handles have `dir`'s data file open, if any: (path claims,
    /// file claims; 0 for an exclusive claim).
    fn claims(dir: &Path) -> Option<(usize, usize)> {
        let reg = registry();
        let path = reg.paths.get(&dir.canonicalize().unwrap())?;
        let (dev, ino) = crate::os::file_id(&File::open(dir.join("data.mdb")).unwrap()).unwrap();
        let id = FileId { dev, ino };
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
        #[cfg(unix)]
        std::os::unix::fs::symlink(&env_dir, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&env_dir, &link).unwrap();
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
        // A handle with a lock file is refused before it creates a lock
        // file: where the path is claimed, and where only the file is (a
        // hard link that no handle uses).
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
        assert!(!dir.path().join("hard2.mdb-lock").exists());
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

    /// The data file and the lock file of an environment `name` in `dir`,
    /// with the environment's path: a directory, created here, or with
    /// `no_sub_dir` the data file.
    fn env_files(dir: &Path, name: &str, no_sub_dir: bool) -> [PathBuf; 3] {
        let path = dir.join(name);
        if no_sub_dir {
            let mut lock = path.clone().into_os_string();
            lock.push("-lock");
            [path.clone(), path, lock.into()]
        } else {
            std::fs::create_dir(&path).unwrap();
            [path.clone(), path.join("data.mdb"), path.join("lock.mdb")]
        }
    }

    /// How many descriptors of the file `id` refused opens left parked.
    fn parked(id: FileId) -> usize {
        registry().parked.get(&id).map_or(0, Vec::len)
    }

    /// The process has an environment open, with a live read transaction,
    /// and its files are reachable under other names (hard links here; a
    /// bind mount looks the same). Opening them must fail before the lock
    /// file is set up again: the exclusive lock would be granted within the
    /// process and the table re-initialised (the reader slot gone,
    /// `mti_txnid` 0, so the next writer would overwrite the reader's
    /// pages), and closing a descriptor of the lock file would drop the
    /// open handle's record locks. Every branch of the open: read-write,
    /// read-only, `NO_LOCK`, both formats, with and without `NO_SUB_DIR`;
    /// both files under another name, and the lock file alone.
    #[test]
    fn an_environment_under_another_name_is_refused_before_its_lock_file() {
        for format in [Format::V09, Format::V10] {
            for no_sub_dir in [false, true] {
                refuse_aliases(format, no_sub_dir);
            }
        }
    }

    fn refuse_aliases(format: Format, no_sub_dir: bool) {
        let dir = tempfile::tempdir().unwrap();
        let empty = EnvironmentFlags::empty();
        let sub = if no_sub_dir {
            EnvironmentFlags::NO_SUB_DIR
        } else {
            empty
        };
        let open = |path: &Path, flags| Environment::builder().flags(sub | flags).open(path);
        let [a, a_data, a_lock] = env_files(dir.path(), "a", no_sub_dir);
        let env = Environment::builder()
            .format(format)
            .flags(sub | EnvironmentFlags::NO_SYNC)
            .open(&a)
            .unwrap();
        let mut txn = env.begin_rw_txn().unwrap();
        // On overflow pages, which the next commit would reuse if it did
        // not see the reader.
        txn.put(Dbi::MAIN, b"key", &[11; 32000], WriteFlags::empty())
            .unwrap();
        txn.commit().unwrap();
        let reader = env.begin_ro_txn().unwrap();
        let held = reader.get(Dbi::MAIN, b"key").unwrap().unwrap();
        let before = held.to_vec();
        // Its own descriptor, closed only after the environment.
        let lock_file = File::open(&a_lock).unwrap();
        let (dev, ino) = crate::os::file_id(&lock_file).unwrap();
        let lock_id = FileId { dev, ino };
        let lock = env.lock_region().unwrap();
        let intact = |what: &str| {
            let list = env.reader_list().unwrap();
            assert_eq!(list.len(), 1, "{what}: reader slot");
            assert_eq!(list[0].pid, crate::os::getpid(), "{what}");
            assert_eq!(list[0].txnid, Some(1), "{what}");
            assert_eq!(lock.raw_num_readers(), 1, "{what}: mti_numreaders");
            assert_eq!(lock.txnid(), 1, "{what}: mti_txnid");
            assert_eq!(held, &before[..], "{what}: held value");
        };
        let refused = |path: &Path, flags: EnvironmentFlags| {
            let what = format!("{format:?} {path:?} {flags:?}");
            let e = open(path, flags).unwrap_err();
            assert!(matches!(e, Error::EnvAlreadyOpened), "{what}: {e:?}");
            intact(&what);
        };

        // Both files.
        let [b, b_data, b_lock] = env_files(dir.path(), "b", no_sub_dir);
        if let Err(e) = std::fs::hard_link(&a_data, &b_data) {
            eprintln!("NOTE: skipped: the file system refuses hard links: {e}");
            return;
        }
        std::fs::hard_link(&a_lock, &b_lock).unwrap();
        let no_lock = EnvironmentFlags::NO_LOCK;
        let read_only = EnvironmentFlags::READ_ONLY;
        for flags in [empty, read_only, no_lock, no_lock | read_only] {
            refused(&b, flags);
        }
        // Refused at the data file: the lock file was not opened.
        assert_eq!(parked(lock_id), 0);

        // The lock file alone: the data file missing (the lock file comes
        // first, so the data file is not created), then another file.
        let [c, c_data, c_lock] = env_files(dir.path(), "c", no_sub_dir);
        std::fs::hard_link(&a_lock, &c_lock).unwrap();
        refused(&c, empty);
        assert!(!c_data.exists());
        std::fs::write(&c_data, std::fs::read(&a_data).unwrap()).unwrap();
        for flags in [empty, read_only] {
            refused(&c, flags);
        }
        // Their descriptors of the lock file are kept open, not closed.
        assert_eq!(parked(lock_id), 3);
        // So is one that names the lock file as a data file, here to sniff
        // its format (it is not a data file).
        let e = crate::core::sniff_format(&c_lock, EnvironmentFlags::NO_SUB_DIR).unwrap_err();
        assert!(matches!(e, Error::Mdb(MdbError::Invalid)), "{e:?}");
        assert_eq!(parked(lock_id), 4);
        intact("sniff_format");

        // The handle carries on: its next writer follows the last commit
        // and keeps the reader's pages.
        let mut txn = env.begin_rw_txn().unwrap();
        assert_eq!(txn.id(), 2);
        txn.put(Dbi::MAIN, b"key", &[22; 32000], WriteFlags::empty())
            .unwrap();
        txn.commit().unwrap();
        assert_eq!(held, &before[..]);
        drop(reader);
        crate::core::audit::audit(&env);
        drop(env);
        drop(lock_file);
        // The parked descriptors went with the handle's claim.
        assert_eq!(parked(lock_id), 0);
        assert!(!registry().files.contains_key(&lock_id));

        // Without the first handle, the other names open the environment.
        let env = open(&b, empty).unwrap();
        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(txn.id(), 2);
        assert_eq!(txn.get(Dbi::MAIN, b"key").unwrap(), Some(&[22; 32000][..]));
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
