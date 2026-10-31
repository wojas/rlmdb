//! The deterministic data set shared by `fill`, `verify`, `delete` and `gen`
//! (a port of `c/dataset.c`).
//!
//! Everything here is a pure function of `(seed, i, d)`, so any peer in any
//! process can regenerate the exact bytes another peer wrote. The
//! definitions are in README.md ("Deterministic data set") and must stay
//! byte-for-byte equal to the C peer's; the `gen_vectors` scenario and
//! `tests/rpeer.rs` compare both.
//!
//! All arithmetic is on `u32`/`u64` with wrap-around, as in C. The
//! comparators at the end sort the *expected* model; they are written out
//! here instead of using `rlmdb::cmp`, so that the model does not depend on
//! the code under test.

use std::cmp::Ordering;

use rlmdb::consts::{
    MDB_DUPFIXED, MDB_DUPSORT, MDB_INTEGERDUP, MDB_INTEGERKEY, MDB_REVERSEDUP, MDB_REVERSEKEY,
};

/// The 32-bit finaliser of MurmurHash3 (`fmix32`). A bijection on `u32`, so
/// distinct inputs give distinct outputs.
pub fn fmix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// SplitMix64 (`splitmix64`): advances `state` and returns 64 bits.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Fills `buf` with SplitMix64 words from `state`, each written
/// little-endian, the last one truncated (the filler of `ds_value`,
/// `wl_value` and nested-test's big value).
pub fn splitmix_fill(state: &mut u64, buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let word = splitmix64(state).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

/// The parameters that define a data set (`struct dataset`).
#[derive(Clone, Debug, Default)]
pub struct DataSet {
    /// Named DB, or `None` for the main DB.
    pub db: Option<String>,
    /// Number of keys (items `i = 0..count`).
    pub count: u32,
    /// Seed.
    pub seed: u32,
    /// `MDB_DUPSORT`, `MDB_INTEGERKEY`, ... (C bit values).
    pub flags: u32,
    /// Value size (max size for DUPSORT); 0 until [`DataSet::finish`].
    pub valsize: u32,
    /// If > 0: items with `i % 3 == 0` get values of this size.
    pub big: u32,
    /// Max number of values per key (DUPSORT); 0 until [`DataSet::finish`].
    pub dups: u32,
    /// 0, or the `--every N` of a prior `delete`.
    pub deleted_every: u32,
}

impl DataSet {
    /// `ds_finish`: fill in the defaults that depend on the flags and reject
    /// impossible combinations (the message is a usage error).
    pub fn finish(&mut self) -> Result<(), &'static str> {
        let mut f = self.flags;
        if f & MDB_INTEGERDUP != 0 {
            f |= MDB_DUPSORT | MDB_DUPFIXED;
        }
        if f & MDB_DUPFIXED != 0 {
            f |= MDB_DUPSORT;
        }
        self.flags = f;

        if self.valsize == 0 {
            self.valsize = if f & MDB_INTEGERDUP != 0 {
                8
            } else if f & MDB_DUPSORT != 0 {
                16
            } else {
                32
            };
        }
        if self.dups == 0 {
            self.dups = if f & MDB_DUPSORT != 0 { 5 } else { 1 };
        }

        if f & MDB_INTEGERDUP != 0 && self.valsize != 4 && self.valsize != 8 {
            return Err("--intdup needs --valsize 4 or 8");
        }
        if f & MDB_DUPFIXED != 0 && !(1..=511).contains(&self.valsize) {
            return Err("DUPFIXED values must be 1..511 bytes (--valsize)");
        }
        if f & MDB_DUPFIXED != 0 && self.valsize < 4 && self.dups > 1 << (8 * self.valsize) {
            return Err("--dups exceeds the distinct values of --valsize");
        }
        if f & MDB_DUPSORT != 0 && f & MDB_DUPFIXED == 0 && !(4..=511).contains(&self.valsize) {
            return Err("DUPSORT values must be 4..511 bytes (--valsize)");
        }
        if f & MDB_DUPSORT != 0 && self.big != 0 {
            return Err("--big cannot be used with DUPSORT (dup data is limited to 511 bytes)");
        }
        if f & MDB_DUPSORT == 0 && self.dups > 1 {
            return Err("--dups needs --dupsort, --dupfixed or --intdup");
        }
        if f & MDB_INTEGERKEY != 0 && f & MDB_REVERSEKEY != 0 {
            return Err("--intkey and --revkey are exclusive");
        }
        Ok(())
    }

    /// Is this a DUPSORT data set?
    pub fn is_dupsort(&self) -> bool {
        self.flags & MDB_DUPSORT != 0
    }

    /// The DB name as printed in outputs: `-` for the main DB.
    pub fn label(&self) -> &str {
        self.db.as_deref().unwrap_or("-")
    }

    /// Per-key hash; the bijection makes keys unique for `i < 2^32`.
    fn key_hash(&self, i: u32) -> u32 {
        fmix32(i.wrapping_add(self.seed.wrapping_mul(0x9e37_79b9)))
    }

    /// `ds_key`: `"%08x.%u"` (10 to 19 bytes, sorts by the hash), or with
    /// `--intkey` a native-endian `u64` with the hash in the high half and
    /// `i` in the low half.
    pub fn key(&self, i: u32) -> Vec<u8> {
        if self.flags & MDB_INTEGERKEY != 0 {
            let k = (u64::from(self.key_hash(i)) << 32) | u64::from(i);
            return k.to_ne_bytes().to_vec();
        }
        format!("{:08x}.{}", self.key_hash(i), i).into_bytes()
    }

    /// `ds_ndups`: number of values of key `i`.
    pub fn ndups(&self, i: u32) -> u32 {
        if self.flags & MDB_DUPSORT == 0 || self.dups <= 1 {
            return 1;
        }
        1 + fmix32(i ^ self.seed.wrapping_mul(0x85eb_ca6b)) % self.dups
    }

    /// `value_tag`: distinct for distinct `d` (XOR with `d` is a bijection),
    /// so the values of one key never collide; also the value's sort prefix.
    fn value_tag(&self, i: u32, d: u32) -> u32 {
        fmix32(i.wrapping_mul(0x9e37_79b9).wrapping_add(self.seed)) ^ d
    }

    /// `ds_vlen`: length of value `d` of key `i`.
    pub fn vlen(&self, i: u32, d: u32) -> usize {
        let f = self.flags;
        if f & (MDB_INTEGERDUP | MDB_DUPFIXED) != 0 {
            return self.valsize as usize;
        }
        if f & MDB_DUPSORT != 0 {
            return (4 + fmix32(self.value_tag(i, d)) % (self.valsize - 3)) as usize;
        }
        if self.big != 0 && i % 3 == 0 {
            return self.big as usize;
        }
        self.valsize as usize
    }

    /// `ds_value`: value `d` of key `i` (`vlen(i, d)` bytes).
    ///
    /// `--intdup`: a native `u64` `tag << 32 | d`, or the native `u32` tag.
    /// `MDB_DUPFIXED` values under 4 bytes: the low bytes of the tag,
    /// big-endian (distinct: `d` only changes those, and `--dups` is
    /// limited to fit). Otherwise the tag as 4 big-endian bytes (truncated
    /// if shorter), then SplitMix64 words from
    /// `seed * 0x9e3779b97f4a7c15 ^ i << 16 ^ d`.
    pub fn value(&self, i: u32, d: u32) -> Vec<u8> {
        let len = self.vlen(i, d);
        let tag = self.value_tag(i, d);
        if self.flags & MDB_INTEGERDUP != 0 {
            return if len == 8 {
                ((u64::from(tag) << 32) | u64::from(d))
                    .to_ne_bytes()
                    .to_vec()
            } else {
                tag.to_ne_bytes().to_vec()
            };
        }
        if self.flags & MDB_DUPFIXED != 0 && len < 4 {
            return tag.to_be_bytes()[4 - len..].to_vec();
        }
        let mut buf = vec![0u8; len];
        let head = len.min(4);
        buf[..head].copy_from_slice(&tag.to_be_bytes()[..head]);
        let mut state = u64::from(self.seed).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (u64::from(i) << 16)
            ^ u64::from(d);
        splitmix_fill(&mut state, &mut buf[head..]);
        buf
    }

    /// `ds_present`: does item `(i, d)` survive `delete --every deleted_every`?
    pub fn present(&self, i: u32, d: u32) -> bool {
        let every = self.deleted_every;
        !delete_key(every, i) && !delete_dup(self, every, i, d)
    }
}

