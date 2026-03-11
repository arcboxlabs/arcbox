//! Error types for the DHCP crate.

use thiserror::Error;

/// Result type alias for DHCP operations.
pub type Result<T> = std::result::Result<T, DhcpError>;

/// Errors that can occur during DHCP operations.
#[derive(Debug, Error)]
pub enum DhcpError {
    /// Packet parsing or processing error.
    #[error("DHCP error: {0}")]
    Protocol(String),

    /// No IP addresses available in the pool.
    #[error("no IP addresses available")]
    PoolExhausted,
}
