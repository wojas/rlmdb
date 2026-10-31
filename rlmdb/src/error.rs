//! Error types: [`MdbError`] for LMDB's own return codes and the crate-wide
//! [`Error`] (shaped like `heed::Error`).
//!
//! C LMDB functions return either `0`, one of its own negative codes
//! (`MDB_KEYEXIST` … `MDB_BAD_DBI`; LMDB 1.0 adds `MDB_PROBLEM` …
//! `MDB_ADDR_BUSY`), or an `errno` value. We keep both kinds
//! distinguishable and losslessly convertible back to a C `int`
//! ([`Error::to_err_code`]), because the C API crate must return exactly what
//! C LMDB returns:
//!
//! * LMDB codes become [`Error::Mdb`].
//! * `errno` values (from system calls, and the `EINVAL`/`EACCES`/… that the
//!   C API itself returns for misuse) become [`Error::Io`] holding
//!   `io::Error::from_raw_os_error(errno)`. This mirrors heed, which converts
//!   `MdbError::Other(errno)` into `Error::Io`.
//!
//! The `Display` text of [`MdbError`] is byte-for-byte what `mdb_strerror`
//! (`mdb.c`, `mdb_errstr[]`) returns.

use std::fmt;
use std::io;
use std::os::raw::c_int;

use crate::consts::*;

/// An LMDB return code (`MDB_*` error constants), or another `errno` value.
///
/// Mirrors `heed::MdbError`, including its variant names.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum MdbError {
    /// `MDB_KEYEXIST`: a key/data pair already exists.
    KeyExist,
    /// `MDB_NOTFOUND`: a key/data pair was not found (EOF).
    NotFound,
    /// `MDB_PAGE_NOTFOUND`: requested page not found - this usually indicates corruption.
    PageNotFound,
    /// `MDB_CORRUPTED`: located page was wrong type.
    Corrupted,
    /// `MDB_PANIC`: update of meta page failed or environment had fatal error.
    Panic,
    /// `MDB_VERSION_MISMATCH`: environment version mismatch.
    VersionMismatch,
    /// `MDB_INVALID`: file is not a valid LMDB file.
    Invalid,
    /// `MDB_MAP_FULL`: environment mapsize reached.
    MapFull,
    /// `MDB_DBS_FULL`: environment maxdbs reached.
    DbsFull,
    /// `MDB_READERS_FULL`: environment maxreaders reached.
    ReadersFull,
    /// `MDB_TLS_FULL`: too many TLS keys in use (Windows only in C).
    TlsFull,
    /// `MDB_TXN_FULL`: txn has too many dirty pages.
    TxnFull,
    /// `MDB_CURSOR_FULL`: cursor stack too deep - internal error.
    CursorFull,
    /// `MDB_PAGE_FULL`: page has not enough space - internal error.
    PageFull,
    /// `MDB_MAP_RESIZED`: database contents grew beyond environment mapsize.
    MapResized,
    /// `MDB_INCOMPATIBLE`: operation and DB incompatible, or DB type changed.
    Incompatible,
    /// `MDB_BAD_RSLOT`: invalid reuse of reader locktable slot.
    BadRslot,
    /// `MDB_BAD_TXN`: transaction must abort, has a child, or is invalid.
    BadTxn,
    /// `MDB_BAD_VALSIZE`: unsupported size of key/DB name/data, or wrong DUP_FIXED size.
    BadValSize,
    /// `MDB_BAD_DBI`: the specified DBI was changed unexpectedly.
    BadDbi,
    /// `MDB_PROBLEM` (LMDB 1.0 only): unexpected problem - txn should abort.
    /// Never produced by this implementation; present for heed parity.
    Problem,
    /// `MDB_BAD_CHECKSUM` (LMDB 1.0 only): page checksum incorrect. Returned
    /// when a 1.0 environment records page checksums, which are not
    /// implemented.
    BadChecksum,
    /// `MDB_CRYPTO_FAIL` (LMDB 1.0 only): encryption/decryption failed.
    CryptoFail,
    /// `MDB_ENV_ENCRYPTION` (LMDB 1.0 only): environment encryption mismatch.
    /// Returned for an encrypted 1.0 environment, which cannot be read.
    EnvEncryption,
    /// `MDB_TXN_PENDING` (LMDB 1.0 only): transaction was already prepared.
    TxnPending,
    /// `MDB_CANT_ROLLBACK` (LMDB 1.0 only): environment can't rollback the
    /// last transaction.
    CantRollback,
    /// `MDB_DBIS_BUSY` (LMDB 1.0 only): can't drop main DBI while other DBIs
    /// are open.
    DbisBusy,
    /// `MDB_SHORT_WRITE` (LMDB 1.0 only): write was incomplete.
    ShortWrite,
    /// `MDB_ENV_BUSY` (LMDB 1.0 only): env is busy, can't use previous
    /// snapshot.
    EnvBusy,
    /// `MDB_IS_READONLY` (LMDB 1.0 only): env or txn is read-only, can't
    /// write.
    IsReadonly,
    /// `MDB_ADDR_BUSY` (LMDB 1.0 only): requested map address is
    /// unavailable.
    AddrBusy,
    /// Any other code, normally an `errno` value.
    Other(c_int),
}