/// `ds_delete_key`: does `delete --every every` delete the whole key `i`?
pub fn delete_key(every: u32, i: u32) -> bool {
    every != 0 && i % every == 0
}

/// `ds_delete_dup`: does `delete --every every` delete value `d` of key `i`
/// one by one?
pub fn delete_dup(ds: &DataSet, every: u32, i: u32, d: u32) -> bool {
    every > 1 && ds.is_dupsort() && i % every == 1 && d % 2 == 0
}

// ---- Comparators: equivalents of mdb.c's, to sort the expected model ----------

/// A comparison function over keys or values.
pub type CmpFn = fn(&[u8], &[u8]) -> Ordering;

/// `mdb_cmp_memn`: bytes, then shorter first (slice order is exactly that).
fn cmp_memn(a: &[u8], b: &[u8]) -> Ordering {
    a.cmp(b)
}

/// `mdb_cmp_memnr`: bytes compared from the end, then shorter first.
fn cmp_memnr(a: &[u8], b: &[u8]) -> Ordering {
    // Lexicographic order of the reversed bytes puts a shorter (reversed)
    // prefix first, which is exactly C's "then shorter first".
    a.iter().rev().cmp(b.iter().rev())
}

/// `mdb_cmp_int`/`mdb_cmp_cint`/`mdb_cmp_long`: native unsigned integers of
/// 4 or 8 bytes (both sides have the same size in our data sets).
fn cmp_uint(a: &[u8], b: &[u8]) -> Ordering {
    fn num(v: &[u8]) -> u64 {
        match v.len() {
            8 => u64::from_ne_bytes(v.try_into().expect("8 bytes")),
            _ => u64::from(u32::from_ne_bytes(v[..4].try_into().expect("4 bytes"))),
        }
    }
    num(a).cmp(&num(b))
}

