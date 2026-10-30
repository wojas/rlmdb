//! Copying an environment: `mdb_env_copy2` (mdb.c line 9788) and
//! `mdb_env_copyfd2` (line 9773), plain (`mdb_env_copyfd0`, line 9674) or
//! compacting (`mdb_env_copyfd1`, line 9543, with `mdb_env_cwalk`, line
//! 9387).
//!
//! # Plain copy
//!
//! The data file as it is, up to the last page of the current snapshot.
//! The two meta pages are copied while holding the writer lock (so no
//! commit can change them halfway), after starting the read transaction
//! that pins the snapshot; then the other pages follow without the lock.
//! The result is byte for byte what C's `mdb_copy` writes. (C writes
//! straight from its memory map; we `pread` the file, which reads the same
//! bytes and does not hand out references to pages that are free in the
//! snapshot and may change while they are copied.)
//!
//! # Compacting copy
//!
//! Only the pages the snapshot uses, renumbered from page 2 in the order of
//! a depth-first walk (each page after the pages below it, so a tree's root
//! comes last), with an empty free DB: meta page 0 is a new environment's
//! (txnid 0), meta page 1 holds the main DB (txnid 1, unless the
//! environment is empty) and the main root is the last page. A count of the
//! pages the snapshot uses from its free list must add up, otherwise the
//! copy fails with `MDB_INCOMPATIBLE` (a page leak or a corrupt database),
//! as in C.
//!
//! C fills two 1 MiB buffers alternately and a second thread writes them;
//! pages are copied into a buffer without their unused middle part
//! (`mdb_page_copy`), which then keeps whatever an earlier page left there.
//! The writer here is a plain loop over the same two buffers, filled and
//! handed over at the same points, so even those unused bytes match and
//! the output is byte for byte C's.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::BorrowedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::consts::*;
use crate::core::dbi::Dbi;
use crate::core::env::Environment;
use crate::core::page_ops;
use crate::core::txn::{Transaction, TxnCore};
use crate::error::{Error, MdbError, Result};
use crate::format::layout::{Format, Layout, OverflowRef};
use crate::format::meta::Meta;
use crate::format::page::{P_LEAF2, P_META};
use crate::format::{put_u16, put_u64, Db, PageRef};
use crate::idl::Idl;
use crate::os;

/// `MDB_WBUF`: size of each of the two write buffers of a compacting copy.
const WBUF: usize = 1 << 20;

/// `MAX_WRITE`: largest single write of a plain copy.
const MAX_WRITE: usize = 0x4000_0000;

/// Read size of a plain copy (C writes straight from the map, up to
/// `MAX_WRITE` at a time; the size here only affects speed).
const PLAIN_CHUNK: usize = 4 << 20;

