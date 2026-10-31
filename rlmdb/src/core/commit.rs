//! Committing a write transaction: `mdb_txn_commit` (mdb.c line 3791),
//! `mdb_page_flush` (line 3394) and `mdb_env_write_meta` (line 3935).
//!
//! The order matters for crash safety and for concurrent readers, and is
//! C's:
//!
//! 1. store the `MDB_db` record of every named DB changed in this
//!    transaction in the main DB (`F_SUBDATA` nodes);
//! 2. save the free list (`RwTxn::freelist_save`);
//! 3. write all dirty pages with `pwritev`, in page order, up to
//!    `MDB_COMMIT_PAGES` (64) contiguous pages per call;
//! 4. `fdatasync` the data file (`F_FULLFSYNC` on macOS), unless `NO_SYNC`;
//! 5. write the tail of the meta page `txnid % 2` (from `mm_mapsize` on)
//!    through the `O_DSYNC` descriptor (unless `NO_SYNC`/`NO_META_SYNC`);
//!    only now does the new snapshot exist on disk;
//! 6. publish the txnid in the lock file, so new readers pick it up
//!    (without one, in the state the file's `NO_LOCK` handles share).
//!
//! A transaction without dirty pages commits without writing anything
//! (`MDB_END_EMPTY_COMMIT`).

use crate::consts::*;
use crate::core::cursor_put::PutData;
use crate::core::dbi::DB_DIRTY;
use crate::core::dirty::PageSlot;
use crate::core::page_ops;
use crate::core::txn_rw::{RwTxn, TXN_DIRTY, TXN_ERROR, TXN_FINISHED, TXN_HAS_CHILD, TXN_SPILLS};
use crate::core::wcursor::CRef;
use crate::error::{MdbError, Result};
use crate::format::layout::{Layout, META_MAPSIZE, META_SIZE};
use crate::format::meta::Meta;
use crate::format::node::F_SUBDATA;
use crate::format::page::{P_DIRTY, P_KEEP, P_LOOSE, P_OVERFLOW};
use crate::os;

/// `MDB_COMMIT_PAGES`: max number of pages to write in one `pwritev` call
/// (below `IOV_MAX` on every supported platform).
const COMMIT_PAGES: usize = 64;

/// `MAX_WRITE`: max bytes to write in one call.
const MAX_WRITE: usize = 0x4000_0000;

impl<'env> RwTxn<'env> {
    /// `_mdb_txn_commit` for a top-level transaction, up to (not including)
    /// `mdb_txn_end`.
    pub(crate) fn commit0(&mut self) -> Result<()> {
        // (TXN_HAS_CHILD is still set if a nested transaction was leaked with
        // `mem::forget`: its pages are with the child, so nothing may be
        // written.)
        if self.w.flags & (TXN_FINISHED | TXN_ERROR | TXN_HAS_CHILD) != 0 {
            return Err(MdbError::BadTxn.into());
        }
        // mdb_cursors_close: no user cursor can be alive (they borrow the
        // transaction), but handle-based ones may be.
        self.w.cursors.clear();

        if self.core.dirty.is_empty() && self.w.flags & (TXN_DIRTY | TXN_SPILLS) == 0 {
            return Ok(());
        }

        self.update_db_records()?;
        self.freelist_save()?;
        self.w.pghead = None;

        self.page_flush()?;
        self.core.env.sync(false)?;
        self.write_meta()
    }

    /// Update DB root pointers: write the record of each named DB this
    /// transaction changed into the main DB.
    fn update_db_records(&mut self) -> Result<()> {
        let n = self.core.dbs.get_mut().len();
        if n <= CORE_DBS as usize {
            return Ok(());
        }
        // C uses an untracked cursor; tracking it is equivalent here and
        // keeps it consistent after nested splits (KNOWN_C_BUGS.md #11).
        let slot = self.open_slot(MAIN_DBI as usize, true);
        let rc = self.update_db_records_with(CRef::main(slot), n);
        self.close_slot(slot);
        rc
    }

    fn update_db_records_with(&mut self, mc: CRef, n: usize) -> Result<()> {
        for i in CORE_DBS as usize..n {
            let e = self.core.dbs.get_mut()[i];
            if e.state & DB_DIRTY == 0 {
                continue;
            }
            if e.seq != self.core.env.dbis.seq_of(i) {
                return Err(MdbError::BadDbi.into());
            }
            let name = self.core.env.dbis.name_of(i).ok_or(MdbError::BadDbi)?;
            let rec: [u8; 48] = e.db.to_bytes();
            self.put_at(mc, &name, PutData::bytes(&rec), u32::from(F_SUBDATA))?;
        }
        Ok(())
    }

