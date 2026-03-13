//! Structured error types for the sandbox service.
//!
//! [`SandboxError`] distinguishes decode failures (bad client payload) from
//! internal / runtime errors so the RPC layer can map them to the correct
//! status code (400 vs 500).

use std::fmt;

/// Error returned by [`SandboxService`](crate::sandbox::SandboxService) methods.
#[derive(Debug)]
pub enum SandboxError {
    /// The request payload could not be decoded (protobuf parse failure).
    Decode(String),
    /// A runtime or business-logic error.
    Internal(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(msg) => write!(f, "decode error: {msg}"),
            Self::Internal(msg) => f.write_str(msg),
        }
    }
}

impl SandboxError {
    /// HTTP-style status code: 400 for decode errors, 500 for internal errors.
    pub const fn status_code(&self) -> i32 {
        match self {
            Self::Decode(_) => 400,
            Self::Internal(_) => 500,
        }
    }
}
