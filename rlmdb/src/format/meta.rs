//! `MDB_meta`: the root of a snapshot, stored right after the page header
//! of pages 0 and 1 (offset 16 in LMDB 0.9, 24 in 1.0: [`Layout::meta_offset`]).
//! The structure itself is the same in both formats; `mm_version` tells
//! them apart ([`Layout::data_version`]).
//!
//! Transaction `N` writes meta page `N % 2` (`mdb_env_write_meta`), so the two
//! pages hold the two most recent snapshots and the valid one with the larger
//! `mm_txnid` is current (`mdb_env_pick_meta`, mdb.c line 4078).
//!
//! Meta pages are rewritten by writers while readers look at them, so callers
//! must decode a private *copy* (taken with volatile reads by `os`), never a
//! long-lived slice of the map.

use super::db::Db;
use super::layout::*;
use super::{get_u32, get_u64, put_u16, put_u32, put_u64};
use crate::consts::MDB_MAGIC;
use crate::format::page::P_META;

/// A decoded `MDB_meta` (mdb.c line 1096).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meta {
    /// `mm_magic`: must be `MDB_MAGIC`.
    pub magic: u32,
    /// `mm_version`: must be `MDB_DATA_VERSION`.
    pub version: u32,
    /// `mm_address`: fixed map address (`MDB_FIXEDMAP`), normally 0.
    pub address: u64,
    /// `mm_mapsize`: map size recorded by the last writer.
    pub map_size: u64,
    /// `mm_dbs`: `[FREE_DBI, MAIN_DBI]`.
    pub dbs: [Db; 2],
    /// `mm_last_pg`: last page in use; the next unallocated page is `+ 1`.
    pub last_pgno: u64,
    /// `mm_txnid`: id of the transaction that committed this snapshot.
    pub txnid: u64,
}

impl Meta {
    /// Decodes a meta from the first [`META_SIZE`] bytes of `buf` (which
    /// must start at the `MDB_meta`, i.e. [`Layout::meta_offset`] bytes into
    /// the page).
    pub fn read(buf: &[u8]) -> Option<Meta> {
        if buf.len() < META_SIZE {
            return None;
        }
        Some(Meta {
            magic: get_u32(buf, META_MAGIC)?,
            version: get_u32(buf, META_VERSION)?,
            address: get_u64(buf, META_ADDRESS)?,
            map_size: get_u64(buf, META_MAPSIZE)?,
            dbs: [
                Db::read(&buf[META_FREE_DB..])?,
                Db::read(&buf[META_MAIN_DB..])?,
            ],
            last_pgno: get_u64(buf, META_LAST_PGNO)?,
            txnid: get_u64(buf, META_TXNID)?,
        })
    }

    /// Encodes the meta into the first [`META_SIZE`] bytes of `buf`.
    ///
    /// # Panics
    /// If `buf` is shorter than [`META_SIZE`].
    pub fn write(&self, buf: &mut [u8]) {
        put_u32(buf, META_MAGIC, self.magic);
        put_u32(buf, META_VERSION, self.version);
        put_u64(buf, META_ADDRESS, self.address);
        put_u64(buf, META_MAPSIZE, self.map_size);
        self.dbs[0].write(&mut buf[META_FREE_DB..]);
        self.dbs[1].write(&mut buf[META_MAIN_DB..]);
        put_u64(buf, META_LAST_PGNO, self.last_pgno);
        put_u64(buf, META_TXNID, self.txnid);
    }

    /// [`Meta::read_page_with_layout`] for an LMDB 0.9 meta page.
    pub fn read_page(page: &[u8]) -> Result<Meta, MetaError> {
        Meta::read_page_with_layout(page, &V09)
    }

