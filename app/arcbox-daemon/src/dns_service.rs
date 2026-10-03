//! Async UDP DNS server for the runtime's configured local domain.
//!
//! Listens on `127.0.0.1:{port}` and resolves container hostnames registered
//! via [`NetworkManager`]. Queries for unregistered names in that domain
//! get a NODATA response (NOERROR, no records); all other queries are
//! forwarded to upstream DNS. NODATA rather than NXDOMAIN because the domain
//! ends in `.local`: mDNSResponder also multicasts the query and ignores a
//! unicast NXDOMAIN, so the lookup would wait out the mDNS timeout instead
//! (`DnsForwarder::try_resolve_locally_or_nodata`).

use anyhow::{Context, Result};
use arcbox_net::NetworkManager;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

/// Async UDP DNS server backed by [`NetworkManager`]'s DNS forwarder.
///
/// Binding and serving are two steps: the socket is bound in
/// `start_control_plane`, before the runtime exists, and served from
/// `start_runtime_services` once the runtime's [`NetworkManager`] does.
/// Queries that arrive in between wait in the socket's receive buffer.
pub struct DnsService {
    socket: UdpSocket,
}

impl DnsService {
    /// Binds the UDP socket on `127.0.0.1`.
    ///
    /// An explicitly requested port (`--dns-port` or `ARCBOX_DNS_PORT`, `0`
    /// for any) must bind or startup fails, as for the Kubernetes proxy and
    /// the SSH server. The profile's `default` port is best-effort with one
    /// difference: `NETWORK_READY` promises DNS, so a taken default falls
    /// back to an OS-allocated port instead of leaving the daemon without
    /// DNS. Self-setup publishes whichever port was bound through
    /// `/etc/resolver/<domain>`, and `abctl dns status` probes the port that
    /// file names.
    ///
    /// Called right after the daemon lease is held, so the previous daemon
    /// has released its socket and a bind failure aborts startup before any
    /// VM boots.
    pub async fn bind_requested(requested: Option<u16>, default: u16) -> Result<Self> {
        let socket = match requested {
            Some(port) => bind_loopback(port)
                .await
                .with_context(|| bind_error(port))?,
            None => match bind_loopback(default).await {
                Ok(socket) => socket,
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    let socket = bind_loopback(0).await.with_context(|| bind_error(0))?;
                    let fallback = socket
                        .local_addr()
                        .context("Failed to read DNS service address")?
                        .port();
                    tracing::warn!(
                        port = default,
                        fallback,
                        "DNS port in use; listening on an OS-allocated port instead"
                    );
                    socket
                }
                Err(error) => Err(error).with_context(|| bind_error(default))?,
            },
        };
        let actual_addr = socket
            .local_addr()
            .context("Failed to read DNS service address")?;

        tracing::info!(%actual_addr, "DNS service bound");
        Ok(Self { socket })
    }

    /// Returns the actual UDP port selected by the bound socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket address cannot be queried.
    pub fn host_port(&self) -> Result<u16> {
        Ok(self
            .socket
            .local_addr()
            .context("Failed to read DNS service address")?
            .port())
    }

    /// Runs the DNS event loop on the socket [`Self::bind_requested`] bound,
    /// resolving through `network_manager`.
    ///
    /// This method never returns under normal operation. Each incoming UDP
    /// packet is handled inline for local queries (fast path) or dispatched
    /// to a blocking task for upstream forwarding (slow path).
    pub async fn run(
        self,
        network_manager: Arc<NetworkManager>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let mut buf = [0u8; 512];
        let socket = Arc::new(self.socket);

        loop {
            let (len, src) = tokio::select! {
                result = socket.recv_from(&mut buf) => result?,
                () = shutdown.cancelled() => {
                    tracing::info!("DNS service shutting down");
                    return Ok(());
                }
            };

            // Fast path: local resolution or NODATA for the configured domain.
            // Operates on a borrowed slice to avoid allocation.
            if let Some(response) = network_manager.try_resolve_dns_or_nodata(&buf[..len]) {
                if let Err(e) = socket.send_to(&response, src).await {
                    tracing::debug!("Failed to send DNS response to {}: {}", src, e);
                }
                continue;
            }

            // Slow path: forward to upstream DNS via blocking I/O.
            // Only clone into owned buffer when actually needed.
            let query = buf[..len].to_vec();
            let nm = Arc::clone(&network_manager);
            let sock = Arc::clone(&socket);
            tokio::spawn(async move {
                // Pre-build SERVFAIL before query is moved into spawn_blocking.
                let servfail = build_servfail(&query);
                let result = tokio::task::spawn_blocking(move || nm.handle_dns_query(&query))
                    .await
                    .ok()
                    .and_then(|r| r.ok());

                match result {
                    Some(response) => {
                        if let Err(e) = sock.send_to(&response, src).await {
                            tracing::debug!("Failed to send DNS response to {}: {}", src, e);
                        }
                    }
                    None => {
                        // Send SERVFAIL so the client fails fast instead of timing out.
                        if let Some(servfail) = servfail {
                            let _ = sock.send_to(&servfail, src).await;
                        }
                        tracing::debug!("Upstream DNS forwarding failed for query from {}", src);
                    }
                }
            });
        }
    }
}

