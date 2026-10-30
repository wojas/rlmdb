//! The data generator shared (by construction) with `c/cbench.c`: the same
//! seeds and formulas produce the same keys, values, index streams and
//! permutations on both sides, so both implementations build identical
//! databases and do the same operations in the same order.
//!
//! * Keys are 16 bytes. Random keys: two big-endian `splitmix64` outputs of
//!   the key number; sequential keys: the big-endian key number followed by
//!   8 hash bytes (so they sort by number).
//! * Values are slices of a fixed pseudo-random pool of 192 KiB, at offset
//!   `(i * 64) & 0xffff` for item `i`: zero-copy on both sides.
//! * Index streams and permutations are `splitmix64(salt + j)` based, one
//!   salt per stream (`S_*`).

/// Length of every key.
pub const KEYLEN: usize = 16;
/// Length of an ordinary value.
pub const VALLEN: usize = 100;

const SEED_KEYS: u64 = 0x5eed_0000_0000_0001;
const SEED_POOL: u64 = 0x5eed_0000_0000_0002;
const SEED_DUPS: u64 = 0x5eed_0000_0000_0003;
/// Stream of `rand_get`.
pub const S_GET: u64 = 0x1000_0000_0000_0000;
/// Permutation of `rand_overwrite`.
pub const S_OVERWRITE: u64 = 0x2000_0000_0000_0000;
/// Permutation of `del_rand`.
pub const S_DELETE: u64 = 0x3000_0000_0000_0000;
/// Streams of `dup_get_both` (keys; `^ 1`: duplicates).
pub const S_DUP_GET: u64 = 0x4000_0000_0000_0000;
/// Permutation of `dup_next_dup`.
pub const S_DUP_SCAN: u64 = 0x5000_0000_0000_0000;
/// Stream of `get_4k`/`get_64k`.
pub const S_OVF_GET: u64 = 0x6000_0000_0000_0000;
/// Streams of `readers8_get` (`+ (thread << 48)`).
pub const S_READER: u64 = 0x7000_0000_0000_0000;
/// Stream of `mixed_writer`.
pub const S_MIXED: u64 = 0x8000_0000_0000_0000;

const POOL_SIZE: usize = 3 * 65536;

/// `splitmix64` used as a hash.
pub fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The value pool (little-endian `splitmix64(SEED_POOL + w)` words).
pub fn pool() -> Vec<u8> {
    (0..POOL_SIZE as u64 / 8)
        .flat_map(|w| splitmix64(SEED_POOL + w).to_le_bytes())
        .collect()
}

/// Offset of the value bytes of item `i` in the pool.
#[inline]
pub fn val_off(i: u64) -> usize {
    ((i * 64) & 0xffff) as usize
}

/// `len` value bytes of item `i`.
#[inline]
pub fn value(pool: &[u8], i: u64, len: usize) -> &[u8] {
    let off = val_off(i);
    &pool[off..off + len]
}

/// `n` random keys.
pub fn rand_keys(n: usize) -> Vec<[u8; KEYLEN]> {
    (0..n as u64)
        .map(|i| {
            key2(
                splitmix64(SEED_KEYS + 2 * i),
                splitmix64(SEED_KEYS + 2 * i + 1),
            )
        })
        .collect()
}

/// `n` keys in ascending order.
pub fn seq_keys(n: usize) -> Vec<[u8; KEYLEN]> {
    (0..n as u64).map(|i| key2(i, splitmix64(i))).collect()
}

fn key2(a: u64, b: u64) -> [u8; KEYLEN] {
    let mut k = [0u8; KEYLEN];
    k[..8].copy_from_slice(&a.to_be_bytes());
    k[8..].copy_from_slice(&b.to_be_bytes());
    k
}

/// `count` indices in `[0, n)` from stream `salt`.
pub fn rand_idx(salt: u64, count: usize, n: usize) -> Vec<u32> {
    (0..count as u64)
        .map(|j| (splitmix64(salt.wrapping_add(j)) % n as u64) as u32)
        .collect()
}

/// A permutation of `[0, n)` (Fisher-Yates from stream `salt`).
pub fn perm(salt: u64, n: usize) -> Vec<u32> {
    let mut p: Vec<u32> = (0..n as u32).collect();
    for j in (1..n).rev() {
        let k = (splitmix64(salt.wrapping_add(j as u64)) % (j as u64 + 1)) as usize;
        p.swap(j, k);
    }
    p
}

/// Duplicate `j` of `DUP_SORT` key `i` (100 bytes) into `out`.
#[inline]
pub fn dup_data(pool: &[u8], i: u64, j: u64, out: &mut [u8; VALLEN]) {
    out[..8].copy_from_slice(&splitmix64(SEED_DUPS + i * 16 + j).to_be_bytes());
    out[8..].copy_from_slice(value(pool, i * 16 + j, VALLEN - 8));
}

/// FNV-1a over `bytes`, continuing from `h`.
fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// A hash of everything the generator produces for `n` (the `datacheck`
/// group of `cbench.c` computes the same; `lmdb-bench --check-data`
/// compares the two).
pub fn datacheck(n: usize) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325;
    h = fnv(h, rand_keys(n).as_flattened());
    h = fnv(h, seq_keys(n).as_flattened());
    h = fnv(h, &pool());
    let words = |v: Vec<u32>| v.iter().flat_map(|w| w.to_ne_bytes()).collect::<Vec<u8>>();
    h = fnv(h, &words(rand_idx(S_GET, n, n)));
    h = fnv(h, &words(perm(S_DELETE, n)));
    let pool = pool();
    let mut buf = [0u8; VALLEN];
    for i in 0..100 {
        for j in 0..10 {
            dup_data(&pool, i, j, &mut buf);
            h = fnv(h, &buf);
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_is_stable() {
        // Pinned so that a change here (which would also need one in
        // cbench.c) does not go unnoticed.
        assert_eq!(splitmix64(0), 0xe220_a839_7b1d_cdaf);
        assert_eq!(seq_keys(3)[2][..8], 2u64.to_be_bytes());
        let p = perm(S_DELETE, 100);
        let mut sorted = p.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..100).collect::<Vec<u32>>());
        assert!(rand_idx(S_GET, 1000, 7).iter().all(|&i| i < 7));
    }
}