    /// Decodes and validates the meta page in `page` (a whole page,
    /// header included) of the format `layout` describes, as
    /// `mdb_env_read_header` does: the page must be a `P_META` page, the
    /// magic must match (`MDB_INVALID` otherwise) and the version must be
    /// the format's (`MDB_VERSION_MISMATCH` otherwise).
    pub fn read_page_with_layout(page: &[u8], layout: &Layout) -> Result<Meta, MetaError> {
        let flags = super::get_u16(page, layout.flags_offset()).ok_or(MetaError::Invalid)?;
        if flags & P_META != P_META {
            return Err(MetaError::Invalid);
        }
        let meta = Meta::read(page.get(layout.meta_offset()..).ok_or(MetaError::Invalid)?)
            .ok_or(MetaError::Invalid)?;
        meta.validate_with_layout(layout)?;
        Ok(meta)
    }

    /// [`Meta::validate_with_layout`] for LMDB 0.9.
    pub fn validate(&self) -> Result<(), MetaError> {
        self.validate_with_layout(&V09)
    }

    /// Checks magic and that the version is `layout`'s `MDB_DATA_VERSION`.
    pub fn validate_with_layout(&self, layout: &Layout) -> Result<(), MetaError> {
        if self.magic != MDB_MAGIC {
            return Err(MetaError::Invalid);
        }
        if self.version != layout.data_version {
            return Err(MetaError::VersionMismatch);
        }
        Ok(())
    }

    /// The page size of the environment (`mm_psize`, alias of
    /// `mm_dbs[FREE_DBI].md_pad`).
    pub fn page_size(&self) -> u32 {
        self.dbs[0].pad
    }

    /// The persistent environment flags (`mm_flags`, alias of
    /// `mm_dbs[FREE_DBI].md_flags`).
    pub fn env_flags(&self) -> u16 {
        self.dbs[0].flags
    }

    /// [`Meta::new_env_with_layout`] for LMDB 0.9.
    pub fn new_env(page_size: u32, map_size: u64, env_flags: u32) -> Meta {
        Meta::new_env_with_layout(&V09, page_size, map_size, env_flags)
    }

    /// The meta for a brand-new environment of the format `layout`
    /// describes (`mdb_env_init_meta0`).
    ///
    /// `env_flags` are the open flags; C stores `me_flags & 0xffff` (which
    /// keeps `MDB_FIXEDMAP` and `MDB_NOSUBDIR`) plus `MDB_INTEGERKEY` in the
    /// free DB's `md_flags`.
    pub fn new_env_with_layout(
        layout: &Layout,
        page_size: u32,
        map_size: u64,
        env_flags: u32,
    ) -> Meta {
        let mut meta = Meta {
            magic: MDB_MAGIC,
            version: layout.data_version,
            map_size,
            last_pgno: 1,
            ..Meta::default()
        };
        meta.dbs[0] = Db::empty((env_flags & 0xffff) as u16 | crate::consts::MDB_INTEGERKEY as u16);
        meta.dbs[0].pad = page_size;
        meta.dbs[1] = Db::empty(0);
        meta
    }

    /// [`Meta::write_initial_pages_with_layout`] for LMDB 0.9.
    pub fn write_initial_pages(&self, buf: &mut [u8], page_size: usize) {
        self.write_initial_pages_with_layout(buf, page_size, &V09);
    }

    /// Writes the two initial meta pages of the format `layout` describes
    /// into `buf` (`2 * page_size` bytes), as `mdb_env_init_meta` does:
    /// page 0 and page 1, both `P_META`, both holding `self`; everything
    /// else zero.
    pub fn write_initial_pages_with_layout(
        &self,
        buf: &mut [u8],
        page_size: usize,
        layout: &Layout,
    ) {
        buf.fill(0);
        for i in 0..2 {
            let page = &mut buf[i * page_size..(i + 1) * page_size];
            put_u64(page, PAGE_PGNO, i as u64);
            put_u16(page, layout.flags_offset(), P_META);
            self.write(&mut page[layout.meta_offset()..]);
        }
    }
}

/// How many bytes at the start of a data file [`sniff_format`] looks at:
/// enough for the magic and version of either format's first meta page.
pub const SNIFF_LEN: usize = 32;

/// `mm_version` of the prerelease 1.0 format some lmdb-js builds wrote. Its
/// files are neither 0.9 nor 1.0 files.
const PRERELEASE_DATA_VERSION: u32 = 2;

