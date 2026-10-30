//! Port forwarding service.
//!
//! This module provides TCP and UDP port forwarding between host and guest.
//! It supports dynamic addition and removal of forwarding rules.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

use crate::error::{NetError, Result};

/// Port forwarding rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortForwardRule {
    /// Host address to listen on.
    pub host_addr: SocketAddr,
    /// Guest address to forward to.
    pub guest_addr: SocketAddr,
    /// Protocol (TCP or UDP).
    pub protocol: Protocol,
}

impl PortForwardRule {
    /// Creates a new TCP port forwarding rule.
    #[must_use]
    pub fn tcp(host_addr: SocketAddr, guest_addr: SocketAddr) -> Self {
        Self {
            host_addr,
            guest_addr,
            protocol: Protocol::Tcp,
        }
    }

    /// Creates a new UDP port forwarding rule.
    #[must_use]
    pub fn udp(host_addr: SocketAddr, guest_addr: SocketAddr) -> Self {
        Self {
            host_addr,
            guest_addr,
            protocol: Protocol::Udp,
        }
    }
}

/// Network protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// TCP protocol.
    Tcp,
    /// UDP protocol.
    Udp,
}

/// Active forwarder state.
struct ActiveForwarder {
    /// Shutdown signal sender.
    shutdown_tx: mpsc::Sender<()>,
    /// Task handle.
    handle: JoinHandle<()>,
}

/// Port forwarding manager.
///
/// Manages port forwarding rules and their associated listeners.
/// Each rule creates a listener on the host that forwards connections
/// to the guest address.
pub struct PortForwarder {
    /// Configured rules.
    rules: Vec<PortForwardRule>,
    /// Active forwarders, keyed by the complete host binding.
    active: Arc<RwLock<HashMap<(SocketAddr, Protocol), ActiveForwarder>>>,
    /// Whether the forwarder is running.
    running: bool,
}

impl PortForwarder {
    /// Creates a new port forwarder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            active: Arc::new(RwLock::new(HashMap::new())),
            running: false,
        }
    }

    /// Adds a forwarding rule.
    pub fn add_rule(&mut self, rule: PortForwardRule) {
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
            self.running = false;
        }
    }

    /// Removes a forwarding rule by host address.
    pub fn remove_rule(&mut self, host_addr: SocketAddr) {
        self.rules.retain(|r| r.host_addr != host_addr);
        self.running = false;
    }

    /// Returns all configured rules.
    #[must_use]
    pub fn rules(&self) -> &[PortForwardRule] {
        &self.rules
    }

    /// Starts port forwarding for all configured rules.
    ///
    /// # Errors
    ///
    /// Returns an error if any listener cannot be started.
    pub async fn start(&mut self) -> Result<()> {
        if self.running {
            let active = self.active.read().await;
            if active.len() == self.rules.len()
                && active
                    .values()
                    .all(|listener| !listener.handle.is_finished())
            {
                return Ok(());
            }
        }

        // A cancelled start or stop can leave only some listeners alive.
        self.stop().await;
        for rule in &self.rules {
            self.start_forwarder(rule.clone()).await?;
        }

        self.running = true;
        tracing::info!("Port forwarder started with {} rules", self.rules.len());
        Ok(())
    }

    /// Stops all port forwarding.
    pub async fn stop(&mut self) {
        self.running = false;
        let mut active = self.active.write().await;
        let addresses: Vec<_> = active.keys().copied().collect();
        for addr in addresses {
            let Some(forwarder) = active.get_mut(&addr) else {
                continue;
            };
            let _ = forwarder.shutdown_tx.try_send(());
            forwarder.handle.abort();
            // Keep the authoritative entry until the task has actually
            // dropped its bound socket. Cancellation leaves it retryable.
            let _ = (&mut forwarder.handle).await;
            active.remove(&addr);
            tracing::debug!("Stopped forwarder for {:?}", addr);
        }

        tracing::info!("Port forwarder stopped");
    }

    /// Starts a single forwarder for a rule.
    async fn start_forwarder(&self, rule: PortForwardRule) -> Result<()> {
        let key = (rule.host_addr, rule.protocol);

        // Hold the authority lock through bind. Cancellation before bind
        // completion drops the local socket; after spawn there is no await
        // before the handle is recorded.
        let mut active_guard = self.active.write().await;
        if active_guard.contains_key(&key) {
            return Err(NetError::PortForward(format!(
                "listener already exists on {}/{:?}",
                rule.host_addr, rule.protocol
            )));
        }

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let handle = match rule.protocol {
            Protocol::Tcp => {
                let listener = TcpListener::bind(rule.host_addr).await.map_err(|e| {
                    NetError::io(std::io::Error::new(
                        e.kind(),
                        format!("failed to bind {}: {}", rule.host_addr, e),
                    ))
                })?;

                tracing::info!(
                    "TCP port forward: {} -> {}",
                    rule.host_addr,
                    rule.guest_addr
                );

                tokio::spawn(tcp_forward_loop(listener, rule.guest_addr, shutdown_rx))
            }
            Protocol::Udp => {
                let socket = UdpSocket::bind(rule.host_addr).await.map_err(|e| {
                    NetError::io(std::io::Error::new(
                        e.kind(),
                        format!("failed to bind {}: {}", rule.host_addr, e),
                    ))
                })?;

                tracing::info!(
                    "UDP port forward: {} -> {}",
                    rule.host_addr,
                    rule.guest_addr
                );

                tokio::spawn(udp_forward_loop(socket, rule.guest_addr, shutdown_rx))
            }
        };

        active_guard.insert(
            key,
            ActiveForwarder {
                shutdown_tx,
                handle,
            },
        );

        Ok(())
    }

    /// Returns the number of active forwarders.
    pub async fn active_count(&self) -> usize {
        self.active.read().await.len()
    }

    /// Returns whether the forwarder is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }
}