/// As `mdb_default_cmp()`: the key comparator for DB `flags`.
pub fn key_cmp(flags: u32) -> CmpFn {
    if flags & MDB_REVERSEKEY != 0 {
        cmp_memnr
    } else if flags & MDB_INTEGERKEY != 0 {
        cmp_uint
    } else {
        cmp_memn
    }
}

/// As `mdb_default_cmp()`: the data comparator of a DUPSORT DB.
pub fn dup_cmp(flags: u32) -> CmpFn {
    if flags & MDB_INTEGERDUP != 0 {
        cmp_uint
    } else if flags & MDB_REVERSEDUP != 0 {
        cmp_memnr
    } else {
        cmp_memn
    }
}

// ---- The expected model -------------------------------------------------------------

/// One key of the expected DB content (`struct model_item`).
#[derive(Debug)]
pub struct ModelItem {
    /// The item index the key was generated from.
    pub i: u32,
    /// The key.
    pub key: Vec<u8>,
    /// The surviving values, sorted by the dup comparator.
    pub vals: Vec<Vec<u8>>,
}

/// The expected content of a DB: items sorted by the DB's comparators
/// (`struct model`).
#[derive(Debug)]
pub struct Model {
    /// Keys present, in key order.
    pub items: Vec<ModelItem>,
    /// Key/value pairs present.
    pub nentries: usize,
    /// Indexes of keys that must be absent.
    pub absent: Vec<u32>,
}

impl Model {
    /// `model_build`.
    pub fn build(ds: &DataSet) -> Model {
        let mut items = Vec::with_capacity(ds.count as usize);
        let mut absent = Vec::new();
        let mut nentries = 0;
        let dcmp = dup_cmp(ds.flags);
        for i in 0..ds.count {
            let mut vals: Vec<Vec<u8>> = (0..ds.ndups(i))
                .filter(|&d| ds.present(i, d))
                .map(|d| ds.value(i, d))
                .collect();
            if vals.is_empty() {
                absent.push(i);
                continue;
            }
            vals.sort_by(|a, b| dcmp(a, b));
            nentries += vals.len();
            items.push(ModelItem {
                i,
                key: ds.key(i),
                vals,
            });
        }
        let kcmp = key_cmp(ds.flags);
        items.sort_by(|a, b| kcmp(&a.key, &b.key));
        Model {
            items,
            nentries,
            absent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(seed: u32, flags: u32) -> DataSet {
        let mut ds = DataSet {
            count: 10,
            seed,
            flags,
            ..DataSet::default()
        };
        ds.finish().unwrap();
        ds
    }

    #[test]
    fn pinned_vectors() {
        // README.md, "Test vectors".
        let d = ds(1, 0);
        assert_eq!(d.key(0), b"92ca2f0e.0");
        let d = ds(1, MDB_INTEGERKEY);
        // A native integer: these are the bytes on little-endian hosts.
        assert_eq!(d.key(3), 0xed81_ded3_0000_0003u64.to_ne_bytes());
    }

    #[test]
    fn comparators() {
        assert_eq!(cmp_memnr(b"ab", b"b"), Ordering::Greater);
        assert_eq!(cmp_memnr(b"ba", b"ab"), Ordering::Less);
        assert_eq!(cmp_memnr(b"b", b"ab"), Ordering::Less);
        assert_eq!(
            cmp_uint(&2u64.to_ne_bytes(), &1u64.to_ne_bytes()),
            Ordering::Greater
        );
        assert_eq!(
            cmp_uint(&256u32.to_ne_bytes(), &255u32.to_ne_bytes()),
            Ordering::Greater
        );
    }
}