impl Environment {
    /// `mdb_env_copy2`: copy the environment to a new data file: `path` is
    /// the directory to put `data.mdb` in (it must exist), or with
    /// [`NO_SUB_DIR`](crate::EnvironmentFlags::NO_SUB_DIR) the file name.
    /// The file must not exist yet (it is created with `O_EXCL` and mode
    /// `0666` before the umask). No lock file is created.
    ///
    /// With `compact`, only the pages in use are copied, renumbered, and
    /// the free list of the copy is empty (`MDB_CP_COMPACT`); see the
    /// module documentation of `copy`. Writers may continue while the copy
    /// runs; it shows the snapshot current when it began.
    pub fn copy_to_path(&self, path: impl AsRef<Path>, compact: bool) -> Result<()> {
        let path = path.as_ref();
        let target = if self.raw_flags() & MDB_NOSUBDIR != 0 {
            path.to_path_buf()
        } else {
            path.join("data.mdb")
        };
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&target)?;
        if self.page_size() >= os::os_page_size() {
            // The source data is already in the OS cache; do not cache the
            // copy too (needs aligned buffers with O_DIRECT, which we use).
            os::set_nocache(&file);
        }
        let rc = self.copy_to_file(&file, compact);
        let closed = os::close(file);
        rc?;
        Ok(closed?)
    }

    /// `mdb_env_copyfd2`: write a copy of the environment's data file to
    /// `fd` (a file, pipe or socket), starting at its current position (see
    /// [`Environment::copy_to_path`] for `compact`). The descriptor stays
    /// open and is not synced.
    pub fn copy_to_fd(&self, fd: BorrowedFd<'_>, compact: bool) -> Result<()> {
        // A duplicate shares the file position with `fd`.
        let file = File::from(fd.try_clone_to_owned()?);
        self.copy_to_file(&file, compact)
    }

    fn copy_to_file(&self, file: &File, compact: bool) -> Result<()> {
        let mut out = file;
        if compact {
            self.copy_compact(&mut out)
        } else {
            self.copy_plain(&mut out)
        }
    }

    /// `mdb_env_copyfd0`.
    fn copy_plain(&self, out: &mut impl Write) -> Result<()> {
        let ps = self.page_size();
        let mut buf = AlignedBuf::new(PLAIN_CHUNK.max(2 * ps));
        let txn = match self.lock_region() {
            None => {
                let txn = self.begin_ro_txn()?;
                read_exact_at(self.data_file(), &mut buf.as_mut()[..2 * ps], 0)?;
                txn
            }
            Some(lock) => {
                // Do the lock/unlock of the reader mutex before starting
                // the write txn. Otherwise other read txns could block
                // writers.
                let inactive = self.begin_ro_txn()?.reset();
                // Temporarily block writers until we snapshot the meta
                // pages; the read transaction must start after that.
                lock.lock_writer(|| self.pick_meta().map(|m| m.txnid))?;
                let snapshot = inactive.renew().and_then(|txn| {
                    read_exact_at(self.data_file(), &mut buf.as_mut()[..2 * ps], 0)?;
                    Ok(txn)
                });
                lock.unlock_writer();
                snapshot?
            }
        };
        out.write_all(&buf.as_mut()[..2 * ps])?;

        let fsize = os::file_size(self.data_file())?;
        let end = (txn.core().next_pgno.saturating_mul(ps as u64)).min(fsize);
        let mut pos = 2 * ps as u64;
        while pos < end {
            let n = ((end - pos) as usize).min(buf.len()).min(MAX_WRITE);
            let chunk = &mut buf.as_mut()[..n];
            read_exact_at(self.data_file(), chunk, pos)?;
            out.write_all(chunk)?;
            pos += n as u64;
        }
        Ok(())
    }

    /// `mdb_env_copyfd1`.
    fn copy_compact(&self, out: &mut impl Write) -> Result<()> {
        let ps = self.page_size();
        let l = self.layout();
        let txn = self.begin_ro_txn()?;
        let core = txn.core();
        let mut w = CopyWriter::new(out, ps, l);

        // Meta page 0: a new environment (mdb_env_init_meta0), with the
        // source's recorded map address.
        let mut meta =
            Meta::new_env_with_layout(l, ps as u32, self.map_size() as u64, self.raw_flags());
        meta.address = self.read_meta(0)?.address;
        let mut meta1 = meta;

        // Set metapage 1 with current main DB.
        let main = core.dbs.borrow()[MAIN_DBI as usize].db;
        let root = main.root;
        let mut new_root = root;
        if root != P_INVALID {
            // Count free pages + freeDB pages. Subtract from last_pg to
            // find the new last_pg, which also becomes the new root.
            let mut freecount = 0u64;
            let mut c = txn.cursor(Dbi::FREE)?;
            while let Some((_, data)) = c.next()? {
                freecount =
                    freecount.wrapping_add(Idl::encoded_len(data).ok_or(MdbError::Corrupted)?);
            }
            let free = core.dbs.borrow()[FREE_DBI as usize].db;
            // size_t arithmetic, as in C: a corrupt count fails the page
            // count check at the end instead.
            freecount = freecount
                .wrapping_add(free.branch_pages)
                .wrapping_add(free.leaf_pages)
                .wrapping_add(free.overflow_pages);
            new_root = core.next_pgno.wrapping_sub(1).wrapping_sub(freecount);
            meta1.last_pgno = new_root;
            meta1.dbs[MAIN_DBI as usize] = main;
            meta1.dbs[MAIN_DBI as usize].root = new_root;
        } else {
            // When the DB is empty, handle it specially to fix any
            // breakage like page leaks from ITS#8174.
            meta1.dbs[MAIN_DBI as usize].flags = main.flags;
        }
        if root != P_INVALID || meta1.dbs[MAIN_DBI as usize].flags != 0 {
            meta1.txnid = 1; // use metapage 1
        }
        for (i, m) in [meta, meta1].iter().enumerate() {
            let page = &mut w.current()[i * ps..(i + 1) * ps];
            put_u64(page, 0, i as u64);
            put_u16(page, l.flags_offset(), P_META);
            m.write(&mut page[l.meta_offset()..]);
        }
        w.wlen[0] = 2 * ps;

        w.nesting = 1;
        let walked = w.cwalk(core, root, false)?;
        if walked != new_root {
            // Page leak or corrupt DB.
            return Err(MdbError::Incompatible.into());
        }
        w.flush()
    }
}

