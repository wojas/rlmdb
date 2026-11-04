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

/// `strerror(code)` of the C runtime, where it differs from
/// [`strerror`]'s (`mdb_strerror`): for 0, which `mdb_strerror` calls
/// "Successful return: 0" (glibc says "Success", macOS "Undefined error:
/// 0", the Windows runtimes "No error"), and on Windows for the `errno`
/// values `mdb_strerror` takes for system error codes (only `ERANGE`,
/// "Result too large", is printed this way).
pub fn crt_strerror(code: i32) -> String {
    if cfg!(windows) {
        return match code {
            0 => "No error".to_owned(),
            34 => "Result too large".to_owned(),
            _ => strerror(code),
        };
    }
    if code != 0 {
        return strerror(code);
    }
    // The C library's text, as std formats an OS error: "<text> (os error
    // 0)".
    let text = std::io::Error::from_raw_os_error(0).to_string();
    match text.strip_suffix(" (os error 0)") {
        Some(t) => t.to_owned(),
        None => text,
    }
}

/// The C return code of an LMDB error (what the C function would return).
pub fn err_code(e: &Error) -> i32 {
    e.to_err_code()
}

/// The `errno` of an I/O error (`EIO` if it has none).
pub fn io_errno(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(5) // EIO
}

/// The `errno` the C runtime leaves when a file function of its own
/// (`fopen`, `freopen`) fails with `e`: the error itself on Unix; on
/// Windows the system error mapped as the C runtime's `_dosmaperr` does
/// (`ERROR_PATH_NOT_FOUND` is `ENOENT`, for example).
pub fn crt_errno(e: &std::io::Error) -> i32 {
    let code = io_errno(e);
    if cfg!(windows) {
        dos_errno(code)
    } else {
        code
    }
}

/// `_dosmaperr`'s table: the `errno` of a Windows system error code
/// (Windows' `errno` values), `EINVAL` for the codes it does not list.
fn dos_errno(code: i32) -> i32 {
    match code {
        2 | 3 | 15 | 18 | 53 | 67 | 161 | 206 => 2, // ENOENT
        4 => 24,                                    // EMFILE
        5 | 16 | 19..=36 | 65 | 82 | 83 | 108 | 132 | 158 | 167 => 13, // EACCES
        6 | 114 | 130 => 9,                         // EBADF
        7 | 8 | 9 | 1816 => 12,                     // ENOMEM
        10 => 7,                                    // E2BIG
        11 | 188..=202 => 8,                        // ENOEXEC
        17 => 18,                                   // EXDEV
        80 | 183 => 17,                             // EEXIST
        89 | 164 | 215 => 11,                       // EAGAIN
        109 => 32,                                  // EPIPE
        112 => 28,                                  // ENOSPC
        128 | 129 => 10,                            // ECHILD
        145 => 41,                                  // ENOTEMPTY
        _ => 22,                                    // EINVAL
    }
}

