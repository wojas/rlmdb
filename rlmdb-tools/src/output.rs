//! Standard output (or the file of `mdb_dump -f`) as the C tools use it:
//! fully buffered, with write errors remembered instead of returned, so the
//! tool's control flow stays that of the C code. Standard error is written
//! unbuffered, as C's `stderr` is.
//!
//! On Windows the C tools' streams are in the C runtime's *text mode*:
//! every `\n` written becomes `\r\n`, and on input `\r\n` becomes `\n` and
//! Ctrl-Z ends the input. [`TextWriter`] and [`TextReader`] do the same, so
//! the Rust tools write and read what the C tools do there. (`mdb_copy`
//! writes the copy to the standard output handle directly, untranslated,
//! as C does.)

use std::fs::File;
use std::io::{self, BufRead, BufWriter, Read, Write};

/// A buffered output stream that records the first write error.
pub struct Output {
    inner: BufWriter<Box<dyn Write>>,
    error: Option<io::Error>,
    /// The tool has reported `error` itself.
    reported: bool,
}

impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Standard output.
    pub fn stdout() -> Output {
        Output::new(text_mode(Box::new(io::stdout())))
    }

    /// A file (`freopen(path, "w", stdout)`).
    pub fn file(file: File) -> Output {
        Output::new(text_mode(Box::new(file)))
    }

    fn new(w: Box<dyn Write>) -> Output {
        Output {
            inner: BufWriter::with_capacity(64 * 1024, w),
            error: None,
            reported: false,
        }
    }

    /// Writes `bytes`; after a failure, further writes are dropped.
    pub fn write(&mut self, bytes: &[u8]) {
        if self.error.is_none() {
            if let Err(e) = self.inner.write_all(bytes) {
                self.error = Some(e);
            }
        }
    }

    /// Writes formatted text (use with `format_args!`).
    pub fn print(&mut self, args: std::fmt::Arguments<'_>) {
        match args.as_str() {
            Some(s) => self.write(s.as_bytes()),
            None => self.write(args.to_string().as_bytes()),
        }
    }

    /// The first write error so far, if any.
    pub fn error(&self) -> Option<&io::Error> {
        self.error.as_ref()
    }

    /// Marks the current error as reported by the tool, so that
    /// [`Output::finish`] does not report it again.
    pub fn set_reported(&mut self) {
        self.reported = self.error.is_some();
    }

    /// Flushes the buffer; returns the first error of the stream's life
    /// that the tool has not reported yet.
    pub fn finish(mut self) -> io::Result<()> {
        match self.error.take() {
            Some(_) if self.reported => Ok(()),
            Some(e) => Err(e),
            None => self.inner.flush(),
        }
    }
}

/// Writes the concatenation of `parts` to standard error in one call
/// (unbuffered, like C's `fprintf(stderr, ...)`). Errors are ignored, as in
/// C.
pub fn eprint_bytes(parts: &[&[u8]]) {
    let mut msg: Vec<u8> = parts.concat();
    if cfg!(windows) {
        msg = crlf(&msg);
    }
    let _ = io::stderr().write_all(&msg);
}

/// `bytes` with every `\n` as `\r\n`.
fn crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for &b in bytes {
        if b == b'\n' {
            out.push(b'\r');
        }
        out.push(b);
    }
    out
}

/// `w` as a C stream of this platform writes: in text mode on Windows
/// ([`TextWriter`]), as it is elsewhere.
fn text_mode(w: Box<dyn Write>) -> Box<dyn Write> {
    if cfg!(windows) {
        Box::new(TextWriter(w))
    } else {
        w
    }
}

/// The input of `mdb_load` (standard input or the `-f` file), as C reads
/// it on this platform: in text mode on Windows ([`TextReader`]), as it is
/// elsewhere.
pub fn text_input(r: Box<dyn Read>) -> Box<dyn BufRead> {
    if cfg!(windows) {
        Box::new(io::BufReader::with_capacity(
            64 * 1024,
            TextReader::new(io::BufReader::with_capacity(64 * 1024, r)),
        ))
    } else {
        Box::new(io::BufReader::with_capacity(64 * 1024, r))
    }
}

/// The C runtime's text mode for output (Windows): each `\n` is written as
/// `\r\n`.
pub struct TextWriter<W: Write>(pub W);

impl<W: Write> Write for TextWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut start = 0;
        for (i, &b) in buf.iter().enumerate() {
            if b == b'\n' {
                self.0.write_all(&buf[start..i])?;
                self.0.write_all(b"\r\n")?;
                start = i + 1;
            }
        }
        self.0.write_all(&buf[start..])?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The C runtime's text mode for input (Windows): `\r\n` is read as `\n`
/// (a `\r` before anything else stays), and Ctrl-Z (0x1A) ends the input.
pub struct TextReader<R: BufRead> {
    inner: R,
    /// Ctrl-Z was seen.
    eof: bool,
}

impl<R: BufRead> TextReader<R> {
    /// Text-mode reads of `inner`.
    pub fn new(inner: R) -> TextReader<R> {
        TextReader { inner, eof: false }
    }
}

impl<R: BufRead> Read for TextReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < out.len() && !self.eof {
            let Some(&b) = self.inner.fill_buf()?.first() else {
                break;
            };
            self.inner.consume(1);
            match b {
                0x1a => self.eof = true,
                // CR LF: the LF comes next.
                b'\r' if self.inner.fill_buf()?.first() == Some(&b'\n') => {}
                _ => {
                    out[n] = b;
                    n += 1;
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_mode_streams() {
        let mut w = TextWriter(Vec::new());
        w.write_all(b"a\nb\r\nc").unwrap();
        assert_eq!(w.0, b"a\r\nb\r\r\nc");
        assert_eq!(crlf(b"x\ny\n"), b"x\r\ny\r\n");
        let mut r = TextReader::new(&b"k\r\nv\r\rw\nend\x1aafter"[..]);
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"k\nv\r\rw\nend");
    }
}
