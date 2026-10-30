//! Key and data comparators: exact ports of `mdb_cmp_memn`, `mdb_cmp_memnr`,
//! `mdb_cmp_long`, `mdb_cmp_int` and `mdb_cmp_cint` (mdb.c lines 5321-5405),
//! and the default selection of `mdb_default_cmp` (line 9940).
//!
//! The sort order of every B-tree is defined by these functions, so a Rust
//! process and a C process must agree bit for bit, including how keys of
//! different lengths compare. Where the C code would read past the end of a
//! too-short operand (integer comparators handed odd or mismatched sizes,
//! which `mdb_put` does not forbid), we treat the missing bytes as zero
//! instead of reading out of bounds; for well-formed integer keys (all 4 or
//! all 8 bytes) the result is identical to C.

use std::cmp::Ordering;

use crate::consts::{
    MDB_DUPFIXED, MDB_DUPSORT, MDB_INTEGERDUP, MDB_INTEGERKEY, MDB_REVERSEDUP, MDB_REVERSEKEY,
};

/// A user-supplied comparison function (`mdb_set_compare`, `mdb_set_dupsort`).
pub type CmpFn = fn(&[u8], &[u8]) -> Ordering;

/// One of LMDB's built-in comparators, or a user function.
#[derive(Clone, Copy)]
pub enum Comparator {
    /// `mdb_cmp_memn`: lexicographic, shorter is smaller on a common prefix.
    Memn,
    /// `mdb_cmp_memnr`: lexicographic from the last byte backwards.
    Memnr,
    /// `mdb_cmp_long`: native `size_t` (u64) comparison. Used for the free DB.
    Long,
    /// `mdb_cmp_int`: native `unsigned int` comparison of the first 4 bytes.
    /// The default for `INTEGERDUP|DUPFIXED`; the cursor code switches to
    /// [`Comparator::Clong`] for 8-byte items exactly where C does
    /// (`mdb_xcursor_init1`, `mdb_cursor_set`, `mdb_dcmp`).
    Int,
    /// `mdb_cmp_clong`: `size_t`s of unknown alignment. C defines it as
    /// `mdb_cmp_long` on x86 (`MISALIGNED_OK`) and as `mdb_cmp_cint`
    /// elsewhere; the two agree on 8-byte operands.
    Clong,
    /// `mdb_cmp_cint`: native unsigned integer of the size of the first
    /// operand, compared 16 bits at a time from the most significant end.
    /// Used for `INTEGERKEY` and `INTEGERDUP` without `DUPFIXED`.
    Cint,
    /// A user-supplied function.
    Custom(CmpFn),
}

impl std::fmt::Debug for Comparator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Comparator::Memn => "Memn",
            Comparator::Memnr => "Memnr",
            Comparator::Long => "Long",
            Comparator::Int => "Int",
            Comparator::Clong => "Clong",
            Comparator::Cint => "Cint",
            Comparator::Custom(_) => "Custom",
        })
    }
}

impl Comparator {
    /// Compares `a` (the search key) with `b` (a key stored in a page).
    #[inline]
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        match self {
            Comparator::Memn => cmp_memn(a, b),
            Comparator::Memnr => cmp_memnr(a, b),
            Comparator::Long => cmp_long(a, b),
            Comparator::Int => cmp_int(a, b),
            Comparator::Clong => cmp_clong(a, b),
            Comparator::Cint => cmp_cint(a, b),
            Comparator::Custom(f) => f(a, b),
        }
    }

    /// Compares `a` with the stored key `b`, where `b_tail` is `b` followed
    /// by whatever bytes come after it in its page.
    ///
    /// The integer comparators read as many bytes of `b` as *they* need
    /// (`a.len()` for `cint`, 4 or 8 for `int`/`long`), not `b.len()`; when
    /// `a` is longer than the stored key C therefore reads the bytes that
    /// follow the key in the page (usually the node's data). Using the tail
    /// reproduces that exactly instead of padding with zeros.
    #[inline]
    pub fn compare_in_page(&self, a: &[u8], b: &[u8], b_tail: &[u8]) -> Ordering {
        match self {
            Comparator::Long | Comparator::Int | Comparator::Clong | Comparator::Cint => {
                self.compare(a, if b_tail.len() > b.len() { b_tail } else { b })
            }
            _ => self.compare(a, b),
        }
    }

    /// `true` for `mdb_cmp_int`.
    pub fn is_int(&self) -> bool {
        matches!(self, Comparator::Int)
    }

    /// `true` for `mdb_cmp_cint`.
    pub fn is_cint(&self) -> bool {
        matches!(self, Comparator::Cint)
    }

    /// The key comparator `mdb_default_cmp` picks for DB flags `flags`.
    pub fn default_key(flags: u32) -> Comparator {
        if flags & MDB_REVERSEKEY != 0 {
            Comparator::Memnr
        } else if flags & MDB_INTEGERKEY != 0 {
            Comparator::Cint
        } else {
            Comparator::Memn
        }
    }

    /// The duplicate-data comparator `mdb_default_cmp` picks for DB flags
    /// `flags`, or `None` if the DB is not `MDB_DUPSORT`.
    pub fn default_dup(flags: u32) -> Option<Comparator> {
        if flags & MDB_DUPSORT == 0 {
            return None;
        }
        Some(if flags & MDB_INTEGERDUP != 0 {
            if flags & MDB_DUPFIXED != 0 {
                Comparator::Int
            } else {
                Comparator::Cint
            }
        } else if flags & MDB_REVERSEDUP != 0 {
            Comparator::Memnr
        } else {
            Comparator::Memn
        })
    }
}