impl MdbError {
    /// Returns `true` if this is [`MdbError::NotFound`].
    pub fn not_found(&self) -> bool {
        *self == MdbError::NotFound
    }

    /// Converts a raw C return code (non-zero) to an `MdbError`.
    pub fn from_err_code(err_code: c_int) -> MdbError {
        match err_code {
            MDB_KEYEXIST => MdbError::KeyExist,
            MDB_NOTFOUND => MdbError::NotFound,
            MDB_PAGE_NOTFOUND => MdbError::PageNotFound,
            MDB_CORRUPTED => MdbError::Corrupted,
            MDB_PANIC => MdbError::Panic,
            MDB_VERSION_MISMATCH => MdbError::VersionMismatch,
            MDB_INVALID => MdbError::Invalid,
            MDB_MAP_FULL => MdbError::MapFull,
            MDB_DBS_FULL => MdbError::DbsFull,
            MDB_READERS_FULL => MdbError::ReadersFull,
            MDB_TLS_FULL => MdbError::TlsFull,
            MDB_TXN_FULL => MdbError::TxnFull,
            MDB_CURSOR_FULL => MdbError::CursorFull,
            MDB_PAGE_FULL => MdbError::PageFull,
            MDB_MAP_RESIZED => MdbError::MapResized,
            MDB_INCOMPATIBLE => MdbError::Incompatible,
            MDB_BAD_RSLOT => MdbError::BadRslot,
            MDB_BAD_TXN => MdbError::BadTxn,
            MDB_BAD_VALSIZE => MdbError::BadValSize,
            MDB_BAD_DBI => MdbError::BadDbi,
            MDB_PROBLEM => MdbError::Problem,
            MDB_BAD_CHECKSUM => MdbError::BadChecksum,
            MDB_CRYPTO_FAIL => MdbError::CryptoFail,
            MDB_ENV_ENCRYPTION => MdbError::EnvEncryption,
            MDB_TXN_PENDING => MdbError::TxnPending,
            MDB_CANT_ROLLBACK => MdbError::CantRollback,
            MDB_DBIS_BUSY => MdbError::DbisBusy,
            MDB_SHORT_WRITE => MdbError::ShortWrite,
            MDB_ENV_BUSY => MdbError::EnvBusy,
            MDB_IS_READONLY => MdbError::IsReadonly,
            MDB_ADDR_BUSY => MdbError::AddrBusy,
            other => MdbError::Other(other),
        }
    }

