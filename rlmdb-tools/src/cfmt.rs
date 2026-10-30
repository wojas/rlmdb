//! What the C tools get from the C library: `printf("%p")`, the `<ctype.h>`
//! classes of the "C" locale, `atoi`, `sscanf` number conversions, C string
//! termination and `mdb_strerror`.
//!
//! The tools never call `setlocale`, so they run in the "C" locale, where
//! `isprint` is exactly `0x20..=0x7e` (checked against both platforms'
//! C libraries by the tests).

use rlmdb::Error;

/// `mdb_strerror(code)`: LMDB's message for its own codes, the C library's
/// `strerror` text otherwise.
pub fn strerror(code: i32) -> String {
    rlmdb::strerror(code)
}

/// The C return code of an LMDB error (what the C function would return).
pub fn err_code(e: &Error) -> i32 {
    e.to_err_code()
}

/// The `errno` of an I/O error (`EIO` if it has none).
pub fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(5) // EIO
}

/// `printf("%p", addr)`: `0x` and lower-case hex; a null pointer is
/// `(nil)` with glibc and `0x0` elsewhere.
pub fn pointer(addr: u64) -> String {
    if addr == 0 && cfg!(target_os = "linux") {
        "(nil)".to_owned()
    } else {
        format!("{addr:#x}")
    }
}

/// `isprint` in the "C" locale.
pub fn is_print(c: u8) -> bool {
    (0x20..=0x7e).contains(&c)
}

/// `isxdigit` in the "C" locale.
pub fn is_xdigit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// `isspace` in the "C" locale (space, `\t`, `\n`, `\v`, `\f`, `\r`).
pub fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The bytes of a C string stored in `buf`: everything before the first
/// NUL byte.
pub fn cstr(buf: &[u8]) -> &[u8] {
    match buf.iter().position(|&b| b == 0) {
        Some(i) => &buf[..i],
        None => buf,
    }
}

/// Splits `s` like `strtoul`/`strtol` do: leading white space, an optional
/// sign, then digits of `radix` (with an optional `0x` prefix for 16).
/// Returns `(negative, digits)`, `digits` empty if there are none.
fn split_number(s: &[u8], radix: u32) -> (bool, &[u8]) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut negative = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        negative = s[i] == b'-';
        i += 1;
    }
    if radix == 16
        && s.len() > i + 2
        && s[i] == b'0'
        && (s[i + 1] | 0x20) == b'x'
        && is_xdigit(s[i + 2])
    {
        i += 2;
    }
    let start = i;
    while i < s.len() && (s[i] as char).is_digit(radix) {
        i += 1;
    }
    (negative, &s[start..i])
}

/// The magnitude of `digits` in `radix`, saturating at `u64::MAX` (as
/// `strtoull` does on overflow).
fn magnitude(digits: &[u8], radix: u32) -> u64 {
    digits.iter().fold(0u64, |acc, &d| {
        let v = u64::from((d as char).to_digit(radix).unwrap_or(0));
        acc.checked_mul(u64::from(radix))
            .and_then(|a| a.checked_add(v))
            .unwrap_or(u64::MAX)
    })
}

/// `atoi(s)`, i.e. `(int)strtol(s, NULL, 10)` on a 64-bit `long`.
pub fn atoi(s: &[u8]) -> i32 {
    let (negative, digits) = split_number(cstr(s), 10);
    let m = magnitude(digits, 10);
    let v: i64 = if negative {
        if m > i64::MAX as u64 {
            i64::MIN
        } else {
            -(m as i64)
        }
    } else {
        m.min(i64::MAX as u64) as i64
    };
    v as i32
}

/// `sscanf(s, "%zu", &v) == 1` (or `"%u"`, `"%p"` with `radix` 16): the
/// value, or `None` when there are no digits. A minus sign negates modulo
/// 2^64, overflow saturates, as `strtoull` does. Callers truncate for
/// narrower types.
pub fn scan_unsigned(s: &[u8], radix: u32) -> Option<u64> {
    let (negative, digits) = split_number(cstr(s), radix);
    if digits.is_empty() {
        return None;
    }
    let m = magnitude(digits, radix);
    Some(if negative { m.wrapping_neg() } else { m })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(atoi(b"3\n"), 3);
        assert_eq!(atoi(b"  -12x"), -12);
        assert_eq!(atoi(b"x"), 0);
        assert_eq!(atoi(b"4294967299"), 3);
        assert_eq!(scan_unsigned(b"1048576", 10), Some(1_048_576));
        assert_eq!(scan_unsigned(b" +7z", 10), Some(7));
        assert_eq!(scan_unsigned(b"-1", 10), Some(u64::MAX));
        assert_eq!(
            scan_unsigned(b"99999999999999999999999", 10),
            Some(u64::MAX)
        );
        assert_eq!(scan_unsigned(b"", 10), None);
        assert_eq!(scan_unsigned(b"abc", 10), None);
        assert_eq!(scan_unsigned(b"0x7f00", 16), Some(0x7f00));
        assert_eq!(scan_unsigned(b"7f00", 16), Some(0x7f00));
        assert_eq!(scan_unsigned(b"(nil)", 16), None);
    }

    #[test]
    fn pointers_and_classes() {
        assert_eq!(pointer(0x1000), "0x1000");
        assert!(is_print(b' ') && is_print(b'~') && !is_print(0x7f) && !is_print(0x80));
        assert_eq!(cstr(b"ab\0cd"), b"ab");
    }
}
