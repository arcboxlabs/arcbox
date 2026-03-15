//! Guest-side DNS server for container and sandbox name resolution.
//!
//! Listens on `0.0.0.0:53` (UDP) inside the guest VM. Lookup priority:
//!
//! 1. Container registry (name → Docker bridge IP)
//! 2. Sandbox registry (id → TAP IP)
//! 3. `*.arcbox.local` not found → NXDOMAIN
//! 4. Everything else → forward to gateway (`192.168.64.1:53`)

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use arcbox_dns::{DEFAULT_TTL, DnsQuery, DnsRecordType};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Gateway address where the host-side DNS forwarder runs.
const GATEWAY: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(192, 168, 64, 1)), 53);

/// Local domain suffix — queries for `*.arcbox.local` that miss the
/// registries get an authoritative NXDOMAIN instead of forwarding.
const LOCAL_DOMAIN: &str = "arcbox.local";

/// Maximum DNS UDP packet size.
const MAX_PACKET: usize = 512;

/// Shared sandbox registry using std::sync::RwLock so it can be written
/// from synchronous code (sandbox.rs) and read from async code (dns_server).
pub type SandboxRegistry = Arc<std::sync::RwLock<HashMap<String, Ipv4Addr>>>;

// Global sandbox registry so sandbox.rs can register without needing
// an async handle. The GuestDnsServer reads from these same Arcs.
static SANDBOX_REGISTRY: std::sync::OnceLock<SandboxRegistry> = std::sync::OnceLock::new();

/// Returns the global sandbox registry. Initialized on first access.
pub fn sandbox_registry() -> &'static SandboxRegistry {
    SANDBOX_REGISTRY.get_or_init(|| Arc::new(std::sync::RwLock::new(HashMap::new())))
}

/// Upstream forwarding timeout.
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Shared container registry: container name → IPv4 address.
pub type ContainerRegistry = Arc<RwLock<HashMap<String, Ipv4Addr>>>;

/// Guest DNS server.
pub struct GuestDnsServer {
    containers: ContainerRegistry,
    sandboxes: SandboxRegistry,
    cancel: CancellationToken,
}

impl GuestDnsServer {
    /// Creates a new server instance. Does not start listening yet.
    ///
    /// The sandbox registry is shared via a global so that `sandbox.rs`
    /// can register entries synchronously without an async handle.
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            containers: Arc::new(RwLock::new(HashMap::new())),
            sandboxes: Arc::clone(sandbox_registry()),
            cancel,
        }
    }

    /// Returns a handle to the container registry for external registration.
    #[allow(dead_code)]
    pub fn containers(&self) -> ContainerRegistry {
        Arc::clone(&self.containers)
    }

    /// Returns a handle to the sandbox registry.
    #[allow(dead_code)]
    pub fn sandboxes(&self) -> SandboxRegistry {
        Arc::clone(&self.sandboxes)
    }

    /// Registers a container name → IP mapping.
    pub async fn register_container(&self, name: &str, ip: Ipv4Addr) {
        let key = name.to_lowercase();
        tracing::debug!(name = %key, %ip, "dns: register container");
        self.containers.write().await.insert(key, ip);
    }

    /// Deregisters a container by name.
    pub async fn deregister_container(&self, name: &str) {
        let key = name.to_lowercase();
        tracing::debug!(name = %key, "dns: deregister container");
        self.containers.write().await.remove(&key);
    }

    /// Registers a sandbox ID → IP mapping.
    #[allow(dead_code)]
    pub fn register_sandbox(&self, id: &str, ip: Ipv4Addr) {
        let key = id.to_lowercase();
        tracing::debug!(id = %key, %ip, "dns: register sandbox");
        if let Ok(mut map) = self.sandboxes.write() {
            map.insert(key, ip);
        }
    }

    /// Deregisters a sandbox by ID.
    #[allow(dead_code)]
    pub fn deregister_sandbox(&self, id: &str) {
        let key = id.to_lowercase();
        tracing::debug!(id = %key, "dns: deregister sandbox");
        if let Ok(mut map) = self.sandboxes.write() {
            map.remove(&key);
        }
    }

    /// Runs the DNS server. Blocks until cancellation.
    pub async fn run(&self) -> anyhow::Result<()> {
        let sock = UdpSocket::bind("0.0.0.0:53").await?;
        tracing::info!("guest DNS server listening on 0.0.0.0:53");

        let mut buf = [0u8; MAX_PACKET];
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => {
                    tracing::info!("guest DNS server shutting down");
                    return Ok(());
                }
                result = sock.recv_from(&mut buf) => {
                    let (len, peer) = result?;
                    let data = &buf[..len];

                    match self.handle_query(data).await {
                        Ok(response) => {
                            if let Err(e) = sock.send_to(&response, peer).await {
                                tracing::warn!(error = %e, "dns: failed to send response");
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "dns: failed to handle query");
                            // Try to send SERVFAIL.
                            if let Ok(fail) = arcbox_dns::build_servfail(data) {
                                let _ = sock.send_to(&fail, peer).await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Handles a single DNS query: local lookup, then forward.
    async fn handle_query(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let query = DnsQuery::parse(data)?;
        let name_lower = query.name.to_lowercase();

        // 1. Container registry lookup.
        if let Some(&ip) = self.containers.read().await.get(&name_lower) {
            return Ok(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?);
        }

        // 2. Try with `.arcbox.local` suffix stripped.
        let bare_name = name_lower
            .strip_suffix(&format!(".{LOCAL_DOMAIN}"))
            .unwrap_or(&name_lower);
        if bare_name != name_lower {
            if let Some(&ip) = self.containers.read().await.get(bare_name) {
                return Ok(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?);
            }
        }

        // 3. Sandbox registry lookup (std::sync::RwLock, not async).
        if let Some(ip) = self
            .sandboxes
            .read()
            .ok()
            .and_then(|g| g.get(&name_lower).copied())
        {
            return Ok(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?);
        }
        if bare_name != name_lower {
            if let Some(ip) = self
                .sandboxes
                .read()
                .ok()
                .and_then(|g| g.get(bare_name).copied())
            {
                return Ok(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?);
            }
        }

        // 4. Authoritative NXDOMAIN for unresolved local-domain queries.
        if name_lower == LOCAL_DOMAIN || name_lower.ends_with(&format!(".{LOCAL_DOMAIN}")) {
            // Only for A/AAAA — let other types pass through.
            if matches!(query.qtype, DnsRecordType::A | DnsRecordType::Aaaa) {
                return Ok(arcbox_dns::build_nxdomain(data)?);
            }
        }

        // 5. Forward to gateway.
        self.forward_to_gateway(data).await
    }

    /// Forwards a query to the gateway DNS forwarder over UDP.
    async fn forward_to_gateway(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        sock.send_to(data, GATEWAY).await?;

        let mut buf = [0u8; MAX_PACKET];
        let len = tokio::time::timeout(FORWARD_TIMEOUT, sock.recv(&mut buf)).await??;
        Ok(buf[..len].to_vec())
    }
}
