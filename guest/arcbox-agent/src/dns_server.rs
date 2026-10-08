//! Guest-side DNS server for container and sandbox name resolution.
//!
//! Listens on `0.0.0.0:53` (UDP) inside the guest VM. Lookup priority:
//!
//! 1. Container registry (name → Docker bridge IP)
//! 2. Sandbox registry (id → TAP IP)
//! 3. Names under the configured domain not found → NXDOMAIN
//! 4. Everything else → forward to gateway (`10.0.2.1:53`)

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use arcbox_constants::dns::Domain;
use arcbox_dns::registry::{Lookup, Registry};
use arcbox_dns::{DEFAULT_TTL, DnsQuery, DnsRecordType, NamePattern};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Gateway address where the host-side DNS forwarder runs.
const GATEWAY: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(10, 0, 2, 1)), 53);

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

/// Current container claims and addresses, keyed by complete container ID.
pub type ContainerRegistry = Arc<RwLock<Registry>>;

/// Guest DNS server.
pub struct GuestDnsServer {
    domain: Domain,
    containers: ContainerRegistry,
    sandboxes: SandboxRegistry,
    cancel: CancellationToken,
}

impl GuestDnsServer {
    /// Creates a new server instance. Does not start listening yet.
    ///
    /// The sandbox registry is shared via a global so that `sandbox.rs`
    /// can register entries synchronously without an async handle.
    pub fn new(domain: Domain, cancel: CancellationToken) -> Self {
        Self {
            domain,
            containers: Arc::default(),
            sandboxes: Arc::clone(sandbox_registry()),
            cancel,
        }
    }