/// `pread` exactly `buf.len()` bytes (`EIO` at end of file).
fn read_exact_at(file: &File, buf: &mut [u8], pos: u64) -> Result<()> {
    if os::read_at(file, buf, pos)? != buf.len() {
        return Err(Error::errno(libc::EIO));
    }
    Ok(())
}

/// A zeroed buffer aligned to the OS page size, as `O_DIRECT` writes need
/// (C uses `memalign`).
struct AlignedBuf {
    storage: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> AlignedBuf {
        let align = os::os_page_size();
        let storage = vec![0u8; len + align];
        let start = storage.as_ptr().align_offset(align).min(align);
        AlignedBuf {
            storage,
            start,
            len,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }
}

/// The double buffer of a compacting copy (`mdb_copy`) with the writer
/// thread replaced by synchronous writes at the hand-over points.
struct CopyWriter<'o, W: Write> {
    out: &'o mut W,
    /// `mc_wbuf[0]` and `mc_wbuf[1]`, back to back.
    bufs: AlignedBuf,
    /// `mc_wlen`.
    wlen: [usize; 2],
    /// `mc_toggle`: the buffer being filled.
    toggle: usize,
    /// `mc_next_pgno`: the number the next page written gets.
    next_pgno: u64,
    ps: usize,
    /// The page layout of the environment (and of the copy).
    layout: &'static Layout,
    /// How many `cwalk`s are running (the main DB, a named DB, its
    /// duplicates' sub-DBs). C recurses without a limit, so a corrupt
    /// sub-DB record pointing back up the tree overflows its stack.
    nesting: usize,
}

/// The deepest valid `cwalk` nesting: main DB, named DB, sub-DB of
/// duplicates.
const MAX_CWALK_NESTING: usize = 3;

impl<'o, W: Write> CopyWriter<'o, W> {
    fn new(out: &'o mut W, ps: usize, layout: &'static Layout) -> Self {
        CopyWriter {
            out,
            bufs: AlignedBuf::new(2 * WBUF),
            wlen: [0; 2],
            toggle: 0,
            next_pgno: NUM_METAS as u64,
            ps,
            layout,
            nesting: 0,
        }
    }

    /// The buffer being filled.
    fn current(&mut self) -> &mut [u8] {
        let t = self.toggle;
        &mut self.bufs.as_mut()[t * WBUF..(t + 1) * WBUF]
    }

    /// `mdb_env_cthr_toggle(my, 1)`: write the filled buffer (and the
    /// overflow pages `tail` that follow it), then fill the other one.
    fn toggle(&mut self, tail: Option<&[u8]>) -> Result<()> {
        let t = self.toggle;
        let n = self.wlen[t];
        self.out
            .write_all(&self.bufs.as_mut()[t * WBUF..t * WBUF + n])?;
        if let Some(tail) = tail {
            self.out.write_all(tail)?;
        }
        self.wlen[t] = 0;
        self.toggle ^= 1;
        self.wlen[self.toggle] = 0;
        Ok(())
    }

    /// Room for the next page: hand the buffer over when it is full.
    fn make_room(&mut self) -> Result<()> {
        if self.wlen[self.toggle] >= WBUF {
            self.toggle(None)?;
        }
        Ok(())
    }

    /// The final hand-over (`mdb_env_cthr_toggle(my, 1 | MDB_EOF)`).
    fn flush(mut self) -> Result<()> {
        self.toggle(None)?;
        self.out.flush()?;
        Ok(())
    }

    /// Appends a copy of page `src` (`mdb_page_copy`) numbered
    /// `mc_next_pgno` and returns that number.
    fn put_page(&mut self, src: &[u8]) -> Result<u64> {
        self.make_room()?;
        let ps = self.ps;
        let l = self.layout;
        let off = self.wlen[self.toggle];
        let pgno = self.next_pgno;
        let dst = &mut self.current()[off..off + ps];
        copy_used(l, dst, src, ps)?;
        put_u64(dst, 0, pgno);
        // 1.0's compacting copy writes every page as txn 1 (mdb.c 1.0,
        // line 11517); a no-op for 0.9.
        l.stamp_clean_page(dst, 1);
        self.next_pgno += 1;
        self.wlen[self.toggle] += ps;
        Ok(pgno)
    }

