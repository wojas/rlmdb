//! The deterministic data set of the interop peers (`rust/interop/c/
//! dataset.c`, documented in `rust/interop/README.md`), so that Rust can
//! write exactly what `cpeer verify` checks. Cross-checked against
//! `cpeer gen` by the tests.

use rlmdb::consts::*;

/// `struct dataset` of `dataset.c`.
#[derive(Debug, Clone)]
pub struct Dataset {
    /// Named DB, or `None` for the main DB.
    pub db: Option<String>,
    pub count: u32,
    pub seed: u32,
    /// `MDB_*` database flags.
    pub flags: u32,
    pub valsize: usize,
    pub big: usize,
    pub dups: u32,
}

pub fn fmix32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl Dataset {
    pub fn new(db: Option<&str>, count: u32, seed: u32) -> Dataset {
        Dataset {
            db: db.map(str::to_owned),
            count,
            seed,
            flags: 0,
            valsize: 0,
            big: 0,
            dups: 0,
        }
    }

    pub fn with_flags(mut self, flags: u32) -> Self {
        self.flags |= flags;
        self
    }

    pub fn valsize(mut self, n: usize) -> Self {
        self.valsize = n;
        self
    }

    pub fn big(mut self, n: usize) -> Self {
        self.big = n;
        self
    }

    pub fn dups(mut self, n: u32) -> Self {
        self.dups = n;
        self
    }

    /// `ds_finish`: implied flags and defaults.
    pub fn finish(mut self) -> Self {
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
        self
    }

    /// The cpeer data options for this set.
    pub fn args(&self) -> Vec<String> {
        let f = self.flags;
        let mut a = Vec::new();
        if f & MDB_INTEGERDUP != 0 {
            a.push("--intdup".to_owned());
        } else if f & MDB_DUPFIXED != 0 {
            a.push("--dupfixed".to_owned());
        } else if f & MDB_REVERSEDUP != 0 {
            a.push("--revdup".to_owned());
        } else if f & MDB_DUPSORT != 0 {
            a.push("--dupsort".to_owned());
        }
        if f & MDB_INTEGERKEY != 0 {
            a.push("--intkey".to_owned());
        }
        if f & MDB_REVERSEKEY != 0 {
            a.push("--revkey".to_owned());
        }
        a.push("--valsize".to_owned());
        a.push(self.valsize.to_string());
        if self.big != 0 {
            a.push("--big".to_owned());
            a.push(self.big.to_string());
        }
        if f & MDB_DUPSORT != 0 {
            a.push("--dups".to_owned());
            a.push(self.dups.to_string());
        }
        a
    }

    /// `<db|-> <count> <seed> [options]` for cpeer commands.
    pub fn cmd_args(&self) -> Vec<String> {
        let mut a = vec![
            self.db.clone().unwrap_or_else(|| "-".to_owned()),
            self.count.to_string(),
            self.seed.to_string(),
        ];
        a.extend(self.args());
        a
    }

    fn key_hash(&self, i: u32) -> u32 {
        fmix32(i.wrapping_add(self.seed.wrapping_mul(0x9e37_79b9)))
    }

    pub fn key(&self, i: u32) -> Vec<u8> {
        if self.flags & MDB_INTEGERKEY != 0 {
            let k = (u64::from(self.key_hash(i)) << 32) | u64::from(i);
            return k.to_ne_bytes().to_vec();
        }
        format!("{:08x}.{}", self.key_hash(i), i).into_bytes()
    }

    pub fn ndups(&self, i: u32) -> u32 {
        if self.flags & MDB_DUPSORT == 0 || self.dups <= 1 {
            return 1;
        }
        1 + fmix32(i ^ self.seed.wrapping_mul(0x85eb_ca6b)) % self.dups
    }

    fn tag(&self, i: u32, d: u32) -> u32 {
        fmix32(i.wrapping_mul(0x9e37_79b9).wrapping_add(self.seed)) ^ d
    }

    pub fn vlen(&self, i: u32, d: u32) -> usize {
        let f = self.flags;
        if f & (MDB_INTEGERDUP | MDB_DUPFIXED) != 0 {
            return self.valsize;
        }
        if f & MDB_DUPSORT != 0 {
            return 4 + (fmix32(self.tag(i, d)) as usize) % (self.valsize - 3);
        }
        if self.big != 0 && i % 3 == 0 {
            return self.big;
        }
        self.valsize
    }

    pub fn value(&self, i: u32, d: u32) -> Vec<u8> {
        let len = self.vlen(i, d);
        let tag = self.tag(i, d);
        if self.flags & MDB_INTEGERDUP != 0 {
            if len == 8 {
                return ((u64::from(tag) << 32) | u64::from(d))
                    .to_ne_bytes()
                    .to_vec();
            }
            return tag.to_ne_bytes().to_vec();
        }
        let mut buf = Vec::with_capacity(len);
        for off in 0..4.min(len) {
            buf.push((tag >> (24 - 8 * off)) as u8);
        }
        let mut state = u64::from(self.seed).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ (u64::from(i) << 16)
            ^ u64::from(d);
        while buf.len() < len {
            let r = splitmix64(&mut state);
            for b in 0..8 {
                if buf.len() == len {
                    break;
                }
                buf.push((r >> (8 * b)) as u8);
            }
        }
        buf
    }

    /// The `rlmdb::DatabaseFlags` of the set.
    pub fn db_flags(&self) -> rlmdb::DatabaseFlags {
        rlmdb::DatabaseFlags::from_bits_retain(self.flags)
    }
}
