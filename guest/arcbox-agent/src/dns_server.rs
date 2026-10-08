//! Guest-side DNS server for container and sandbox name resolution.
//!
//! Listens on `0.0.0.0:53` (UDP) inside the guest VM. Lookup priority:
//!
//! 1. DNS registry (container claims and trusted namespace reservations)
//! 2. Exact sandbox IDs within the reserved sandbox tree
//! 3. Names under the configured domain not found → NXDOMAIN
//! 4. Everything else → forward to gateway (`10.0.2.1:53`)

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};

use arcbox_constants::dns::Domain;
use arcbox_dns::claims::Boundary;
use arcbox_dns::registry::{InstanceTarget, Lookup, Registry};
use arcbox_dns::{DEFAULT_TTL, DnsQuery, DnsRecordType, NamePattern};
use tokio::net::UdpSocket;
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
/// Keep guards within synchronous memory operations so teardown can revoke authority.
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
    ///
    /// # Errors
    /// Returns an error if the configured suffix cannot reserve the built-in names.
    pub fn new(domain: Domain, cancel: CancellationToken) -> anyhow::Result<Self> {
        let registry = Registry::with_builtin_names(&domain).map_err(anyhow::Error::msg)?;
        Ok(Self {
            domain,
            containers: Arc::new(RwLock::new(registry)),
            sandboxes: Arc::clone(sandbox_registry()),
            cancel,
        })
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
        match self.local_response(data, &query)? {
            Some(response) => Ok(response),
            None => self.forward_to_gateway(data).await,
        }
    }

    fn local_response(&self, data: &[u8], query: &DnsQuery) -> anyhow::Result<Option<Vec<u8>>> {
        let name_lower = query.name.to_lowercase();
        let suffix = format!(".{}", self.domain);

        // Known names have IPv4 data only; other record types receive NODATA.
        let is_a_query = query.qtype == DnsRecordType::A;
        let mut owned_instance = false;
        let registry = self.containers.read().expect("DNS registry lock poisoned");

        if let Ok(name) = name_lower.parse::<Domain>() {
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
                Lookup::Instance { authority, .. } => match authority.target {
                    InstanceTarget::Current => owned_instance = true,
                    InstanceTarget::Other { .. } => return Ok(None),
                    InstanceTarget::Unavailable => {
                        return Ok(Some(arcbox_dns::build_servfail(data)?));
                    }
                },
                // The host publishes these exact names from its current Registry.
                Lookup::Reserved(Boundary::ReservedName { authority, .. })
                    if matches!(authority.as_str(), "system" | "index") =>
                {
                    return Ok(None);
                }
                Lookup::Reserved(Boundary::ReservedTree { authority, .. })
                    if authority == "machine" =>
                {
                    return Ok(None);
                }
                Lookup::Reserved(Boundary::ReservedTree { root, authority })
                    if authority == "sandbox" =>
                {
                    let sandboxes = self.sandboxes.read().expect("sandbox DNS lock poisoned");
                    let ip = name
                        .as_str()
                        .strip_suffix(&format!(".{root}"))
                        .and_then(|id| sandboxes.get(id));
                    return Ok(Some(match ip {
                        Some(ip) if is_a_query => {
                            arcbox_dns::build_response_a(data, *ip, DEFAULT_TTL)?
                        }
                        Some(_) => arcbox_dns::build_nodata(data)?,
                        None => arcbox_dns::build_nxdomain(data)?,
                    }));
                }
                Lookup::Reserved(_) => return Ok(Some(arcbox_dns::build_nxdomain(data)?)),
                Lookup::Delegated(_) => return Ok(None),
                Lookup::Unmatched => {}
            }
        }

        // 4. Authoritative NXDOMAIN for unresolved local-domain queries.
        // NXDOMAIN (not NODATA) because the name genuinely doesn't exist.
        if owned_instance || name_lower == self.domain.as_str() || name_lower.ends_with(&suffix) {
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

    fn register(server: &GuestDnsServer, name: &str, id: &str, ip: Ipv4Addr) {
        let claims = Claim::generated(
            format!("{name}.{}", server.domain).parse().unwrap(),
            id.into(),
            "container".into(),
        );
        let mut registry = server.containers.write().unwrap();
        registry
            .replace(id.into(), claims.into(), Some(ip.into()))
            .unwrap();
    }

    fn local_query(server: &GuestDnsServer, name: &str, qtype: DnsRecordType) -> Option<Vec<u8>> {
        let mut packet = vec![0x12, 0x34, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(u8::try_from(label.len()).unwrap());
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&u16::from(qtype).to_be_bytes());
        packet.extend_from_slice(&1_u16.to_be_bytes());
        let query = DnsQuery::parse(&packet).unwrap();
        let response = server.local_response(&packet, &query).unwrap();
        if let Some(response) = &response {
            assert_eq!(&response[12..packet.len()], &packet[12..]);
        }
        response
    }

    #[test]
    fn resolves_relative_and_configured_names_for_containers_and_sandboxes() {
        let mut server =
            GuestDnsServer::new("containers.test".parse().unwrap(), CancellationToken::new())
                .unwrap();
        server.sandboxes = Arc::new(std::sync::RwLock::new(HashMap::new()));
        let container_ip = Ipv4Addr::new(172, 17, 0, 2);
        let sandbox_ip = Ipv4Addr::new(10, 0, 3, 2);
        register(&server, "web.demo", "container", container_ip);
        server.register_sandbox("sandbox", sandbox_ip);

        for (name, ip) in [
            ("web.demo", container_ip),
            ("WEB.demo.Containers.Test", container_ip),
            ("deep.web.demo.containers.test", container_ip),
            ("SANDBOX.sandbox.containers.test", sandbox_ip),
        ] {
            let response = local_query(&server, name, DnsRecordType::A).unwrap();
            assert_eq!(&response[..4], &[0x12, 0x34, 0x81, 0x80], "{name}");
            assert_eq!(&response[6..8], &[0, 1], "{name}");
            assert!(response.ends_with(&ip.octets()), "{name}");

            for qtype in [28, 64, 65, 65_280] {
                let response = local_query(&server, name, DnsRecordType::from(qtype)).unwrap();
                assert_eq!(&response[2..4], &[0x85, 0x80], "{name} type {qtype}");
                assert_eq!(&response[6..12], &[0; 6], "{name} type {qtype}");
            }
        }
        assert!(local_query(&server, "sandbox", DnsRecordType::A).is_none());
        assert_eq!(
            local_query(&server, "sandbox.containers.test", DnsRecordType::A).unwrap()[3] & 15,
            3
        );
    }

    #[test]
    fn only_the_configured_domain_receives_authoritative_negative_answers() {
        let server =
            GuestDnsServer::new("containers.test".parse().unwrap(), CancellationToken::new())
                .unwrap();
        register(&server, "com", "container", Ipv4Addr::new(172, 17, 0, 2));
        for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
            let response = local_query(&server, "missing.containers.test", qtype).unwrap();
            assert_eq!(&response[2..4], &[0x85, 0x83]);
            assert_eq!(&response[6..12], &[0; 6]);
            for name in [
                "containers.test",
                "web.arcbox.local",
                "web.example.test",
                "web.containers.test.evil",
                "example.com",
                "deep.com",
            ] {
                assert!(local_query(&server, name, qtype).is_none(), "{name}");
            }
        }
    }

    #[test]
    fn builtin_names_forward_without_local_addresses_despite_hostile_claims() {
        let mut server = GuestDnsServer::new(
            "dev.arcbox.local".parse().unwrap(),
            CancellationToken::new(),
        )
        .unwrap();
        server.sandboxes = Arc::default();
        let ip = Ipv4Addr::new(172, 17, 0, 2);
        let names = [
            "host",
            "host.dev.arcbox.local",
            "host.docker.internal",
            "gateway.docker.internal",
            "dev.arcbox.local",
        ];
        for name in names {
            server.register_sandbox(name, ip);
        }
        for patterns in [
            Vec::new(),
            names
                .map(|name| NamePattern::Exact(name.parse().unwrap()))
                .to_vec(),
            ["local", "docker.internal"]
                .map(|name| NamePattern::Descendants(name.parse().unwrap()))
                .to_vec(),
        ] {
            let claims = patterns
                .into_iter()
                .map(|pattern| Claim::exclusive(pattern, "hostile".into(), "container".into()))
                .collect();
            server
                .containers
                .write()
                .unwrap()
                .replace("hostile".into(), claims, Some(ip.into()))
                .unwrap();
            for name in names {
                for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
                    assert!(local_query(&server, name, qtype).is_none(), "{name}");
                }
            }
        }
        for name in [
            "child.host.dev.arcbox.local",
            "child.dev.arcbox.local",
            "child.host.docker.internal",
            "child.gateway.docker.internal",
        ] {
            let response = local_query(&server, name, DnsRecordType::A).unwrap();
            assert_eq!(&response[6..8], &[0, 1], "{name}");
            assert!(response.ends_with(&ip.octets()), "{name}");
        }
    }

    #[test]
    fn server_rejects_a_suffix_that_replaces_a_host_alias() {
        for name in ["host", "host.docker.internal", "gateway.docker.internal"] {
            assert!(GuestDnsServer::new(name.parse().unwrap(), CancellationToken::new()).is_err());
        }
    }

    #[test]
    fn resource_trees_reject_container_wildcards_and_keep_machine_queries_on_the_host() {
        let mut server = GuestDnsServer::new(Domain::default(), CancellationToken::new()).unwrap();
        server.sandboxes = Arc::default();
        let sandbox_ip = Ipv4Addr::new(10, 0, 3, 2);
        server
            .containers
            .write()
            .unwrap()
            .replace(
                "hostile".into(),
                Claim::generated(Domain::default(), "hostile".into(), "container".into()).into(),
                Some(Ipv4Addr::new(172, 17, 0, 2).into()),
            )
            .unwrap();
        server.register_sandbox("box", sandbox_ip);
        let live = local_query(&server, "box.sandbox.arcbox.local", DnsRecordType::A).unwrap();
        assert!(live.ends_with(&sandbox_ip.octets()));
        server.deregister_sandbox("box");
        for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
            for name in [
                "sandbox.arcbox.local",
                "box.sandbox.arcbox.local",
                "deep.box.sandbox.arcbox.local",
            ] {
                let response = local_query(&server, name, qtype).unwrap();
                assert_eq!(response[3] & 15, 3, "{name}");
                assert_eq!(&response[6..12], &[0; 6], "{name}");
            }
            assert!(local_query(&server, "default.machine.arcbox.local", qtype).is_none());
        }
    }

    #[test]
    fn instance_roots_gate_sandbox_fallback_and_every_query_type() {
        use arcbox_dns::registry::InstanceAuthority;
        let mut server =
            GuestDnsServer::new("custom.local".parse().unwrap(), CancellationToken::new()).unwrap();
        server.sandboxes = Arc::default();
        server.register_sandbox("sandbox", Ipv4Addr::new(10, 0, 3, 2));
        for target in [
            InstanceTarget::Current,
            InstanceTarget::Other { port: 5354 },
            InstanceTarget::Unavailable,
        ] {
            server
                .containers
                .write()
                .unwrap()
                .replace_instance_authorities(vec![InstanceAuthority {
                    root: "custom.local".parse().unwrap(),
                    target,
                }]);
            for name in [
                "sandbox.sandbox.custom.local",
                "missing.sandbox.custom.local",
            ] {
                for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
                    let response = local_query(&server, name, qtype);
                    if matches!(target, InstanceTarget::Other { .. }) {
                        assert!(response.is_none());
                    } else {
                        let code = if target == InstanceTarget::Unavailable {
                            2
                        } else if name.starts_with("missing.") {
                            3
                        } else {
                            0
                        };
                        assert_eq!(response.unwrap()[3] & 15, code);
                    }
                }
            }
        }
    }

    #[test]
    fn terminal_ownership_results_do_not_reach_sandboxes_or_short_names() {
        let mut server = GuestDnsServer::new(Domain::default(), CancellationToken::new()).unwrap();
        server.sandboxes = Arc::default();
        *server.containers.write().unwrap() = Registry::new(vec![
            Boundary::ReservedName {
                name: "reserved.local".parse().unwrap(),
                authority: "host".into(),
            },
            Boundary::ReservedTree {
                root: "system-tree.local".parse().unwrap(),
                authority: "system".into(),
            },
            Boundary::DelegatedTree {
                root: "remote.local".parse().unwrap(),
                instance: "other".into(),
            },
        ]);
        let ip = Ipv4Addr::new(172, 17, 0, 2);
        for name in ["reserved.local", "remote.local", "conflict.arcbox.local"] {
            register(&server, name, name, ip);
            server.register_sandbox(name, ip);
        }
        for id in ["first", "second"] {
            register(&server, "conflict", id, ip);
        }
        for qtype in [1, 28, 64, 65, 65_280].map(DnsRecordType::from) {
            for (name, code) in [
                ("reserved.local", 3),
                ("system-tree.local", 3),
                ("conflict.arcbox.local", 2),
                ("conflict", 2),
            ] {
                let response = local_query(&server, name, qtype).unwrap();
                assert_eq!(response[3] & 15, code, "{name}");
                assert_eq!(&response[6..12], &[0; 6], "{name}");
            }
            assert!(local_query(&server, "remote.local", qtype).is_none());
        }
    }
}
