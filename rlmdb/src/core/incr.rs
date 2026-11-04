//! Incremental backups of LMDB 1.0 environments: [`Environment::incr_dump`]
//! and [`Environment::incr_dump_to_path`] (`mdb_env_incr_dumpfd`, mdb.c 1.0
//! line 11825, and `mdb_env_incr_dump`, line 11958) and
//! [`Environment::incr_load`] (`mdb_env_incr_loadfd`, line 11978).
//!
//! # The dump
//!
//! An incremental dump is a run of page records without any other framing:
//! each is a page, or an overflow run, as the data file holds it, starting
//! with its page header, whose page number (`mp_pgno`) is where the load
//! writes it. First come the meta pages whose `mm_txnid` is above the given
//! txnid (page 0 before page 1), then pages from 2 to the snapshot's last
//! page in file order. From txnid 0 the dump holds every page that has been
//! written (a full backup; `lmdb.h` asks for a txnid above zero, `mdb.c`
//! takes 0); from the txnid of an earlier backup, the pages written since,
//! which turn a copy of the file as it was then into the file as it is.
//! LMDB 0.9 pages have no `mp_txnid`, so a 0.9 environment cannot be dumped
//! (`ENOTSUP`, as for the load).
//!
//! The meta pages are copied while holding the writer lock, after starting
//! the read transaction that pins the snapshot, as for a plain copy
//! (`Environment::begin_with_meta_pages`); the pages follow without the
//! lock. They are read with `pread`, not borrowed from the map, because the
//! dump reads free pages too, which writers may change while it reads them.
//!
//! Every record is the file's own bytes at their own position, so applying
//! one is never wrong: it makes those bytes of the target the source's.
//! The dump looks at every page position `pg` of the snapshot (up to the
//! end of the file: past it nothing was ever written) and takes the page
//! there for a record if
//!
//! * its header names it: `mp_pgno == pg`;
//! * `txnid < mp_txnid <= snapshot`: written after `txnid` and part of the
//!   snapshot this dump pins (a larger `mp_txnid` is a free page that a
//!   writer rewrote since, or a page of an aborted transaction);
//! * for an overflow header, its run of `mp_pages` pages is not empty and
//!   ends by the snapshot's last page and the end of the file;
//!
//! and the record reaches past what earlier records hold (it is then
//! written whole, overlapping them where it begins inside one). The meta
//! pages follow the same bound (`mm_txnid <= snapshot`).
//!
//! Every page of the snapshot written after `txnid` is in the dump: C 1.0
//! and rlmdb stamp each page they write with the writing transaction's
//! txnid, so such a page carries a txnid in the range and its own number,
//! and the body of an overflow run comes with its header. Records for
//! other pages (free pages, stale headers of freed runs, by a remote
//! chance a page of a value that looks like a header naming its own
//! position) only add bytes that the source's file holds there, which the
//! snapshot does not use.
//!
//! C 1.0 steps from header to header instead, one page or `mp_pages`, from
//! whatever free pages hold (`KNOWN_C_BUGS.md` row 72): after an overflow
//! run is freed and partly reused it takes the old value's bytes for
//! headers, dumps them under the page numbers they give, steps over live
//! pages, or fails (`EFAULT`, an endless loop). Where no such page is
//! involved, and no page of a transaction after the snapshot, the two dumps
//! are the same bytes.
//!
//! # The load
//!
//! [`Environment::incr_load`] reads such page records (a page header, then
//! the rest of the page, or of the run of `mp_pages` pages) up to the end of
//! the input and writes each at `mp_pgno * page size` into the data file,
//! as C does through the environment's descriptor, but not in the stream's
//! order: C writes the meta pages first, which come first in a dump, and
//! does not sync, so a load that stops early leaves meta pages naming
//! pages never written. Here a dump's meta pages are held back until its
//! data pages are written and synced, then written and synced in turn, as
//! a commit writes its meta page (a meta page that follows data pages
//! begins the next dump of a stream of several, whose meta pages are then
//! held back in the same way). The finished file is the same bytes. An
//! interrupted load leaves the base's meta pages, and the base snapshot
//! whole only if the dump spans at most two commits after it: from the
//! third on, a commit may reuse pages the base tree still names, and the
//! dump holds their new bytes. Loading the dump again completes the load
//! (each record is written at its own position). A stream cut at a record
//! boundary cannot be told from a whole one: C's format has neither a
//! count nor a checksum. C checks nothing, and
//! `lmdb.h` asks that no other task use the environment meanwhile. Here
//! the load holds the writer lock, makes sure no transaction of this
//! process can see the file change under it (see the method), and
//! afterwards publishes the loaded snapshot as the current one in the lock
//! file (`mti_txnid`) or, without one, to the other handles of the file in
//! this process: the next transaction, here or in another process, reads
//! it. C leaves `mti_txnid` as it was, so the loading handle's next
//! transaction mixes the old txnid with the new meta pages
//! (`KNOWN_C_BUGS.md` row 65).
//!
//! The input is checked where C fails to (`KNOWN_C_BUGS.md` row 66): the
//! end of the input inside a page is an error (C stops at a short header,
//! reporting success, and loops for ever inside a page), so are read errors
//! (C ignores them in a header); a record with `mp_pages` 0 (C overruns its
//! buffer) or one that would end past the larger of the map size, the
//! file's size and the map size of the meta pages loaded so far is
//! `MDB_CORRUPTED` (C writes it wherever its header says, also exabytes
//! into the file).

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::sync::{Arc, MutexGuard, RwLockWriteGuard};

