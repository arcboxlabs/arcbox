//! Installed web routes joined with the current DNS resolution.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, RwLock};

use arcbox_connect::v1::{
    ContainerWebService, ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse,
    container_web_service::HttpsBackend,
};
use arcbox_constants::dns::Domain;
use arcbox_dns::{
    NamePattern,
    registry::{Lookup, Registry},
};

use super::{ContainerFacts, SelectedRoutes};
use crate::dns_server::ContainerRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Installed {
    ip: Ipv4Addr,
    selected: SelectedRoutes,
}

/// Pins one handshake to the current owner, registration, and installed upstream.
#[derive(Debug, Eq)]
pub(super) struct TlsRoute {
    owner: String,
    revision: u64,
    installed: Arc<Installed>,
    pub(super) http_port: u16,
}

impl PartialEq for TlsRoute {
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
        let Lookup::Matched {
            claim,
            address,
            revision,
        } = dns.resolve(&name)
        else {
            return None;
        };
        let route = installed.get(claim.owner())?;
        if address != IpAddr::V4(destination) || route.ip != destination {
            return None;
        }
        Some(Self {
            owner: claim.owner().into(),
            revision,
            installed: Arc::clone(route),
            http_port: route.selected.terminated_port()?,
        })
    }
}

/// Installed routes and HTTPS access observation shared with the RPC connections.
#[derive(Clone, Default)]
pub struct WebServices {
    pub(super) domain: Domain,
    installed: Arc<RwLock<HashMap<String, Arc<Installed>>>>,
    dns: ContainerRegistry,
    access: super::access::Access,
}

impl WebServices {
    pub(super) async fn observe_managed_tls(
        &self,
        name: &str,
        destination: Ipv4Addr,
        source: IpAddr,
    ) -> anyhow::Result<TlsRoute> {
        let route = self.tls_route(name, destination).await.ok_or_else(|| {
            anyhow::anyhow!("TLS name {name:?} has no managed route to {destination}")
        })?;
        self.access.observe(name, destination, source).await;
        self.verify_tls_route(name, destination, &route).await?;
        Ok(route)
    }

    /// Renews host observation and returns the first managed access in that observation.
    pub fn reconcile_https_access(
        &self,
        request: ReconcileHttpsAccessRequest,
    ) -> anyhow::Result<ReconcileHttpsAccessResponse> {
        self.access.reconcile(request)
    }

    pub(crate) fn new(domain: Domain, dns: ContainerRegistry) -> Self {
        Self {
            domain,
            dns,
            ..Self::default()
        }
    }

    /// Publish only after the selected NAT rules have been installed.
    pub(crate) fn publish(&self, facts: &ContainerFacts, selected: SelectedRoutes) {
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

    pub(super) fn contains(&self, id: &str) -> bool {
        self.installed
            .read()
            .expect("web services lock poisoned")
            .contains_key(id)
    }

    async fn tls_route(&self, name: &str, destination: Ipv4Addr) -> Option<TlsRoute> {
        let dns = self.dns.read().await;
        let installed = self.installed.read().expect("web services lock poisoned");
        TlsRoute::resolve(name, destination, &dns, &installed)
    }

    pub(super) async fn invalidate_tls_route(
        &self,
        name: &str,
        destination: Ipv4Addr,
        expected: &TlsRoute,
    ) {
        let dns = self.dns.read().await;
        let mut installed = self.installed.write().expect("web services lock poisoned");
        if TlsRoute::resolve(name, destination, &dns, &installed).as_ref() == Some(expected) {
            installed.remove(&expected.owner);
        }
    }

    /// Recheck after each asynchronous handshake stage before forwarding application bytes.
    pub(super) async fn verify_tls_route(
        &self,
        name: &str,
        destination: Ipv4Addr,
        expected: &TlsRoute,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.tls_route(name, destination).await.as_ref() == Some(expected),
            "managed TLS route for {name:?} at {destination} changed during the handshake"
        );
        Ok(())
    }

