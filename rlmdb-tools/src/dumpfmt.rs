//! The data lines of the dump format (`mdb_dump` writes them, `mdb_load`
//! reads them), a BDB-compatible flat text format:
//!
//! ```text
//! VERSION=3
//! format=bytevalue          (or print)
//! database=name             (only for named databases)
//! type=btree
//! mapsize=1048576
//! mapaddr=0x...             (only if the environment records one)
//! maxreaders=126
//! duplicates=1              (DUPSORT databases)
//! dupsort=1                 (one line per database flag)
//! db_pagesize=4096
//! HEADER=END
//!  6b6579                   (a key: a space, then the encoded bytes)
//!  76616c7565               (its value)
//! DATA=END
//! ```
//!
//! Two encodings: **bytevalue** writes every byte as two lower-case hex
//! digits (`byte()` in `mdb_dump.c`); **print** writes printable bytes
//! (`isprint` in the "C" locale) as themselves, a backslash as `\\` and
//! every other byte as `\` and two hex digits (`text()`). `mdb_load`
//! accepts upper-case hex digits too (`unhex()` in `mdb_load.c`).

use crate::cfmt::{is_print, is_xdigit};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Appends a data line for `value`: a space, the encoded bytes, a newline.
pub fn encode_line(out: &mut Vec<u8>, value: &[u8], print: bool) {
    out.push(b' ');
    for &c in value {
        if print && is_print(c) {
            if c == b'\\' {
                out.push(b'\\');
            }
            out.push(c);
        } else {
            if print {
                out.push(b'\\');
            }
            out.push(HEX[usize::from(c >> 4)]);
            out.push(HEX[usize::from(c & 0xf)]);
        }
    }
    out.push(b'\n');
}

/// `unhex()`: the byte of two hex digits (either case).
fn unhex(hi: u8, lo: u8) -> u8 {
    let digit = |c: u8| {
        let x = c & 0x4f;
        if x & 0x40 != 0 {
            x - 55
        } else {
            x
        }
    };
    (digit(hi) << 4) | digit(lo)
}

/// Decodes the content of a data line (without the leading space and the
/// newline) in `mdb_load`'s way. `None` if the line is malformed: a
/// backslash not followed by a backslash or two hex digits (print), or an
/// odd number of characters or a non-hex character (bytevalue).
pub fn decode_line(line: &[u8], print: bool) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    decode_line_into(line, print, &mut out).then_some(out)
}

/// [`decode_line`] into a reused buffer: `out` is replaced by the decoded
/// bytes; `false` if the line is malformed.
pub fn decode_line_into(line: &[u8], print: bool, out: &mut Vec<u8>) -> bool {
    out.clear();
    if print {
        let mut i = 0;
        while i < line.len() {
            if line[i] == b'\\' {
                if line.get(i + 1) == Some(&b'\\') {
                    out.push(b'\\');
                    i += 2;
                } else {
                    if i + 3 > line.len() || !is_xdigit(line[i + 1]) || !is_xdigit(line[i + 2]) {
                        return false;
                    }
                    out.push(unhex(line[i + 1], line[i + 2]));
                    i += 3;
                }
            } else {
                out.push(line[i]);
                i += 1;
            }
        }
    } else {
        if line.len() % 2 != 0 {
            return false;
        }
        for pair in line.chunks_exact(2) {
            if !is_xdigit(pair[0]) || !is_xdigit(pair[1]) {
                return false;
            }
            out.push(unhex(pair[0], pair[1]));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: &[u8], print: bool) -> Vec<u8> {
        let mut out = Vec::new();
        encode_line(&mut out, v, print);
        out
    }

    #[test]
    fn encodings() {
        assert_eq!(enc(b"a\\b\n\x7f\xff ~", true), b" a\\\\b\\0a\\7f\\ff ~\n");
        assert_eq!(enc(b"a\\\x00", false), b" 615c00\n");
        assert_eq!(enc(b"", true), b" \n");
    }

    #[test]
    fn round_trip_all_bytes() {
        let all: Vec<u8> = (0..=255).collect();
        for print in [false, true] {
            let line = enc(&all, print);
            let decoded = decode_line(&line[1..line.len() - 1], print).unwrap();
            assert_eq!(decoded, all);
        }
    }

    #[test]
    fn decoding() {
        assert_eq!(decode_line(b"4A4b", false).unwrap(), b"JK");
        assert_eq!(decode_line(b"abc", false), None);
        assert_eq!(decode_line(b"zz", false), None);
        assert_eq!(decode_line(b"x\\41\\\\", true).unwrap(), b"xA\\");
        assert_eq!(decode_line(b"x\\4", true), None);
        assert_eq!(decode_line(b"x\\", true), None);
        assert_eq!(decode_line(b"\\g0", true), None);
    }
}
