//! Host-owned signing names and public URLs derived from installed guest web routes.

use arcbox_connect::v1::{
    ContainerWebService, ListContainerWebServicesRequest, ReconcileHttpsAccessRequest,
    ReconcileHttpsAccessResponse, WebService, container_web_service::HttpsBackend,
    web_service::HttpsMode,
};
use arcbox_constants::dns::{Domain, LOCAL_CA_DNS_SUBTREE};
use arcbox_dns::registry::{Lookup, Registry};
use tokio_util::sync::CancellationToken;

use super::Runtime;
use crate::error::{CoreError, Result};

impl Runtime {
    /// Authorizes a local FQDN for signing and returns its DNS resolution revision.
    /// Effective resource or instance changes invalidate the revision across asynchronous work.
    pub async fn https_signing_revision(&self, fqdn: &str) -> Option<u64> {
        let name = fqdn.parse::<Domain>().ok()?;
        let registry = self
            .dns_registry
            .read()
            .expect("DNS registry lock poisoned");
        match registry.resolve(&name) {
            Lookup::Matched {
                claim, revision, ..
            } if Self::is_container_dns_owner(claim.owner()) => Some(revision),
            _ => None,
        }
    }

    /// Returns the selected container address for a current, conflict-free DNS name.
    pub fn container_dns_address(&self, fqdn: &str) -> Option<std::net::IpAddr> {
        let name = fqdn.parse::<Domain>().ok()?;
        let registry = self
            .dns_registry
            .read()
            .expect("DNS registry lock poisoned");
        match registry.resolve(&name) {
            Lookup::Matched { claim, address, .. }
                if Self::is_container_dns_owner(claim.owner()) =>
            {
                Some(address)
            }
            _ => None,
        }
    }

    /// Renews the guest's leased observation of managed HTTPS access from the Mac.
    /// The caller must cancel through the token and await completion, including connection setup.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest cannot be reached or rejects the observation.
    pub async fn reconcile_https_access(
        &self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        self.connect_system_agent()
            .await?
            .reconcile_https_access(request, cancelled)
            .await
            .map_err(Into::into)
    }

    /// Lists container web services with their current DNS names and installed routes.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest cannot be reached or reports an invalid route.
    pub async fn list_web_services(&self, hostname: Option<Domain>) -> Result<Vec<WebService>> {
        let request = ListContainerWebServicesRequest {
            hostname: hostname
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
            ..Default::default()
        };
        let mut agent = self.connect_system_agent().await?;
        let response = if agent.is_blocking() {
            tokio::task::spawn_blocking(move || agent.list_container_web_services_blocking(request))
                .await
                .map_err(|error| CoreError::Vm(format!("web services task panicked: {error}")))??
        } else {
            agent.list_container_web_services(request).await?
        };
        let registry = self
            .dns_registry
            .read()
            .expect("DNS registry lock poisoned");
        selected_services(response.services, hostname.as_ref(), &registry)
    }
}

fn selected_services(
    routes: Vec<ContainerWebService>,
    hostname: Option<&Domain>,
    registry: &Registry,
) -> Result<Vec<WebService>> {
    if let Some(name) = hostname {
        if routes.len() > 1
            || routes
                .iter()
                .any(|route| route.hostnames != [name.as_str()])
        {
            return Err(CoreError::Vm(
                "guest web route does not match the requested hostname".into(),
            ));
        }
    }
    routes
        .into_iter()
        .try_fold(Vec::new(), |mut services, route| {
            services.extend(container_services(route, registry)?);
            Ok(services)
        })
}