    /// Converts this error back to the raw C return code.
    pub fn to_err_code(&self) -> c_int {
        match *self {
            MdbError::KeyExist => MDB_KEYEXIST,
            MdbError::NotFound => MDB_NOTFOUND,
            MdbError::PageNotFound => MDB_PAGE_NOTFOUND,
            MdbError::Corrupted => MDB_CORRUPTED,
            MdbError::Panic => MDB_PANIC,
            MdbError::VersionMismatch => MDB_VERSION_MISMATCH,
            MdbError::Invalid => MDB_INVALID,
            MdbError::MapFull => MDB_MAP_FULL,
            MdbError::DbsFull => MDB_DBS_FULL,
            MdbError::ReadersFull => MDB_READERS_FULL,
            MdbError::TlsFull => MDB_TLS_FULL,
            MdbError::TxnFull => MDB_TXN_FULL,
            MdbError::CursorFull => MDB_CURSOR_FULL,
            MdbError::PageFull => MDB_PAGE_FULL,
            MdbError::MapResized => MDB_MAP_RESIZED,
            MdbError::Incompatible => MDB_INCOMPATIBLE,
            MdbError::BadRslot => MDB_BAD_RSLOT,
            MdbError::BadTxn => MDB_BAD_TXN,
            MdbError::BadValSize => MDB_BAD_VALSIZE,
            MdbError::BadDbi => MDB_BAD_DBI,
            MdbError::Problem => MDB_PROBLEM,
            MdbError::BadChecksum => MDB_BAD_CHECKSUM,
            MdbError::CryptoFail => MDB_CRYPTO_FAIL,
            MdbError::EnvEncryption => MDB_ENV_ENCRYPTION,
            MdbError::TxnPending => MDB_TXN_PENDING,
            MdbError::CantRollback => MDB_CANT_ROLLBACK,
            MdbError::DbisBusy => MDB_DBIS_BUSY,
            MdbError::ShortWrite => MDB_SHORT_WRITE,
            MdbError::EnvBusy => MDB_ENV_BUSY,
            MdbError::IsReadonly => MDB_IS_READONLY,
            MdbError::AddrBusy => MDB_ADDR_BUSY,
            MdbError::Other(code) => code,
        }
    }
}