/// Which format a data file is in, from its first [`SNIFF_LEN`] bytes
/// (`head`, or fewer if the file is shorter), without trusting more than
/// the magic and the version word of the first meta page:
///
/// * an empty file: `None`, a new environment;
/// * `MDB_MAGIC` at 16 (right after the 0.9 page header) and version 1:
///   [`Format::V09`];
/// * `MDB_MAGIC` at 24 (after the 1.0 header) and version 3:
///   [`Format::V10`];
/// * version 2 at either place (lmdb-js prerelease files):
///   [`MetaError::VersionMismatch`];
/// * anything else, including a 0.9-shaped header claiming version 3 and
///   a 1.0-shaped one claiming 1: [`MetaError::Invalid`].
///
/// This only picks the layout; the meta pages are validated in full when
/// the environment is opened with it (`mdb_env_read_header`).
pub fn sniff_format(head: &[u8]) -> Result<Option<Format>, MetaError> {
    if head.is_empty() {
        return Ok(None);
    }
    for layout in [&V09, &V10] {
        let at = layout.meta_offset();
        if super::get_u32(head, at + META_MAGIC) != Some(MDB_MAGIC) {
            continue;
        }
        match super::get_u32(head, at + META_VERSION) {
            Some(v) if v == layout.data_version => return Ok(Some(layout.format)),
            Some(PRERELEASE_DATA_VERSION) => return Err(MetaError::VersionMismatch),
            _ => {}
        }
    }
    Err(MetaError::Invalid)
}

/// Picks the current snapshot among the two meta pages, like
/// `mdb_env_pick_meta`: `metas[ metas[0].txnid < metas[1].txnid ]`.
/// Returns the index (0 or 1).
pub fn pick_newest(metas: &[Meta; 2]) -> usize {
    usize::from(metas[0].txnid < metas[1].txnid)
}

/// Why a meta page was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaError {
    /// Not a meta page or bad magic (`MDB_INVALID`).
    Invalid,
    /// Unsupported data format version (`MDB_VERSION_MISMATCH`).
    VersionMismatch,
}

