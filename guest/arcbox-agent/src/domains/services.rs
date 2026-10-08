//! Installed web routes and Docker directory visibility joined with current DNS ownership.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use arcbox_connect::v1::{
    ContainerWebService, GuestWebService, IndexWebService, ReconcileHttpsAccessRequest,
    ReconcileHttpsAccessResponse, container_web_service::HttpsBackend,
};
use arcbox_constants::dns::{Domain, LOCAL_CA_DNS_SUBTREE};
use arcbox_dns::{
    NamePattern,
    registry::{Lookup, Registry},
    web_services::{HttpsMode, WebServiceUrls},
};

use super::{EndpointFacts, HttpBackend, SelectedRoutes};
use crate::dns_server::DnsRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Installed {
    ip: Ipv4Addr,
    selected: SelectedRoutes,
}

impl Installed {
    fn service(&self, owner: &str, hostnames: Vec<String>) -> ContainerWebService {
        ContainerWebService {
            container_id: owner.into(),
            hostnames,
            ipv4_address: self.ip.to_string(),
            http_port: self.selected.http.map_or(0, u32::from),
            https_backend: self.selected.https.map(|backend| match backend {
                super::HttpsBackend::Native(port) => HttpsBackend::NativeHttpsPort(u32::from(port)),
                super::HttpsBackend::Terminate(backend) => {
                    HttpsBackend::TerminatedHttpPort(u32::from(backend.port))
                }
            }),
            ..Default::default()
        }
    }
}

struct SelectedContainer {
    owner: String,
    installed: Arc<Installed>,
    domains: Vec<Domain>,
}

/// Pins a managed TLS handshake to an installed container or the built-in index.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TlsRoute {
    Container(ContainerTlsRoute),
    Index(super::index::Route),
}

#[derive(Debug, Eq)]
pub(super) struct ContainerTlsRoute {
    owner: String,
    revision: u64,
    installed: Arc<Installed>,
    pub(super) backend: HttpBackend,
}

impl PartialEq for ContainerTlsRoute {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.revision == other.revision
            && Arc::ptr_eq(&self.installed, &other.installed)
    }
}

impl TlsRoute {
    fn resolve(
        name: &str,
        destination: Ipv4Addr,
        dns: &Registry,
        installed: &HashMap<String, Arc<Installed>>,
    ) -> Option<Self> {
        let name = name.to_ascii_lowercase().parse::<Domain>().ok()?;
        if !name.as_str().ends_with(LOCAL_CA_DNS_SUBTREE) {
            return None;
        }
        if let Some(index) = super::index::Route::resolve(dns, destination)
            && index.name() == &name
        {
            return Some(Self::Index(index));
        }
        let Lookup::Matched {
            claim,
            address,
            revision,
            ..
        } = dns.resolve(&name)
        else {
            return None;
        };
        let route = installed.get(claim.owner())?;
        if address != IpAddr::V4(destination) || route.ip != destination {
            return None;
        }
        Some(Self::Container(ContainerTlsRoute {
            owner: claim.owner().into(),
            revision,
            installed: Arc::clone(route),
            backend: route.selected.terminated_backend()?,
        }))
    }
}

/// Installed routes and HTTPS access observation shared with the RPC connections.
#[derive(Clone, Default)]
pub struct WebServices {
    installed: Arc<RwLock<HashMap<String, Arc<Installed>>>>,
    dns: DnsRegistry,
    access: super::access::Access,
    directory_available: Arc<AtomicBool>,
}

/// The single Docker watcher's current subscription and complete snapshot.
pub struct DirectorySource {
    available: Arc<AtomicBool>,
}

impl DirectorySource {
    pub(crate) fn ready(&self) {
        self.available.store(true, Ordering::Release);
    }
}

impl Drop for DirectorySource {
    fn drop(&mut self) {
        self.available.store(false, Ordering::Release);
    }
}

impl WebServices {
    /// The Docker watcher holds this guard until its current subscription ends.
    pub(crate) fn directory_source(&self) -> DirectorySource {
        self.directory_available.store(false, Ordering::Release);
        DirectorySource {
            available: Arc::clone(&self.directory_available),
        }
    }