/// Byte `i` of `buf`, or 0 past either end (`i` may be negative).
#[inline]
fn byte_or_zero(buf: &[u8], i: isize) -> u8 {
    if i < 0 {
        0
    } else {
        buf.get(i as usize).copied().unwrap_or(0)
    }
}

/// The 16-bit word at byte offset `i` read in host order, with
/// out-of-range bytes as zero.
#[inline]
fn word_at(buf: &[u8], i: isize) -> u16 {
    u16::from_ne_bytes([byte_or_zero(buf, i), byte_or_zero(buf, i + 1)])
}

/// `mdb_cmp_memn`: `memcmp` over the common length, then the shorter
/// operand sorts first.
#[inline]
pub fn cmp_memn(a: &[u8], b: &[u8]) -> Ordering {
    let common = a.len().min(b.len());
    if common > MEMN_INLINE_MAX {
        // Slice `Ord` for `[u8]` is exactly memcmp + length tie-break.
        return a.cmp(b);
    }
    // Short keys are compared here, eight bytes at a time as big-endian
    // integers (which order exactly like memcmp), then byte by byte. The
    // binary searches compare the search key with a dozen keys per page,
    // and a call into the C library's memcmp costs more than such a
    // comparison and makes the search loop spill its registers.
    let (a8, b8) = (&a[..common], &b[..common]);
    let mut words_a = a8.chunks_exact(8);
    let mut words_b = b8.chunks_exact(8);
    for (x, y) in (&mut words_a).zip(&mut words_b) {
        let x = u64::from_be_bytes(x.try_into().unwrap_or_default());
        let y = u64::from_be_bytes(y.try_into().unwrap_or_default());
        if x != y {
            return x.cmp(&y);
        }
    }
    for (x, y) in words_a.remainder().iter().zip(words_b.remainder()) {
        if x != y {
            return x.cmp(y);
        }
    }
    a.len().cmp(&b.len())
}

/// Longest common length [`cmp_memn`] compares itself; longer operands go
/// to memcmp, which is faster on long common prefixes.
const MEMN_INLINE_MAX: usize = 32;

/// `mdb_cmp_memnr`: compare bytes from the end backwards over the common
/// length, then the shorter operand sorts first.
#[inline]
pub fn cmp_memnr(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        match x.cmp(y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    a.len().cmp(&b.len())
}

/// `mdb_cmp_long`: compares the first 8 bytes of each operand as native
/// `u64`s (missing bytes count as zero).
#[inline]
pub fn cmp_long(a: &[u8], b: &[u8]) -> Ordering {
    native_u64(a).cmp(&native_u64(b))
}

/// `mdb_cmp_int`: compares the first 4 bytes as native `u32`s.
#[inline]
pub fn cmp_int(a: &[u8], b: &[u8]) -> Ordering {
    native_u32(a).cmp(&native_u32(b))
}

/// `mdb_cmp_clong`: `mdb_cmp_long` on x86, `mdb_cmp_cint` elsewhere.
#[inline]
pub fn cmp_clong(a: &[u8], b: &[u8]) -> Ordering {
    if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        cmp_long(a, b)
    } else {
        cmp_cint(a, b)
    }
}

