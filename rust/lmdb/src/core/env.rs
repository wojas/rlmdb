//! The environment: data file, lock file, memory map, meta pages.
//!
//! [`EnvironmentBuilder::open`] follows `mdb_env_open` (mdb.c line 5102):
//!
//! 1. Resolve file names (`mdb_fname_init`): `path/data.mdb` and
//!    `path/lock.mdb`, or with `NO_SUB_DIR` `path` and `path-lock`.
//! 2. Unless read-only or `NO_LOCK`, set up the lock file *first*
//!    (`LockRegion::open`, `mdb_env_setup_locks`). Read-only environments
//!    open the data file first (it must exist) and the lock file second.
//! 3. Open the data file (`O_RDWR|O_CREAT`, or `O_RDONLY`).
//! 4. `mdb_env_open2`: read both meta pages with `pread`
//!    (`mdb_env_read_header`); an empty file is a new environment whose two
//!    meta pages are written immediately (`mdb_env_init_meta`), with the OS
//!    page size capped at 32 KiB. The map size is the configured one, else the
//!    one recorded in the meta (1 MiB for a new file), raised to cover all
//!    pages in use. Then map the file read-only.
//! 5. Open the `O_DSYNC` descriptor used for meta-page writes (`me_mfd`).
//! 6. If we initialised the lock table, publish the current txnid and
//!    downgrade to a shared lock (`mdb_env_share_locks`).
//!
//! Dropping an [`Environment`] mirrors `mdb_env_close0`: unmap the data
//! file, close the descriptors, clear any reader slots this process still
//! owns, close the shared locks, unmap and close the lock file. Only then is
//! the path removed from the process-wide registry of open environments.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::cmp::CmpFn;
use crate::consts::*;
use crate::core::dbi::{Dbi, DbiRegistry};
use crate::core::reader::{LockRegion, ReaderInfo};
use crate::core::txn::{RoTxn, Transaction};
use crate::core::{Info, Stat};
use crate::error::{Error, MdbError, Result};
use crate::flags::EnvironmentFlags;
use crate::format;
use crate::format::layout::{META_OFFSET_IN_PAGE, META_SIZE, PAGE_HDR_SIZE};
use crate::format::meta::{pick_newest, Meta};
use crate::idl::Idl;
use crate::os::{self, DataMap};

/// Smallest page size we accept in an existing file. C does not check; a
/// page must at least hold a header and a meta.
const MIN_PAGESIZE: usize = 256;

/// Default file mode for [`EnvironmentBuilder::open`].
const DEFAULT_MODE: u32 = 0o644;

// ---------------------------------------------------------------------------
// Process-wide registry of open environments
// ---------------------------------------------------------------------------

fn registry() -> &'static Mutex<HashSet<PathBuf>> {
    static OPEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Membership in the registry; removed on drop. An [`Environment`] keeps it
/// as its last field so the path is released only after every file of the
/// environment is closed.
struct Registration {
    key: PathBuf,
}