impl From<MetaError> for crate::error::MdbError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Invalid => crate::error::MdbError::Invalid,
            MetaError::VersionMismatch => crate::error::MdbError::VersionMismatch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::P_INVALID;

    #[test]
    fn new_env_pages() {
        let meta = Meta::new_env(4096, 1 << 20, 0x01 /* MDB_FIXEDMAP */);
        assert_eq!(meta.env_flags(), 0x09);
        assert_eq!(meta.page_size(), 4096);
        let mut buf = vec![0xffu8; 8192];
        meta.write_initial_pages(&mut buf, 4096);
        for i in 0..2 {
            let page = &buf[i * 4096..(i + 1) * 4096];
            assert_eq!(super::super::get_u64(page, 0), Some(i as u64));
            let m = Meta::read_page(page).unwrap();
            assert_eq!(m, meta);
            assert_eq!(m.dbs[1].root, P_INVALID);
            assert_eq!(m.last_pgno, 1);
        }
        // Everything outside the header and meta is zero.
        assert!(buf[16 + META_SIZE..4096].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_bad_pages() {
        let meta = Meta::new_env(4096, 1 << 20, 0);
        let mut buf = vec![0u8; 8192];
        meta.write_initial_pages(&mut buf, 4096);
        let mut bad = buf[..4096].to_vec();
        super::super::put_u16(&mut bad, 10, 0x02); // P_LEAF instead of P_META
        assert_eq!(Meta::read_page(&bad), Err(MetaError::Invalid));
        let mut bad = buf[..4096].to_vec();
        bad[16] ^= 1;
        assert_eq!(Meta::read_page(&bad), Err(MetaError::Invalid));
        let mut bad = buf[..4096].to_vec();
        super::super::put_u32(&mut bad, 20, 2);
        assert_eq!(Meta::read_page(&bad), Err(MetaError::VersionMismatch));
    }

    #[test]
    fn v10_pages() {
        let meta = Meta::new_env_with_layout(&V10, 4096, 1 << 20, 0);
        assert_eq!(meta.version, 3);
        let mut buf = vec![0xffu8; 8192];
        meta.write_initial_pages_with_layout(&mut buf, 4096, &V10);
        let page = &buf[..4096];
        // The meta follows the 24-byte header; mp_flags is at 18.
        assert_eq!(super::super::get_u16(page, 18), Some(P_META));
        assert_eq!(super::super::get_u32(page, 24), Some(MDB_MAGIC));
        assert_eq!(Meta::read_page_with_layout(page, &V10), Ok(meta));
        // A 1.0 page is not a 0.9 one, and a 0.9 meta is not a 1.0 one.
        assert_eq!(Meta::read_page(page), Err(MetaError::Invalid));
        let mut v09 = vec![0u8; 8192];
        Meta::new_env(4096, 1 << 20, 0).write_initial_pages(&mut v09, 4096);
        assert_eq!(
            Meta::read_page_with_layout(&v09[..4096], &V10),
            Err(MetaError::Invalid)
        );
        let mut wrong = meta;
        wrong.version = 1;
        assert_eq!(
            wrong.validate_with_layout(&V10),
            Err(MetaError::VersionMismatch)
        );
        assert_eq!(wrong.validate(), Ok(()));
    }

    #[test]
    fn sniff() {
        let mut v09 = vec![0u8; 8192];
        Meta::new_env(4096, 1 << 20, 0).write_initial_pages(&mut v09, 4096);
        let mut v10 = vec![0u8; 8192];
        Meta::new_env_with_layout(&V10, 4096, 1 << 20, 0)
            .write_initial_pages_with_layout(&mut v10, 4096, &V10);
        assert_eq!(sniff_format(&v09[..SNIFF_LEN]), Ok(Some(Format::V09)));
        assert_eq!(sniff_format(&v10[..SNIFF_LEN]), Ok(Some(Format::V10)));
        assert_eq!(sniff_format(&[]), Ok(None));
        // Too short to hold a magic and version, but not empty.
        assert_eq!(sniff_format(&v09[..20]), Err(MetaError::Invalid));
        assert_eq!(sniff_format(&[0u8; SNIFF_LEN]), Err(MetaError::Invalid));
        // A file written on a host of the other byte order: its magic
        // reads byte-swapped (MDB_INVALID, as in C).
        for (buf, layout) in [(&v09, &V09), (&v10, &V10)] {
            let mut swapped = buf[..SNIFF_LEN].to_vec();
            let at = layout.meta_offset() + META_MAGIC;
            swapped[at..at + 4].reverse();
            assert_eq!(sniff_format(&swapped), Err(MetaError::Invalid));
        }
        // Each shape with the other format's version word.
        let mut bad = v09[..SNIFF_LEN].to_vec();
        bad[20..24].copy_from_slice(&3u32.to_ne_bytes());
        assert_eq!(sniff_format(&bad), Err(MetaError::Invalid));
        let mut bad = v10[..SNIFF_LEN].to_vec();
        bad[28..32].copy_from_slice(&1u32.to_ne_bytes());
        assert_eq!(sniff_format(&bad), Err(MetaError::Invalid));
        // Version 2 (lmdb-js prerelease files) in either place.
        for at in [20, 28] {
            let mut v2 = if at == 20 { v09.clone() } else { v10.clone() };
            v2[at..at + 4].copy_from_slice(&2u32.to_ne_bytes());
            assert_eq!(
                sniff_format(&v2[..SNIFF_LEN]),
                Err(MetaError::VersionMismatch)
            );
        }
    }

    #[test]
    fn pick() {
        let mut a = Meta::new_env(4096, 0, 0);
        let mut b = a;
        a.txnid = 7;
        b.txnid = 8;
        assert_eq!(pick_newest(&[a, b]), 1);
        assert_eq!(pick_newest(&[b, a]), 0);
        // Ties go to page 0, like C.
        assert_eq!(pick_newest(&[a, a]), 0);
    }
}
