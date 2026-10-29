//! The harness error type: a message meant for a human reading a failed test.

use std::fmt;

/// A harness failure: a peer exited with an error, an output did not parse,
/// or a check failed. The message includes the command line and the peer's
/// output where that helps.
#[derive(Debug)]
pub struct Error(String);

/// `Result` with the harness [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An error with the given message.
    pub fn new(msg: impl Into<String>) -> Self {
        Error(msg.into())
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.0
    }

    /// Prefix the message with some context.
    pub fn context(self, ctx: impl fmt::Display) -> Self {
        Error(format!("{ctx}: {}", self.0))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(format!("I/O error: {e}"))
    }
}

/// Return an [`Error`] with a formatted message unless `cond` holds.
///
/// ```ignore
/// interop::ensure!(stat.psize == 4096, "page size {}", stat.psize);
/// ```
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($fmt:tt)+) => {
        if !$cond {
            return Err($crate::Error::new(format!($($fmt)+)));
        }
    };
}

/// Return an [`Error`] with a formatted message.
#[macro_export]
macro_rules! bail {
    ($($fmt:tt)+) => {
        return Err($crate::Error::new(format!($($fmt)+)))
    };
}
