//! The environment: data file, lock file, memory map, meta pages.
//!
//! [`EnvironmentBuilder::open`] follows `mdb_env_open` (mdb.c line 5102):
//!
//! 1. Resolve file names (`mdb_fname_init`): `path/data.mdb` and
//!    `path/lock.mdb`, or with `NO_SUB_DIR` `path` and `path-lock`.
//! 2. Claim the environment in the process-wide registry (`registry`; not
//!    in C): its path, then the data file, if it exists, which is opened
//!    read-only for that and claimed by device and inode before the lock
//!    file is touched. A second open of an environment with a lock file is
//!    refused there, also under another name (a hard link, a bind mount),
//!    before it could set up the open handle's lock file again.
//! 3. Decide the data-file format ([`sniff_format`] of that descriptor: the
//!    magic and version of the first meta page; not in C, which handles one
//!    format only). An existing file keeps its format, a new one gets the
//!    requested format or the [`default_format`] (the process default of
//!    [`set_default_format`], the experimental `RLMDB_DEFAULT_FORMAT`
//!    variable, LMDB 0.9). This happens before the lock file is touched, so
//!    opening a file of the other format never re-initialises a peer's lock
//!    file.
//! 4. Unless read-only or `NO_LOCK`, set up the lock file *first*
//!    (`LockRegion::open`, `mdb_env_setup_locks`; it claims the lock file in
//!    the registry before it locks it). Read-only environments open the
//!    data file first (it must exist) and the lock file second. Without a
//!    lock file (`NO_LOCK`, or read-only on a read-only file system) the
//!    environment tracks its read transactions in an in-memory table
//!    instead (`reader::LocalReaders`; C tracks none), so that its writers
//!    never reuse pages they read.
//! 5. Open the data file (`O_RDWR|O_CREAT`; read-only, the descriptor of
//!    step 2) and claim it, unless it is the file claimed in step 2.
//! 6. `mdb_env_open2`: read both meta pages with `pread`
//!    (`mdb_env_read_header`); an empty file is a new environment whose two
//!    meta pages are written immediately (`mdb_env_init_meta`), with the OS
//!    page size (1.0: or the one set with `mdb_env_set_pagesize`) capped at
//!    the format's `MAX_PAGESIZE` (32 KiB for LMDB 0.9, 64 KiB for 1.0).
//!    An existing 1.0 file that uses encryption or page checksums is
//!    refused. The map size is the configured one, else the one recorded
//!    in the meta (1 MiB for a new file), raised to cover all pages in use.
//!    Then map the file read-only.
//! 7. Open the `O_DSYNC` descriptor used for meta-page writes (`me_mfd`).
//! 8. If we initialised the lock table, publish the current txnid and
//!    downgrade to a shared lock (`mdb_env_share_locks`).
//!
//! The data-file format of the environment ([`Format`]) decides the page
//! layout; the environment keeps its [`Layout`] and every transaction reads
//! and writes pages through it. The size-derived limits (`me_nodemax`, the
//! maximum key size, `me_maxfree_1pg`) follow from the layout and the page
//! size. The lock file version follows the format too
//! (`format::lockfile::LockLayout`): version 1 for 0.9, version 2 for 1.0.
//!
//! Dropping an [`Environment`] mirrors `mdb_env_close0`: unmap the data
//! file, close the descriptors, clear any reader slots this process still
//! owns, close the shared locks, unmap and close the lock file. Only then is
//! the environment removed from the process-wide registry of open
//! environments (`registry`), which refuses a second open of an environment
//! with a lock file and lets `NO_LOCK` handles of one data file share their
//! writer lock and reader table.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::cmp::CmpFn;
use crate::consts::*;
use crate::core::dbi::{Dbi, DbiRegistry};
use crate::core::reader::{LockRegion, ReaderInfo, ReaderTable};
use crate::core::registry::{LocalShared, Registration};
use crate::core::txn::{RoTxn, Transaction};
use crate::core::txn_rw::{RwTxn, SpareTxn};
use crate::core::{Info, Stat};
use crate::error::{Error, MdbError, Result};
use crate::flags::{DatabaseFlags, EnvironmentFlags};
use crate::format::get_u64;
use crate::format::layout::{Format, Layout, META_LAST_PGNO, META_SIZE, V10};
use crate::format::meta::{self, pick_newest, Meta, SNIFF_LEN};
use crate::idl::Idl;
use crate::os::{self, DataMap};

/// Smallest page size we accept in an existing file. C does not check; a
/// page must at least hold a header and a meta.
const MIN_PAGESIZE: usize = 256;

/// Default file mode for [`EnvironmentBuilder::open`].
const DEFAULT_MODE: u32 = 0o644;

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
    format: Option<Format>,
    page_size: Option<usize>,
}

