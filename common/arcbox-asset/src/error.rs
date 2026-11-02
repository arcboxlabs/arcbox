//! Error types for asset operations.

/// Errors that can occur during asset download, verification, or caching.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    /// SHA-256 checksum mismatch after download.
    #[error("sha256 mismatch for '{name}': expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },

    /// HTTP or network-level download failure.
    #[error("download failed: {0}")]
    Download(String),

    /// The server answered with a status the download cannot use.
    #[error("HTTP {status} for {url}")]
    HttpStatus { url: String, status: u16 },

    /// Filesystem I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl AssetError {
    /// Whether the same request may succeed when retried: a network failure,
    /// or a status that reports a server-side or rate-limit condition. A
    /// checksum mismatch, any other status, or a local I/O error repeats.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Download(_) => true,
            Self::HttpStatus { status, .. } => *status >= 500 || matches!(status, 408 | 429),
            Self::ChecksumMismatch { .. } | Self::Io(_) => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, AssetError>;
