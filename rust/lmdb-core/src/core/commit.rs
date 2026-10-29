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
//! 6. publish the txnid in the lock file, so new readers pick it up.
//!
//! A transaction without dirty pages commits without writing anything
//! (`MDB_END_EMPTY_COMMIT`).

use crate::consts::*;
use crate::core::cursor_put::PutData;
use crate::core::dbi::DB_DIRTY;
use crate::core::txn_rw::{RwTxn, TXN_DIRTY, TXN_ERROR, TXN_FINISHED, TXN_SPILLS};
use crate::core::wcursor::CRef;
use crate::error::{MdbError, Result};
use crate::format::layout::{META_MAPSIZE, META_OFFSET_IN_PAGE, META_SIZE};
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
        if self.w.flags & (TXN_FINISHED | TXN_ERROR) != 0 {
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

        self.page_flush(0)?;
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

    /// `mdb_page_flush`: write the dirty pages after the first `keep` to
    /// the data file, clearing `P_DIRTY` in each first. Loose pages (and
    /// `P_KEEP` ones, used by spilling) stay dirty and in the list; written
    /// pages leave it.
    pub(crate) fn page_flush(&mut self, keep: usize) -> Result<()> {
        let ps = self.psize();
        // Pass 1: decide what to write and clear the dirty flags.
        let mut writes: Vec<(u64, usize)> = Vec::new();
        let pgnos: Vec<u64> = self.core.dirty.pgnos().skip(keep).collect();
        for pgno in pgnos {
            let page = self.dirty_mut(pgno)?;
            let flags = crate::core::page_ops::flags(page);
            // Don't flush this page yet.
            if flags & (P_LOOSE | P_KEEP) != 0 {
                crate::core::page_ops::set_flags(page, flags & !P_KEEP);
                continue;
            }
            crate::core::page_ops::set_flags(page, flags & !P_DIRTY);
            let npages = if flags & P_OVERFLOW != 0 {
                crate::format::get_u32(page, crate::format::layout::PAGE_OVERFLOW_PAGES)
                    .unwrap_or(1) as usize
            } else {
                1
            };
            writes.push((pgno, npages * ps));
        }

        // Pass 2: write them, contiguous runs batched.
        let file = self.core.env.data_file();
        let mut batch: Vec<&[u8]> = Vec::with_capacity(COMMIT_PAGES);
        let mut wpos = 0u64;
        let mut wsize = 0usize;
        let mut next_pos = u64::MAX;
        for &(pgno, size) in &writes {
            let pos = pgno * ps as u64;
            if pos != next_pos || batch.len() == COMMIT_PAGES || wsize + size > MAX_WRITE {
                write_batch(file, &batch, wpos, wsize)?;
                batch.clear();
                wpos = pos;
                wsize = 0;
            }
            let buf = self.core.dirty.get(pgno).ok_or(MdbError::Corrupted)?;
            batch.push(buf.get(..size).ok_or(MdbError::Corrupted)?);
            next_pos = pos + size as u64;
            wsize += size;
        }
        write_batch(file, &batch, wpos, wsize)?;

        // Pass 3: drop the written pages from the dirty list.
        for &(pgno, _) in &writes {
            self.core.dirty.remove(pgno);
        }
        self.w.dirty_room += writes.len();
        Ok(())
    }

    /// `mdb_env_write_meta`: write the new meta page (`txnid % 2`) and
    /// publish the txnid. A failed write makes the environment unusable
    /// (`MDB_FATAL_ERROR`; later transactions fail with `MDB_PANIC`), after
    /// trying to put the old txnid back so the half-written page is not
    /// used.
    fn write_meta(&mut self) -> Result<()> {
        let env = self.core.env;
        let ps = self.psize();
        let txnid = self.core.txnid;
        let toggle = (txnid & 1) as usize;
        let flags = env.raw_flags();
        // Persist any increases of mapsize config.
        let mapsize = env
            .read_meta(toggle ^ 1)?
            .map_size
            .max(env.map_size() as u64);
        let old = env.read_meta(toggle).ok();
        let dbs = self.core.dbs.borrow();
        let mut meta = Meta {
            magic: MDB_MAGIC,
            version: MDB_DATA_VERSION,
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
        let off = (toggle * ps + META_OFFSET_IN_PAGE + META_MAPSIZE) as u64;

        // Write to the SYNC fd unless NO_SYNC/NO_META_SYNC. (The meta fd
        // goes to the same file, but writing to it also syncs to disk.)
        let sync_fd = flags & (MDB_NOSYNC | MDB_NOMETASYNC) == 0;
        let fd = match (sync_fd, env.meta_file()) {
            (true, Some(f)) => f,
            _ => env.data_file(),
        };
        if let Err(e) = os::write_all_at(fd, bytes, off) {
            // On a failure, the page cache still contains the new data.
            // Write some old data back, to prevent it from being used.
            if let Some(old) = old {
                meta.last_pgno = old.last_pgno;
                meta.txnid = old.txnid;
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
        Ok(())
    }
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