/// Binds a UDP socket on `127.0.0.1:{port}`; `0` asks the OS for any port.
async fn bind_loopback(port: u16) -> io::Result<UdpSocket> {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await
}

fn bind_error(port: u16) -> String {
    format!("DNS service failed to bind 127.0.0.1:{port}")
}

/// Builds a SERVFAIL response from the original query bytes.
fn build_servfail(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let mut response = query.to_vec();
    // QR=1, RD preserved, RCODE=2 (SERVFAIL)
    response[2] |= 0x80; // set QR
    response[3] = (response[3] & 0xF0) | 0x02; // RCODE=SERVFAIL
    // Zero answer/authority counts; keep question.
    response[6] = 0x00;
    response[7] = 0x00;
    response[8] = 0x00;
    response[9] = 0x00;
    response[10] = 0x00;
    response[11] = 0x00;
    // Truncate to just header + question (strip any additional sections).
    // For simplicity, return the full query as response — the counts are zeroed
    // so extra bytes are harmless, and most DNS libraries ignore trailing data.
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Builds a minimal DNS query packet for a given domain name (A record, IN class).
    fn build_dns_query(name: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(64);
        // Header: ID=0x1234, QR=0, QDCOUNT=1
        packet.extend_from_slice(&[0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00]);
        packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // Question: encode name labels
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0x00); // root label
        packet.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
        packet.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
        packet
    }

    #[tokio::test]
    async fn explicit_port_in_use_fails_startup() {
        let blocker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = blocker.local_addr().unwrap().port();

        let result = DnsService::bind_requested(Some(port), port).await;
        assert!(
            result.is_err(),
            "an explicitly requested port that is taken must fail, not fall back"
        );
    }

    #[tokio::test]
    async fn default_port_in_use_falls_back_to_an_os_port() {
        let blocker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let taken = blocker.local_addr().unwrap().port();

        let service = DnsService::bind_requested(None, taken)
            .await
            .expect("a taken default port must not fail startup");
        let bound = service.host_port().unwrap();
        assert_ne!(bound, taken);
        assert_ne!(bound, 0);
    }

    #[tokio::test]
    async fn test_dns_local_resolution_roundtrip() {
        let nm = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
        let ip = std::net::IpAddr::V4(Ipv4Addr::new(172, 17, 0, 2));
        nm.register_dns("my-nginx", ip);

        let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
        let server_addr = ("127.0.0.1", service.host_port().unwrap());

        let server_handle =
            tokio::spawn(async move { service.run(nm, CancellationToken::new()).await });

        // Send query from client.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let query = build_dns_query("my-nginx.arcbox.local");
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("DNS response timeout")
        .unwrap();

        let response = &buf[..len];
        // Verify it's a response (QR=1) with RCODE=0 (no error) and ANCOUNT=1.
        assert_eq!(response[2] & 0x80, 0x80, "QR bit should be set");
        assert_eq!(response[3] & 0x0F, 0, "RCODE should be 0 (NoError)");
        assert_eq!(response[7], 1, "ANCOUNT should be 1");

        // Extract the A record IP from the answer section.
        let answer_start = 12 + query.len() - 12; // skip header + question
        let rdata_offset = answer_start + 2 + 2 + 2 + 4 + 2; // name_ptr + type + class + ttl + rdlen
        let ip_bytes = &response[rdata_offset..rdata_offset + 4];
        assert_eq!(ip_bytes, &[172, 17, 0, 2]);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_dns_nodata_for_unregistered_local() {
        let nm = Arc::new(NetworkManager::new(arcbox_net::NetConfig::default()));
        // Nothing registered: the query gets NODATA, not NXDOMAIN.

        let service = DnsService::bind_requested(Some(0), 0).await.unwrap();
        let server_addr = ("127.0.0.1", service.host_port().unwrap());

        let server_handle =
            tokio::spawn(async move { service.run(nm, CancellationToken::new()).await });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let query = build_dns_query("nonexistent.arcbox.local");
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buf),
        )
        .await
        .expect("DNS response timeout")
        .unwrap();

        let response = &buf[..len];
        // NODATA: QR=1, RCODE=0, no answer records.
        assert_eq!(response[2] & 0x80, 0x80, "QR bit should be set");
        assert_eq!(response[3] & 0x0F, 0, "RCODE should be 0 (NoError)");
        assert_eq!(response[7], 0, "ANCOUNT should be 0");

        server_handle.abort();
    }

    #[test]
    fn test_build_servfail() {
        let query = build_dns_query("example.com");
        let response = build_servfail(&query).unwrap();
        assert_eq!(response[2] & 0x80, 0x80, "QR bit should be set");
        assert_eq!(response[3] & 0x0F, 2, "RCODE should be 2 (SERVFAIL)");
        assert_eq!(response[7], 0, "ANCOUNT should be 0");
    }

    #[test]
    fn test_build_servfail_too_short() {
        assert!(build_servfail(&[0; 5]).is_none());
    }
}