    /// `mdb_page_flush(txn, 0)` at commit: write every dirty page to the
    /// data file, clearing `P_DIRTY` in each first. Loose pages (and
    /// `P_KEEP` ones, which lose the mark) stay dirty and in the list;
    /// written pages leave it.
    ///
    /// As in C, one pass over the list prepares the pages, then they are
    /// written; each page is reached through its slot, without a search.
    pub(crate) fn page_flush(&mut self) -> Result<()> {
        let l = self.layout();
        let ps = self.psize();
        let txnid = self.core.txnid;
        let mut runs = std::mem::take(&mut self.w.flush);
        runs.clear();
        self.core.dirty.for_each_mut(|pgno, slot, page| {
            let flags = page_ops::flags(l, page);
            // Don't flush this page yet.
            if flags & (P_LOOSE | P_KEEP) != 0 {
                page_ops::set_flags(l, page, flags & !P_KEEP);
            } else {
                runs.push((pgno, slot, prepare_page(l, ps, page, flags, txnid)));
            }
        });
        let rc = self.write_runs(&runs);
        self.w.flush = runs;
        rc
    }

    /// The writing part of `mdb_page_flush` for the dirty pages `pgnos`
    /// (ascending), used by `mdb_page_spill`: clear `P_DIRTY` in each and
    /// finalise its header for the format, write them and drop them from
    /// the dirty list (see [`RwTxn::page_flush`]).
    pub(crate) fn write_pages(&mut self, pgnos: &[u64]) -> Result<()> {
        let l = self.layout();
        let ps = self.psize();
        let txnid = self.core.txnid;
        let mut runs = Vec::with_capacity(pgnos.len());
        for &pgno in pgnos {
            let slot = self.core.dirty.find(pgno).ok_or(MdbError::Corrupted)?;
            let page = self
                .core
                .dirty
                .at_mut(slot, pgno)
                .ok_or(MdbError::Corrupted)?;
            let flags = page_ops::flags(l, page);
            runs.push((pgno, slot, prepare_page(l, ps, page, flags, txnid)));
        }
        self.write_runs(&runs)
    }

    /// Writes the prepared dirty pages `runs` (page number, slot in the
    /// dirty list, bytes; ascending), contiguous runs batched into one
    /// `pwritev` of up to `MDB_COMMIT_PAGES` buffers (C's `iov` array,
    /// here on the stack too), then drops them from the dirty list
    /// (`mt_dirty_room` grows accordingly).
    fn write_runs(&mut self, runs: &[(u64, PageSlot, usize)]) -> Result<()> {
        let ps = self.psize();
        let file = self.core.env.data_file();
        let dirty = &self.core.dirty;
        let mut batch: [&[u8]; COMMIT_PAGES] = [&[]; COMMIT_PAGES];
        let mut n = 0;
        let mut wpos = 0u64;
        let mut wsize = 0usize;
        let mut next_pos = u64::MAX;
        for &(pgno, slot, size) in runs {
            let pos = pgno * ps as u64;
            if pos != next_pos || n == COMMIT_PAGES || wsize + size > MAX_WRITE {
                write_batch(file, &batch[..n], wpos, wsize)?;
                n = 0;
                wpos = pos;
                wsize = 0;
            }
            let buf = dirty.at(slot, pgno).ok_or(MdbError::Corrupted)?;
            batch[n] = buf.get(..size).ok_or(MdbError::Corrupted)?;
            n += 1;
            next_pos = pos + size as u64;
            wsize += size;
        }
        write_batch(file, &batch[..n], wpos, wsize)?;

        // Drop the written pages from the dirty list (both lists are
        // ascending).
        let mut next = runs.iter().map(|w| w.0).peekable();
        self.core.dirty.retain(|pgno, _| {
            while next.peek().is_some_and(|&w| w < pgno) {
                next.next();
            }
            next.peek() != Some(&pgno)
        });
        self.w.dirty_room += runs.len();
        Ok(())
    }