use crate::consts::NUM_METAS;
use crate::core::copy::{read_exact_at, AlignedBuf};
use crate::core::env::{Environment, WriterState};
use crate::core::reader::{LocalReadersGuard, LockRegion};
use crate::core::txn::Transaction;
use crate::error::{Error, MdbError, Result};
use crate::format::layout::{Format, Layout, META_TXNID, V10};
use crate::format::meta::{pick_newest, Meta};
use crate::format::page::{P_META, P_OVERFLOW};
use crate::format::{get_u16, get_u32, get_u64};
use crate::os::{self, DataMap};

/// Read size of the dump's walk (C reads its map; the size here only
/// affects speed).
const DUMP_CHUNK: usize = 4 << 20;

/// Write size of the load for the rest of an overflow run (C reads a whole
/// run into memory first; this bounds the memory a crafted `mp_pages`
/// takes).
const LOAD_CHUNK: usize = 1 << 20;

/// Read-ahead of the load's input (C reads the header and the rest of each
/// page with separate `read`s).
const LOAD_BUFFER: usize = 1 << 16;

impl Environment {
    /// `mdb_env_incr_dumpfd`: write an incremental dump of the environment
    /// since transaction `txnid` to `out`: the meta pages and the pages
    /// written by transactions after `txnid`, as they are in the data file
    /// (see the module documentation of `incr` for which pages, and where
    /// the dump differs from C 1.0's). `txnid` is normally the txnid of an
    /// earlier backup (the `last_txnid` of [`Environment::info`] then, or
    /// of the copy that the chain starts from); 0 dumps every page
    /// (`lmdb.h` asks for more than 0, C takes it).
    /// [`Environment::incr_load`], or C 1.0's `mdb_env_incr_loadfd`,
    /// applies the dump to a copy of the file at `txnid`.
    ///
    /// Writers may continue while the dump runs; it shows the snapshot
    /// current when it began, which it pins with a read transaction (so a
    /// long dump keeps freed pages from being reused, as any reader does).
    /// Before that it waits for a write transaction that is active, as C
    /// does with a lock file (without one, for the write transactions of
    /// this process's `NO_LOCK` handles of the file), so a thread must not
    /// call it while it has a write transaction of the environment open.
    ///
    /// LMDB 1.0 environments only: `ENOTSUP` for an LMDB 0.9 one, whose
    /// pages do not record the transaction that wrote them. A write to
    /// `out` that writes nothing is `MDB_SHORT_WRITE`, as in C. `out` is
    /// flushed at the end.
    pub fn incr_dump(&self, mut out: impl Write, txnid: u64) -> Result<()> {
        if self.format() != Format::V10 {
            return Err(Error::errno(libc::ENOTSUP));
        }
        let ps = self.page_size();
        let l = self.layout();
        let mut buf = AlignedBuf::new(DUMP_CHUNK.max(2 * ps));
        // Pins the snapshot until the walk is done.
        let txn = self.begin_with_meta_pages(&mut buf.as_mut()[..2 * ps])?;
        let snapshot = txn.id();
        for i in 0..NUM_METAS {
            let page = &buf.as_mut()[i * ps..(i + 1) * ps];
            let meta_txnid = get_u64(page, l.meta_offset() + META_TXNID).unwrap_or(0);
            // A newer meta page (a commit whose meta page write failed, or
            // without a lock file one of another process) names pages the
            // dump does not hold.
            if txnid < meta_txnid && meta_txnid <= snapshot {
                write_all(&mut out, page)?;
            }
        }
        let range = Range {
            since: txnid,
            snapshot,
        };
        let mut file = FileView::new(self.data_file(), ps, buf)?;
        walk(l, &mut file, txn.core().next_pgno, range, &mut out)?;
        drop(txn);
        out.flush()?;
        Ok(())
    }

