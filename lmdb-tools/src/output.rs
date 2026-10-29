//! Standard output (or the file of `mdb_dump -f`) as the C tools use it:
//! fully buffered, with write errors remembered instead of returned, so the
//! tool's control flow stays that of the C code. Standard error is written
//! unbuffered, as C's `stderr` is.

use std::fs::File;
use std::io::{self, BufWriter, Write};

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
        Output::new(Box::new(io::stdout()))
    }

    /// A file (`freopen(path, "w", stdout)`).
    pub fn file(file: File) -> Output {
        Output::new(Box::new(file))
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
    let msg: Vec<u8> = parts.concat();
    let _ = io::stderr().write_all(&msg);
}