impl Registration {
    /// Registers the canonical form of `path`, failing with
    /// [`Error::EnvAlreadyOpened`] if this process has it open already.
    fn new(path: &Path, no_sub_dir: bool) -> Result<Registration> {
        let key = canonical_key(path, no_sub_dir)?;
        let mut open = registry().lock().unwrap_or_else(|e| e.into_inner());
        if !open.insert(key.clone()) {
            return Err(Error::EnvAlreadyOpened);
        }
        Ok(Registration { key })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
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

/// `mdb_fname_init` + `mdb_suffixes`: (data file, lock file) names.
fn file_names(path: &Path, no_sub_dir: bool) -> (PathBuf, PathBuf) {
    if no_sub_dir {
        let mut lock = path.as_os_str().to_owned();
        lock.push("-lock");
        (path.to_path_buf(), PathBuf::from(lock))
    } else {
        (path.join("data.mdb"), path.join("lock.mdb"))
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Options for opening an [`Environment`] (`mdb_env_create` +
/// `mdb_env_set_*` + `mdb_env_open`).
#[derive(Debug, Clone)]
pub struct EnvironmentBuilder {
    map_size: Option<usize>,
    max_readers: u32,
    max_dbs: u32,
    flags: EnvironmentFlags,
}

impl Default for EnvironmentBuilder {
    fn default() -> Self {
        EnvironmentBuilder {
            map_size: None,
            max_readers: DEFAULT_READERS,
            max_dbs: 0,
            flags: EnvironmentFlags::empty(),
        }
    }
}

impl EnvironmentBuilder {
    /// Default options: map size from the file (1 MiB for a new one), 126
    /// readers, no named databases.
    pub fn new() -> Self {
        Self::default()
    }

    /// `mdb_env_set_mapsize`: size of the memory map, i.e. the maximum size
    /// of the database. Used as given (C 0.9 does not round it); raised if
    /// the file already holds more data. `0` means "use the size recorded in
    /// the file".
    pub fn map_size(&mut self, size: usize) -> &mut Self {
        self.map_size = (size != 0).then_some(size);
        self
    }

    /// `mdb_env_set_maxreaders`: reader table size when creating the lock
    /// file (an existing lock file keeps its size). Must be at least 1.
    pub fn max_readers(&mut self, readers: u32) -> &mut Self {
        self.max_readers = readers;
        self
    }

    /// `mdb_env_set_maxdbs`: number of named databases that can be open at
    /// the same time.
    pub fn max_dbs(&mut self, dbs: u32) -> &mut Self {
        self.max_dbs = dbs;
        self
    }

    /// Environment flags.
    pub fn flags(&mut self, flags: EnvironmentFlags) -> &mut Self {
        self.flags = flags;
        self
    }

    /// Opens the environment with file mode `0o644`.
    pub fn open(&self, path: impl AsRef<Path>) -> Result<Environment> {
        self.open_with_permissions(path, DEFAULT_MODE)
    }

    /// Opens the environment, creating files with mode `mode`.
    pub fn open_with_permissions(&self, path: impl AsRef<Path>, mode: u32) -> Result<Environment> {
        Environment::open(self, path.as_ref(), mode)
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// An open LMDB environment (`MDB_env`). `Send + Sync`; share it with `Arc`
/// or references. Dropping it closes the environment.
pub struct Environment {
    // Field order is drop order and mirrors mdb_env_close0: unmap the data
    // file, close its descriptors, then the lock file, and only then leave
    // the registry.
    map: DataMap,
    meta_file: Option<File>,
    data_file: File,
    lock: Option<LockRegion>,
    pub(crate) dbis: DbiRegistry,
    flags: AtomicU32,
    page_size: usize,
    map_size: usize,
    max_pgno: u64,
    max_readers: u32,
    node_max: usize,
    path: PathBuf,
    _registration: Registration,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("path", &self.path)
            .field("page_size", &self.page_size)
            .field("map_size", &self.map_size)
            .finish_non_exhaustive()
    }
}

impl Environment {
    /// Options for opening an environment.
    pub fn builder() -> EnvironmentBuilder {
        EnvironmentBuilder::new()
    }

    /// `mdb_env_open` (see the module documentation for the sequence).
    fn open(opts: &EnvironmentBuilder, path: &Path, mode: u32) -> Result<Environment> {
        let mut flags = opts.flags.bits();
        if flags & !(CHANGEABLE | CHANGELESS) != 0 || opts.max_readers < 1 {
            return Err(Error::einval());
        }
        let read_only = flags & MDB_RDONLY != 0;
        if read_only {
            // Silently ignore WRITEMAP when we're only getting read access.
            flags &= !MDB_WRITEMAP;
        }
        let no_sub_dir = flags & MDB_NOSUBDIR != 0;
        let no_lock = flags & MDB_NOLOCK != 0;
        let (data_path, lock_path) = file_names(path, no_sub_dir);

        // Declared first so that it is dropped last on every error path.
        let registration = Registration::new(path, no_sub_dir)?;

        let mut lock = None;
        if !read_only && !no_lock {
            lock = LockRegion::open(&lock_path, mode, opts.max_readers, false)?;
        }
        let data_file = if read_only {
            File::open(&data_path)?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(mode)
                .open(&data_path)?
        };
        if read_only && !no_lock {
            // For RDONLY, get the lock file after we know the data file exists.
            lock = LockRegion::open(&lock_path, mode, opts.max_readers, true)?;
        }

        // mdb_env_open2
        let (page_size, mut meta, new_env) = match read_header(&data_file)? {
            Some(meta) => (meta.page_size() as usize, meta, false),
            None => {
                let ps = os::os_page_size().min(MAX_PAGESIZE);
                (
                    ps,
                    Meta::new_env(ps as u32, DEFAULT_MAPSIZE as u64, flags),
                    true,
                )
            }
        };
        if !page_size.is_power_of_two() || !(MIN_PAGESIZE..=MAX_PAGESIZE).contains(&page_size) {
            return Err(MdbError::Invalid.into());
        }
        let mut map_size = opts.map_size.unwrap_or(meta.map_size as usize);
        // Make sure mapsize >= committed data size, even when using
        // mm_mapsize, which could be broken in old files (ITS#7789).
        let min_size = (meta.last_pgno as usize)
            .checked_add(1)
            .and_then(|n| n.checked_mul(page_size))
            .ok_or(MdbError::Invalid)?;
        map_size = map_size.max(min_size);
        meta.map_size = map_size as u64;

        if new_env {
            // mdb_env_init_meta: write both meta pages before mapping, so the
            // file is valid even if the rest of the open fails.
            let mut buf = vec![0u8; 2 * page_size];
            meta.write_initial_pages(&mut buf, page_size);
            os::write_all_at(&data_file, &buf, 0)?;
        }

        let map =
            DataMap::map_readonly(data_file.try_clone()?, map_size, flags & MDB_NORDAHEAD != 0)?;

        let meta_file = if read_only {
            None
        } else {
            // Synchronous fd for meta writes. Needed even with
            // MDB_NOSYNC/MDB_NOMETASYNC, in case these get reset.
            Some(
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_DSYNC)
                    .open(&data_path)?,
            )
        };

        let max_readers = lock.as_ref().map_or(opts.max_readers, |l| l.max_readers());
        let env = Environment {
            map,
            meta_file,
            data_file,
            lock,
            dbis: DbiRegistry::new(opts.max_dbs),
            flags: AtomicU32::new(flags),
            page_size,
            map_size,
            max_pgno: (map_size / page_size) as u64,
            max_readers,
            node_max: format::node_max(page_size),
            path: path.to_path_buf(),
            _registration: registration,
        };

        if let Some(lock) = &env.lock {
            if lock.is_exclusive() {
                lock.downgrade(env.pick_meta()?.txnid)?;
            }
        }
        Ok(env)
    }

    // --- meta pages ---------------------------------------------------------

    /// Reads meta page `i` (0 or 1) with volatile reads into a private copy.
    /// Other processes may be rewriting it; the caller decides whether the
    /// copy is consistent (see `RoTxn` begin).
    pub(crate) fn read_meta(&self, i: usize) -> Result<Meta> {
        let mut buf = [0u8; META_SIZE];
        if !self
            .map
            .read_volatile(i * self.page_size + META_OFFSET_IN_PAGE, &mut buf)
        {
            return Err(MdbError::Corrupted.into());
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        let meta = Meta::read(&buf).ok_or(MdbError::Corrupted)?;
        meta.validate().map_err(|_| MdbError::Corrupted)?;
        Ok(meta)
    }

    /// `mdb_env_pick_meta`: the newer of the two meta pages.
    pub(crate) fn pick_meta(&self) -> Result<Meta> {
        let metas = [self.read_meta(0)?, self.read_meta(1)?];
        Ok(metas[pick_newest(&metas)])
    }

    // --- internals used by transactions --------------------------------------

    pub(crate) fn data_map(&self) -> &DataMap {
        &self.map
    }

    pub(crate) fn lock_region(&self) -> Option<&LockRegion> {
        self.lock.as_ref()
    }

    /// `me_maxpg`: number of pages the map can hold.
    pub(crate) fn max_pgno(&self) -> u64 {
        self.max_pgno
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & MDB_RDONLY != 0
    }

    /// `me_nodemax`: largest node that stays inline in a page.
    #[allow(dead_code)] // used by the write path (stage 2)
    pub(crate) fn node_max(&self) -> usize {
        self.node_max
    }

    #[allow(dead_code)] // used by the write path (stage 2)
    pub(crate) fn meta_file(&self) -> Option<&File> {
        self.meta_file.as_ref()
    }

    // --- public API -------------------------------------------------------

    /// `mdb_dbi_open(NULL or name, 0)` in a short read transaction: the
    /// handle of an existing database. `MdbError::NotFound` if there is no
    /// such database.
    pub fn open_db(&self, name: Option<&str>) -> Result<Dbi> {
        let txn = self.begin_ro_txn()?;
        txn.open_db(name)
    }

    /// `mdb_dbi_close`: forget a handle. Later use of it in any transaction
    /// fails with `MdbError::BadDbi`.
    pub fn close_db(&self, db: Dbi) {
        self.dbis.close(db)
    }

    /// `mdb_txn_begin(MDB_RDONLY)`: start a read transaction on the latest
    /// snapshot.
    pub fn begin_ro_txn(&self) -> Result<RoTxn<'_>> {
        RoTxn::begin(self)
    }

    /// `mdb_env_sync`: flush the data file to disk (`fdatasync`, or
    /// `F_FULLFSYNC` on macOS). Skipped with `NO_SYNC` unless `force`.
    /// `EACCES` for a read-only environment.
    pub fn sync(&self, force: bool) -> Result<()> {
        let flags = self.flags.load(Ordering::Relaxed);
        if flags & MDB_RDONLY != 0 {
            return Err(Error::errno(libc::EACCES));
        }
        if force || flags & MDB_NOSYNC == 0 {
            os::fdatasync(&self.data_file)?;
        }
        Ok(())
    }

    /// `mdb_env_stat`: statistics of the main DB in the latest snapshot.
    pub fn stat(&self) -> Result<Stat> {
        let meta = self.pick_meta()?;
        Ok(Stat::from_db(self.page_size, &meta.dbs[MAIN_DBI as usize]))
    }

    /// `mdb_env_info`.
    pub fn info(&self) -> Result<Info> {
        let meta = self.pick_meta()?;
        Ok(Info {
            map_size: self.map_size,
            last_pgno: meta.last_pgno as usize,
            last_txnid: meta.txnid as usize,
            max_readers: self.max_readers,
            num_readers: self.lock.as_ref().map_or(0, |l| l.raw_num_readers()),
        })
    }

    /// Number of pages on the free list of the latest snapshot (the
    /// "Free pages" line of `mdb_stat -f`).
    pub fn freelist(&self) -> Result<usize> {
        let txn = self.begin_ro_txn()?;
        let mut cursor = txn.cursor(Dbi::FREE)?;
        let mut pages = 0usize;
        while let Some((_, data)) = cursor.next()? {
            pages += Idl::encoded_len(data).ok_or(MdbError::Corrupted)? as usize;
        }
        Ok(pages)
    }

    /// `mdb_env_get_flags`.
    pub fn flags(&self) -> EnvironmentFlags {
        EnvironmentFlags::from_bits_retain(
            self.flags.load(Ordering::Relaxed) & (CHANGEABLE | CHANGELESS),
        )
    }

    /// `mdb_env_set_flags`: only `NO_SYNC`, `NO_META_SYNC`, `MAP_ASYNC` and
    /// `NO_MEM_INIT` can be changed on an open environment (`EINVAL`
    /// otherwise).
    pub fn set_flags(&self, flags: EnvironmentFlags, on: bool) -> Result<()> {
        let bits = flags.bits();
        if bits & !CHANGEABLE != 0 {
            return Err(Error::einval());
        }
        if on {
            self.flags.fetch_or(bits, Ordering::Relaxed);
        } else {
            self.flags.fetch_and(!bits, Ordering::Relaxed);
        }
        Ok(())
    }

    /// `mdb_env_get_path`: the path given to `open`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `mdb_env_get_maxreaders`: size of the reader table.
    pub fn max_readers(&self) -> u32 {
        self.max_readers
    }

    /// `mdb_env_get_maxkeysize`: 511.
    pub fn max_key_size(&self) -> usize {
        MDB_MAXKEYSIZE
    }

    /// The page size of the database (`ms_psize`).
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// The size of the memory map (`me_mapsize`).
    pub fn map_size(&self) -> usize {
        self.map_size
    }

    /// `mdb_env_get_fd`-like access to the data file.
    pub fn data_file(&self) -> &File {
        &self.data_file
    }

    /// The active reader slots (`mdb_reader_list`). Empty if the
    /// environment has no lock file (`NO_LOCK`). Use
    /// [`format_reader_list`](crate::core::format_reader_list) for C's text.
    pub fn reader_list(&self) -> Result<Vec<ReaderInfo>> {
        Ok(self
            .lock
            .as_ref()
            .map_or_else(Vec::new, |l| l.reader_list()))
    }

    /// `mdb_reader_check`: clear reader slots of dead processes; returns how
    /// many were cleared.
    pub fn reader_check(&self) -> Result<usize> {
        match &self.lock {
            Some(l) => l.reader_check(),
            None => Ok(0),
        }
    }

    /// `mdb_set_compare`: use `cmp` (or the default again with `None`) to
    /// order keys of `db` in transactions that start using the handle after
    /// this call. A C process sharing the environment must use the same
    /// order.
    pub fn set_compare(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()> {
        self.dbis.set_comparator(db, cmp, false)
    }

    /// `mdb_set_dupsort`: as [`Self::set_compare`] for duplicate data items.
    pub fn set_dupsort(&self, db: Dbi, cmp: Option<CmpFn>) -> Result<()> {
        self.dbis.set_comparator(db, cmp, true)
    }
}

/// `mdb_env_read_header`: read both meta pages with `pread` and return the
/// newer valid one, or `None` for an empty file (a new environment).
fn read_header(file: &File) -> Result<Option<Meta>> {
    const SIZE: usize = PAGE_HDR_SIZE + META_SIZE; // sizeof(MDB_metabuf)
    let mut best: Option<Meta> = None;
    let mut off = 0u64;
    for _ in 0..NUM_METAS {
        let mut buf = [0u8; SIZE];
        let n = os::read_at(file, &mut buf, off)?;
        if n != SIZE {
            if n == 0 && off == 0 {
                return Ok(None);
            }
            return Err(MdbError::Invalid.into());
        }
        let meta = Meta::read_page(&buf).map_err(MdbError::from)?;
        match best {
            Some(b) if meta.txnid <= b.txnid => {}
            _ => best = Some(meta),
        }
        // As C: the second meta page is at the first one's page size.
        off += u64::from(best.map_or(0, |b| b.page_size()));
    }
    Ok(best)
}