    /// `mdb_env_incr_dump`: [`Environment::incr_dump`] into a new file
    /// `path` (the file itself: C sets `MDB_NOSUBDIR` for it, in the
    /// environment's flags, which rlmdb leaves alone, `KNOWN_C_BUGS.md`
    /// row 74). The file must not exist yet: it is created as a copy's is
    /// ([`Environment::copy_to_path`]: `O_EXCL`, mode `0666` before the
    /// umask, not cached), and stays, with what was written, if the dump
    /// fails. `ENOTSUP` for an LMDB 0.9 environment, before the file is
    /// created.
    pub fn incr_dump_to_path(&self, path: impl AsRef<Path>, txnid: u64) -> Result<()> {
        if self.format() != Format::V10 {
            return Err(Error::errno(libc::ENOTSUP));
        }
        let nocache = self.page_size() >= os::os_page_size();
        let file = os::open_copy_target(path.as_ref(), nocache)?;
        let rc = self.incr_dump(&file, txnid);
        let closed = os::close(file);
        rc?;
        Ok(closed?)
    }

    /// `mdb_env_incr_loadfd`: apply an incremental dump (or several, back
    /// to back) read from `input` to the data file: each page is written
    /// where its header says, up to the end of the input. Applied to a
    /// copy of the file at the dump's `txnid` (a plain copy, or the result
    /// of the earlier loads of a chain), it makes the file the dumped
    /// environment's as it was when the dump began, unless the dump is C
    /// 1.0's and holds stale pages its walk took for headers
    /// (`KNOWN_C_BUGS.md` row 72); nothing checks that the dump fits the
    /// file (C checks nothing either).
    ///
    /// Each dump's meta pages are written after its data pages, each step
    /// synced, whatever [`NO_SYNC`](crate::EnvironmentFlags::NO_SYNC) says
    /// (C writes the meta pages first and does not sync; row 65). An
    /// interrupted load (an error, a crash) leaves the base's meta pages,
    /// and its snapshot whole only if the dump spans at most two commits
    /// after the base: a third may reuse pages the base tree still names.
    /// Loading the same dump again completes it. A stream cut at a record
    /// boundary cannot be told from a whole one (C's format has neither a
    /// count nor a checksum) and loads as if it were. So load onto a copy
    /// that can be thrown away, or keep a copy of the base (or at least
    /// have it synced) until the load has succeeded.
    ///
    /// `lmdb.h` asks that no other task access the environment while the
    /// load runs, and that it be opened with `MDB_WRITEMAP`, which neither
    /// C's load nor this one needs: both write through the file, with the
    /// flag or without (it does not change how rlmdb writes, see
    /// [`WRITE_MAP`](crate::EnvironmentFlags::WRITE_MAP)). Here:
    ///
    /// * `&mut self`: no transaction of this handle can be open (they
    ///   borrow it), so this does not compile:
    ///
    ///   ```compile_fail
    ///   # fn f(mut env: rlmdb::Environment) -> rlmdb::Result<()> {
    ///   let txn = env.begin_ro_txn()?;
    ///   env.incr_load(std::io::empty())?;
    ///   drop(txn);
    ///   # Ok(())
    ///   # }
    ///   ```
    ///
    ///   Other handles of the file in this process (without a lock file,
    ///   see [`Environment::in_memory_readers`]) are checked when the load
    ///   begins: it fails with `EBUSY` while one of them has a read
    ///   transaction, also a reset one, or a write transaction, and their
    ///   new transactions wait until it is done.
    /// * With a lock file, it fails with `EBUSY` while a reader of another
    ///   live process pins a snapshot, and waits for a write transaction of
    ///   another process to end (it holds the writer lock until it is
    ///   done). C checks neither; processes that open the environment or
    ///   begin a transaction meanwhile are still the caller's business, as
    ///   in C.
    ///
    /// After the load, the newest of the two meta pages in the file is the
    /// current snapshot: it is published in the lock file (`mti_txnid`)
    /// where C publishes nothing (`KNOWN_C_BUGS.md` row 65), so the next
    /// transaction of every process reads it. One that needs a larger map
    /// fails with `MDB_MAP_RESIZED`; [`Environment::set_map_size`] with 0
    /// adopts the loaded map size.
    ///
    /// Errors: `ENOTSUP` for an LMDB 0.9 environment; the error of the
    /// first write for a read-only one, as in C (`EBADF` on Unix); the
    /// input's read errors, and `EIO` (an [`io::ErrorKind::UnexpectedEof`])
    /// if it ends inside a page; `MDB_CORRUPTED` for a record with an
    /// overflow header of 0 pages, or one that would end past the larger of
    /// the map size, the file's size and the map size of the meta pages
    /// loaded so far. The pages before stay written (as in C), and the
    /// meta pages are published as they then are.
    pub fn incr_load(&mut self, input: impl Read) -> Result<()> {
        self.incr_load_shared(input)
    }

