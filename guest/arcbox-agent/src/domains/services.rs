//! Installed web routes joined with the current DNS resolution.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, RwLock};

use arcbox_connect::v1::{ContainerWebService, container_web_service::HttpsBackend};

use super::{ContainerFacts, SelectedRoutes};
use crate::dns_server::ContainerRegistry;

struct Installed {
    aliases: Vec<String>,
    ip: Ipv4Addr,
    selected: SelectedRoutes,
}

/// A read-only snapshot handle shared by the routing task and RPC connections.
#[derive(Clone, Default)]
pub struct WebServices {
    installed: Arc<RwLock<HashMap<String, Installed>>>,
    dns: ContainerRegistry,
}

impl WebServices {
    pub(crate) fn new(dns: ContainerRegistry) -> Self {
        Self {
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
            .strip_suffix(arcbox_constants::dns::LOCAL_DOMAIN)
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
                    .map(|alias| format!("{alias}.{}", arcbox_constants::dns::LOCAL_DOMAIN))
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

    #[tokio::test]
    async fn tls_names_follow_live_dns_ownership() {
        let first = Ipv4Addr::new(172, 17, 0, 2);
        let second = Ipv4Addr::new(172, 17, 0, 3);
        let dns = Arc::new(tokio::sync::RwLock::new(HashMap::from([
            ("web.demo".into(), first),
            ("db.demo".into(), second),
        ])));
        let services = WebServices::new(dns.clone());
        assert!(services.resolves_to("WEB.demo.arcbox.local", first).await);
        for name in [
            "db.demo.arcbox.local",
            "missing.arcbox.local",
            "web.demo",
            "web.demo.arcbox.local.evil",
        ] {
            assert!(!services.resolves_to(name, first).await, "{name}");
        }
        dns.write().await.insert("web.demo".into(), second);
        assert!(!services.resolves_to("web.demo.arcbox.local", first).await);
        assert!(services.resolves_to("web.demo.arcbox.local", second).await);
    }

    #[tokio::test]
    async fn shared_alias_follows_dns_ownership_and_removed_routes_disappear() {
        let dns = Arc::new(tokio::sync::RwLock::new(HashMap::from([
            ("web.demo".into(), Ipv4Addr::new(172, 17, 0, 3)),
            ("demo-web-1".into(), Ipv4Addr::new(172, 17, 0, 2)),
            ("demo-web-2".into(), Ipv4Addr::new(172, 17, 0, 3)),
        ])));
        let services = WebServices::new(dns.clone());
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
        assert_eq!(snapshot[0].hostnames, ["demo-web-1.arcbox.local"]);
        assert_eq!(
            snapshot[1].hostnames,
            ["web.demo.arcbox.local", "demo-web-2.arcbox.local"]
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
            ["web.demo.arcbox.local", "demo-web-1.arcbox.local"]
        );
        assert_eq!(snapshot[1].hostnames, ["demo-web-2.arcbox.local"]);

        services.remove("a");
        assert_eq!(services.snapshot().await.len(), 1);
        dns.write().await.clear();
        assert_eq!(services.snapshot().await, Vec::<ContainerWebService>::new());
    }
}