/// `printf("%p", addr)`: `0x` and lower-case hex; a null pointer is
/// `(nil)` with glibc and `0x0` elsewhere. On Windows 16 hex digits:
/// upper-case from MSVC's C runtime, lower-case from MinGW-w64's own
/// `printf`, which C99 programs on msvcrt (the runtime of Rust's
/// `x86_64-pc-windows-gnu`) get by default (`__USE_MINGW_ANSI_STDIO`).
pub fn pointer(addr: u64) -> String {
    if cfg!(all(windows, target_env = "msvc")) {
        format!("{addr:016X}")
    } else if cfg!(windows) {
        format!("{addr:016x}")
    } else if addr == 0 && cfg!(target_os = "linux") {
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

/// `atoi(s)` of the C runtime the C tools are built with, which differ
/// past the range of `int`: glibc and macOS compute `(int)strtol(s, NULL,
/// 10)` on a 64-bit `long` and MinGW's msvcrt accumulates in a wrapping
/// `int`, both keeping the low 32 bits (`99999999999` is `1215752191`);
/// MSVC's UCRT saturates at `INT_MAX`/`INT_MIN` (and sets `ERANGE`).
pub fn atoi(s: &[u8]) -> i32 {
    let (negative, digits) = split_number(cstr(s), 10);
    let m = magnitude(digits, 10);
    if cfg!(target_env = "msvc") {
        let limit = if negative { 1 << 31 } else { (1 << 31) - 1 };
        let m = m.min(limit) as i64;
        return (if negative { -m } else { m }) as i32;
    }
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

/// Whether `sscanf` leaves `ERANGE` in `errno` after a number that
/// overflows. The BSD C library does (measured on macOS; FreeBSD's
/// `vfscanf`, which macOS's comes from, and those of the other BSDs
/// convert with `strtoumax`), and so do the C runtimes of both Windows
/// toolchains (measured with C 1.0's `mdb_dump` built by MSVC and by
/// MinGW-w64); glibc's `sscanf` does not (C 1.0's `mdb_dump -i
/// 99999999999999999999 -i 0` says "Success" there).
pub const SSCANF_KEEPS_ERANGE: bool = cfg!(any(
    windows,
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
));

/// `sscanf(s, "%zu", &v) == 1` as glibc and macOS read it (`strtoull`):
/// the value and whether it overflowed (`ERANGE`, which `sscanf` may leave
/// in `errno`, [`SSCANF_KEEPS_ERANGE`]; the value is `SIZE_MAX`, whatever
/// the sign), or `None` when there are no digits. A minus sign negates
/// modulo 2^64.
pub fn scan_size(s: &[u8]) -> Option<(u64, bool)> {
    let (negative, digits) = split_number(cstr(s), 10);
    if digits.is_empty() {
        return None;
    }
    let exact = digits.iter().try_fold(0u64, |acc, &d| {
        acc.checked_mul(10)?
            .checked_add(u64::from((d as char).to_digit(10)?))
    });
    Some(match exact {
        Some(m) if negative => (m.wrapping_neg(), false),
        Some(m) => (m, false),
        None => (u64::MAX, true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(scan_size(b"5x"), Some((5, false)));
        assert_eq!(scan_size(b"  +3"), Some((3, false)));
        assert_eq!(scan_size(b"-1"), Some((u64::MAX, false)));
        assert_eq!(scan_size(b"0x10"), Some((0, false)));
        assert_eq!(scan_size(b"18446744073709551615"), Some((u64::MAX, false)));
        assert_eq!(scan_size(b"18446744073709551616"), Some((u64::MAX, true)));
        assert_eq!(scan_size(b"-99999999999999999999"), Some((u64::MAX, true)));
        assert_eq!(scan_size(b""), None);
        assert_eq!(scan_size(b"-"), None);
        assert_eq!(scan_size(b"abc"), None);
    }

    #[test]
    fn numbers() {
        assert_eq!(atoi(b"3\n"), 3);
        assert_eq!(atoi(b"  -12x"), -12);
        assert_eq!(atoi(b"x"), 0);
        // Past int: the low 32 bits, or INT_MAX/INT_MIN with MSVC's UCRT.
        let msvc = cfg!(target_env = "msvc");
        assert_eq!(atoi(b"4294967299"), if msvc { i32::MAX } else { 3 });
        assert_eq!(
            atoi(b"99999999999"),
            if msvc { i32::MAX } else { 1_215_752_191 }
        );
        assert_eq!(atoi(b"-2147483648"), i32::MIN);
        assert_eq!(
            atoi(b"-99999999999"),
            if msvc { i32::MIN } else { -1_215_752_191 }
        );
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
    fn dos_errors_as_errno() {
        assert_eq!(
            [2, 3, 5, 32, 80, 87, 112, 9999].map(dos_errno),
            [2, 2, 13, 13, 17, 22, 28, 22]
        );
    }

    #[test]
    fn pointers_and_classes() {
        let p = if cfg!(all(windows, target_env = "msvc")) {
            "0000000000001ABC"
        } else if cfg!(windows) {
            "0000000000001abc"
        } else {
            "0x1abc"
        };
        assert_eq!(pointer(0x1abc), p);
        assert!(is_print(b' ') && is_print(b'~') && !is_print(0x7f) && !is_print(0x80));
        assert_eq!(cstr(b"ab\0cd"), b"ab");
    }
}
