use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    InvalidPassword,
    Authentication(&'static str),
    InvalidFormat(String),
    Unsupported(String),
    InvalidArgument(String),
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

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidFormat(msg.into())
}