/// The text `mdb_strerror` returns for `err`.
///
/// For LMDB codes this is the `mdb_errstr[]` table (the 0.9 codes, then the
/// ones LMDB 1.0 adds, with 1.0's texts); for positive values it is the C
/// library's `strerror`; other negative values give
/// `"Invalid error code"`, and `0` gives `"Successful return: 0"`. On
/// Windows codes other than LMDB's are the system's, as in C there: the C
/// runtime's text for the `errno` values LMDB uses, else the system's
/// message, which is empty for negative values.
pub fn strerror(err: c_int) -> String {
    let lmdb_text = match err {
        0 => "Successful return: 0",
        MDB_KEYEXIST => "MDB_KEYEXIST: Key/data pair already exists",
        MDB_NOTFOUND => "MDB_NOTFOUND: No matching key/data pair found",
        MDB_PAGE_NOTFOUND => "MDB_PAGE_NOTFOUND: Requested page not found",
        MDB_CORRUPTED => "MDB_CORRUPTED: Located page was wrong type",
        MDB_PANIC => "MDB_PANIC: Update of meta page failed or environment had fatal error",
        MDB_VERSION_MISMATCH => "MDB_VERSION_MISMATCH: Database environment version mismatch",
        MDB_INVALID => "MDB_INVALID: File is not an LMDB file",
        MDB_MAP_FULL => "MDB_MAP_FULL: Environment mapsize limit reached",
        MDB_DBS_FULL => "MDB_DBS_FULL: Environment maxdbs limit reached",
        MDB_READERS_FULL => "MDB_READERS_FULL: Environment maxreaders limit reached",
        MDB_TLS_FULL => "MDB_TLS_FULL: Thread-local storage keys full - too many environments open",
        MDB_TXN_FULL => "MDB_TXN_FULL: Transaction has too many dirty pages - transaction too big",
        MDB_CURSOR_FULL => "MDB_CURSOR_FULL: Internal error - cursor stack limit reached",
        MDB_PAGE_FULL => "MDB_PAGE_FULL: Internal error - page has no more space",
        MDB_MAP_RESIZED => "MDB_MAP_RESIZED: Database contents grew beyond environment mapsize",
        MDB_INCOMPATIBLE => "MDB_INCOMPATIBLE: Operation and DB incompatible, or DB flags changed",
        MDB_BAD_RSLOT => "MDB_BAD_RSLOT: Invalid reuse of reader locktable slot",
        MDB_BAD_TXN => "MDB_BAD_TXN: Transaction must abort, has a child, or is invalid",
        MDB_BAD_VALSIZE => {
            "MDB_BAD_VALSIZE: Unsupported size of key/DB name/data, or wrong DUPFIXED size"
        }
        MDB_BAD_DBI => "MDB_BAD_DBI: The specified DBI handle was closed/changed unexpectedly",
        // LMDB 1.0's additions (its mdb_errstr[]).
        MDB_PROBLEM => "MDB_PROBLEM: Unexpected problem - txn should abort",
        MDB_BAD_CHECKSUM => "MDB_BAD_CHECKSUM: Page checksum mismatch",
        MDB_CRYPTO_FAIL => "MDB_CRYPTO_FAIL: Page encryption or decryption failed",
        MDB_ENV_ENCRYPTION => "MDB_ENV_ENCRYPTION: Environment encryption mismatch",
        MDB_TXN_PENDING => "MDB_TXN_PENDING: Transaction already prepared, must abort or commit",
        MDB_CANT_ROLLBACK => "MDB_CANT_ROLLBACK: Environment can't rollback last transaction",
        MDB_DBIS_BUSY => "MDB_DBIS_BUSY: Can't drop main DBI while other DBIs are open",
        MDB_SHORT_WRITE => "MDB_SHORT_WRITE: Fewer bytes were written than requested",
        MDB_ENV_BUSY => "MDB_ENV_BUSY: Environment is busy, can't use previous snapshot",
        MDB_IS_READONLY => "MDB_IS_READONLY: Can't write in readonly txn or environment",
        MDB_ADDR_BUSY => "MDB_ADDR_BUSY: Requested map address is unavailable",
        // C on Windows asks the system (FormatMessage), which has no text
        // for these: an empty string.
        e if e < 0 && !cfg!(windows) => "Invalid error code",
        e => return crate::os::strerror(e),
    };
    lmdb_text.to_owned()
}

impl fmt::Display for MdbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&strerror(self.to_err_code()))
    }
}

impl std::error::Error for MdbError {}

/// A boxed error that can be sent between threads (heed's `BoxedError`).
pub type BoxedError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// An error of this crate (shaped like `heed::Error`).
#[derive(Debug)]
pub enum Error {
    /// An operating system error (`errno`), or an `errno`-style code that the
    /// C API returns for invalid arguments (`EINVAL`, `EACCES`, …).
    Io(io::Error),
    /// An LMDB error code.
    Mdb(MdbError),
    /// Encoding a key or value failed (heed layer).
    Encoding(BoxedError),
    /// Decoding a key or value failed (heed layer).
    Decoding(BoxedError),
    /// The environment is already open in this process. LMDB must not open
    /// the same environment twice in one process: POSIX record locks are
    /// per process, so closing one handle would drop the other's locks.
    /// Handles opened with `NO_LOCK` (no lock file) are exempt among
    /// themselves: they share the process's reader table and writer lock
    /// for the file. A `NO_LOCK` handle and one with a lock file of the
    /// same file still refuse each other.
    EnvAlreadyOpened,
}

