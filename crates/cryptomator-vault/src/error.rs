//! Error type shared by all modules.

use std::fmt;

/// Errors returned by this crate.
///
/// The variants are deliberately coarse so callers can map them to user
/// facing messages: a wrong password is distinct from tampered data, which is
/// distinct from data that is simply not in a format this crate understands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The password does not unlock the masterkey file (AES key unwrap
    /// integrity check failed).
    InvalidPassword,
    /// A ciphertext, MAC, tag or signature did not verify. The data was
    /// tampered with, is corrupt, or belongs to a different vault/directory.
    Authentication(&'static str),
    /// The input is malformed (bad JSON, bad Base64, wrong length, ...).
    InvalidFormat(String),
    /// The input is well-formed but uses a feature this crate does not
    /// implement (other vault format, cipher combo, key loader, ...).
    Unsupported(String),
    /// A caller supplied argument is invalid (for example an empty file name).
    InvalidArgument(String),
    /// The operating system random number generator failed.
    Random,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidPassword => f.write_str("invalid password"),
            Error::Authentication(what) => write!(f, "authentication failed: {what}"),
            Error::InvalidFormat(what) => write!(f, "invalid format: {what}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Error::Random => f.write_str("random number generator failure"),
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidFormat(msg.into())
}