    pub(super) async fn observe_managed_tls(
        &self,
        name: &str,
        destination: Ipv4Addr,
        source: IpAddr,
    ) -> anyhow::Result<TlsRoute> {
        let route = self.tls_route(name, destination).ok_or_else(|| {
            anyhow::anyhow!("TLS name {name:?} has no managed route to {destination}")
        })?;
        self.access.observe(name, destination, source).await;
        self.verify_tls_route(name, destination, &route)?;
        Ok(route)
    }

    /// Renews host observation and returns the first managed access in that observation.
    pub fn reconcile_https_access(
        &self,
        request: ReconcileHttpsAccessRequest,
    ) -> anyhow::Result<ReconcileHttpsAccessResponse> {
        self.access.reconcile(request)
    }

    pub(crate) fn new(dns: DnsRegistry) -> Self {
        Self {
            dns,
            ..Self::default()
        }
    }

    /// Publish only after the selected NAT rules have been installed.
    pub(crate) fn publish(&self, facts: &EndpointFacts, selected: SelectedRoutes) {
        let mut installed = self.installed.write().expect("web services lock poisoned");
        if selected == SelectedRoutes::default() {
            installed.remove(&facts.id);
        } else {
            let route = Installed {
                ip: facts.primary_ip,
                selected,
            };
            if installed
                .get(&facts.id)
                .is_some_and(|current| **current == route)
            {
                return;
            }
            installed.insert(facts.id.clone(), Arc::new(route));
        }
    }

    pub(super) fn remove(&self, id: &str) {
        self.installed
            .write()
            .expect("web services lock poisoned")
            .remove(id);
    }

    pub(crate) fn clear(&self) {
        self.installed
            .write()
            .expect("web services lock poisoned")
            .clear();
    }

    pub(super) fn contains(&self, id: &str) -> bool {
        self.installed
            .read()
            .expect("web services lock poisoned")
            .contains_key(id)
    }

    fn tls_route(&self, name: &str, destination: Ipv4Addr) -> Option<TlsRoute> {
        let dns = self.dns.read().expect("DNS registry lock poisoned");
        let installed = self.installed.read().expect("web services lock poisoned");
        TlsRoute::resolve(name, destination, &dns, &installed)
    }

    pub(super) fn invalidate_tls_route(
        &self,
        name: &str,
        destination: Ipv4Addr,
        expected: &TlsRoute,
    ) {
        let dns = self.dns.read().expect("DNS registry lock poisoned");
        let mut installed = self.installed.write().expect("web services lock poisoned");
        if TlsRoute::resolve(name, destination, &dns, &installed).as_ref() == Some(expected)
            && let TlsRoute::Container(route) = expected
        {
            installed.remove(&route.owner);
        }
    }