impl Error {
    /// The C return code for this error (as `mdb_*` would have returned it).
    ///
    /// `Io` errors without an OS code map to `EIO`; `Encoding`/`Decoding`
    /// map to `EINVAL`; `EnvAlreadyOpened` maps to `EBUSY`.
    pub fn to_err_code(&self) -> c_int {
        match self {
            Error::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            Error::Mdb(e) => e.to_err_code(),
            Error::Encoding(_) | Error::Decoding(_) => libc::EINVAL,
            Error::EnvAlreadyOpened => libc::EBUSY,
        }
    }

    /// Converts a non-zero C return code: LMDB codes become [`Error::Mdb`],
    /// anything else [`Error::Io`].
    pub fn from_err_code(code: c_int) -> Error {
        MdbError::from_err_code(code).into()
    }

    /// Returns `true` if this is `Error::Mdb(MdbError::NotFound)`.
    pub fn not_found(&self) -> bool {
        matches!(self, Error::Mdb(MdbError::NotFound))
    }

    /// An `errno` error, as C LMDB returns for invalid arguments.
    pub(crate) fn errno(code: c_int) -> Error {
        Error::Io(io::Error::from_raw_os_error(code))
    }

    /// `EINVAL`.
    pub(crate) fn einval() -> Error {
        Error::errno(libc::EINVAL)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "{error}"),
            Error::Mdb(error) => write!(f, "{error}"),
            Error::Encoding(error) => write!(f, "error while encoding: {error}"),
            Error::Decoding(error) => write!(f, "error while decoding: {error}"),
            Error::EnvAlreadyOpened => f.write_str(
                "environment already open in this program; \
                close it to be able to open it again with different options",
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Mdb(e) => Some(e),
            Error::Encoding(e) | Error::Decoding(e) => Some(e.as_ref()),
            Error::EnvAlreadyOpened => None,
        }
    }
}

impl From<MdbError> for Error {
    fn from(error: MdbError) -> Error {
        match error {
            MdbError::Other(e) => Error::Io(io::Error::from_raw_os_error(e)),
            _ => Error::Mdb(error),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

/// Either a success or an [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for code in MDB_KEYEXIST..=MDB_LAST_ERRCODE_V10 {
            let e = MdbError::from_err_code(code);
            assert!(!matches!(e, MdbError::Other(_)), "code {code}");
            assert_eq!(e.to_err_code(), code);
            assert_eq!(Error::from_err_code(code).to_err_code(), code);
        }
        assert_eq!(
            MdbError::from_err_code(libc::EINVAL),
            MdbError::Other(libc::EINVAL)
        );
        assert!(matches!(Error::from_err_code(libc::EACCES), Error::Io(_)));
        assert_eq!(
            Error::from_err_code(libc::EACCES).to_err_code(),
            libc::EACCES
        );
    }

    #[test]
    fn strerror_texts() {
        assert_eq!(
            MdbError::NotFound.to_string(),
            "MDB_NOTFOUND: No matching key/data pair found"
        );
        assert_eq!(
            MdbError::BadDbi.to_string(),
            "MDB_BAD_DBI: The specified DBI handle was closed/changed unexpectedly"
        );
        // LMDB 1.0's codes have 1.0's texts.
        assert_eq!(
            MdbError::EnvEncryption.to_string(),
            "MDB_ENV_ENCRYPTION: Environment encryption mismatch"
        );
        assert_eq!(
            strerror(MDB_ADDR_BUSY),
            "MDB_ADDR_BUSY: Requested map address is unavailable"
        );
        let invalid = if cfg!(windows) {
            ""
        } else {
            "Invalid error code"
        };
        assert_eq!(strerror(MDB_ADDR_BUSY + 1), invalid);
        assert_eq!(strerror(0), "Successful return: 0");
        assert_eq!(strerror(-1), invalid);
        assert_eq!(strerror(libc::EACCES), "Permission denied");
        assert_eq!(
            MdbError::Other(libc::ENOENT).to_string(),
            "No such file or directory"
        );
    }
}