impl Default for PortForwarder {
    fn default() -> Self {
        Self::new()
    }
}

/// TCP forwarding loop.
///
/// Accepts connections on the listener and forwards them to the guest.
async fn tcp_forward_loop(
    listener: TcpListener,
    guest_addr: SocketAddr,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    loop {
        tokio::select! {
            // Check for shutdown signal.
            _ = shutdown_rx.recv() => {
                tracing::debug!("TCP forwarder shutdown");
                break;
            }
            // Accept new connections.
            result = listener.accept() => {
                match result {
                    Ok((client, peer_addr)) => {
                        tracing::debug!("TCP connection from {} -> {}", peer_addr, guest_addr);
                        tokio::spawn(handle_tcp_connection(client, guest_addr));
                    }
                    Err(e) => {
                        tracing::warn!("TCP accept error: {}", e);
                    }
                }
            }
        }
    }
}

/// Handles a single TCP connection by forwarding data bidirectionally.
async fn handle_tcp_connection(mut client: TcpStream, guest_addr: SocketAddr) {
    // Connect to guest.
    let mut guest = match TcpStream::connect(guest_addr).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::warn!("Failed to connect to guest {}: {}", guest_addr, e);
            return;
        }
    };

    // Split streams for bidirectional forwarding.
    let (mut client_read, mut client_write) = client.split();
    let (mut guest_read, mut guest_write) = guest.split();

    // Forward in both directions concurrently.
    let client_to_guest = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match client_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if guest_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    };

    let guest_to_client = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match guest_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if client_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    };

    // Run both directions concurrently, complete when either finishes.
    tokio::select! {
        () = client_to_guest => {}
        () = guest_to_client => {}
    }
}

