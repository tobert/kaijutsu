//! The crate's error surface. One rule cuts across every variant: **nothing
//! that touches a key file or a token may carry file content** — paths and
//! content-free parse messages only.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Io(String),

    /// A directory listing or descriptor read failed. The message names the
    /// path and the OS error, never file content.
    #[error("{0}")]
    Registry(String),

    /// Envelope grammar or validation failure (charset, mandatory newlines,
    /// injection). The messages are verbose on purpose: the silent-downgrade
    /// hazard means an envelope error must explain exactly what would
    /// otherwise have been delivered anonymously.
    #[error("{0}")]
    Envelope(String),

    /// Frame codec failure: a wire line that is not newline-delimited JSON,
    /// or a `user` frame missing its required fields.
    #[error("{0}")]
    Frame(String),

    /// Fail-closed refusal on the send path: target not Alive, wrong
    /// peer protocol, or a key-file problem.
    #[error("{0}")]
    Send(String),
}

impl Error {
    pub fn io(source: std::io::Error, what: impl std::fmt::Display) -> Self {
        Error::Io(format!("{what}: {source}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
