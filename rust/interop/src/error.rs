//! The harness error type: a message meant for a human reading a failed test.

use std::fmt;

/// A harness failure: a peer exited with an error, an output did not parse,
/// or a check failed. The message includes the command line and the peer's
/// output where that helps.
///
/// A peer that exits with status 4 does not implement the command yet
/// (`rpeer` before stage 3); such errors are marked
/// [`is_not_implemented`](Self::is_not_implemented) so that
/// [`scenario_tests!`](crate::scenario_tests) can report the scenario as
/// skipped instead of failed.
#[derive(Debug)]
pub struct Error {
    msg: String,
    not_implemented: bool,
}

/// `Result` with the harness [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// An error with the given message.
    pub fn new(msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            not_implemented: false,
        }
    }

    /// A peer does not implement a command (it exited with status 4).
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Error {
            msg: msg.into(),
            not_implemented: true,
        }
    }

    /// Did a peer report that it does not implement a command?
    pub fn is_not_implemented(&self) -> bool {
        self.not_implemented
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.msg
    }

    /// Prefix the message with some context.
    pub fn context(self, ctx: impl fmt::Display) -> Self {
        Error {
            msg: format!("{ctx}: {}", self.msg),
            ..self
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(format!("I/O error: {e}"))
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
