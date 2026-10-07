//! # arcbox-transport
//!
//! Transport layer abstractions for `ArcBox`.
//!
//! This crate provides message transports and raw vsock streams:
//!
//! - [`UnixTransport`]: Length-prefixed messages over a caller-selected Unix socket.
//! - [`VsockTransport`]: Asynchronous framed messages between host and guest.
//! - [`vsock::BlockingVsockTransport`]: Blocking framed messages over a connected file descriptor.
//! - [`vsock::VsockStream`]: Raw asynchronous bytes for tunnels and proxies.
//!
//! ## Architecture
//!
//! ```text
//! UnixTransport          → UnixStream → caller-supplied socket path
//! VsockTransport         → VsockStream → connected vsock file descriptor
//! BlockingVsockTransport              → connected vsock file descriptor
//! ```
//!
//! Linux can connect to a [`vsock::VsockAddr`] (CID and port). On macOS, the
//! VMM supplies the connected file descriptor. Socket paths and VM selection
//! belong to callers, not to this crate.
//!
//! [`vsock::HalfCloseStream`] adds EOF and flow-control framing to a byte
//! stream. Both peers must use this framing for tunnels that need independent
//! read and write completion, such as the guest Docker API proxy.

pub mod error;
pub mod unix;
pub mod vsock;

pub use error::{Result, TransportError};
pub use unix::UnixTransport;
pub use vsock::VsockTransport;

use async_trait::async_trait;
use bytes::Bytes;

/// Transport trait for sending and receiving messages.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Connects to the remote endpoint.
    async fn connect(&mut self) -> Result<()>;

    /// Disconnects from the remote endpoint.
    async fn disconnect(&mut self) -> Result<()>;

    /// Sends a message.
    async fn send(&mut self, data: Bytes) -> Result<()>;

    /// Receives a message.
    async fn recv(&mut self) -> Result<Bytes>;

    /// Returns whether the transport is connected.
    fn is_connected(&self) -> bool;
}

/// Transport listener for accepting connections.
#[async_trait]
pub trait TransportListener: Send + Sync {
    /// The transport type for accepted connections.
    type Transport: Transport;

    /// Binds to the endpoint.
    async fn bind(&mut self) -> Result<()>;

    /// Accepts a new connection.
    async fn accept(&mut self) -> Result<Self::Transport>;

    /// Closes the listener.
    async fn close(&mut self) -> Result<()>;
}
