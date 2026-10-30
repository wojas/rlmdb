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
//! * The `ls` group (Lightning Stream style records) has a data set of its
//!   own, generated record by record from the record number (`ls_key`,
//!   `ls_value`): DNS-name-like keys of 20-40 bytes and values made of a
//!   24-byte Lightning Stream header and a 40-100 byte payload from the
//!   pool.

/// Length of every key.
pub const KEYLEN: usize = 16;
/// Length of an ordinary value.
pub const VALLEN: usize = 100;

const SEED_KEYS: u64 = 0x5eed_0000_0000_0001;
const SEED_POOL: u64 = 0x5eed_0000_0000_0002;
const SEED_DUPS: u64 = 0x5eed_0000_0000_0003;
const SEED_LS_KEYS: u64 = 0x5eed_0000_0000_0004;
const SEED_LS_HDR: u64 = 0x5eed_0000_0000_0005;
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
/// Stream of `ls_update`.
pub const S_LS_UPDATE: u64 = 0x9000_0000_0000_0000;

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

/// Records of the `ls` data set per key of `--n`.
pub const LS_SCALE: usize = 5;
/// Length of the Lightning Stream header that starts every `ls` value:
/// timestamp of the last change (big-endian nanoseconds since the Unix
/// epoch), transaction id (big-endian), version (0), number of extra
/// 8-byte blocks (0), flags (2 bytes, bit 0: deleted), 4 bytes padding.
pub const LS_HDRLEN: usize = 24;
/// Offset of the transaction id in the header.
pub const LS_TXNID: usize = 8;
/// Longest `ls` key.
pub const LS_KEYMAX: usize = 40;
/// Longest `ls` value (header and payload).
pub const LS_VALMAX: usize = LS_HDRLEN + 100;
/// The transaction id of the last sync: every record of a freshly built
/// `ls` data set has a header transaction id at or below it, so a scan for
/// changed records (`ls_scan`) finds none.
pub const LS_SYNC_TXNID: u64 = 1_000_000;
/// Timestamp of transaction id 0, in nanoseconds since the Unix epoch.
const LS_TS_BASE: u64 = 1_700_000_000_000_000_000;
/// Nanoseconds between two transaction ids: the timestamp of a header
/// lies in its transaction id's window.
const LS_NS_PER_TXN: u64 = 2_000_000_000;
const LS_TLDS: [&[u8]; 8] = [b"com", b"net", b"org", b"nl", b"de", b"uk", b"io", b"eu"];
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The shape word of `ls` record `i`: key length, top-level domain, zone
/// label length, payload length.
#[inline]
fn ls_shape(i: u64) -> u64 {
    splitmix64(SEED_LS_KEYS + 64 * i)
}

/// The key of `ls` record `i` into `out`; returns its length (20-40
/// bytes). The key is `<host>.<zone>.<tld>`: the host label is random
/// letters followed by `i` in 6 base-36 digits, so keys are unique for
/// `i < 36^6` (the host label ends at the first dot) and sort in random
/// order. Letter `j` is `splitmix64(SEED_LS_KEYS + 64 * i + 1 + j)`.
pub fn ls_key(i: u64, out: &mut [u8; LS_KEYMAX]) -> usize {
    let shape = ls_shape(i);
    let len = 20 + (shape % 21) as usize;
    let tld = LS_TLDS[((shape >> 16) % 8) as usize];
    let letters = len - 8 - tld.len();
    let zone = 3 + ((shape >> 24) % (letters as u64 - 3)) as usize;
    let host = letters - zone;
    let letter = |j: usize| b'a' + (splitmix64(SEED_LS_KEYS + 64 * i + 1 + j as u64) % 26) as u8;
    for (j, b) in out[..host].iter_mut().enumerate() {
        *b = letter(j);
    }
    let mut v = i;
    for b in out[host..host + 6].iter_mut().rev() {
        *b = BASE36[(v % 36) as usize];
        v /= 36;
    }
    out[host + 6] = b'.';
    for (j, b) in out[host + 7..host + 7 + zone].iter_mut().enumerate() {
        *b = letter(host + j);
    }
    out[len - tld.len() - 1] = b'.';
    out[len - tld.len()..len].copy_from_slice(tld);
    len
}

/// Payload length of `ls` record `i` (40-100 bytes; the same for every
/// value the record gets).
#[inline]
fn ls_payload_len(i: u64) -> usize {
    40 + ((ls_shape(i) >> 40) % 61) as usize
}

/// A Lightning Stream header with timestamp `ts` and transaction id
/// `txnid`, flags 0, into `out[..LS_HDRLEN]`.
fn ls_header(out: &mut [u8], ts: u64, txnid: u64) {
    out[..8].copy_from_slice(&ts.to_be_bytes());
    out[LS_TXNID..LS_TXNID + 8].copy_from_slice(&txnid.to_be_bytes());
    out[16..LS_HDRLEN].fill(0);
}