    /// Shared Compose aliases belong only to the container DNS currently selects.
    pub async fn snapshot(&self) -> Vec<ContainerWebService> {
        let dns = self.dns.read().await;
        let installed = self.installed.read().expect("web services lock poisoned");
        let mut services: Vec<_> = installed
            .iter()
            .filter_map(|(id, route)| {
                let hostnames: Vec<_> = dns.get(id)?
                    .claims
                    .iter()
                    .filter_map(|claim| match claim.pattern() {
                        NamePattern::Exact(name) if matches!(dns.resolve(name), Lookup::Matched { claim, address, .. } if claim.owner() == id && address == IpAddr::V4(route.ip)) => Some(name.to_string()),
                        _ => None,
                    })
                    .collect();
                if hostnames.is_empty() {
                    return None;
                }
                Some(ContainerWebService {
                    container_id: id.clone(),
                    hostnames,
                    ipv4_address: route.ip.to_string(),
                    http_port: route.selected.http.map_or(0, u32::from),
                    https_backend: route.selected.https.map(|backend| match backend {
                        super::HttpsBackend::Native(port) => {
                            HttpsBackend::NativeHttpsPort(u32::from(port))
                        }
                        super::HttpsBackend::Terminate(port) => {
                            HttpsBackend::TerminatedHttpPort(u32::from(port))
                        }
                    }),
                    ..Default::default()
                })
            })
            .collect();
        services.sort_by(|a, b| a.container_id.cmp(&b.container_id));
        services
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn register(services: &WebServices, id: &str, names: &[&str], ip: Ipv4Addr) {
        let claims = names
            .iter()
            .flat_map(|name| {
                arcbox_dns::claims::Claim::compose_service(
                    format!("{name}.{}", services.domain).parse().unwrap(),
                    id.into(),
                    "container".into(),
                    ("demo".into(), (*name).into()),
                )
            })
            .collect();
        let mut registry = services.dns.write().await;
        registry
            .replace(id.into(), claims, Some(ip.into()))
            .unwrap();
        let facts = ContainerFacts::from_inspect(
            &serde_json::json!({
                "Id": id, "Name": "/web", "State": { "Pid": 1 },
                "NetworkSettings": {
                    "SandboxKey": "/var/run/docker/netns/web",
                    "Networks": { "bridge": { "IPAddress": ip.to_string() } }
                },
            }),
            &services.domain,
        )
        .unwrap();
        services.publish(
            &facts,
            SelectedRoutes {
                http: Some(3000),
                https: Some(super::super::HttpsBackend::Terminate(3000)),
            },
        );
    }

    #[tokio::test(start_paused = true)]
    async fn managed_access_checks_ownership_before_reporting_and_after_waiting() {
        let destination = Ipv4Addr::new(172, 17, 0, 2);
        let source: IpAddr = "192.168.64.1".parse().unwrap();
        let services = WebServices::new(Domain::default(), Arc::default());
        register(&services, "a", &["web"], destination).await;
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
        register(&services, "a", &["web"], Ipv4Addr::new(172, 17, 0, 3)).await;
        request.enabled = false;
        services.reconcile_https_access(request).unwrap();
        assert!(handshake.await.unwrap().is_err());
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
            let services = WebServices::new(Domain::default(), Arc::default());
            register(&services, "a", &["web"], ip).await;
            let route = services
                .observe_managed_tls(name, ip, ip.into())
                .await
                .unwrap();
            match change {
                "idempotent" => register(&services, "a", &["web"], ip).await,
                "recreate" | "transfer" => {
                    services.dns.write().await.remove("a");
                    let owner = if change == "recreate" { "a" } else { "b" };
                    register(&services, owner, &["web"], ip).await;
                }
                "port" | "native" => {
                    let mut installed = services.installed.write().unwrap();
                    Arc::make_mut(installed.get_mut("a").unwrap())
                        .selected
                        .https = Some(if change == "port" {
                        super::super::HttpsBackend::Terminate(4000)
                    } else {
                        super::super::HttpsBackend::Native(3000)
                    });
                }
                "removed" => services.remove("a"),
                "foreign" => {
                    register(&services, "b", &["web"], ip).await;
                    services.remove("a");
                }
                "republish" => {
                    services.remove("a");
                    register(&services, "a", &["web"], ip).await;
                }
                _ => unreachable!(),
            }
            assert_eq!(
                services.verify_tls_route(name, ip, &route).await.is_ok(),
                change == "idempotent",
                "{change}"
            );
            let before = services.snapshot().await;
            services.invalidate_tls_route(name, ip, &route).await;
            if change == "idempotent" {
                assert!(!services.contains("a"));
            } else {
                assert_eq!(services.snapshot().await, before, "{change}");
            }
        }
    }

    #[tokio::test]
    async fn tls_names_follow_live_dns_ownership() {
        let first = Ipv4Addr::new(172, 17, 0, 2);
        let second = Ipv4Addr::new(172, 17, 0, 3);
        let services = WebServices::new("one.dev.arcbox.local".parse().unwrap(), Arc::default());
        register(&services, "a", &["web.demo"], first).await;
        register(&services, "b", &["db.demo"], second).await;
        assert!(
            services
                .tls_route("WEB.demo.one.dev.arcbox.local", first)
                .await
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
            assert!(services.tls_route(name, first).await.is_none(), "{name}");
        }
        register(&services, "a", &["web.demo"], second).await;
        assert!(
            services
                .tls_route("web.demo.one.dev.arcbox.local", first)
                .await
                .is_none()
        );
        assert!(
            services
                .tls_route("web.demo.one.dev.arcbox.local", second)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn shared_alias_follows_dns_ownership_and_removed_routes_disappear() {
        let services = WebServices::new("one.dev.arcbox.local".parse().unwrap(), Arc::default());
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
            register(&services, id, &aliases, ip).await;
        }
        let snapshot = services.snapshot().await;
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

        services.dns.write().await.remove("a");
        let snapshot = services.snapshot().await;
        assert_eq!(
            snapshot[0].hostnames,
            [
                "web.demo.one.dev.arcbox.local",
                "demo-web-2.one.dev.arcbox.local"
            ]
        );
        assert_eq!(snapshot.len(), 1);

        services.remove("b");
        assert_eq!(services.snapshot().await, Vec::<ContainerWebService>::new());
    }
}
