//! Error types for migration planning and execution.

use std::path::PathBuf;

/// Result alias for migration operations.
pub type Result<T> = std::result::Result<T, MigrationError>;

/// Migration failures.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The source runtime could not be located.
    #[error("migration source '{kind}' is unavailable at {path}")]
    MissingSource {
        /// Source kind.
        kind: &'static str,
        /// Expected socket path.
        path: PathBuf,
    },
    /// The source runtime is not supported by v1.
    #[error("unsupported migration source: {0}")]
    UnsupportedSource(String),
    /// The discovered object is out of scope for v1.
    #[error("unsupported resource for v1 migration: {0}")]
    UnsupportedResource(String),
    /// The current state blocks migration until the user confirms or changes it.
    #[error("migration is blocked: {0}")]
    Blocked(String),
    /// The migration plan is internally inconsistent.
    #[error("invalid migration plan: {0}")]
    InvalidPlan(String),
    /// The remote Docker API returned an error.
    #[error("docker API error: {0}")]
    Docker(String),
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON encoding or decoding failed.
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
}