/// UDP forwarding loop.
///
/// Receives packets on the socket and forwards them to the guest.
/// Also forwards responses back to the original sender.
async fn udp_forward_loop(
    socket: UdpSocket,
    guest_addr: SocketAddr,
    mut shutdown_rx: mpsc::Receiver<()>,
) {
    let socket = Arc::new(socket);
    // Track client addresses for response routing (simplified: last sender).
    let client_addr: Arc<RwLock<Option<SocketAddr>>> = Arc::new(RwLock::new(None));

    let mut buf = vec![0u8; 65535];

    loop {
        tokio::select! {
            // Check for shutdown signal.
            _ = shutdown_rx.recv() => {
                tracing::debug!("UDP forwarder shutdown");
                break;
            }
            // Receive packets.
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, peer_addr)) => {
                        // If packet is from guest, send to client.
                        if peer_addr == guest_addr {
                            let client = client_addr.read().await;
                            if let Some(addr) = *client {
                                let _ = socket.send_to(&buf[..len], addr).await;
                            }
                        } else {
                            // Packet from client, forward to guest.
                            {
                                let mut client = client_addr.write().await;
                                *client = Some(peer_addr);
                            }
                            let _ = socket.send_to(&buf[..len], guest_addr).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("UDP recv error: {}", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn test_port_forward_rule_tcp() {
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 64, 2), 80));

        let rule = PortForwardRule::tcp(host, guest);
        assert_eq!(rule.host_addr, host);
        assert_eq!(rule.guest_addr, guest);
        assert_eq!(rule.protocol, Protocol::Tcp);
    }

    #[test]
    fn test_port_forward_rule_udp() {
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5353));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 64, 2), 53));

        let rule = PortForwardRule::udp(host, guest);
        assert_eq!(rule.protocol, Protocol::Udp);
    }

    #[test]
    fn test_port_forwarder_add_remove() {
        let mut forwarder = PortForwarder::new();

        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 64, 2), 80));

        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        assert_eq!(forwarder.rules().len(), 1);

        forwarder.remove_rule(host);
        assert_eq!(forwarder.rules(), []);
    }

    #[tokio::test]
    async fn test_port_forwarder_not_running() {
        let forwarder = PortForwarder::new();
        assert!(!forwarder.is_running());
        assert_eq!(forwarder.active_count().await, 0);
    }

    #[tokio::test]
    async fn stop_drains_listeners_after_partial_start_failure() {
        let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let occupied_addr = occupied.local_addr().unwrap();
        let mut forwarder = PortForwarder::new();
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80));
        forwarder.add_rule(PortForwardRule::tcp(
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
            guest,
        ));
        forwarder.add_rule(PortForwardRule::tcp(occupied_addr, guest));

        assert!(forwarder.start().await.is_err());
        assert!(!forwarder.is_running());
        assert_eq!(forwarder.active_count().await, 1);
        forwarder.stop().await;
        assert_eq!(forwarder.active_count().await, 0);
    }

    #[tokio::test]
    async fn tcp_and_udp_share_a_host_address_without_sharing_a_listener() {
        let mut forwarder = PortForwarder::new();
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80));
        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        forwarder.add_rule(PortForwardRule::udp(host, guest));
        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        forwarder.start().await.unwrap();
        assert_eq!(forwarder.active_count().await, 2);

        forwarder.stop().await;
        assert_eq!(forwarder.active_count().await, 0);
    }

    #[tokio::test]
    async fn cancelled_stop_requires_a_complete_restart() {
        let mut forwarder = PortForwarder::new();
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80));
        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        forwarder.start().await.unwrap();

        let active = Arc::clone(&forwarder.active);
        let lock = active.write().await;
        tokio::select! {
            biased;
            () = forwarder.stop() => panic!("the active lock must block cleanup"),
            () = std::future::ready(()) => {}
        }
        assert!(!forwarder.is_running());
        for listener in lock.values() {
            listener.handle.abort();
        }
        drop(lock);

        forwarder.start().await.unwrap();
        assert!(forwarder.is_running());
        assert!(
            active
                .read()
                .await
                .values()
                .all(|listener| !listener.handle.is_finished())
        );
        assert_eq!(forwarder.active_count().await, 1);
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn restart_replaces_an_exited_listener() {
        let mut forwarder = PortForwarder::new();
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80));
        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        forwarder.start().await.unwrap();
        {
            let mut active = forwarder.active.write().await;
            let listener = active.values_mut().next().unwrap();
            listener.handle.abort();
            listener.shutdown_tx.closed().await;
        }

        forwarder.start().await.unwrap();
        assert!(
            forwarder
                .active
                .read()
                .await
                .values()
                .all(|listener| !listener.handle.is_finished())
        );
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn conflicting_destinations_cannot_share_one_listener() {
        let mut forwarder = PortForwarder::new();
        let host = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
        for port in [80, 81] {
            let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
            forwarder.add_rule(PortForwardRule::tcp(host, guest));
        }
        assert!(forwarder.start().await.is_err());
        assert!(!forwarder.is_running());
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn stop_waits_until_the_bound_socket_is_reusable() {
        let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let host = reservation.local_addr().unwrap();
        drop(reservation);

        let mut forwarder = PortForwarder::new();
        let guest = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80));
        forwarder.add_rule(PortForwardRule::tcp(host, guest));
        forwarder.start().await.unwrap();
        forwarder.stop().await;

        std::net::TcpListener::bind(host).expect("stop must release the socket before returning");
    }
}