    /// Appends the overflow run `run` (`pages` pages), renumbered.
    fn put_overflow(&mut self, run: &[u8], pages: usize) -> Result<u64> {
        self.make_room()?;
        let ps = self.ps;
        let l = self.layout;
        let off = self.wlen[self.toggle];
        let pgno = self.next_pgno;
        let dst = &mut self.current()[off..off + ps];
        dst.copy_from_slice(&run[..ps]);
        put_u64(dst, 0, pgno);
        // As in `put_page` (mdb.c 1.0, lines 11436, 11448).
        l.stamp_clean_page(dst, 1);
        self.next_pgno += pages as u64;
        self.wlen[self.toggle] += ps;
        if pages > 1 {
            self.toggle(Some(&run[ps..pages * ps]))?;
        }
        Ok(pgno)
    }

    /// `mdb_env_cwalk`: copy the tree rooted at `root` depth-first, each
    /// page after its children, and return the root's new number. With
    /// `dupdata` (a `DUP_SORT` sub-DB) the leaves are not scanned, since
    /// they hold neither overflow pages nor sub-DBs.
    fn cwalk(&mut self, txn: &TxnCore<'_>, root: u64, dupdata: bool) -> Result<u64> {
        // Empty DB, nothing to do.
        if root == P_INVALID {
            return Ok(root);
        }
        // The cursor: writable copies of the branch pages on the path (their
        // child pointers are rewritten as the children get their new
        // numbers) with the index of the child being copied, and the leaf
        // to copy next, if any.
        let mut branches: Vec<(Vec<u8>, usize)> = Vec::new();
        let mut leaf = Some(descend(txn, root, &mut branches)?);
        loop {
            let new_pgno = match leaf.take() {
                Some(pgno) => {
                    let page = txn.page(pgno)?;
                    let modified = if !page.is_leaf2() && !dupdata {
                        self.copy_leaf_payloads(txn, page.bytes())?
                    } else {
                        None
                    };
                    match modified {
                        Some(copy) => self.put_page(&copy)?,
                        None => self.put_page(page.bytes())?,
                    }
                }
                None => {
                    let l = self.layout;
                    let (bpage, ki) = branches.last_mut().ok_or(MdbError::Corrupted)?;
                    *ki += 1;
                    if *ki < page_ops::num_keys(l, bpage) {
                        // Whenever we advance to a sibling branch page, we
                        // must proceed all the way down to its first leaf.
                        let child = child_at(l, bpage, *ki)?;
                        leaf = Some(descend(txn, child, &mut branches)?);
                        continue;
                    }
                    let (bpage, _) = branches.pop().ok_or(MdbError::Corrupted)?;
                    self.put_page(&bpage)?
                }
            };
            match branches.last_mut() {
                // Update parent if there is one.
                Some((parent, ki)) => {
                    page_ops::set_node_pgno(self.layout, parent, *ki, new_pgno)
                        .ok_or(MdbError::Corrupted)?;
                }
                // Otherwise we're done.
                None => return Ok(new_pgno),
            }
        }
    }

    /// The leaf part of `mdb_env_cwalk`: copy the overflow pages and
    /// sub-DBs of the nodes of `leaf` and return the leaf with their new
    /// numbers, or `None` if it has no such nodes.
    fn copy_leaf_payloads(&mut self, txn: &TxnCore<'_>, leaf: &[u8]) -> Result<Option<Vec<u8>>> {
        let l = self.layout;
        let mut copy: Option<Vec<u8>> = None;
        let n = page_ops::num_keys(l, leaf);
        for i in 0..n {
            let page = PageRef::with_layout(leaf, l).ok_or(MdbError::Corrupted)?;
            let node = page.node(i).ok_or(MdbError::Corrupted)?;
            if node.is_bigdata() {
                let r = node.overflow_ref().ok_or(MdbError::Corrupted)?;
                let omp = txn.page(r.pgno)?;
                // 0.9 takes the run length from the page, 1.0 from the
                // reference's `op_pages` (the same in a sound file).
                let pages = match l.format {
                    Format::V09 => omp.overflow_pages() as usize,
                    Format::V10 => usize::try_from(r.pages).map_err(|_| MdbError::Corrupted)?,
                };
                let run = overflow_run(txn, r.pgno, pages, self.ps)?;
                let new = self.put_overflow(run, pages)?;
                let c = copy.get_or_insert_with(|| leaf.to_vec());
                // The new page number; 1.0 also writes the run as txn 1
                // and keeps op_pages (mdb.c 1.0, lines 11418-11420).
                let moved = OverflowRef {
                    pgno: new,
                    txnid: 1,
                    pages: r.pages,
                };
                page_ops::set_node_overflow_ref(l, c, i, moved).ok_or(MdbError::Corrupted)?;
            } else if node.is_subdata() {
                let data = node.inline_data().ok_or(MdbError::Corrupted)?;
                let mut db = Db::read(data).ok_or(MdbError::Corrupted)?;
                if self.nesting >= MAX_CWALK_NESTING {
                    return Err(MdbError::Corrupted.into());
                }
                self.nesting += 1;
                let walked = self.cwalk(txn, db.root, node.is_dupdata());
                self.nesting -= 1;
                db.root = walked?;
                let c = copy.get_or_insert_with(|| leaf.to_vec());
                let range = page_ops::node_data_range(l, c, i).ok_or(MdbError::Corrupted)?;
                db.write(c.get_mut(range).ok_or(MdbError::Corrupted)?);
            }
        }
        Ok(copy)
    }
}

