//! UDP-tunnel socket backend (macOS).

#![cfg(target_os = "macos")]

use std::collections::VecDeque;

use crate::backend::NetBackend;
use crate::header::NetPacket;

/// Socket-based network backend for macOS using UDP tunnel.
pub struct SocketBackend {
    /// Socket file descriptor.
    socket: std::net::UdpSocket,
    /// Remote address.
    remote: std::net::SocketAddr,
    /// Pending packets.
    pending: VecDeque<Vec<u8>>,
}

impl SocketBackend {
    /// Creates a new socket backend.
    ///
    /// This creates a UDP socket for packet exchange with an external network daemon.
    pub fn new(local_addr: &str, remote_addr: &str) -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind(local_addr)?;
        socket.set_nonblocking(true)?;

        let remote: std::net::SocketAddr = remote_addr
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        tracing::info!("Created socket backend: {} -> {}", local_addr, remote_addr);

        Ok(Self {
            socket,
            remote,
            pending: VecDeque::new(),
        })
    }

    /// Creates a loopback socket backend on random ports.
    pub fn loopback() -> std::io::Result<Self> {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let local_addr = socket.local_addr()?;
        socket.set_nonblocking(true)?;
        socket.connect(local_addr)?;

        tracing::info!("Created loopback socket backend: {}", local_addr);

        Ok(Self {
            socket,
            remote: local_addr,
            pending: VecDeque::new(),
        })
    }
}

impl NetBackend for SocketBackend {
    fn send(&mut self, packet: &NetPacket) -> std::io::Result<usize> {
        self.socket.send_to(&packet.data, self.remote)
    }

    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(data) = self.pending.pop_front() {
            let len = data.len().min(buf.len());
            buf[..len].copy_from_slice(&data[..len]);
            return Ok(len);
        }

        match self.socket.recv(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn has_data(&self) -> bool {
        !self.pending.is_empty() || {
            let mut buf = [0u8; 1];
            matches!(self.socket.peek(&mut buf), Ok(n) if n > 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_backend_loopback() {
        let backend = SocketBackend::loopback();
        assert!(backend.is_ok());
    }

    #[test]
    fn test_socket_backend_new_invalid_remote() {
        let result = SocketBackend::new("127.0.0.1:0", "invalid-address");
        assert!(result.is_err());
    }

    #[test]
    fn test_socket_backend_has_data_empty() {
        let backend = SocketBackend::loopback().unwrap();
        assert!(!backend.has_data());
    }

    #[test]
    fn test_socket_backend_recv_would_block() {
        let mut backend = SocketBackend::loopback().unwrap();
        let mut buf = [0u8; 100];
        let n = backend.recv(&mut buf).unwrap();
        assert_eq!(n, 0); // Would block returns 0
    }
}