fn container_services(route: ContainerWebService, registry: &Registry) -> Result<Vec<WebService>> {
    if route.container_id.is_empty() || !Runtime::is_container_dns_owner(&route.container_id) {
        return Err(CoreError::Vm(
            "guest web route has invalid container identity".into(),
        ));
    }
    let ip = route
        .ipv4_address
        .parse::<std::net::Ipv4Addr>()
        .map_err(|error| {
            CoreError::Vm(format!("guest web route has invalid IPv4 address: {error}"))
        })?;
    if route.hostnames.is_empty() {
        return Err(CoreError::Vm("guest web route has no DNS names".into()));
    }
    let (https_mode, https_port) = match route.https_backend {
        Some(HttpsBackend::NativeHttpsPort(port)) => (HttpsMode::Passthrough, port),
        Some(HttpsBackend::TerminatedHttpPort(port)) => (HttpsMode::Managed, port),
        None => (HttpsMode::None, 0),
    };
    if route.http_port > u32::from(u16::MAX)
        || https_port > u32::from(u16::MAX)
        || (https_mode != HttpsMode::None && https_port == 0)
        || (route.http_port == 0 && https_port == 0)
    {
        return Err(CoreError::Vm(
            "guest web route has invalid upstream ports".into(),
        ));
    }
    let mut domains = Vec::new();
    for hostname in route.hostnames {
        let name = hostname.parse::<Domain>().map_err(|error| {
            CoreError::Vm(format!("guest web route has invalid domain: {error}"))
        })?;
        if matches!(registry.resolve(&name), Lookup::Matched { claim, address, .. }
            if claim.owner() == route.container_id && address == std::net::IpAddr::V4(ip))
        {
            domains.push(hostname);
        }
    }
    let groups: Vec<(Vec<String>, HttpsMode, u32)> = if https_mode == HttpsMode::Managed {
        let (local, foreign) = domains
            .into_iter()
            .partition(|name: &String| name.ends_with(LOCAL_CA_DNS_SUBTREE));
        vec![
            (local, https_mode, https_port),
            (foreign, HttpsMode::None, 0),
        ]
    } else {
        vec![(domains, https_mode, https_port)]
    };
    Ok(groups
        .into_iter()
        .filter_map(|(domains, mode, port)| {
            let hostname = domains.first()?.clone();
            if route.http_port == 0 && port == 0 {
                return None;
            }
            Some(WebService {
                owner: format!("container:{}", route.container_id),
                http_url: if route.http_port == 0 {
                    String::new()
                } else {
                    format!("http://{hostname}")
                },
                https_url: if port == 0 {
                    String::new()
                } else {
                    format!("https://{hostname}")
                },
                hostname,
                domains,
                https_mode: mode.into(),
                http_port: route.http_port,
                https_port: port,
                ..Default::default()
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn container_claims_keep_canonical_names_and_withdraw_addressless_owners() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        let mut inspect = serde_json::json!({
            "Id": "container", "Name": "/web",
            "Config": {"Labels": {"dev.arcbox.domains": "custom.local,*.api.local"}},
            "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "10.0.2.3"}}}
        });
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(&inspect).unwrap();
        runtime.register_container_dns(&metadata).await;
        let revision = runtime
            .https_signing_revision("custom.local")
            .await
            .unwrap();
        for name in ["web.arcbox.local", "custom.local", "v1.api.local"] {
            assert_eq!(
                runtime.container_dns_address(name),
                Some("10.0.2.3".parse().unwrap())
            );
            assert_eq!(runtime.https_signing_revision(name).await, Some(revision));
        }
        for name in ["custom.local.arcbox.local", "api.local"] {
            assert_eq!(runtime.container_dns_address(name), None);
            assert_eq!(runtime.https_signing_revision(name).await, None);
        }
        let hosts = runtime.network_manager().local_hosts_table();
        assert!(hosts.read().unwrap().contains_key("custom.local"));
        assert!(
            !hosts
                .read()
                .unwrap()
                .contains_key("custom.local.arcbox.local")
        );
        runtime.register_container_dns(&metadata).await;
        assert_eq!(
            runtime.https_signing_revision("custom.local").await,
            Some(revision)
        );

        inspect["NetworkSettings"]["Networks"] = serde_json::json!({});
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(&inspect).unwrap();
        runtime.register_container_dns(&metadata).await;
        runtime.container_aliases.write().await.clear();
        assert!(
            runtime
                .registered_container_ids()
                .await
                .contains("container")
        );
        assert_eq!(runtime.container_dns_address("custom.local"), None);
        assert_eq!(runtime.https_signing_revision("custom.local").await, None);
        assert!(!hosts.read().unwrap().contains_key("custom.local"));
        runtime.deregister_dns_by_id("container").await;
        assert!(runtime.registered_container_ids().await.is_empty());
    }

    #[tokio::test]
    async fn signing_authorization_follows_current_container_dns_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        let name = "web.arcbox.local";
        assert_eq!(runtime.https_signing_revision(name).await, None);
        let ip = "10.0.2.3".parse().unwrap();
        let aliases = [
            "web".into(),
            "web.demo.arcbox.local".into(),
            "absolute.arcbox.local".into(),
        ];
        runtime.register_dns("first", &aliases, ip).await;
        let first = runtime.https_signing_revision(name).await.unwrap();
        for alias in [name, "web.demo.arcbox.local", "absolute.arcbox.local"] {
            assert_eq!(runtime.https_signing_revision(alias).await, Some(first));
        }
        assert_eq!(
            runtime.https_signing_revision("web.example.com").await,
            None
        );

        runtime.register_dns("first", &aliases, ip).await;
        let renewed = runtime.https_signing_revision(name).await.unwrap();
        assert_eq!(renewed, first);
        runtime.register_dns("second", &["web".into()], ip).await;
        assert_eq!(runtime.https_signing_revision(name).await, None);
        runtime.deregister_dns_by_id("second").await;
        for owner in ["sandbox:other", "machine:other", "system:host"] {
            runtime.register_dns(owner, &["web".into()], ip).await;
            assert_eq!(runtime.https_signing_revision(name).await, None, "{owner}");
            runtime.deregister_dns_by_id(owner).await;
            assert_eq!(runtime.https_signing_revision(name).await, Some(first));
        }
        assert_eq!(runtime.https_signing_revision(name).await, Some(renewed));
        runtime
            .network_manager()
            .local_hosts_table()
            .write()
            .unwrap()
            .insert(name.into(), "10.0.2.4".parse().unwrap());
        assert_eq!(runtime.https_signing_revision(name).await, Some(first));
        runtime.deregister_dns_by_id("first").await;
        assert_eq!(runtime.https_signing_revision(name).await, None);
    }

    fn route() -> ContainerWebService {
        ContainerWebService {
            container_id: "abc123".into(),
            ipv4_address: "10.0.2.3".into(),
            hostnames: vec![
                "web.demo.arcbox.local".into(),
                "demo-web-1.arcbox.local".into(),
            ],
            http_port: 3000,
            https_backend: Some(HttpsBackend::TerminatedHttpPort(3000)),
            ..Default::default()
        }
    }

    fn registry() -> Registry {
        let route = route();
        let mut registry = Registry::default();
        let claims = route
            .hostnames
            .iter()
            .map(|name| {
                arcbox_dns::claims::Claim::exclusive(
                    arcbox_dns::NamePattern::Exact(name.parse().unwrap()),
                    route.container_id.clone(),
                    "container".into(),
                )
            })
            .collect();
        registry
            .replace(
                route.container_id,
                claims,
                Some(route.ipv4_address.parse().unwrap()),
            )
            .unwrap();
        registry
    }

    #[test]
    fn urls_use_the_owned_compose_alias_and_default_ingress_ports() {
        let services = container_services(route(), &registry()).unwrap();
        let service = &services[0];
        assert_eq!(service.owner, "container:abc123");
        assert_eq!(service.hostname, "web.demo.arcbox.local");
        assert_eq!(service.domains.len(), 2);
        assert_eq!(service.http_url, "http://web.demo.arcbox.local");
        assert_eq!(service.https_url, "https://web.demo.arcbox.local");
        assert_eq!(service.https_mode, HttpsMode::Managed);
        assert_eq!(service.http_port, 3000);
        assert_eq!(service.https_port, 3000);
    }

    #[test]
    fn native_tls_without_http_preserves_the_backend_mode() {
        let mut route = route();
        route.http_port = 0;
        route.https_backend = Some(HttpsBackend::NativeHttpsPort(8443));
        let services = container_services(route, &registry()).unwrap();
        let service = &services[0];
        assert_eq!(service.http_url, "");
        assert_eq!(service.https_url, "https://web.demo.arcbox.local");
        assert_eq!(service.https_mode, HttpsMode::Passthrough);
        assert_eq!(service.https_port, 8443);
    }

    #[test]
    fn absent_https_does_not_produce_an_https_url() {
        let mut route = route();
        route.https_backend = None;
        let services = container_services(route, &registry()).unwrap();
        let service = &services[0];
        assert_eq!(service.https_url, "");
        assert_eq!(service.https_mode, HttpsMode::None);
    }

    #[test]
    fn invalid_guest_routes_fail_before_becoming_browser_urls() {
        for hostname in ["", "evil.com/#.arcbox.local", "bad_name.local", "a..local"] {
            let mut route = route();
            route.hostnames = vec![hostname.into()];
            assert!(container_services(route, &registry()).is_err());
        }
        let mut overflow = route();
        overflow.http_port = u32::MAX;
        assert!(container_services(overflow, &registry()).is_err());
        let mut zero_tls = route();
        zero_tls.https_backend = Some(HttpsBackend::NativeHttpsPort(0));
        assert!(container_services(zero_tls, &registry()).is_err());
        for ip in ["", "::1", "invalid"] {
            let mut invalid = route();
            invalid.ipv4_address = ip.into();
            assert!(container_services(invalid, &registry()).is_err());
        }
        for owner in ["", "system:host", "machine:test", "sandbox:test"] {
            let mut invalid = route();
            invalid.container_id = owner.into();
            assert!(container_services(invalid, &registry()).is_err());
        }
    }

    #[tokio::test]
    async fn configured_suffix_reaches_host_dns_guest_boot_and_signing_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let suffix: Domain = "one.dev.arcbox.local".parse().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            network: crate::config::NetworkConfig {
                dns_domain: suffix.clone(),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert_eq!(runtime.vm_lifecycle().config().dns_domain, suffix);
        assert_eq!(
            runtime.network_manager().dns_domain().as_deref(),
            Some(suffix.as_str())
        );
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await;
        let name = "web.one.dev.arcbox.local";
        let first = runtime.https_signing_revision(name).await.unwrap();
        for name in [
            "web.arcbox.local",
            "web.two.dev.arcbox.local",
            "web.one.dev.arcbox.local.evil",
        ] {
            assert_eq!(runtime.https_signing_revision(name).await, None, "{name}");
        }
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await;
        let renewed = runtime.https_signing_revision(name).await.unwrap();
        assert_eq!(renewed, first);
        runtime.deregister_dns_by_id("container").await;
        assert_eq!(runtime.https_signing_revision(name).await, None);
    }

    #[test]
    fn mixed_aliases_expose_managed_https_only_for_local_names() {
        let mut registry = registry();
        let mut custom = route();
        custom.hostnames = vec!["web.containers.test".into(), "api.local".into()];
        let claims = custom
            .hostnames
            .iter()
            .map(|name| {
                arcbox_dns::claims::Claim::exclusive(
                    arcbox_dns::NamePattern::Exact(name.parse().unwrap()),
                    custom.container_id.clone(),
                    "container".into(),
                )
            })
            .collect();
        registry
            .replace(
                custom.container_id.clone(),
                claims,
                Some("10.0.2.3".parse().unwrap()),
            )
            .unwrap();
        let services = container_services(custom.clone(), &registry).unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].https_url, "https://api.local");
        assert_eq!(services[0].domains, ["api.local"]);
        assert_eq!(services[0].https_mode, HttpsMode::Managed);
        assert_eq!(services[1].http_url, "http://web.containers.test");
        assert_eq!(services[1].domains, ["web.containers.test"]);
        assert_eq!(services[1].https_url, "");
        assert_eq!(services[1].https_mode, HttpsMode::None);
        assert_eq!(services[1].https_port, 0);
        custom.http_port = 0;
        let services = container_services(custom.clone(), &registry).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].https_url, "https://api.local");
        custom.https_backend = Some(HttpsBackend::NativeHttpsPort(8443));
        let services = container_services(custom, &registry).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].https_url, "https://web.containers.test");
        assert_eq!(services[0].https_mode, HttpsMode::Passthrough);
    }

    #[test]
    fn stale_routes_and_conflicting_names_are_withdrawn_from_the_directory() {
        let mut registry = registry();
        for (id, ip) in [("other", "10.0.2.3"), ("abc123", "10.0.2.4")] {
            let mut stale = route();
            stale.container_id = id.into();
            stale.ipv4_address = ip.into();
            assert_eq!(container_services(stale, &registry).unwrap(), []);
        }
        let conflict = arcbox_dns::claims::Claim::exclusive(
            arcbox_dns::NamePattern::Exact("web.demo.arcbox.local".parse().unwrap()),
            "other".into(),
            "container".into(),
        );
        registry
            .replace(
                "other".into(),
                vec![conflict],
                Some("10.0.2.3".parse().unwrap()),
            )
            .unwrap();
        let services = container_services(route(), &registry).unwrap();
        assert_eq!(services[0].hostname, "demo-web-1.arcbox.local");
        assert_eq!(services[0].domains, ["demo-web-1.arcbox.local"]);
        registry.remove("abc123");
        assert_eq!(container_services(route(), &registry).unwrap(), []);
    }

    #[test]
    fn hostname_queries_validate_the_guest_selection_before_publishing_urls() {
        let name: Domain = "first.api.local".parse().unwrap();
        let mut registry = Registry::default();
        registry
            .replace(
                "abc123".into(),
                vec![arcbox_dns::claims::Claim::exclusive(
                    arcbox_dns::NamePattern::parse_custom("*.api.local").unwrap(),
                    "abc123".into(),
                    "container".into(),
                )],
                Some("10.0.2.3".parse().unwrap()),
            )
            .unwrap();
        let mut route = route();
        route.hostnames = vec![name.to_string()];
        let services = selected_services(vec![route.clone()], Some(&name), &registry).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].https_url, "https://first.api.local");
        assert_eq!(services[0].https_mode, HttpsMode::Managed);
        assert!(
            selected_services(vec![route.clone(), route.clone()], Some(&name), &registry).is_err()
        );
        for hostnames in [
            vec![],
            vec!["other.api.local".into()],
            vec![name.to_string(), "other.api.local".into()],
        ] {
            let mut tampered = route.clone();
            tampered.hostnames = hostnames;
            assert!(selected_services(vec![tampered], Some(&name), &registry).is_err());
        }
        for (owner, address) in [("other", "10.0.2.3"), ("abc123", "10.0.2.4")] {
            let mut stale = route.clone();
            stale.container_id = owner.into();
            stale.ipv4_address = address.into();
            assert_eq!(
                selected_services(vec![stale], Some(&name), &registry).unwrap(),
                []
            );
        }
    }
}