/// Goes down from page `pgno` to the first leaf below it, pushing a copy of
/// each branch page on the way (`mdb_page_search_root` with
/// `MDB_PS_FIRST`), and returns the leaf's number.
fn descend(txn: &TxnCore<'_>, mut pgno: u64, branches: &mut Vec<(Vec<u8>, usize)>) -> Result<u64> {
    loop {
        let page = txn.page(pgno)?;
        if page.is_leaf() {
            return Ok(pgno);
        }
        if !page.is_branch() {
            return Err(MdbError::Corrupted.into());
        }
        if branches.len() + 1 >= CURSOR_STACK {
            return Err(MdbError::CursorFull.into());
        }
        pgno = page.node(0).ok_or(MdbError::Corrupted)?.child_pgno();
        branches.push((page.bytes().to_vec(), 0));
    }
}

/// The child page number of branch node `i` of a page of layout `l`.
fn child_at(l: &'static Layout, branch: &[u8], i: usize) -> Result<u64> {
    let page = PageRef::with_layout(branch, l).ok_or(MdbError::Corrupted)?;
    Ok(page.node(i).ok_or(MdbError::Corrupted)?.child_pgno())
}

/// The `pages` pages of the overflow run at `pgno` in the snapshot.
fn overflow_run<'t>(txn: &'t TxnCore<'_>, pgno: u64, pages: usize, ps: usize) -> Result<&'t [u8]> {
    if pages == 0
        || pgno
            .checked_add(pages as u64)
            .is_none_or(|e| e > txn.next_pgno)
    {
        return Err(MdbError::PageNotFound.into());
    }
    let off = usize::try_from(pgno)
        .ok()
        .and_then(|p| p.checked_mul(ps))
        .ok_or(MdbError::PageNotFound)?;
    txn.map
        .slice(off, pages * ps)
        .ok_or_else(|| MdbError::PageNotFound.into())
}

/// `mdb_page_copy`: copy page `src` into `dst` except the unused space
/// between the node pointers and the nodes, with the boundaries widened to
/// 8-byte alignment; if less than 8 bytes are unused, or for `P_LEAF2`
/// pages, everything up to the (aligned) unused tail. `lower` and `upper`
/// are taken from the page start (`+ PAGEBASE`), as in C 1.0.
fn copy_used(l: &Layout, dst: &mut [u8], src: &[u8], ps: usize) -> Result<()> {
    let bad = || Error::from(MdbError::Corrupted);
    if src.len() < l.page_hdr {
        return Err(bad());
    }
    let lower = page_ops::lower(l, src);
    let upper = page_ops::upper(l, src);
    let flags = page_ops::flags(l, src);
    if src.len() < ps || dst.len() < ps || lower > upper || upper > ps {
        return Err(bad());
    }
    let unused = (upper - lower) & !7;
    if unused != 0 && flags & P_LEAF2 == 0 {
        let upper = upper & !7;
        let lower = (lower + 7) & !7;
        dst[..lower].copy_from_slice(&src[..lower]);
        dst[upper..ps].copy_from_slice(&src[upper..ps]);
    } else {
        dst[..ps - unused].copy_from_slice(&src[..ps - unused]);
    }
    Ok(())
}
