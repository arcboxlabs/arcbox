use arcbox_transport::Transport;
use arcbox_transport::vsock::{BlockingVsockTransport, VsockTransport};
use bytes::Bytes;
use std::time::Duration;

/// Transport backend for agent RPC.
///
/// `Async` is the default for Linux AF_VSOCK and macOS VZ backend (real vsock
/// fds that tokio/kqueue handles correctly).
///
/// `Blocking` serves synchronous shutdown and macOS HV socketpair fds.
/// HV connection retries can stall the tokio/kqueue reactor, so this transport
/// uses `libc::poll` without registering the socket with tokio.
pub(super) enum AgentTransport {
    Async(VsockTransport),
    Blocking(BlockingVsockTransport),
}

/// Default RPC deadline for blocking transport operations.
pub(super) const BLOCKING_RPC_TIMEOUT: Duration = Duration::from_secs(5);

impl AgentTransport {
    pub(super) fn close(&mut self) {
        match self {
            Self::Async(transport) => transport.close(),
            Self::Blocking(transport) => transport.close(),
        }
    }

    /// Async send — only valid for `Async` variant. Streaming RPCs that
    /// consume `self` and spawn async tasks must go through the async path.
    pub(super) async fn async_send(
        &mut self,
        data: Bytes,
    ) -> std::result::Result<(), arcbox_transport::error::TransportError> {
        match self {
            Self::Async(t) => t.send(data).await,
            Self::Blocking(_) => Err(arcbox_transport::error::TransportError::Protocol(
                "streaming RPCs not supported on blocking transport".into(),
            )),
        }
    }

    /// Async recv — only valid for `Async` variant.
    pub(super) async fn async_recv(
        &mut self,
    ) -> std::result::Result<Bytes, arcbox_transport::error::TransportError> {
        match self {
            Self::Async(t) => t.recv().await,
            Self::Blocking(_) => Err(arcbox_transport::error::TransportError::Protocol(
                "streaming RPCs not supported on blocking transport".into(),
            )),
        }
    }

    /// Split into send/recv halves — only valid for `Async` variant.
    pub(super) fn into_split(
        self,
    ) -> std::result::Result<
        (
            arcbox_transport::vsock::VsockSender,
            arcbox_transport::vsock::VsockReceiver,
        ),
        arcbox_transport::error::TransportError,
    > {
        match self {
            Self::Async(t) => t.into_split(),
            Self::Blocking(_) => Err(arcbox_transport::error::TransportError::Protocol(
                "split not supported on blocking transport".into(),
            )),
        }
    }
}
