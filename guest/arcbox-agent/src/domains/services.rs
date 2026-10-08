//! Installed web routes joined with the current DNS resolution.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, RwLock};

use arcbox_connect::v1::{
    ContainerWebService, ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse,
    container_web_service::HttpsBackend,
};
use arcbox_constants::dns::Domain;

use super::{ContainerFacts, SelectedRoutes};
use crate::dns_server::ContainerRegistry;

struct Installed {
    aliases: Vec<String>,
    ip: Ipv4Addr,
    selected: SelectedRoutes,
}

/// Installed routes and HTTPS access observation shared with the RPC connections.
#[derive(Clone, Default)]
pub struct WebServices {
    pub(super) domain: Domain,
    installed: Arc<RwLock<HashMap<String, Installed>>>,
    dns: ContainerRegistry,
    access: super::access::Access,
}

impl WebServices {
    pub(super) async fn observe_managed_tls(
        &self,
        name: &str,
        destination: Ipv4Addr,
        source: IpAddr,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.resolves_to(name, destination).await,
            "TLS name {name:?} does not resolve to {destination}"
        );
        self.access.observe(name, destination, source).await;
        // DNS ownership can change while the Mac authorizes certificate trust.
        anyhow::ensure!(
            self.resolves_to(name, destination).await,
            "TLS name {name:?} no longer resolves to {destination}"
        );
        Ok(())
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
            installed.insert(
                facts.id.clone(),
                Installed {
                    aliases: facts.aliases.clone(),
                    ip: facts.primary_ip,
                    selected,
                },
            );
        }
    }

    pub(super) fn remove(&self, id: &str) {
        self.installed
            .write()
            .expect("web services lock poisoned")
            .remove(id);
    }

    /// A TLS name must resolve to the address the client actually dialled.
    pub(super) async fn resolves_to(&self, name: &str, address: Ipv4Addr) -> bool {
        let name = name.to_ascii_lowercase();
        let Some(alias) = name
            .strip_suffix(self.domain.as_str())
            .and_then(|prefix| prefix.strip_suffix('.'))
        else {
            return false;
        };
        self.dns.read().await.get(alias) == Some(&address)
    }

    /// Shared Compose aliases belong only to the container DNS currently selects.
    pub async fn snapshot(&self) -> Vec<ContainerWebService> {
        let dns = self.dns.read().await;
        let installed = self.installed.read().expect("web services lock poisoned");
        let mut services: Vec<_> = installed
            .iter()
            .filter_map(|(id, route)| {
                let hostnames: Vec<_> = route
                    .aliases
                    .iter()
                    .filter(|alias| dns.get(*alias) == Some(&route.ip))
                    .map(|alias| format!("{alias}.{}", self.domain))
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

    #[tokio::test(start_paused = true)]
    async fn managed_access_checks_ownership_before_reporting_and_after_waiting() {
        let destination = Ipv4Addr::new(172, 17, 0, 2);
        let source: IpAddr = "192.168.64.1".parse().unwrap();
        let dns = Arc::new(tokio::sync::RwLock::new(HashMap::from([(
            "web".into(),
            destination,
        )])));
        let services = WebServices::new(Domain::default(), dns.clone());
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
        dns.write()
            .await
            .insert("web".into(), Ipv4Addr::new(172, 17, 0, 3));
        request.enabled = false;
        services.reconcile_https_access(request).unwrap();
        assert!(handshake.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn tls_names_follow_live_dns_ownership() {
        let first = Ipv4Addr::new(172, 17, 0, 2);
        let second = Ipv4Addr::new(172, 17, 0, 3);
        let dns = Arc::new(tokio::sync::RwLock::new(HashMap::from([
            ("web.demo".into(), first),
            ("db.demo".into(), second),
        ])));
        let services = WebServices::new("one.dev.arcbox.local".parse().unwrap(), dns.clone());
        assert!(
            services
                .resolves_to("WEB.demo.one.dev.arcbox.local", first)
                .await
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
            assert!(!services.resolves_to(name, first).await, "{name}");
        }
        dns.write().await.insert("web.demo".into(), second);
        assert!(
            !services
                .resolves_to("web.demo.one.dev.arcbox.local", first)
                .await
        );
        assert!(
            services
                .resolves_to("web.demo.one.dev.arcbox.local", second)
                .await
        );
    }

    #[tokio::test]
    async fn shared_alias_follows_dns_ownership_and_removed_routes_disappear() {
        let dns = Arc::new(tokio::sync::RwLock::new(HashMap::from([
            ("web.demo".into(), Ipv4Addr::new(172, 17, 0, 3)),
            ("demo-web-1".into(), Ipv4Addr::new(172, 17, 0, 2)),
            ("demo-web-2".into(), Ipv4Addr::new(172, 17, 0, 3)),
        ])));
        let services = WebServices::new("one.dev.arcbox.local".parse().unwrap(), dns.clone());
        for (id, ip, aliases) in [
            (
                "a",
                Ipv4Addr::new(172, 17, 0, 2),
                ["web.demo", "demo-web-1"],
            ),
            (
                "b",
                Ipv4Addr::new(172, 17, 0, 3),
                ["web.demo", "demo-web-2"],
            ),
        ] {
            services.installed.write().unwrap().insert(
                id.into(),
                Installed {
                    aliases: aliases.map(str::to_owned).to_vec(),
                    ip,
                    selected: SelectedRoutes {
                        http: Some(3000),
                        https: Some(super::super::HttpsBackend::Terminate(3000)),
                    },
                },
            );
        }
        let snapshot = services.snapshot().await;
        assert_eq!(snapshot[0].hostnames, ["demo-web-1.one.dev.arcbox.local"]);
        assert_eq!(
            snapshot[1].hostnames,
            [
                "web.demo.one.dev.arcbox.local",
                "demo-web-2.one.dev.arcbox.local"
            ]
        );
        assert_eq!(
            snapshot[1].https_backend,
            Some(HttpsBackend::TerminatedHttpPort(3000))
        );

        dns.write()
            .await
            .insert("web.demo".into(), Ipv4Addr::new(172, 17, 0, 2));
        let snapshot = services.snapshot().await;
        assert_eq!(
            snapshot[0].hostnames,
            [
                "web.demo.one.dev.arcbox.local",
                "demo-web-1.one.dev.arcbox.local"
            ]
        );
        assert_eq!(snapshot[1].hostnames, ["demo-web-2.one.dev.arcbox.local"]);

        services.remove("a");
        assert_eq!(services.snapshot().await.len(), 1);
        dns.write().await.clear();
        assert_eq!(services.snapshot().await, Vec::<ContainerWebService>::new());
    }
}
