//! Errors of the vhost-user back-end.

/// A vhost-user back-end error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The socket, an `mmap`, or a notifier failed.
    #[error("vhost-user I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The front-end sent something the protocol does not allow.
    #[error("vhost-user protocol: {0}")]
    Protocol(String),
    /// The front-end asked for something this back-end does not support.
    #[error("vhost-user unsupported: {0}")]
    Unsupported(String),
    /// The device rejected a request.
    #[error("vhost-user device: {0}")]
    Device(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
