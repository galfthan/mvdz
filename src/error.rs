//! The library error type.

/// Library error. New variants may be added in minor releases, so a
/// `match` on it needs a `_` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Not an MVDZ file, or an unsupported version.
    Format(&'static str),
    /// The file is damaged or breaks a rule of the format (any checksum,
    /// structure or limit of FORMAT.md).
    Corrupt(&'static str),
    /// Decoding would exceed the caller's memory or work budget.
    Limit(&'static str),
    /// A bad argument (seek past the end, unknown attachment, ...).
    Invalid(&'static str),
    /// No attachment of that name.
    NotFound,
    /// An I/O error from the underlying reader or writer.
    Io(std::io::ErrorKind),
    /// The encoder could not produce a file that decodes to its input.
    Internal(&'static str),
    /// The input to compress is not an MVD demo (or is corrupt gzip).
    NotMvd(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Format(s) => write!(f, "format: {s}"),
            Error::Corrupt(s) => write!(f, "corrupt: {s}"),
            Error::Limit(s) => write!(f, "limit: {s}"),
            Error::Invalid(s) => write!(f, "invalid argument: {s}"),
            Error::NotFound => write!(f, "no such attachment"),
            Error::Io(k) => write!(f, "i/o: {k}"),
            Error::Internal(s) => write!(f, "internal: {s}"),
            Error::NotMvd(s) => write!(f, "not an MVD demo: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        match e.get_ref().and_then(|x| x.downcast_ref::<Error>()) {
            Some(d) => *d,
            None => Error::Io(e.kind()),
        }
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> std::io::Error {
        match e {
            Error::Io(k) => std::io::Error::new(k, e),
            _ => std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand for a corrupt-file error.
#[inline]
pub fn bad<T>(what: &'static str) -> Result<T> {
    Err(Error::Corrupt(what))
}