impl Default for EnvironmentBuilder {
    fn default() -> Self {
        EnvironmentBuilder {
            map_size: None,
            max_readers: DEFAULT_READERS,
            max_dbs: 0,
            flags: EnvironmentFlags::empty(),
            format: None,
            page_size: None,
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

    /// Environment flags. Bits that are not environment flags of LMDB 0.9
    /// make the open fail with `EINVAL`, for either format; among them are
    /// LMDB 1.0's `MDB_PREVSNAPSHOT` (`0x2000000`, not implemented: rlmdb
    /// always opens the newest snapshot) and `MDB_ENCRYPT` (`0x2000`).
    pub fn flags(&mut self, flags: EnvironmentFlags) -> &mut Self {
        self.flags = flags;
        self
    }

    /// The data-file format to use: [`Format::V09`] (`MDB_DATA_VERSION 1`)
    /// or [`Format::V10`] (`MDB_DATA_VERSION 3`). An existing data file
    /// keeps its format: opening it fails with `MDB_VERSION_MISMATCH` if
    /// this asks for the other one ([`sniff_format`] tells which it is).
    /// Without a request an existing file is opened in its format and a new
    /// one is created in the [`default_format`]: the process default
    /// ([`set_default_format`]), else the experimental `RLMDB_DEFAULT_FORMAT`
    /// variable ([`DEFAULT_FORMAT_ENV`]), else LMDB 0.9.
    pub fn format(&mut self, format: Format) -> &mut Self {
        self.format = Some(format);
        self
    }

    /// `mdb_env_set_pagesize` (LMDB 1.0): the page size of a new 1.0
    /// environment, any power of two from 256 to 65536 (the default is the
    /// OS page size). An existing environment keeps the page size recorded
    /// in its file, as in C. LMDB 0.9 has no such call: opening an
    /// environment that turns out to be 0.9 with a page size set fails with
    /// `EINVAL`, as does a size that is not a power of two in range.
    pub fn page_size(&mut self, size: usize) -> &mut Self {
        self.page_size = Some(size);
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
    /// The current data map. Transactions keep an `Arc` of the map that was
    /// current when they began, so [`Environment::set_map_size`] can install
    /// a new map without invalidating slices held by live transactions.
    map: RwLock<Arc<DataMap>>,
    meta_file: Option<File>,
    data_file: File,
    /// The lock file's reader table, or without a lock file the in-memory
    /// table of this process's read transactions.
    readers: ReaderTable,
    pub(crate) dbis: DbiRegistry,
    flags: AtomicU32,
    /// `MDB_FATAL_ERROR`: a meta page write (or the sync after it) failed;
    /// every new transaction fails with `MDB_PANIC`.
    fatal: AtomicBool,
    /// The `errno` the sync after the next meta page write is to fail with
    /// (0: none), for tests ([`Environment::fail_next_meta_sync`]).
    meta_sync_fault: AtomicI32,
    /// What to run once the next meta page write of this handle reached the
    /// file, for tests ([`Environment::after_next_meta_write`]).
    meta_write_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Writer state that lives in `MDB_env` in C (`me_pgoldest`). Its mutex
    /// also serialises write transactions within this process (the shared
    /// writer mutex does that across processes; without a lock file it is
    /// the only writer lock, and other processes are the caller's business,
    /// as with C's `MDB_NOLOCK`).
    writer: Mutex<WriterState>,
    page_size: usize,
    map_size: AtomicUsize,
    /// `MDB_IDL_UM_MAX`: how many pages a write transaction may hold dirty
    /// before it spills some to disk. Fixed in C; lowered by unit tests to
    /// exercise spilling with small transactions.
    dirty_limit: AtomicUsize,
    max_readers: u32,
    /// The page layout of the environment's data-file format.
    layout: &'static Layout,
    node_max: usize,
    /// `me_maxkey` / `ENV_MAXKEY`: checked on every put.
    max_key: usize,
    path: PathBuf,
    _registration: Registration,
}

/// Per-environment state of the (single) write transaction.
#[derive(Debug, Default)]
pub(crate) struct WriterState {
    /// `me_pgoldest`: the oldest reader's snapshot the last time a writer
    /// looked (`mdb_page_alloc`). Only grows; refreshed when it is in the
    /// way.
    pub(crate) pg_oldest: u64,
    /// The containers of the last write transaction, emptied, with the
    /// spare page buffers (`me_dpages`, a pool for the environment's page
    /// size), kept for the next one (a write transaction holds them while
    /// it runs; see `SpareTxn`).
    pub(crate) spare: Option<SpareTxn>,
    /// `me_pbuf->mp_pad`: the key size field of C's scratch page for
    /// `MDB_DUPSORT` puts, which outlives transactions. A sub-page made from
    /// a single item of a non-`MDB_DUPFIXED` database inherits it (C never
    /// sets it there), so the `mp_pad` it gets depends on earlier puts
    /// (see `RwTxn::dup_prepare`). State, not a container: it is kept here
    /// whatever happens to `spare`.
    pub(crate) pbuf_pad: u16,
    /// The meta page the last write transaction of this process wrote:
    /// while the lock file still names its txnid, no other writer has
    /// committed since, so it is the newest meta page, and the next write
    /// transaction begins from it without reading it back. Never used
    /// without a lock file (`MDB_NOLOCK`).
    pub(crate) last_meta: Option<Meta>,
}

impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("path", &self.path)
            .field("format", &self.layout.format)
            .field("page_size", &self.page_size)
            .field("map_size", &self.map_size())
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
        // With a lock file this refuses a second open before the lock file
        // is touched (closing it would drop the first handle's locks).
        let mut registration = Registration::claim_path(path, no_sub_dir, no_lock)?;

        // The same for the data file under another name: claim it before
        // the lock file is touched. A lock file this process holds already
        // would be set up again (the exclusive lock is granted within the
        // process) and its reader table re-initialised under the open
        // handle's readers.
        let mut local = None;
        let existing = match open_existing(&data_path, mode)? {
            Some(file) => {
                let (file, shared) = registration.claim_data_file(file)?;
                local = shared;
                Some(file)
            }
            None => None,
        };

        // Decide the format before the lock file is touched: C sets up the
        // lock file first and would re-initialise a lock file of the other
        // version (DUAL_FORMAT.md §4.2).
        let found = match &existing {
            Some(file) => sniff_file(file)?,
            None => None,
        };
        let layout = choose_format(found, opts.format)?.layout();
        let new_page_size = match opts.page_size {
            None => None,
            Some(ps) if layout.format == Format::V10 && valid_set_pagesize(ps, layout) => Some(ps),
            Some(_) => return Err(Error::einval()),
        };

        // Lock-file version (dual-format stage 3): it follows the data
        // format, version 1 for 0.9 and version 2 for 1.0.
        let lock_layout = layout.lock();
        let mut lock = None;
        if !read_only && !no_lock {
            lock = LockRegion::open(
                &lock_path,
                mode,
                opts.max_readers,
                false,
                lock_layout,
                &mut registration,
            )?;
        }
        let data_file = match existing {
            Some(file) if read_only => file,
            probe => {
                let mut options = OpenOptions::new();
                options.read(true);
                if !read_only {
                    options.write(true).create(true);
                }
                let file = os::env_file_options(&mut options, mode).open(&data_path)?;
                // A new file, or the existing one again for writing (another
                // file only if it was replaced since). The NO_LOCK handles
                // of one data file in this process share a writer lock and a
                // reader table.
                let (file, shared) = registration.claim_data_file(file)?;
                local = shared;
                // Closed while its claim holds (registration outlives it).
                drop(probe);
                file
            }
        };
        if read_only && !no_lock {
            // For RDONLY, get the lock file after we know the data file exists.
            lock = LockRegion::open(
                &lock_path,
                mode,
                opts.max_readers,
                true,
                lock_layout,
                &mut registration,
            )?;
        }

        // mdb_env_open2
        let mut header = read_header(&data_file, layout);
        // Another NO_LOCK handle of the process may be setting up the same
        // new file (its size grows while the meta pages are written). If
        // there is no complete header yet, wait for that under the file's
        // writer lock and read again; an empty file is then set up under
        // the lock, so one handle writes the meta pages and the others see
        // them. (A thread with a write transaction of the file in another
        // handle always finds a complete header, so it never waits here.)
        let setup = match local.as_deref() {
            Some(local) if !matches!(header, Ok(Some(_))) => {
                let guard = local.lock_writer();
                header = read_header(&data_file, layout);
                Some(guard)
            }
            _ => None,
        };
        let max_pagesize = layout.max_pagesize;
        let (page_size, mut meta, new_env, meta0_flags) = match header? {
            Some((meta, meta0)) => (meta.page_size() as usize, meta, false, meta0.env_flags()),
            None => {
                // me_os_psize, which mdb_env_set_pagesize replaces in 1.0.
                let ps = new_page_size
                    .unwrap_or_else(os::os_page_size)
                    .min(max_pagesize);
                (
                    ps,
                    Meta::new_env_with_layout(layout, ps as u32, DEFAULT_MAPSIZE as u64, flags),
                    true,
                    0,
                )
            }
        };
        if !page_size.is_power_of_two() || !(MIN_PAGESIZE..=max_pagesize).contains(&page_size) {
            return Err(MdbError::Invalid.into());
        }
        if layout.format == Format::V10 && !new_env {
            check_v10_features(&data_file, page_size, meta0_flags)?;
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
            meta.write_initial_pages_with_layout(&mut buf, page_size, layout);
            os::write_all_at(&data_file, &buf, 0)?;
        }
        drop(setup);

        let random = flags & MDB_NORDAHEAD != 0;
        let map = os::map_data(data_file.try_clone()?, map_size, random, layout.format)?
            .with_meta_pages(NUM_METAS * page_size);

        let meta_file = if read_only {
            None
        } else {
            // Synchronous fd for meta writes. Needed even with
            // MDB_NOSYNC/MDB_NOMETASYNC, in case these get reset. Opened by
            // name, as in C, so claimed like the data file (it is that file
            // unless the name now leads elsewhere).
            let file = os::sync_write_options(OpenOptions::new().write(true)).open(&data_path)?;
            Some(registration.claim_data_file(file)?.0)
        };

        let max_readers = lock.as_ref().map_or(opts.max_readers, |l| l.max_readers());
        let readers = match (lock, local) {
            (Some(lock), _) => ReaderTable::Shared(lock),
            (None, local) => {
                // No lock file without NO_LOCK (read-only on a read-only
                // file system): the only handle of the file in the process.
                let local = local.unwrap_or_default();
                // The last commit, from the header read above, unless an
                // earlier handle of the file knows it already.
                local.init_txnid(meta.txnid);
                ReaderTable::Local(local)
            }
        };
        let env = Environment {
            map: RwLock::new(Arc::new(map)),
            meta_file,
            data_file,
            readers,
            dbis: DbiRegistry::new(opts.max_dbs),
            flags: AtomicU32::new(flags),
            fatal: AtomicBool::new(false),
            meta_sync_fault: AtomicI32::new(0),
            meta_write_hook: Mutex::new(None),
            writer: Mutex::new(WriterState {
                pbuf_pad: 0,
                pg_oldest: 0,
                spare: Some(SpareTxn::new(page_size)),
                last_meta: None,
            }),
            page_size,
            map_size: AtomicUsize::new(map_size),
            dirty_limit: AtomicUsize::new(MDB_IDL_UM_MAX),
            max_readers,
            layout,
            node_max: layout.node_max(page_size),
            max_key: layout.max_key_size(page_size),
            path: path.to_path_buf(),
            _registration: registration,
        };

        if let Some(lock) = env.lock_region() {
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
            .data_map()
            .read_volatile(i * self.page_size + self.layout.meta_offset(), &mut buf)
        {
            return Err(MdbError::Corrupted.into());
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        let meta = Meta::read(&buf).ok_or(MdbError::Corrupted)?;
        meta.validate_with_layout(self.layout)
            .map_err(|_| MdbError::Corrupted)?;
        Ok(meta)
    }

    /// `mm_last_pg` and `mm_txnid` of meta page `i` (0 or 1), read with
    /// volatile reads and not validated: what `mdb_env_write_meta` keeps
    /// of the meta page it is about to overwrite (`metab`), to write back
    /// if that write fails. `None` if the page is not readable.
    pub(crate) fn meta_ids(&self, i: usize) -> Option<(u64, u64)> {
        let mut buf = [0u8; 16];
        let off = i * self.page_size + self.layout.meta_offset() + META_LAST_PGNO;
        if !self.data_map().read_volatile(off, &mut buf) {
            return None;
        }
        Some((get_u64(&buf, 0)?, get_u64(&buf, 8)?))
    }

    /// `mdb_env_pick_meta`: the newer of the two meta pages.
    pub(crate) fn pick_meta(&self) -> Result<Meta> {
        let metas = [self.read_meta(0)?, self.read_meta(1)?];
        Ok(metas[pick_newest(&metas)])
    }

    // --- internals used by transactions --------------------------------------

    /// The current data map. A transaction keeps the `Arc` it got at
    /// begin for its whole life.
    pub(crate) fn data_map(&self) -> Arc<DataMap> {
        Arc::clone(&self.map.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// `MDB_Invalidate`: after `len` bytes at `offset` were written to the
    /// data file, drops the map's copy of them where the OS does not keep
    /// the two in step (OpenBSD, `os::MAP_NEEDS_INVALIDATE`); nothing
    /// elsewhere.
    #[inline]
    pub(crate) fn invalidate_map(&self, offset: u64, len: usize) {
        if os::MAP_NEEDS_INVALIDATE && len > 0 {
            self.data_map().invalidate(offset, len);
        }
    }

    /// The map to use in place of `old`, if `old` is shorter than the map
    /// size (it then covers the file only as it was when it was made: a
    /// read-only environment on Windows): the environment's map, made anew
    /// if the file has grown past it since (also when another transaction
    /// renewed `old` already, at an older size); `None` if `old` is not
    /// short (always on Unix). Read transactions keep the map they use, as
    /// with [`Environment::set_map_size`].
    pub(crate) fn renew_short_map(&self, old: &Arc<DataMap>) -> Result<Option<Arc<DataMap>>> {
        let size = self.map_size.load(Ordering::SeqCst);
        if old.len() >= size {
            return Ok(None);
        }
        let mut current = self.map.write().unwrap_or_else(|e| e.into_inner());
        let file_size = self.data_file.metadata()?.len();
        if current.len() < size && file_size > current.len() as u64 {
            let random = self.raw_flags() & MDB_NORDAHEAD != 0;
            let map = os::map_data(
                self.data_file.try_clone()?,
                size,
                random,
                self.layout.format,
            )?
            .with_meta_pages(NUM_METAS * self.page_size);
            *current = Arc::new(map);
        }
        Ok(Some(Arc::clone(&current)))
    }

    /// The lock file's table, `None` without a lock file.
    pub(crate) fn lock_region(&self) -> Option<&LockRegion> {
        match &self.readers {
            ReaderTable::Shared(lock) => Some(lock),
            ReaderTable::Local(_) => None,
        }
    }

    /// Where read transactions advertise their snapshots.
    pub(crate) fn readers(&self) -> &ReaderTable {
        &self.readers
    }

    /// Without a lock file, the writer lock and reader table this handle
    /// shares with the file's other `NO_LOCK` handles in the process.
    pub(crate) fn local_shared(&self) -> Option<&LocalShared> {
        match &self.readers {
            ReaderTable::Shared(_) => None,
            ReaderTable::Local(local) => Some(local),
        }
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & MDB_RDONLY != 0
    }

    /// `me_flags & MDB_*` as raw bits (including `MDB_RDONLY`).
    pub(crate) fn raw_flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// `me_flags & MDB_FATAL_ERROR`.
    pub(crate) fn has_fatal_error(&self) -> bool {
        self.fatal.load(Ordering::SeqCst)
    }

    /// Sets `MDB_FATAL_ERROR` (a meta page write or the sync after it
    /// failed, `mdb_env_write_meta`).
    pub(crate) fn set_fatal_error(&self) {
        self.fatal.store(true, Ordering::SeqCst);
    }

    /// Makes the sync after the next meta page write of this handle fail
    /// with `errno`, after the page has reached the file: what a failing
    /// disk does to the `F_FULLFSYNC` after the write on macOS, or to the
    /// `O_DSYNC` write itself elsewhere. Not part of the API: for this
    /// crate's tests (including the integration tests).
    #[doc(hidden)]
    pub fn fail_next_meta_sync(&self, errno: i32) {
        self.meta_sync_fault.store(errno, Ordering::Relaxed);
    }

    /// Runs `hook` on the committing thread once the next meta page write
    /// of this handle has reached the file (and, on macOS, its sync is
    /// done), before the commit is published or fails as
    /// [`Environment::fail_next_meta_sync`] asks: a commit stopped in
    /// flight, holding the writer lock, for tests of what other processes
    /// see meanwhile. Not part of the API: for this crate's tests
    /// (including the integration tests).
    #[doc(hidden)]
    pub fn after_next_meta_write(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .meta_write_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Box::new(hook));
    }

    /// Runs the hook of [`Environment::after_next_meta_write`], then returns
    /// the failure [`Environment::fail_next_meta_sync`] asked for; each
    /// once.
    pub(crate) fn injected_meta_sync_error(&self) -> std::io::Result<()> {
        let hook = self
            .meta_write_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(hook) = hook {
            hook();
        }
        if self.meta_sync_fault.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        match self.meta_sync_fault.swap(0, Ordering::Relaxed) {
            0 => Ok(()),
            errno => Err(std::io::Error::from_raw_os_error(errno)),
        }
    }

    /// `MDB_IDL_UM_MAX`: the dirty list's capacity (see the field).
    pub(crate) fn dirty_limit(&self) -> usize {
        self.dirty_limit.load(Ordering::Relaxed)
    }

    /// Lowers the dirty-list capacity for write transactions that begin
    /// later, so tests can make small transactions spill. Not part of the
    /// API: for this crate's tests (including the integration tests).
    #[doc(hidden)]
    pub fn set_dirty_limit(&self, pages: usize) {
        self.dirty_limit.store(pages.max(16), Ordering::Relaxed);
    }

    /// `me_nodemax`: largest node that stays inline in a page.
    pub(crate) fn node_max(&self) -> usize {
        self.node_max
    }

    /// The page layout of the environment's data-file format.
    #[inline]
    pub(crate) fn layout(&self) -> &'static Layout {
        self.layout
    }

    /// `me_mfd`: the `O_DSYNC` descriptor for meta page writes (`None` for
    /// a read-only environment).
    pub(crate) fn meta_file(&self) -> Option<&File> {
        self.meta_file.as_ref()
    }

    /// Locks the in-process writer state. Held by a write transaction for
    /// its whole life (which also makes `RwTxn` `!Send`).
    pub(crate) fn lock_writer_state(&self) -> MutexGuard<'_, WriterState> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
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

    /// The comparison functions installed on handle `db` (by
    /// [`Self::set_compare`] / [`Self::set_dupsort`] or their
    /// [`Transaction`] forms), key order first; `None` for an order that is
    /// the default: C's `me_dbxs[db].md_cmp` and `md_dcmp` where the
    /// program set them. They belong to the handle: closing it (also by
    /// dropping its database, or by the abort of the transaction that
    /// created it) forgets them, and the abort of a nested transaction
    /// that dropped the database brings them back with the handle. Not
    /// part of the API: the C API (`rlmdb-capi`) answers `mdb_cmp` and
    /// `mdb_dcmp` with them.
    #[doc(hidden)]
    pub fn installed_comparators(&self, db: Dbi) -> (Option<CmpFn>, Option<CmpFn>) {
        self.dbis
            .read()
            .get(db.0 as usize)
            .map_or((None, None), |s| (s.custom_cmp, s.custom_dcmp))
    }

    /// `mdb_txn_begin(MDB_RDONLY)`: start a read transaction on the latest
    /// snapshot.
    pub fn begin_ro_txn(&self) -> Result<RoTxn<'_>> {
        RoTxn::begin(self)
    }

    /// `mdb_txn_begin(env, NULL, 0)`: start the write transaction, waiting
    /// for the writer lock (held by another thread or process). `EACCES`
    /// for a read-only environment. The transaction is bound to the calling
    /// thread (`!Send`), because the Linux writer mutex is owned by the
    /// thread that locked it.
    pub fn begin_rw_txn(&self) -> Result<RwTxn<'_>> {
        RwTxn::begin(self)
    }

    /// `mdb_dbi_open` with `MDB_CREATE` in a short write transaction: the
    /// handle of the database `name` (`None` = the main DB), created with
    /// `flags` if it does not exist yet.
    pub fn create_db(&self, name: Option<&str>, flags: DatabaseFlags) -> Result<Dbi> {
        let mut txn = self.begin_rw_txn()?;
        let db = txn.create_db(name, flags)?;
        txn.commit()?;
        Ok(db)
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

    /// `mdb_env_info`. Without a lock file `num_readers` is 0, as in C
    /// (see [`Environment::in_memory_readers`]).
    pub fn info(&self) -> Result<Info> {
        let meta = self.pick_meta()?;
        Ok(Info {
            map_size: self.map_size(),
            last_pgno: meta.last_pgno as usize,
            last_txnid: meta.txnid as usize,
            max_readers: self.max_readers,
            num_readers: self.lock_region().map_or(0, |l| l.raw_num_readers()),
            map_address: meta.address as usize,
        })
    }

    /// Number of pages on the free list of the latest snapshot (the
    /// "Free pages" line of `mdb_stat -f`).
    pub fn freelist(&self) -> Result<usize> {
        let txn = self.begin_ro_txn()?;
        let mut cursor = txn.cursor(Dbi::FREE)?;
        let mut pages = 0usize;
        while let Some((_, data)) = cursor.next()? {
            let n = Idl::encoded_len(data).ok_or(MdbError::Corrupted)?;
            pages = usize::try_from(n)
                .ok()
                .and_then(|n| pages.checked_add(n))
                .ok_or(MdbError::Corrupted)?;
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

    /// `mdb_env_get_maxkeysize`: the largest key (and `DUP_SORT` data
    /// item) this environment writes: 511 for LMDB 0.9; for 1.0 it follows
    /// from the page size (`me_nodemax - (NODESIZE + sizeof(MDB_db))`: 1978
    /// on 4 KiB pages, 8122 on 16 KiB, 32698 on 64 KiB).
    pub fn max_key_size(&self) -> usize {
        self.max_key
    }

    /// The data-file format of the environment.
    pub fn format(&self) -> Format {
        self.layout.format
    }

    /// The page size of the database (`ms_psize`).
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// The size of the memory map (`me_mapsize`).
    pub fn map_size(&self) -> usize {
        self.map_size.load(Ordering::SeqCst)
    }

    /// `mdb_env_set_mapsize` on an open environment: map the data file
    /// with `size` bytes (`0` = the size recorded in the newest meta page),
    /// raised to cover all pages in use. The value is persisted by the next
    /// commit (`mdb_env_write_meta` keeps the larger of the old and the new
    /// size).
    ///
    /// C requires that no transaction is active. Here read transactions may
    /// be live: they keep using the map they started with. Like C, this
    /// fails with `EINVAL` while this process has a write transaction
    /// active. Also the way to adopt a larger size after another process
    /// grew the database (`MDB_MAP_RESIZED`).
    pub fn set_map_size(&self, size: usize) -> Result<()> {
        // C: `if (env->me_txn) return EINVAL;`
        let mut writer = match self.writer.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(Error::einval()),
        };
        // The next write transaction reads its meta page again (this does
        // not write one, so `last_meta` would still be right; reading is
        // the simple rule after a change of the map).
        writer.last_meta = None;
        let meta = self.pick_meta()?;
        let mut size = if size == 0 {
            meta.map_size as usize
        } else {
            size
        };
        // Silently round up to minimum if the size is too small.
        let min_size = (meta.last_pgno as usize)
            .checked_add(1)
            .and_then(|n| n.checked_mul(self.page_size))
            .ok_or(MdbError::Invalid)?;
        size = size.max(min_size);
        let random = self.raw_flags() & MDB_NORDAHEAD != 0;
        let map = os::map_data(
            self.data_file.try_clone()?,
            size,
            random,
            self.layout.format,
        )?
        .with_meta_pages(NUM_METAS * self.page_size);
        let mut current = self.map.write().unwrap_or_else(|e| e.into_inner());
        *current = Arc::new(map);
        self.map_size.store(size, Ordering::SeqCst);
        Ok(())
    }

    /// `true` if the environment uses a lock file. `false` with
    /// [`EnvironmentFlags::NO_LOCK`], and for a read-only environment whose
    /// lock file could not be created because the file system is read-only
    /// (`EROFS`, as C allows).
    pub fn has_lock_file(&self) -> bool {
        self.lock_region().is_some()
    }

    /// The number of read transactions of this process that pin a snapshot
    /// in the in-memory reader table of an environment without a lock file
    /// (see [`Environment::has_lock_file`]); `None` with a lock file, whose
    /// table [`Environment::reader_list`] shows.
    ///
    /// Without a lock file the read transactions of this process are
    /// tracked in memory, so that its write transactions never reuse pages
    /// they read; C does not track them (`MDB_NOLOCK` leaves that to the
    /// caller). The table, like the writer lock, is shared by every
    /// `NO_LOCK` handle of the data file in the process (each may be opened
    /// with its own `Environment`), so this counts the read transactions of
    /// all of them. Other processes are not covered: with `NO_LOCK`,
    /// keeping them away while this one writes is the caller's job, as in
    /// C. The
    /// functions that report C's lock-file table ([`Environment::info`],
    /// [`Environment::reader_list`], [`Environment::reader_check`]) report
    /// what C does without one.
    pub fn in_memory_readers(&self) -> Option<usize> {
        self.local_shared().map(|local| local.readers.active())
    }

    /// `mdb_env_get_fd`-like access to the data file.
    pub fn data_file(&self) -> &File {
        &self.data_file
    }

    /// The active reader slots (`mdb_reader_list`). Empty if the
    /// environment has no lock file (`NO_LOCK`; C prints `(no reader
    /// locks)` then, see [`Environment::in_memory_readers`]). Use
    /// [`format_reader_list`](crate::core::format_reader_list) for C's text.
    pub fn reader_list(&self) -> Result<Vec<ReaderInfo>> {
        Ok(self
            .lock_region()
            .map_or_else(Vec::new, |l| l.reader_list()))
    }

    /// `mdb_reader_check`: clear reader slots of dead processes; returns how
    /// many were cleared. 0 without a lock file, as in C (the in-memory
    /// table has no dead readers).
    pub fn reader_check(&self) -> Result<usize> {
        match self.lock_region() {
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

/// `rlmdb_sniff_format`: the data-file format of the environment at `path`
/// (a directory, or with [`NO_SUB_DIR`](EnvironmentFlags::NO_SUB_DIR) the
/// data file), from the magic and version of its first meta page, without
/// opening the environment or touching its lock file. `None` if there is
/// no data file yet or it is empty (opening creates a new environment);
/// `MDB_VERSION_MISMATCH` for the version 2 files of lmdb-js prerelease
/// builds; `MDB_INVALID` for anything else that is not an LMDB data file.
/// The file is not validated beyond that: opening it may still fail.
pub fn sniff_format(path: impl AsRef<Path>, flags: EnvironmentFlags) -> Result<Option<Format>> {
    let no_sub_dir = flags.bits() & MDB_NOSUBDIR != 0;
    let (data_path, _) = file_names(path.as_ref(), no_sub_dir);
    sniff_data_file(&data_path)
}

/// [`sniff_format`] of the data file `data_path`: a missing file is a new
/// environment.
fn sniff_data_file(data_path: &Path) -> Result<Option<Format>> {
    let file = match File::open(data_path) {
        Ok(f) => f,
        Err(e) if os::is_missing(&e, data_path) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = sniff_file(&file);
    // The name may lead to the lock file of an environment open in this
    // process, whose record locks closing it would drop.
    crate::core::registry::dispose(file);
    found
}

/// [`sniff_format`] of the open data file `file`.
fn sniff_file(file: &File) -> Result<Option<Format>> {
    let mut head = [0u8; SNIFF_LEN];
    let n = os::read_at(file, &mut head, 0)?;
    Ok(meta::sniff_format(&head[..n]).map_err(MdbError::from)?)
}

/// The data file `data_path` opened read-only (with the options of the
/// data file of a read-only environment), or `None` if it does not exist.
fn open_existing(data_path: &Path, mode: u32) -> Result<Option<File>> {
    match os::env_file_options(OpenOptions::new().read(true), mode).open(data_path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if os::is_missing(&e, data_path) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The format to open with: the existing file's (`found`), which must be
/// the one `requested` if any (`MDB_VERSION_MISMATCH` otherwise: there is
/// no conversion in place), else the requested one, else
/// [`default_format`]. The defaults never apply to an existing file.
fn choose_format(found: Option<Format>, requested: Option<Format>) -> Result<Format> {
    match (found, requested) {
        (Some(f), Some(r)) if f != r => Err(MdbError::VersionMismatch.into()),
        (Some(f), _) | (None, Some(f)) => Ok(f),
        (None, None) => Ok(default_format()),
    }
}

// ---------------------------------------------------------------------------
// The format of new environments
// ---------------------------------------------------------------------------

/// The environment variable that sets the format of new environments that
/// do not ask for one: `RLMDB_DEFAULT_FORMAT=0.9` or `=1.0`
/// ([`Format::name`]).
///
/// **Experimental**: it may be removed or restricted in a later release
/// (capability `default-format-env` tells whether it is honoured). A stray
/// variable in a service's environment silently creates LMDB 1.0 files
/// that C LMDB 0.9 cannot open, so programs and deployments should prefer
/// [`EnvironmentBuilder::format`] and [`set_default_format`]. It is read at
/// every open of a new environment, also through the C API and by the
/// tools. A value other than `0.9` or `1.0` is ignored, never an error.
pub const DEFAULT_FORMAT_ENV: &str = "RLMDB_DEFAULT_FORMAT";

/// The process default of [`set_default_format`]: 0 = none, else
/// [`format_code`].
static PROCESS_DEFAULT_FORMAT: AtomicU8 = AtomicU8::new(0);

/// The value [`PROCESS_DEFAULT_FORMAT`] stores for `format`.
const fn format_code(format: Format) -> u8 {
    match format {
        Format::V09 => 9,
        Format::V10 => 10,
    }
}

/// `rlmdb_set_default_format`: the format of new environments of this
/// process that do not ask for one with [`EnvironmentBuilder::format`];
/// `None` removes the process default again. It takes precedence over the
/// experimental [`DEFAULT_FORMAT_ENV`] variable.
///
/// An existing data file always keeps its format; the default only decides
/// what a new environment is created as. The setting applies to every
/// environment opened afterwards, by any thread, and to the C API.
pub fn set_default_format(format: Option<Format>) {
    PROCESS_DEFAULT_FORMAT.store(format.map_or(0, format_code), Ordering::SeqCst);
}

/// The process default of [`set_default_format`], if one is set.
pub fn process_default_format() -> Option<Format> {
    match PROCESS_DEFAULT_FORMAT.load(Ordering::SeqCst) {
        9 => Some(Format::V09),
        10 => Some(Format::V10),
        _ => None,
    }
}

/// The format a new environment gets when it does not ask for one: the
/// process default ([`set_default_format`]), else the experimental
/// [`DEFAULT_FORMAT_ENV`] variable if it names a format, else
/// [`Format::V09`], so that new files stay readable by every C LMDB 0.9.
pub fn default_format() -> Format {
    process_default_format()
        .or_else(env_default_format)
        .unwrap_or(Format::V09)
}

/// The format [`DEFAULT_FORMAT_ENV`] names, read now; `None` if it is unset
/// or names none.
fn env_default_format() -> Option<Format> {
    std::env::var_os(DEFAULT_FORMAT_ENV).and_then(|v| Format::from_name(v.as_encoded_bytes()))
}

/// The checks of 1.0's `mdb_env_set_pagesize`: a power of two from 256 to
/// `MAX_PAGESIZE`.
fn valid_set_pagesize(size: usize, layout: &Layout) -> bool {
    size.is_power_of_two() && (MIN_PAGESIZE..=layout.max_pagesize).contains(&size)
}

/// Refuses an existing 1.0 environment that uses what this crate does not
/// implement, with 1.0's error codes (`mdb_env_open2`): encryption
/// (`MDB_ENCRYPT` in meta page 0's `mm_flags`, `MDB_ENV_ENCRYPTION`) and
/// page checksums (C 1.0 records the checksum size in the last two bytes of
/// page 0; `MDB_BAD_CHECKSUM` if it is not 0, where C would compare it with
/// its own checksum size).
fn check_v10_features(file: &File, page_size: usize, meta0_flags: u16) -> Result<()> {
    if u32::from(meta0_flags) & MDB_ENCRYPT != 0 {
        return Err(MdbError::EnvEncryption.into());
    }
    let mut sumsize = [0u8; 2];
    if os::read_at(file, &mut sumsize, page_size as u64 - 2)? != sumsize.len() {
        return Err(MdbError::Invalid.into());
    }
    if u16::from_ne_bytes(sumsize) != 0 {
        return Err(MdbError::BadChecksum.into());
    }
    Ok(())
}

/// `mdb_env_read_header`: read both meta pages of the format `layout`
/// describes with `pread` and return the newer valid one and the one of page
/// 0, or `None` for an empty file (a new environment).
fn read_header(file: &File, layout: &Layout) -> Result<Option<(Meta, Meta)>> {
    // sizeof(MDB_metabuf): the page header and the meta. The buffer is
    // sized for the largest header (1.0's).
    let size = layout.meta_offset() + META_SIZE;
    let mut storage = [0u8; V10.page_hdr + META_SIZE];
    let buf = storage.get_mut(..size).ok_or(MdbError::Invalid)?;
    let mut best: Option<Meta> = None;
    let mut first: Option<Meta> = None;
    let mut off = 0u64;
    for _ in 0..NUM_METAS {
        let n = os::read_at(file, buf, off)?;
        if n != size {
            if n == 0 && off == 0 {
                return Ok(None);
            }
            return Err(MdbError::Invalid.into());
        }
        let meta = Meta::read_page_with_layout(buf, layout).map_err(MdbError::from)?;
        first.get_or_insert(meta);
        match best {
            Some(b) if meta.txnid <= b.txnid => {}
            _ => best = Some(meta),
        }
        // As C: the second meta page is at the first one's page size.
        off += u64::from(best.map_or(0, |b| b.page_size()));
    }
    Ok(best.zip(first))
}