    /// [`Environment::incr_load`] through a shared reference, for the C API
    /// (`mdb_env_incr_loadfd`), whose handles cannot borrow the environment
    /// exclusively: also fails with `EBUSY` if this handle has a write
    /// transaction or a read transaction that pins a snapshot (a reset one
    /// may stay: it renews after the load), and makes its new transactions
    /// wait until the load is done. Not part of the API.
    #[doc(hidden)]
    pub fn incr_load_shared(&self, input: impl Read) -> Result<()> {
        if self.format() != Format::V10 {
            return Err(Error::errno(libc::ENOTSUP));
        }
        let mut excl = Exclusive::take(self)?;
        let mut input = BufReader::with_capacity(LOAD_BUFFER, input);
        let mut written = false;
        let rc = load(self, &excl.map, &mut input, &mut written);
        if written {
            excl.publish(self);
        }
        rc
    }
}

/// Writes all of `buf` to `out`, as C's `DO_WRITE` loops do for the data
/// pages (C writes the meta pages once, short or not, `KNOWN_C_BUGS.md`
/// row 73): a write that takes nothing is `MDB_SHORT_WRITE` (C 1.0's code
/// for it).
fn write_all(out: &mut impl Write, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        match out.write(buf) {
            Ok(0) => return Err(MdbError::ShortWrite.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The data file as the dump reads it: a window of it read with `pread`.
struct FileView<'f> {
    file: &'f File,
    ps: usize,
    buf: AlignedBuf,
    /// The file offset of `buf[0]`.
    start: u64,
    /// How many bytes of `buf` hold the file's.
    len: usize,
    /// The file's size when the dump began (its snapshot is all below).
    size: u64,
}

impl<'f> FileView<'f> {
    fn new(file: &'f File, ps: usize, buf: AlignedBuf) -> Result<FileView<'f>> {
        Ok(FileView {
            file,
            ps,
            buf,
            start: 0,
            len: 0,
            size: os::file_size(file)?,
        })
    }

    /// The number of whole pages of the file.
    fn pages(&self) -> u64 {
        self.size / self.ps as u64
    }

    /// `n` bytes of the file at `pos` (`pos + n` at most the file's size,
    /// `n` at most the buffer's size).
    fn bytes(&mut self, pos: u64, n: usize) -> Result<&[u8]> {
        let held = self.start + self.len as u64;
        if pos < self.start || pos + n as u64 > held {
            let len = (self.buf.len() as u64).min(self.size - pos) as usize;
            read_exact_at(self.file, &mut self.buf.as_mut()[..len], pos)?;
            self.start = pos;
            self.len = len;
        }
        let off = (pos - self.start) as usize;
        Ok(&self.buf.as_mut()[off..off + n])
    }

    /// Writes the file's bytes from `pos` to `end` (within the file) to
    /// `out`.
    fn copy(&mut self, mut pos: u64, end: u64, out: &mut impl Write) -> Result<()> {
        while pos < end {
            let n = (end - pos).min(self.buf.len() as u64) as usize;
            write_all(out, self.bytes(pos, n)?)?;
            pos += n as u64;
        }
        Ok(())
    }
}

/// The transactions whose pages a dump holds: written after `since`, at
/// most `snapshot` (the snapshot the dump pins).
#[derive(Debug, Clone, Copy)]
struct Range {
    since: u64,
    snapshot: u64,
}

/// The page walk of the dump (see the module documentation): every page
/// position from 2 to `next_pgno - 1` (or the end of the file), writing to
/// `out` the records [`record_pages`] finds that reach past the records
/// before.
fn walk(
    l: &Layout,
    file: &mut FileView<'_>,
    next_pgno: u64,
    range: Range,
    out: &mut impl Write,
) -> Result<()> {
    let ps = file.ps as u64;
    // Pages past the end of the file were never written: no page of the
    // snapshot is there, and C reads them as zeros, or dies of SIGBUS.
    let last = next_pgno.min(file.pages());
    // The first page the records so far do not hold.
    let mut covered = NUM_METAS as u64;
    for pg in NUM_METAS as u64..last {
        let mut hdr = [0u8; V10.page_hdr];
        hdr[..l.page_hdr].copy_from_slice(file.bytes(pg * ps, l.page_hdr)?);
        let Some(pages) = record_pages(l, &hdr, pg, last, range) else {
            continue;
        };
        let end = pg + pages;
        if end > covered {
            file.copy(pg * ps, end * ps, out)?;
            covered = end;
        }
    }
    Ok(())
}

/// The length in pages of the record that the page at `pg` with header
/// `hdr` starts, if it is one: its header names it (`mp_pgno`), its
/// `mp_txnid` is in `range`, and an overflow run is not empty and ends by
/// page `last` (the snapshot's end or the file's). `None` for any other
/// page: a free page that holds something else (a page of a freed run's
/// value, a header from a later transaction), or one of the snapshot that
/// was not written in the range.
fn record_pages(l: &Layout, hdr: &[u8], pg: u64, last: u64, range: Range) -> Option<u64> {
    if get_u64(hdr, 0)? != pg {
        return None;
    }
    let txnid = get_u64(hdr, l.txnid_offset()?)?;
    if txnid <= range.since || txnid > range.snapshot {
        return None;
    }
    if get_u16(hdr, l.flags_offset())? & P_OVERFLOW == 0 {
        return Some(1);
    }
    let pages = u64::from(get_u32(hdr, l.pages_offset())?);
    (pages >= 1 && pg.checked_add(pages)? <= last).then_some(pages)
}

/// What the load holds while it runs (see [`Environment::incr_load`]).
/// Field order is drop order.
struct Exclusive<'e> {
    /// Without a lock file: the reader table of the file's handles in this
    /// process, locked with no entry owned.
    _local_readers: Option<LocalReadersGuard<'e>>,
    /// This handle's map, so that none of its transactions begins.
    map: RwLockWriteGuard<'e, Arc<DataMap>>,
    /// The writer lock every process (or, without a lock file, every
    /// handle of the file in this process) takes.
    _writer: SharedWriter<'e>,
    /// This handle's writer state (no write transaction of it runs).
    state: MutexGuard<'e, WriterState>,
}

/// The writer lock of the lock file, or the in-process one without a lock
/// file; released on drop.
enum SharedWriter<'e> {
    Lock(&'e LockRegion),
    Local(#[allow(dead_code)] MutexGuard<'e, ()>),
}

impl Drop for SharedWriter<'_> {
    fn drop(&mut self) {
        if let SharedWriter::Lock(lock) = self {
            lock.unlock_writer();
        }
    }
}

impl<'e> Exclusive<'e> {
    /// Takes the locks of a load, or fails with `EBUSY` (see
    /// [`Environment::incr_load_shared`]).
    fn take(env: &'e Environment) -> Result<Exclusive<'e>> {
        let busy = || Error::errno(libc::EBUSY);
        // A write transaction of this handle holds it; waiting for it on
        // its own thread would never end.
        let state = env.try_lock_writer_state().ok_or_else(busy)?;
        let writer = match (env.lock_region(), env.local_shared()) {
            (Some(lock), _) => {
                lock.lock_writer(|| env.pick_meta().map(|m| m.txnid))?;
                SharedWriter::Lock(lock)
            }
            // Held by a write transaction of another handle, maybe on this
            // thread.
            (None, Some(local)) => SharedWriter::Local(local.try_lock_writer().ok_or_else(busy)?),
            (None, None) => return Err(busy()),
        };
        // From here on no transaction of this handle begins or reads a
        // meta page, so one that has not published a snapshot yet reads
        // the loaded one.
        let map = env.lock_map();
        let local_readers = match (env.lock_region(), env.local_shared()) {
            (Some(lock), _) => {
                let (own, others) = lock.pinning_readers();
                if own + others > 0 {
                    return Err(busy());
                }
                None
            }
            (None, Some(local)) => Some(local.readers.lock_if_unowned().ok_or_else(busy)?),
            (None, None) => None,
        };
        Ok(Exclusive {
            _local_readers: local_readers,
            map,
            _writer: writer,
            state,
        })
    }

    /// Publishes the newest meta page in the file as the current snapshot
    /// (if both meta pages are valid; otherwise the next transaction fails
    /// on them), and forgets the oldest reader this handle's writer saw.
    fn publish(&mut self, env: &Environment) {
        if let Ok(newest) = newest_meta(env) {
            match (env.lock_region(), env.local_shared()) {
                (Some(lock), _) => lock.set_txnid(newest.txnid),
                (None, Some(local)) => local.set_txnid(newest.txnid),
                (None, None) => {}
            }
        }
        // It only grows otherwise, and the loaded txnids may be lower.
        self.state.pg_oldest = 0;
    }
}

/// The newer of the two meta pages of the data file, read with `pread` as
/// an open reads them; an error if either is not a valid meta page.
fn newest_meta(env: &Environment) -> Result<Meta> {
    let l = env.layout();
    let ps = env.page_size();
    let mut page = vec![0u8; l.meta_offset() + crate::format::layout::META_SIZE];
    let mut metas = [Meta::default(); NUM_METAS];
    for (i, meta) in metas.iter_mut().enumerate() {
        read_exact_at(env.data_file(), &mut page, (i * ps) as u64)?;
        *meta = Meta::read_page_with_layout(&page, l).map_err(MdbError::from)?;
    }
    Ok(metas[pick_newest(&metas)])
}

/// The record loop of `mdb_env_incr_loadfd` (see the module documentation
/// for what is checked, and for the order of the writes). Sets `written`
/// once a page was written.
fn load(env: &Environment, map: &DataMap, input: &mut impl Read, written: &mut bool) -> Result<()> {
    let l = env.layout();
    let ps = env.page_size();
    let file = env.data_file();
    let corrupted = || Error::from(MdbError::Corrupted);
    let mut limit = (env.map_size() as u64).max(os::file_size(file)?);
    let mut buf = vec![0u8; LOAD_CHUNK.max(ps)];
    let mut metas = HeldMetas::default();
    loop {
        let hdr = l.page_hdr;
        match read_full(input, &mut buf[..hdr])? {
            0 => break,
            n if n < hdr => return Err(truncated()),
            _ => {}
        }
        let pgno = get_u64(&buf, 0).unwrap_or(0);
        let flags = get_u16(&buf, l.flags_offset()).unwrap_or(0);
        let pages = if flags & P_OVERFLOW != 0 {
            u64::from(get_u32(&buf, l.pages_offset()).unwrap_or(0))
        } else {
            1
        };
        // No run is empty, and none starts at a meta page.
        if pages == 0 || (pgno < NUM_METAS as u64 && pages != 1) {
            return Err(corrupted());
        }
        let pos = pgno.checked_mul(ps as u64).ok_or_else(corrupted)?;
        let end = pages
            .checked_mul(ps as u64)
            .and_then(|n| pos.checked_add(n))
            .ok_or_else(corrupted)?;
        if end > limit {
            return Err(corrupted());
        }
        read_exact(input, &mut buf[hdr..ps])?;
        if pgno < NUM_METAS as u64 {
            // The pages of a dump whose meta pages raise the map size lie
            // past the map and the file; the meta pages come first.
            if flags & P_META != 0 {
                if let Ok(meta) = Meta::read_page_with_layout(&buf[..ps], l) {
                    limit = limit.max(meta.map_size);
                }
            }
            // A meta page after data pages begins the next dump of the
            // stream: the one before is complete.
            if metas.data_since {
                metas.commit(file, map, ps)?;
            }
            metas.pages[pgno as usize] = Some(buf[..ps].to_vec());
            continue;
        }
        write_page_bytes(file, map, &buf[..ps], pos)?;
        *written = true;
        metas.data_since = true;
        let mut at = pos + ps as u64;
        while at < end {
            let n = (end - at).min(buf.len() as u64) as usize;
            read_exact(input, &mut buf[..n])?;
            write_page_bytes(file, map, &buf[..n], at)?;
            at += n as u64;
        }
    }
    if metas.commit(file, map, ps)? {
        *written = true;
    }
    Ok(())
}

/// The meta pages of the dump being loaded, held back until its data pages
/// are written (`KNOWN_C_BUGS.md` row 65).
#[derive(Default)]
struct HeldMetas {
    /// Meta pages 0 and 1, as the dump holds them.
    pages: [Option<Vec<u8>>; NUM_METAS],
    /// Whether data pages were written since the meta pages arrived.
    data_since: bool,
}

impl HeldMetas {
    /// Writes the held meta pages as a commit does its meta page: after a
    /// sync of the data pages written before them, then synced in turn
    /// (whatever `NO_SYNC` says). Returns whether there were any.
    fn commit(&mut self, file: &File, map: &DataMap, ps: usize) -> Result<bool> {
        self.data_since = false;
        if self.pages.iter().all(Option::is_none) {
            return Ok(false);
        }
        os::fdatasync(file)?;
        for (i, page) in self.pages.iter_mut().enumerate() {
            if let Some(page) = page.take() {
                write_page_bytes(file, map, &page, (i * ps) as u64)?;
            }
        }
        os::fdatasync(file)?;
        Ok(true)
    }
}

/// `pwrite`s `bytes` at `pos` of the data file and drops the map's copy of
/// them where it does not see such writes by itself (OpenBSD,
/// `MDB_Invalidate`; C's load does not, its process normally exits).
fn write_page_bytes(file: &File, map: &DataMap, bytes: &[u8], pos: u64) -> Result<()> {
    os::write_all_at(file, bytes, pos)?;
    if os::MAP_NEEDS_INVALIDATE {
        map.invalidate(pos, bytes.len());
    }
    Ok(())
}

/// Reads until `buf` is full or the input ends; returns how much was read.
fn read_full(input: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match input.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(done)
}

/// Fills `buf` from the input; its end is [`truncated`].
fn read_exact(input: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    if read_full(input, buf)? < buf.len() {
        return Err(truncated());
    }
    Ok(())
}

/// The input of a load ends inside a page (`EIO` to C callers).
fn truncated() -> Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "incremental dump ends inside a page",
    )
    .into()
}