    /// Returns the suffix shared by DNS registration and HTTP discovery.
    #[must_use]
    pub fn domain(&self) -> &Domain {
        &self.domain
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
                            tracing::debug!(error = %e, "dns: query parse failed, forwarding to gateway");
                            // The gateway may handle query formats rejected by this parser.
                            match self.forward_to_gateway(data).await {
                                Ok(response) => {
                                    let _ = sock.send_to(&response, peer).await;
                                }
                                Err(_) => {
                                    if let Ok(fail) = arcbox_dns::build_servfail(data) {
                                        let _ = sock.send_to(&fail, peer).await;
                                    }
                                }
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
        match self.local_response(data, &query).await? {
            Some(response) => Ok(response),
            None => self.forward_to_gateway(data).await,
        }
    }

    async fn local_response(
        &self,
        data: &[u8],
        query: &DnsQuery,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let name_lower = query.name.to_lowercase();
        let suffix = format!(".{}", self.domain);

        // Known names have IPv4 data only; other record types receive NODATA.
        let is_a_query = query.qtype == DnsRecordType::A;

        if let Ok(name) = name_lower.parse::<Domain>() {
            let registry = self.containers.read().await;
            let relative = format!("{name}.{}", self.domain).parse::<Domain>().ok();
            let mut lookup = registry.resolve(&name);
            // Only exact aliases support short names; wildcards must not capture external DNS.
            if matches!(lookup, Lookup::Unmatched)
                && let Some(relative) = &relative
                && registry
                    .owners()
                    .filter_map(|id| registry.get(id))
                    .filter(|entry| entry.address.is_some())
                    .flat_map(|entry| &entry.claims)
                    .any(|claim| {
                        matches!(claim.pattern(), NamePattern::Exact(exact) if exact == relative)
                    })
            {
                lookup = registry.resolve(relative);
                if matches!(&lookup, Lookup::Matched { claim, .. } if !matches!(claim.pattern(), NamePattern::Exact(_)))
                {
                    lookup = Lookup::Unmatched;
                }
            }
            match lookup {
                Lookup::Matched {
                    address: IpAddr::V4(ip),
                    ..
                } if is_a_query => {
                    return Ok(Some(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?));
                }
                Lookup::Matched { .. } => return Ok(Some(arcbox_dns::build_nodata(data)?)),
                Lookup::Conflict => return Ok(Some(arcbox_dns::build_servfail(data)?)),
                Lookup::Reserved(_) => return Ok(Some(arcbox_dns::build_nxdomain(data)?)),
                Lookup::Delegated(_) => return Ok(None),
                Lookup::Unmatched => {}
            }
        }

        // 3. Sandbox registry lookup (std::sync::RwLock, not async).
        let bare_name = name_lower.strip_suffix(&suffix).unwrap_or(&name_lower);
        if let Some(ip) = self
            .sandboxes
            .read()
            .ok()
            .and_then(|g| g.get(&name_lower).copied())
        {
            if is_a_query {
                return Ok(Some(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?));
            }
            return Ok(Some(arcbox_dns::build_nodata(data)?));
        }
        if bare_name != name_lower {
            if let Some(ip) = self
                .sandboxes
                .read()
                .ok()
                .and_then(|g| g.get(bare_name).copied())
            {
                if is_a_query {
                    return Ok(Some(arcbox_dns::build_response_a(data, ip, DEFAULT_TTL)?));
                }
                return Ok(Some(arcbox_dns::build_nodata(data)?));
            }
        }

        // 4. Authoritative NXDOMAIN for unresolved local-domain queries.
        // NXDOMAIN (not NODATA) because the name genuinely doesn't exist.
        if name_lower == self.domain.as_str() || name_lower.ends_with(&suffix) {
            return Ok(Some(arcbox_dns::build_nxdomain(data)?));
        }

        Ok(None)
    }

    /// Forwards a query to the gateway DNS forwarder over UDP.
    async fn forward_to_gateway(&self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        // Connect so the kernel accepts only the gateway's reply (recv on an
        // unconnected UDP socket takes a datagram from ANY local sender), and
        // verify the reply echoes the query's transaction id.
        sock.connect(GATEWAY).await?;
        sock.send(data).await?;

        let mut buf = [0u8; MAX_PACKET];
        let len = tokio::time::timeout(FORWARD_TIMEOUT, sock.recv(&mut buf)).await??;
        if len < 12 || data.get(0..2) != buf.get(0..2) {
            anyhow::bail!("gateway DNS reply failed validation");
        }
        Ok(buf[..len].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_dns::claims::Claim;

    async fn register(server: &GuestDnsServer, name: &str, id: &str, ip: Ipv4Addr) {
        let claims = Claim::generated(
            format!("{name}.{}", server.domain).parse().unwrap(),
            id.into(),
            "container".into(),
        );
        let mut registry = server.containers.write().await;
        registry
            .replace(id.into(), claims.into(), Some(ip.into()))
            .unwrap();
    }

    async fn local_query(
        server: &GuestDnsServer,
        name: &str,
        qtype: DnsRecordType,
    ) -> Option<Vec<u8>> {
        let mut packet = vec![0x12, 0x34, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(u8::try_from(label.len()).unwrap());
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&u16::from(qtype).to_be_bytes());
        packet.extend_from_slice(&1_u16.to_be_bytes());
        let query = DnsQuery::parse(&packet).unwrap();
        let response = server.local_response(&packet, &query).await.unwrap();
        if let Some(response) = &response {
            assert_eq!(&response[12..packet.len()], &packet[12..]);
        }
        response
    }

    #[tokio::test]
    async fn resolves_relative_and_configured_names_for_containers_and_sandboxes() {
        let mut server =
            GuestDnsServer::new("containers.test".parse().unwrap(), CancellationToken::new());
        server.sandboxes = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let container_ip = Ipv4Addr::new(172, 17, 0, 2);
        let sandbox_ip = Ipv4Addr::new(10, 0, 3, 2);
        register(&server, "web.demo", "container", container_ip).await;
        server.register_sandbox("sandbox", sandbox_ip);

        for (name, ip) in [
            ("web.demo", container_ip),
            ("WEB.demo.Containers.Test", container_ip),
            ("deep.web.demo.containers.test", container_ip),
            ("sandbox", sandbox_ip),
            ("SANDBOX.containers.test", sandbox_ip),
        ] {
            let response = local_query(&server, name, DnsRecordType::A).await.unwrap();
            assert_eq!(&response[..4], &[0x12, 0x34, 0x81, 0x80], "{name}");
            assert_eq!(&response[6..8], &[0, 1], "{name}");
            assert!(response.ends_with(&ip.octets()), "{name}");

            for qtype in [28, 64, 65, 65_280] {
                let response = local_query(&server, name, DnsRecordType::from(qtype))
                    .await
                    .unwrap();
                assert_eq!(&response[2..4], &[0x85, 0x80], "{name} type {qtype}");
                assert_eq!(&response[6..12], &[0; 6], "{name} type {qtype}");
            }
        }
    }

    #[tokio::test]
    async fn only_the_configured_domain_receives_authoritative_negative_answers() {
        let server =
            GuestDnsServer::new("containers.test".parse().unwrap(), CancellationToken::new());
        register(&server, "com", "container", Ipv4Addr::new(172, 17, 0, 2)).await;
        for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
            for name in ["missing.containers.test", "containers.test"] {
                let response = local_query(&server, name, qtype).await.unwrap();
                assert_eq!(&response[2..4], &[0x85, 0x83], "{name}");
                assert_eq!(&response[6..12], &[0; 6], "{name}");
            }
            for name in [
                "web.arcbox.local",
                "web.example.test",
                "web.containers.test.evil",
                "example.com",
                "deep.com",
            ] {
                assert!(local_query(&server, name, qtype).await.is_none(), "{name}");
            }
        }
    }

}