    /// `mdb_env_write_meta`: write the new meta page (`txnid % 2`) and
    /// publish the txnid. A failed write makes the environment unusable
    /// (`MDB_FATAL_ERROR`; later transactions fail with `MDB_PANIC`), after
    /// trying to put the old txnid back so the half-written page is not
    /// used.
    ///
    /// Without a lock file the txnid is published in the state the file's
    /// `NO_LOCK` handles share (`registry::LocalShared`). Readers copy the
    /// meta page of the txnid they find there and re-check it afterwards,
    /// as with `mti_txnid` (`RoTxn::take_snapshot`), so it must not name
    /// the page being rewritten: first it is set to the snapshot this
    /// transaction began from (`txnid - 1`, on the other meta page). It
    /// already is that unless another process committed since this process
    /// last published a commit.
    fn write_meta(&mut self) -> Result<()> {
        let env = self.core.env;
        let ps = self.psize();
        let txnid = self.core.txnid;
        let toggle = (txnid & 1) as usize;
        let flags = env.raw_flags();
        // Persist any increases of mapsize config. Meta page `toggle ^ 1`
        // is the one this transaction began from (it holds txnid - 1), and
        // only a writer changes meta pages, so its map size is still the
        // one read at begin.
        let mapsize = self.w.base_mapsize.max(env.map_size() as u64);
        // `metab`: what to write back if the write fails.
        let old = env.meta_ids(toggle);
        let dbs = self.core.dbs.borrow();
        let l = env.layout();
        let mut meta = Meta {
            magic: MDB_MAGIC,
            version: l.data_version,
            address: 0,
            map_size: mapsize,
            dbs: [dbs[FREE_DBI as usize].db, dbs[MAIN_DBI as usize].db],
            last_pgno: self.core.next_pgno - 1,
            txnid,
        };
        drop(dbs);
        let mut buf = [0u8; META_SIZE];
        meta.write(&mut buf);
        let bytes = &buf[META_MAPSIZE..];
        let off = (toggle * ps + l.meta_offset() + META_MAPSIZE) as u64;

        // Write to the SYNC fd unless NO_SYNC/NO_META_SYNC. (The meta fd
        // goes to the same file, but writing to it also syncs to disk.)
        let sync_fd = flags & (MDB_NOSYNC | MDB_NOMETASYNC) == 0;
        let fd = match (sync_fd, env.meta_file()) {
            (true, Some(f)) => f,
            _ => env.data_file(),
        };
        let local = env.local_shared();
        if let Some(local) = local {
            local.set_txnid(txnid.wrapping_sub(1));
        }
        if let Err(e) = os::write_all_at(fd, bytes, off) {
            // On a failure, the page cache still contains the new data.
            // Write some old data back, to prevent it from being used.
            if let Some((last_pgno, txnid)) = old {
                meta.last_pgno = last_pgno;
                meta.txnid = txnid;
                meta.write(&mut buf);
                let _ = os::write_all_at(env.data_file(), &buf[META_MAPSIZE..], off);
            }
            env.set_fatal_error();
            return Err(e.into());
        }
        if cfg!(any(target_os = "macos", target_os = "ios")) && sync_fd {
            // O_DSYNC does not flush the drive cache on macOS.
            if let Some(f) = env.meta_file() {
                os::fdatasync(f)?;
            }
        }
        if let Some(lock) = env.lock_region() {
            lock.set_txnid(txnid);
        }
        if let Some(local) = local {
            local.set_txnid(txnid);
        }
        self.wrote_meta(meta);
        Ok(())
    }
}

/// Prepares dirty page `page` (flags `flags`, pages of `ps` bytes) for
/// writing as `mdb_page_flush` does: clears `P_DIRTY` and finalises the
/// header for the format
/// ([`Layout::stamp_clean_page`](crate::format::Layout::stamp_clean_page)).
/// Returns how many bytes to write (the whole run for overflow pages).
fn prepare_page(l: &Layout, ps: usize, page: &mut [u8], flags: u16, txnid: u64) -> usize {
    page_ops::set_flags(l, page, flags & !P_DIRTY);
    l.stamp_clean_page(page, txnid);
    let npages = if flags & P_OVERFLOW != 0 {
        page_ops::overflow_pages(l, page).unwrap_or(1) as usize
    } else {
        1
    };
    npages * ps
}

/// Writes one batch of contiguous pages at `pos` (`pwritev`, or `pwrite`
/// for a single buffer). A short write is `EIO`, as in C.
fn write_batch(file: &std::fs::File, bufs: &[&[u8]], pos: u64, size: usize) -> Result<()> {
    match bufs.len() {
        0 => Ok(()),
        1 => Ok(os::write_all_at(file, bufs[0], pos)?),
        _ => {
            let n = os::pwritev(file, bufs, pos)?;
            if n != size {
                return Err(crate::error::Error::errno(libc::EIO));
            }
            Ok(())
        }
    }
}