#[inline]
fn native_u64(buf: &[u8]) -> u64 {
    match buf.get(..8) {
        Some(b) => u64::from_ne_bytes(b.try_into().unwrap_or([0; 8])),
        None => {
            let mut tmp = [0u8; 8];
            tmp[..buf.len()].copy_from_slice(buf);
            u64::from_ne_bytes(tmp)
        }
    }
}

#[inline]
fn native_u32(buf: &[u8]) -> u32 {
    match buf.get(..4) {
        Some(b) => u32::from_ne_bytes(b.try_into().unwrap_or([0; 4])),
        None => {
            let mut tmp = [0u8; 4];
            tmp[..buf.len()].copy_from_slice(buf);
            u32::from_ne_bytes(tmp)
        }
    }
}

/// `mdb_cmp_cint`: both operands are taken to be `a.len()` bytes long and
/// compared as unsigned 16-bit words starting from the most significant
/// word (the last one on little-endian hosts, the first on big-endian
/// hosts). For 4- and 8-byte operands this equals a native integer
/// comparison.
#[inline]
pub fn cmp_cint(a: &[u8], b: &[u8]) -> Ordering {
    // Fast paths for the sizes INTEGERKEY is meant for.
    if a.len() == b.len() {
        if a.len() == 8 {
            return cmp_long(a, b);
        }
        if a.len() == 4 {
            return cmp_int(a, b);
        }
    }
    let len = a.len() as isize;
    if cfg!(target_endian = "little") {
        // u = a + size; do { x = *--u - *--c; } while (!x && u > a);
        let mut u = len;
        loop {
            u -= 2;
            match word_at(a, u).cmp(&word_at(b, u)) {
                Ordering::Equal if u > 0 => continue,
                other => return other,
            }
        }
    } else {
        // u = a; do { x = *u++ - *c++; } while (!x && u < end);
        let mut u = 0;
        loop {
            let r = word_at(a, u).cmp(&word_at(b, u));
            u += 2;
            if r != Ordering::Equal || u >= len {
                return r;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Ordering::*;

    #[test]
    fn memn_lengths() {
        assert_eq!(cmp_memn(b"abc", b"abd"), Less);
        assert_eq!(cmp_memn(b"ab", b"abc"), Less);
        assert_eq!(cmp_memn(b"abc", b"ab"), Greater);
        assert_eq!(cmp_memn(b"abc", b"abc"), Equal);
        assert_eq!(cmp_memn(b"", b"a"), Less);
        assert_eq!(cmp_memn(b"b", b"abc"), Greater);
        assert_eq!(cmp_memn(&[0xff], &[0x01, 0x00]), Greater);
    }

    #[test]
    fn memn_is_memcmp_order() {
        // Every pair of lengths up to twice the inline limit, sharing
        // every possible prefix, random bytes after it: the same order as
        // slices (memcmp, then length).
        let mut seed = 0x5eed_u64;
        let mut byte = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 56) as u8
        };
        for la in 0..=2 * MEMN_INLINE_MAX + 3 {
            let a: Vec<u8> = (0..la).map(|_| byte()).collect();
            for lb in 0..=2 * MEMN_INLINE_MAX + 3 {
                for shared in 0..=la.min(lb) {
                    let mut b: Vec<u8> = a[..shared].to_vec();
                    b.extend((shared..lb).map(|_| byte()));
                    assert_eq!(cmp_memn(&a, &b), a.cmp(&b), "{a:?} {b:?}");
                    assert_eq!(cmp_memn(&b, &a), b.cmp(&a), "{b:?} {a:?}");
                }
            }
        }
    }

    #[test]
    fn memnr_compares_from_the_end() {
        assert_eq!(cmp_memnr(b"xa", b"ab"), Less); // 'a' < 'b' at the end
        assert_eq!(cmp_memnr(b"ba", b"aa"), Greater);
        // Common suffix: shorter sorts first.
        assert_eq!(cmp_memnr(b"bc", b"abc"), Less);
        assert_eq!(cmp_memnr(b"abc", b"bc"), Greater);
        assert_eq!(cmp_memnr(b"abc", b"abc"), Equal);
        // Different suffixes decide before length.
        assert_eq!(cmp_memnr(b"zzzb", b"a"), Greater);
    }

    #[test]
    fn integer_comparators_are_native() {
        let k = |v: u32| v.to_ne_bytes();
        let l = |v: u64| v.to_ne_bytes();
        assert_eq!(cmp_int(&k(1), &k(256)), Less);
        assert_eq!(cmp_cint(&k(1), &k(256)), Less);
        assert_eq!(cmp_cint(&k(0x0001_0000), &k(0x0000_ffff)), Greater);
        assert_eq!(cmp_long(&l(1 << 40), &l(1 << 39)), Greater);
        assert_eq!(cmp_cint(&l(1 << 40), &l(1 << 39)), Greater);
        assert_eq!(cmp_cint(&l(u64::MAX), &l(u64::MAX)), Equal);
        // Int looks at 4 bytes only; Clong at 8.
        let int_of_long = if cfg!(target_endian = "little") {
            Less
        } else {
            Greater
        };
        assert_eq!(Comparator::Int.compare(&l(1 << 33), &l(1)), int_of_long);
        assert_eq!(Comparator::Int.compare(&k(7), &k(9)), Less);
        assert_eq!(Comparator::Clong.compare(&l(1 << 33), &l(1)), Greater);
        assert_eq!(Comparator::Clong.compare(&l(5), &l(5)), Equal);
    }

    /// Literal port of the little-endian `mdb_cmp_cint` loop over padded
    /// buffers, to check the generic path on even sizes other than 4/8.
    fn c_cint_le(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len() / 2;
        let w = |buf: &[u8], i: usize| -> i32 {
            i32::from(u16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]))
        };
        let mut i = n;
        loop {
            i -= 1;
            let x = w(a, i) - w(b, i);
            if x != 0 || i == 0 {
                return x;
            }
        }
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn cint_generic_matches_c_on_le() {
        let cases: [(&[u8], &[u8]); 5] = [
            (&[1, 0, 0, 0, 0, 0], &[0, 0, 0, 0, 0, 1]),
            (&[0, 0, 0, 0, 0, 1], &[1, 0, 0, 0, 0, 0]),
            (&[9, 9, 2, 0, 7, 7], &[1, 1, 3, 0, 7, 7]),
            (&[5, 0], &[4, 0]),
            (&[3, 4, 5, 6, 7, 8], &[3, 4, 5, 6, 7, 8]),
        ];
        for (a, b) in cases {
            assert_eq!(cmp_cint(a, b), c_cint_le(a, b).cmp(&0), "{a:?} vs {b:?}");
        }
        // Only a.len() bytes of b take part (b longer than a).
        assert_eq!(cmp_cint(&[1, 0], &[1, 0, 9, 9]), Equal);
        // b shorter than a: missing bytes count as zero...
        assert_eq!(cmp_cint(&[0, 0, 0, 0, 0, 1], &[5, 0]), Greater);
        // ...unless the bytes after the key in its page are known, which C
        // would read: here the tail makes b's top word 7 > 1.
        let tail = [5u8, 0, 0, 0, 0, 7];
        assert_eq!(
            Comparator::Cint.compare_in_page(&[0, 0, 0, 0, 0, 1], &tail[..2], &tail),
            Less
        );
        assert_eq!(
            Comparator::Memn.compare_in_page(b"ab", b"a", b"abc"),
            Greater
        );
        // Odd sizes do not panic.
        let _ = cmp_cint(&[1, 2, 3], &[1, 2, 4]);
        let _ = cmp_cint(&[], &[]);
    }

    #[test]
    fn default_selection() {
        let name = |c: Comparator| format!("{c:?}");
        assert_eq!(name(Comparator::default_key(0)), "Memn");
        assert_eq!(name(Comparator::default_key(MDB_INTEGERKEY)), "Cint");
        assert_eq!(
            name(Comparator::default_key(MDB_REVERSEKEY | MDB_INTEGERKEY)),
            "Memnr"
        );
        assert!(Comparator::default_dup(MDB_INTEGERDUP).is_none());
        assert_eq!(name(Comparator::default_dup(MDB_DUPSORT).unwrap()), "Memn");
        assert_eq!(
            name(Comparator::default_dup(MDB_DUPSORT | MDB_INTEGERDUP).unwrap()),
            "Cint"
        );
        assert_eq!(
            name(Comparator::default_dup(MDB_DUPSORT | MDB_INTEGERDUP | MDB_DUPFIXED).unwrap()),
            "Int"
        );
        assert_eq!(
            name(Comparator::default_dup(MDB_DUPSORT | MDB_REVERSEDUP).unwrap()),
            "Memnr"
        );
        // INTEGERDUP wins over REVERSEDUP, as in C.
        assert_eq!(
            name(Comparator::default_dup(MDB_DUPSORT | MDB_REVERSEDUP | MDB_INTEGERDUP).unwrap()),
            "Cint"
        );
    }
}
