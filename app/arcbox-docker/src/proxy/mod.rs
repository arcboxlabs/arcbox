//! Smart proxy for forwarding Docker API requests to guest dockerd.
//!
//! Provides HTTP/1.1 client over vsock to forward requests, with support
//! for streaming responses and HTTP upgrades (attach, exec, BuildKit).

mod activity;
mod connector;
mod fallback;
mod forward;
mod headers;
mod session;
mod state;
mod upgrade;
mod upload;
mod uri;

pub use activity::ActivityClass;
pub use connector::VsockConnector;
pub(crate) use fallback::invalidate_on_guest_error;
pub use fallback::proxy_fallback;
pub use forward::{proxy_to_guest_pooled, proxy_to_guest_stream_pooled};
pub use session::GuestHttpClient;
pub use state::{ActivityHook, ActivityLease, ProxyState};
pub use upgrade::proxy_with_upgrade;
pub use upload::proxy_streaming_upload;

use crate::error::Result;
pub use arcbox_transport::vsock::{HalfCloseStream, VsockShutdown, VsockStream};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Timeout for the HTTP/1.1 handshake with guest dockerd.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// The byte stream a guest dockerd connection is spoken over.
///
/// The vsock fd underneath cannot half-close on macOS, so EOF travels
/// in-band, and an unread guest→host vsock stalls the whole VM on
/// Virtualization.framework, so each direction's backpressure travels
/// in-band too: the guest agent wraps its end of the same channel in the
/// same framing (agent protocol v5). Anything that must close one direction
/// of an attach or exec session while the other keeps flowing depends on
/// the first; `docker version` answering during a paused `docker attach`
/// depends on the second.
pub type GuestStream = HalfCloseStream<VsockStream>;

/// Abstraction over guest connection establishment.
///
/// Production code connects via vsock ([`VsockConnector`]); integration tests
/// can connect via Unix socket. Both produce a [`TokioIo<GuestStream>`]
/// because [`VsockStream`] wraps any pollable file descriptor.
pub trait GuestConnector: Send + Sync + 'static {
    /// Opens a new connection to guest dockerd.
    fn connect(&self) -> Pin<Box<dyn Future<Output = Result<TokioIo<GuestStream>>> + Send + '_>>;
}