    /// Recheck after each asynchronous handshake stage before forwarding application bytes.
    pub(super) fn verify_tls_route(
        &self,
        name: &str,
        destination: Ipv4Addr,
        expected: &TlsRoute,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tls_route(name, destination).as_ref() == Some(expected),
            "managed TLS route for {name:?} at {destination} changed during the handshake"
        );
        Ok(())
    }

    pub(super) fn index_route(&self, destination: Ipv4Addr) -> Option<super::index::Route> {
        super::index::Route::resolve(
            &self.dns.read().expect("DNS registry lock poisoned"),
            destination,
        )
    }

    pub(super) fn directory_urls(
        &self,
        expected: &super::index::Route,
    ) -> anyhow::Result<Option<Vec<WebServiceUrls>>> {
        let dns = self.dns.read().expect("DNS registry lock poisoned");
        anyhow::ensure!(
            super::index::Route::resolve(&dns, expected.destination).as_ref() == Some(expected),
            "index registration changed"
        );
        Ok(self.directory_available.load(Ordering::Acquire).then(|| {
            let installed = self.installed.read().expect("web services lock poisoned");
            Self::selected_containers(&dns, &installed, None)
                .into_iter()
                .flat_map(|row| {
                    let selected = row.installed.selected;
                    let https = match selected.https {
                        None => HttpsMode::None,
                        Some(super::HttpsBackend::Native(_)) => HttpsMode::Passthrough,
                        Some(super::HttpsBackend::Terminate(_)) => HttpsMode::Managed,
                    };
                    WebServiceUrls::project(row.domains, selected.http.is_some(), https)
                })
                .collect()
        }))
    }

    fn index_snapshot(&self, hostname: Option<&Domain>) -> Option<GuestWebService> {
        let dns = self.dns.read().expect("DNS registry lock poisoned");
        let IpAddr::V4(destination) = dns.get("system:index")?.address? else {
            return None;
        };
        let route = super::index::Route::resolve(&dns, destination)?;
        if hostname.is_some_and(|name| name != route.name()) {
            return None;
        }
        Some(GuestWebService {
            route: IndexWebService {
                hostname: route.name().to_string(),
                ipv4_address: destination.to_string(),
                managed_https: route.name().as_str().ends_with(LOCAL_CA_DNS_SUBTREE),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
    }

    /// Returns typed installed endpoints selected by current DNS ownership.
    pub fn snapshot(&self, hostname: Option<&Domain>) -> Vec<GuestWebService> {
        self.directory_snapshot(hostname)
            .unwrap_or_default()
            .into_iter()
            .map(|container| GuestWebService {
                route: container.into(),
                ..Default::default()
            })
            .chain(self.index_snapshot(hostname))
            .collect()
    }

    /// Distinguishes an unavailable Docker source from a current empty directory.
    pub(super) fn directory_snapshot(
        &self,
        hostname: Option<&Domain>,
    ) -> Option<Vec<ContainerWebService>> {
        self.directory_available
            .load(Ordering::Acquire)
            .then(|| self.container_snapshot(hostname))
    }

    /// Shared Compose aliases belong only to the container DNS currently selects.
    fn container_snapshot(&self, hostname: Option<&Domain>) -> Vec<ContainerWebService> {
        let dns = self.dns.read().expect("DNS registry lock poisoned");
        let installed = self.installed.read().expect("web services lock poisoned");
        Self::selected_containers(&dns, &installed, hostname)
            .into_iter()
            .map(|row| {
                row.installed.service(
                    &row.owner,
                    row.domains
                        .into_iter()
                        .map(|name| name.to_string())
                        .collect(),
                )
            })
            .collect()
    }

    fn selected_containers(
        dns: &Registry,
        installed: &HashMap<String, Arc<Installed>>,
        hostname: Option<&Domain>,
    ) -> Vec<SelectedContainer> {
        if let Some(name) = hostname {
            let Lookup::Matched { claim, address, .. } = dns.resolve(name) else {
                return Vec::new();
            };
            return installed
                .get(claim.owner())
                .filter(|route| address == IpAddr::V4(route.ip))
                .map(|route| SelectedContainer {
                    owner: claim.owner().into(),
                    installed: Arc::clone(route),
                    domains: vec![name.clone()],
                })
                .into_iter()
                .collect();
        }
        let mut services: Vec<_> = installed
            .iter()
            .filter_map(|(id, route)| {
                let domains: Vec<_> = dns.get(id)?
                    .claims
                    .iter()
                    .filter_map(|claim| match claim.pattern() {
                        NamePattern::Exact(name) if matches!(dns.resolve(name), Lookup::Matched { claim, address, .. } if claim.owner() == id && address == IpAddr::V4(route.ip)) => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                if domains.is_empty() {
                    return None;
                }
                Some(SelectedContainer { owner: id.clone(), installed: Arc::clone(route), domains })
            })
            .collect();
        services.sort_by(|a, b| a.owner.cmp(&b.owner));
        services
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(services: &WebServices, id: &str, names: &[&str], ip: Ipv4Addr, domain: &str) {
        let domain: Domain = domain.parse().unwrap();
        let claims = names
            .iter()
            .flat_map(|name| {
                arcbox_dns::claims::Claim::compose_service(
                    format!("{name}.{domain}").parse().unwrap(),
                    id.into(),
                    "container".into(),
                    ("demo".into(), (*name).into()),
                )
            })
            .collect();
        let mut registry = services.dns.write().unwrap();
        registry
            .replace(id.into(), claims, Some(ip.into()))
            .unwrap();
        let facts = EndpointFacts::from_inspect(
            &serde_json::json!({
                "Id": id, "Name": "/web", "State": { "Pid": 1 },
                "NetworkSettings": {
                    "SandboxKey": "/var/run/docker/netns/web",
                    "Networks": { "bridge": { "IPAddress": ip.to_string() } }
                },
            }),
            &domain,
        )
        .unwrap();
        services.publish(
            &facts,
            SelectedRoutes {
                http: Some(3000),
                https: Some(super::super::HttpsBackend::Terminate(HttpBackend {
                    port: 3000,
                    protocol: Some(super::super::HttpProtocol::Http1),
                })),
            },
        );
    }

    #[test]
    fn lost_directory_source_hides_links_without_removing_installed_routes() {
        let services = WebServices::default();
        let ip = "172.17.0.2".parse().unwrap();
        let name = "web.arcbox.local";
        register(&services, "a", &["web"], ip, "arcbox.local");
        assert_eq!(services.directory_snapshot(None), None);
        assert_eq!(services.snapshot(None), []);
        let source = services.directory_source();
        assert_eq!(services.directory_snapshot(None), None);
        source.ready();
        assert_eq!(services.snapshot(None).len(), 1);
        drop(source);
        assert_eq!(services.directory_snapshot(None), None);
        assert_eq!(services.snapshot(Some(&name.parse().unwrap())), []);
        assert!(services.tls_route(name, ip).is_some());
        let source = services.directory_source();
        services.remove("a");
        source.ready();
        assert_eq!(services.directory_snapshot(None), Some(Vec::new()));
    }

    #[test]
    fn managed_tls_uses_local_claims_under_a_foreign_default_suffix() {
        use arcbox_dns::claims::Claim;

        let ip = Ipv4Addr::new(172, 17, 0, 2);
        let services = WebServices::default();
        register(&services, "a", &["web"], ip, "containers.test");
        {
            let mut registry = services.dns.write().unwrap();
            let mut claims = registry.get("a").unwrap().claims.clone();
            claims.extend(
                ["api.local", "evilarcbox.local", "*.svc.local"].map(|name| {
                    Claim::exclusive(
                        NamePattern::parse_custom(name).unwrap(),
                        "a".into(),
                        "container".into(),
                    )
                }),
            );
            claims.extend(["local", "web.localhost", "web.local.evil"].map(|name| {
                Claim::exclusive(
                    NamePattern::Exact(name.parse().unwrap()),
                    "a".into(),
                    "container".into(),
                )
            }));
            registry
                .replace("a".into(), claims, Some(ip.into()))
                .unwrap();
        }
        for name in [
            "API.LOCAL",
            "evilarcbox.local",
            "a.svc.local",
            "a.b.svc.local",
        ] {
            assert!(services.tls_route(name, ip).is_some(), "{name}");
        }
        for name in [
            "web.containers.test",
            "local",
            "web.localhost",
            "web.local.evil",
            "svc.local",
            "missing.local",
            "api.local.",
            "bad_name.local",
            "*.svc.local",
        ] {
            assert!(services.tls_route(name, ip).is_none(), "{name}");
        }
        let snapshot = services.container_snapshot(None);
        assert!(
            snapshot[0]
                .hostnames
                .contains(&"web.containers.test".into())
        );
        assert!(snapshot[0].hostnames.contains(&"api.local".into()));
        assert_eq!(snapshot[0].http_port, 3000);
        Arc::make_mut(services.installed.write().unwrap().get_mut("a").unwrap())
            .selected
            .https = Some(super::super::HttpsBackend::Native(8443));
        let snapshot = services.container_snapshot(None);
        assert!(
            snapshot[0]
                .hostnames
                .contains(&"web.containers.test".into())
        );
        assert_eq!(
            snapshot[0].https_backend,
            Some(HttpsBackend::NativeHttpsPort(8443))
        );
    }

    #[test]
    fn instance_reacquisition_invalidates_a_pinned_tls_route() {
        use arcbox_dns::registry::{InstanceAuthority, InstanceTarget};
        let ip = Ipv4Addr::new(172, 17, 0, 2);
        let services = WebServices::default();
        register(&services, "a", &["web"], ip, "arcbox.local");
        let route = services.tls_route("web.arcbox.local", ip).unwrap();
        for target in [InstanceTarget::Unavailable, InstanceTarget::Current] {
            services
                .dns
                .write()
                .unwrap()
                .replace_instance_authorities(vec![InstanceAuthority {
                    root: "arcbox.local".parse().unwrap(),
                    target,
                }]);
            assert!(
                services
                    .verify_tls_route("web.arcbox.local", ip, &route)
                    .is_err()
            );
        }
        assert!(services.tls_route("web.arcbox.local", ip).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn managed_access_checks_ownership_before_reporting_and_after_waiting() {
        let destination = Ipv4Addr::new(172, 17, 0, 2);
        let source: IpAddr = "192.168.64.1".parse().unwrap();
        let services = WebServices::default();
        register(&services, "a", &["web"], destination, "arcbox.local");
        let mut request = ReconcileHttpsAccessRequest {
            observation_id: "one".into(),
            host_addresses: vec![source.to_string()],
            enabled: true,
            ..Default::default()
        };
        services.reconcile_https_access(request.clone()).unwrap();
        assert!(
            services
                .observe_managed_tls("wrong.arcbox.local", destination, source)
                .await
                .is_err()
        );
        assert_eq!(
            services
                .reconcile_https_access(request.clone())
                .unwrap()
                .sequence,
            0
        );
        let handshake = tokio::spawn({
            let services = services.clone();
            async move {
                services
                    .observe_managed_tls("WEB.arcbox.local", destination, source)
                    .await
            }
        });
        tokio::task::yield_now().await;
        let access = services.reconcile_https_access(request.clone()).unwrap();
        assert_eq!(access.hostname, "web.arcbox.local");
        assert_eq!(access.sequence, 1);
        register(
            &services,
            "a",
            &["web"],
            Ipv4Addr::new(172, 17, 0, 3),
            "arcbox.local",
        );
        request.enabled = false;
        services.reconcile_https_access(request).unwrap();
        assert!(handshake.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn authorization_wait_rejects_replaced_owners_at_the_same_address() {
        let destination = Ipv4Addr::new(172, 17, 0, 2);
        let source: IpAddr = "192.168.64.1".parse().unwrap();
        for replacement in ["a", "b"] {
            let services = WebServices::default();
            register(&services, "a", &["web"], destination, "arcbox.local");
            let mut request = ReconcileHttpsAccessRequest {
                observation_id: replacement.into(),
                host_addresses: vec![source.to_string()],
                enabled: true,
                ..Default::default()
            };
            services.reconcile_https_access(request.clone()).unwrap();
            let handshake = tokio::spawn({
                let services = services.clone();
                async move {
                    services
                        .observe_managed_tls("web.arcbox.local", destination, source)
                        .await
                }
            });
            tokio::task::yield_now().await;
            assert_eq!(
                services
                    .reconcile_https_access(request.clone())
                    .unwrap()
                    .sequence,
                1
            );
            services.dns.write().unwrap().remove("a");
            register(
                &services,
                replacement,
                &["web"],
                destination,
                "arcbox.local",
            );
            request.enabled = false;
            services.reconcile_https_access(request).unwrap();
            assert!(handshake.await.unwrap().is_err(), "{replacement}");
        }
    }

    #[tokio::test]
    async fn handshake_route_checks_registration_and_installed_backend() {
        let ip = Ipv4Addr::new(172, 17, 0, 2);
        let name = "web.arcbox.local";
        for change in [
            "idempotent",
            "recreate",
            "transfer",
            "port",
            "native",
            "removed",
            "foreign",
            "republish",
        ] {
            let services = WebServices::default();
            register(&services, "a", &["web"], ip, "arcbox.local");
            let route = services
                .observe_managed_tls(name, ip, ip.into())
                .await
                .unwrap();
            match change {
                "idempotent" => register(&services, "a", &["web"], ip, "arcbox.local"),
                "recreate" | "transfer" => {
                    services.dns.write().unwrap().remove("a");
                    let owner = if change == "recreate" { "a" } else { "b" };
                    register(&services, owner, &["web"], ip, "arcbox.local");
                }
                "port" | "native" => {
                    let mut installed = services.installed.write().unwrap();
                    Arc::make_mut(installed.get_mut("a").unwrap())
                        .selected
                        .https = Some(if change == "port" {
                        super::super::HttpsBackend::Terminate(HttpBackend {
                            port: 4000,
                            protocol: Some(super::super::HttpProtocol::Http1),
                        })
                    } else {
                        super::super::HttpsBackend::Native(3000)
                    });
                }
                "removed" => services.remove("a"),
                "foreign" => {
                    register(&services, "b", &["web"], ip, "arcbox.local");
                    services.remove("a");
                }
                "republish" => {
                    services.remove("a");
                    register(&services, "a", &["web"], ip, "arcbox.local");
                }
                _ => unreachable!(),
            }
            assert_eq!(
                services.verify_tls_route(name, ip, &route).is_ok(),
                change == "idempotent",
                "{change}"
            );
            let before = services.container_snapshot(None);
            services.invalidate_tls_route(name, ip, &route);
            if change == "idempotent" {
                assert!(!services.contains("a"));
            } else {
                assert_eq!(services.container_snapshot(None), before, "{change}");
            }
        }
    }

    #[test]
    fn tls_names_follow_live_dns_ownership() {
        let first = Ipv4Addr::new(172, 17, 0, 2);
        let second = Ipv4Addr::new(172, 17, 0, 3);
        let services = WebServices::default();
        register(&services, "a", &["web.demo"], first, "one.dev.arcbox.local");
        register(&services, "b", &["db.demo"], second, "one.dev.arcbox.local");
        assert!(
            services
                .tls_route("WEB.demo.one.dev.arcbox.local", first)
                .is_some()
        );
        for name in [
            "db.demo.one.dev.arcbox.local",
            "missing.one.dev.arcbox.local",
            "web.demo",
            "web.demo.one.dev.arcbox.local.evil",
            "web.demo.arcbox.local",
            "web.demo.other.dev.arcbox.local",
            "web.demo.containers.test",
        ] {
            assert!(services.tls_route(name, first).is_none(), "{name}");
        }
        register(
            &services,
            "a",
            &["web.demo"],
            second,
            "one.dev.arcbox.local",
        );
        assert!(
            services
                .tls_route("web.demo.one.dev.arcbox.local", first)
                .is_none()
        );
        assert!(
            services
                .tls_route("web.demo.one.dev.arcbox.local", second)
                .is_some()
        );
    }

    #[test]
    fn shared_alias_follows_dns_ownership_and_removed_routes_disappear() {
        let services = WebServices::default();
        for (id, ip, aliases) in [
            (
                "a",
                Ipv4Addr::new(172, 17, 0, 2),
                ["web.demo", "demo-web-1"],
            ),
            (
                "b",
                Ipv4Addr::new(172, 17, 0, 2),
                ["web.demo", "demo-web-2"],
            ),
        ] {
            register(&services, id, &aliases, ip, "one.dev.arcbox.local");
        }
        let snapshot = services.container_snapshot(None);
        assert_eq!(snapshot[1].hostnames, ["demo-web-2.one.dev.arcbox.local"]);
        assert_eq!(
            snapshot[0].hostnames,
            [
                "web.demo.one.dev.arcbox.local",
                "demo-web-1.one.dev.arcbox.local"
            ]
        );
        assert_eq!(
            snapshot[1].https_backend,
            Some(HttpsBackend::TerminatedHttpPort(3000))
        );

        services.dns.write().unwrap().remove("a");
        let snapshot = services.container_snapshot(None);
        assert_eq!(
            snapshot[0].hostnames,
            [
                "web.demo.one.dev.arcbox.local",
                "demo-web-2.one.dev.arcbox.local"
            ]
        );
        assert_eq!(snapshot.len(), 1);

        services.remove("b");
        assert_eq!(
            services.container_snapshot(None),
            Vec::<ContainerWebService>::new()
        );
    }
}
