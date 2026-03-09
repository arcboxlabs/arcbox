use super::addr::VsockAddr;
use super::stream::VsockStream;
use crate::error::{Result, TransportError};
use std::os::fd::RawFd;
use tokio::sync::mpsc;

/// Information about an incoming vsock connection.
#[derive(Debug, Clone)]
pub struct IncomingVsockConnection {
    /// File descriptor for the connection.
    pub fd: RawFd,
    /// Source port (guest port).
    pub source_port: u32,
    /// Destination port (host port).
    pub destination_port: u32,
}

/// On macOS, direct vsock connection is not supported.
/// Use `VsockTransport::from_raw_fd()` with a fd obtained from the hypervisor layer.
pub fn connect_vsock(_addr: VsockAddr) -> Result<VsockStream> {
    Err(TransportError::Protocol(
        "macOS vsock requires connection through hypervisor".to_string(),
    ))
}

/// Vsock listener handle for macOS.
///
/// On macOS, vsock listening is done through `VZVirtioSocketDevice`.
/// This handle wraps a channel that receives incoming connections
/// from the Virtualization.framework layer.
#[allow(dead_code)]
pub struct VsockListenerHandle {
    /// Port number being listened on.
    port: u32,
    /// Channel receiver for incoming connections.
    receiver: mpsc::UnboundedReceiver<IncomingVsockConnection>,
}

#[allow(dead_code)]
impl VsockListenerHandle {
    /// Creates a new listener handle from a channel.
    pub const fn new(
        port: u32,
        receiver: mpsc::UnboundedReceiver<IncomingVsockConnection>,
    ) -> Self {
        Self { port, receiver }
    }

    /// Returns the port number.
    pub const fn port(&self) -> u32 {
        self.port
    }

    /// Accepts an incoming connection.
    pub async fn accept(&mut self) -> Result<(VsockStream, VsockAddr)> {
        match self.receiver.recv().await {
            Some(conn) => {
                // SAFETY: conn.fd comes from VZVirtioSocketDevice and is a
                // valid connected vsock file descriptor.
                let stream =
                    unsafe { VsockStream::from_raw_fd(conn.fd) }.map_err(TransportError::io)?;
                let addr = VsockAddr::new(VsockAddr::CID_ANY, conn.source_port);
                Ok((stream, addr))
            }
            None => Err(TransportError::ConnectionReset),
        }
    }

    /// Tries to accept a connection without blocking.
    pub fn try_accept(&mut self) -> Option<Result<(VsockStream, VsockAddr)>> {
        match self.receiver.try_recv() {
            Ok(conn) => {
                // SAFETY: conn.fd comes from VZVirtioSocketDevice and is a
                // valid connected vsock file descriptor.
                let stream =
                    unsafe { VsockStream::from_raw_fd(conn.fd) }.map_err(TransportError::io);
                match stream {
                    Ok(s) => {
                        let addr = VsockAddr::new(VsockAddr::CID_ANY, conn.source_port);
                        Some(Ok((s, addr)))
                    }
                    Err(e) => Some(Err(e)),
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => None,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                Some(Err(TransportError::ConnectionReset))
            }
        }
    }
}