/// The value of `ls` record `i` as built (into `out`; returns its
/// length): transaction id `1 + h % LS_SYNC_TXNID` (all at or below the
/// last sync) and a timestamp in that transaction's window, where `h` is
/// `splitmix64(SEED_LS_HDR + i)`; payload from the pool at `val_off(i)`.
pub fn ls_value(pool: &[u8], i: u64, out: &mut [u8; LS_VALMAX]) -> usize {
    let h = splitmix64(SEED_LS_HDR + i);
    let txnid = 1 + h % LS_SYNC_TXNID;
    let ts = LS_TS_BASE + txnid * LS_NS_PER_TXN + (h >> 32) % LS_NS_PER_TXN;
    ls_header(out, ts, txnid);
    let plen = ls_payload_len(i);
    out[LS_HDRLEN..LS_HDRLEN + plen].copy_from_slice(value(pool, i, plen));
    LS_HDRLEN + plen
}

/// The new value of `ls` record `i` written by put `j` of repetition `it`
/// of `ls_update` (into `out`; returns its length): transaction id
/// `LS_SYNC_TXNID + 1 + it`, timestamp `j` ns into that transaction's
/// window, the same payload length as before, payload at
/// `val_off(i + it + 1)`.
pub fn ls_update_value(pool: &[u8], i: u64, it: u64, j: u64, out: &mut [u8; LS_VALMAX]) -> usize {
    let txnid = LS_SYNC_TXNID + 1 + it;
    ls_header(out, LS_TS_BASE + txnid * LS_NS_PER_TXN + j, txnid);
    let plen = ls_payload_len(i);
    out[LS_HDRLEN..LS_HDRLEN + plen].copy_from_slice(value(pool, i + it + 1, plen));
    LS_HDRLEN + plen
}

/// The transaction id in the Lightning Stream header of `v`, or `None` if
/// `v` is too short to hold a header.
#[inline]
pub fn ls_txnid(v: &[u8]) -> Option<u64> {
    if v.len() < LS_HDRLEN {
        return None;
    }
    let b = v[LS_TXNID..LS_TXNID + 8].try_into().ok()?;
    Some(u64::from_be_bytes(b))
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
    let words = |v: &[u32]| v.iter().flat_map(|w| w.to_ne_bytes()).collect::<Vec<u8>>();
    h = fnv(h, &words(&rand_idx(S_GET, n, n)));
    h = fnv(h, &words(&perm(S_DELETE, n)));
    let pool = pool();
    let mut buf = [0u8; VALLEN];
    for i in 0..100 {
        for j in 0..10 {
            dup_data(&pool, i, j, &mut buf);
            h = fnv(h, &buf);
        }
    }
    // The `ls` data set: every key and value (each preceded by its length
    // byte), the stream of `ls_update` and the values of its first rep.
    let nls = LS_SCALE * n;
    let (mut k, mut v) = ([0u8; LS_KEYMAX], [0u8; LS_VALMAX]);
    for i in 0..nls as u64 {
        let kl = ls_key(i, &mut k);
        h = fnv(h, &[kl as u8]);
        h = fnv(h, &k[..kl]);
        let vl = ls_value(&pool, i, &mut v);
        h = fnv(h, &[vl as u8]);
        h = fnv(h, &v[..vl]);
    }
    let upd = rand_idx(S_LS_UPDATE, nls / 100, nls);
    h = fnv(h, &words(&upd));
    for (j, &i) in upd.iter().enumerate() {
        let vl = ls_update_value(&pool, u64::from(i), 0, j as u64, &mut v);
        h = fnv(h, &v[..vl]);
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

    #[test]
    fn ls_records() {
        let pool = pool();
        let (mut k, mut v) = ([0u8; LS_KEYMAX], [0u8; LS_VALMAX]);
        let mut keys = std::collections::HashSet::new();
        for i in 0..100_000 {
            let kl = ls_key(i, &mut k);
            assert!((20..=LS_KEYMAX).contains(&kl), "key length {kl}");
            assert!(k[..kl]
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'.'));
            assert!(keys.insert(k[..kl].to_vec()), "duplicate key {i}");
            let vl = ls_value(&pool, i, &mut v);
            assert!(
                (LS_HDRLEN + 40..=LS_VALMAX).contains(&vl),
                "value length {vl}"
            );
            let txnid = ls_txnid(&v[..vl]).expect("header");
            assert!((1..=LS_SYNC_TXNID).contains(&txnid));
            assert_eq!(ls_update_value(&pool, i, 3, 7, &mut v), vl);
            assert_eq!(ls_txnid(&v[..vl]), Some(LS_SYNC_TXNID + 4));
        }
        assert_eq!(ls_txnid(&[0; LS_HDRLEN - 1]), None);
    }
}
