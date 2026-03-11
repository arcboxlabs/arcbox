//! Smart proxy for forwarding Docker API requests to guest dockerd.
//!
//! Provides HTTP/1.1 client over vsock to forward requests, with support
//! for streaming responses and HTTP upgrades (attach, exec, BuildKit).

mod connector;
mod fallback;
mod forward;
mod port_bindings;
mod stream;
mod upload;
mod upgrade;

pub use connector::VsockConnector;
pub use fallback::proxy_fallback;
pub use forward::{proxy_to_guest, proxy_to_guest_stream};
pub use port_bindings::{PortBindingInfo, parse_port_bindings};
pub use stream::RawFdStream;
pub use upload::proxy_streaming_upload;
pub use upgrade::proxy_with_upgrade;

use crate::error::Result;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Timeout for the HTTP/1.1 handshake with guest dockerd.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Abstraction over guest connection establishment.
///
/// Production code connects via vsock ([`VsockConnector`]); integration tests
/// can connect via Unix socket. Both produce a [`TokioIo<RawFdStream>`]
/// because [`RawFdStream`] wraps any pollable file descriptor.
pub trait GuestConnector: Send + Sync + 'static {
    /// Opens a new connection to guest dockerd.
    fn connect(&self) -> Pin<Box<dyn Future<Output = Result<TokioIo<RawFdStream>>> + Send + '_>>;
}
