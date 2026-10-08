//! Host-owned signing names and guest routes projected through shared URL policy.

use arcbox_connect::v1::{
    ContainerWebService, GuestWebService, IndexWebService, ListGuestWebServicesRequest,
    ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse, WebService,
    container_web_service::HttpsBackend, guest_web_service::Route, web_service::HttpsMode,
};
use arcbox_constants::dns::Domain;
use arcbox_dns::registry::{Lookup, Registry};
use arcbox_dns::web_services::{HttpsMode as UrlHttpsMode, WebServiceUrls};
use tokio_util::sync::CancellationToken;

mod guest;
pub use guest::CapturedWebGuest;

#[cfg(test)]
mod https_access_tests;
mod index;

use super::Runtime;
use crate::error::{CoreError, Result};

impl Runtime {
    /// Returns the current container or built-in index address for a conflict-free DNS name.
    pub fn web_service_dns_address(&self, fqdn: &str) -> Option<std::net::IpAddr> {
        self.web_service_dns_entry(fqdn).map(|(address, _)| address)
    }

    fn web_service_dns_entry(&self, fqdn: &str) -> Option<(std::net::IpAddr, u64)> {
        let name = fqdn.parse::<Domain>().ok()?;
        let registry = self
            .dns_registry
            .read()
            .expect("DNS registry lock poisoned");
        self.system_web_service_dns_entry(&registry, &name)
    }

    fn system_web_service_dns_entry(
        &self,
        registry: &Registry,
        name: &Domain,
    ) -> Option<(std::net::IpAddr, u64)> {
        if name == &self.config.network.dns_domain {
            return IndexEndpoint {
                apex: &self.config.network.dns_domain,
                gateway: self.config.container.cidr.docker_bridge_gateway(),
            }
            .current(registry);
        }
        match registry.resolve(name) {
            Lookup::Matched {
                claim,
                address,
                revision,
                ..
            } if Self::is_container_dns_owner(claim.owner()) => Some((address, revision)),
            _ => None,
        }
    }

    /// Renews the guest's leased observation of managed HTTPS access from the Mac.
    /// The caller must cancel through the token and await completion, including connection setup.
    ///
    /// # Errors
    ///
    /// Returns an error if the captured VM changes, cancellation occurs, or the guest rejects the observation.
    pub async fn reconcile_https_access(
        &self,
        expected: &arcbox_engine::vm::VmIncarnation,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        if cancelled.is_cancelled() {
            return Err(CoreError::invalid_state(
                "HTTPS access observation cancelled",
            ));
        }
        self.with_host_network_incarnation(self.default_machine_name(), expected, || ())?;
        let response = self
            .connect_system_agent_expected(expected.clone())
            .await?
            .reconcile_https_access(request, cancelled)
            .await?;
        self.with_host_network_incarnation(self.default_machine_name(), expected, || response)
    }

    /// Lists guest web services with their current DNS names and installed routes.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest cannot be reached or reports an invalid route.
    pub async fn list_web_services(&self, hostname: Option<Domain>) -> Result<Vec<WebService>> {
        let request = ListGuestWebServicesRequest {
            hostname: hostname
                .as_ref()
                .map_or_else(String::new, ToString::to_string),
            ..Default::default()
        };
        let expected = self.container_network_incarnation()?;
        let agent = self.connect_system_agent_expected(expected.clone()).await?;
        let response = agent
            .list_guest_web_services(request, &CancellationToken::new())
            .await?;
        self.with_host_network_incarnation(self.default_machine_name(), &expected, || {
            let registry = self
                .dns_registry
                .read()
                .expect("DNS registry lock poisoned");
            selected_services(
                response.services,
                hostname.as_ref(),
                &registry,
                &IndexEndpoint {
                    apex: &self.config.network.dns_domain,
                    gateway: self.config.container.cidr.docker_bridge_gateway(),
                },
            )
        })?
    }
}

fn selected_services(
    routes: Vec<GuestWebService>,
    hostname: Option<&Domain>,
    registry: &Registry,
    index: &IndexEndpoint<'_>,
) -> Result<Vec<WebService>> {
    if hostname.is_some() && routes.len() > 1 {
        return Err(CoreError::Vm(
            "guest returned multiple routes for one hostname".into(),
        ));
    }
    routes
        .into_iter()
        .try_fold(Vec::new(), |mut services, route| {
            let projected = match route.route {
                Some(Route::Container(route)) => {
                    if hostname.is_some_and(|name| route.hostnames != [name.as_str()]) {
                        return Err(CoreError::Vm(
                            "guest web route does not match the requested hostname".into(),
                        ));
                    }
                    container_services(*route, registry)?
                }
                Some(Route::Index(route)) => {
                    if hostname.is_some_and(|name| route.hostname != name.as_str()) {
                        return Err(CoreError::Vm(
                            "guest index does not match the requested hostname".into(),
                        ));
                    }
                    index.project(*route, registry)?
                }
                Some(Route::Machine(_)) => {
                    return Err(CoreError::Vm(
                        "System VM reported a machine web route".into(),
                    ));
                }
                None => return Err(CoreError::Vm("guest web route has no resource kind".into())),
            };
            services.extend(projected);
            Ok(services)
        })
}

struct IndexEndpoint<'a> {
    apex: &'a Domain,
    gateway: std::net::Ipv4Addr,
}

impl IndexEndpoint<'_> {
    fn current(&self, registry: &Registry) -> Option<(std::net::IpAddr, u64)> {
        if registry.get("system:index")?.claims.len() != 1 {
            return None;
        }
        match registry.resolve(self.apex) {
            Lookup::Matched {
                claim,
                address,
                revision,
                publishable: true,
            } if claim.owner() == "system:index"
                && claim.authority() == "index"
                && claim.pattern() == &arcbox_dns::NamePattern::Exact(self.apex.clone())
                && address == std::net::IpAddr::V4(self.gateway) =>
            {
                Some((address, revision))
            }
            _ => None,
        }
    }

    fn project(&self, route: IndexWebService, registry: &Registry) -> Result<Vec<WebService>> {
        let Self { apex, gateway } = *self;
        if route.hostname != apex.as_str() || route.ipv4_address != gateway.to_string() {
            return Err(CoreError::Vm(
                "guest index does not match the configured apex and gateway".into(),
            ));
        }
        if self.current(registry).is_none() {
            return Ok(Vec::new());
        }
        let mode = if route.managed_https {
            UrlHttpsMode::Managed
        } else {
            UrlHttpsMode::None
        };
        Ok(WebServiceUrls::project(vec![apex.clone()], true, mode)
            .into_iter()
            .map(|urls| WebService {
                owner: "system:index".into(),
                hostname: urls.hostname,
                domains: urls.domains,
                http_url: urls.http_url,
                https_url: urls.https_url,
                https_mode: if urls.https_mode == UrlHttpsMode::Managed {
                    HttpsMode::Managed
                } else {
                    HttpsMode::None
                }
                .into(),
                ..Default::default()
            })
            .collect())
    }
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
        Some(HttpsBackend::NativeHttpsPort(port)) => (UrlHttpsMode::Passthrough, port),
        Some(HttpsBackend::TerminatedHttpPort(port)) => (UrlHttpsMode::Managed, port),
        None => (UrlHttpsMode::None, 0),
    };
    if route.http_port > u32::from(u16::MAX)
        || https_port > u32::from(u16::MAX)
        || (https_mode != UrlHttpsMode::None && https_port == 0)
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
            domains.push(name);
        }
    }
    Ok(
        WebServiceUrls::project(domains, route.http_port != 0, https_mode)
            .into_iter()
            .map(|urls| {
                let (mode, port) = match urls.https_mode {
                    UrlHttpsMode::None => (HttpsMode::None, 0),
                    UrlHttpsMode::Managed => (HttpsMode::Managed, https_port),
                    UrlHttpsMode::Passthrough => (HttpsMode::Passthrough, https_port),
                };
                WebService {
                    owner: format!("container:{}", route.container_id),
                    http_url: urls.http_url,
                    https_url: urls.https_url,
                    hostname: urls.hostname,
                    domains: urls.domains,
                    https_mode: mode.into(),
                    http_port: route.http_port,
                    https_port: port,
                    ..Default::default()
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dns_revision(runtime: &Runtime, name: &str) -> Option<u64> {
        runtime
            .web_service_dns_entry(name)
            .map(|(_, revision)| revision)
    }

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
        runtime
            .dns_registry
            .write()
            .unwrap()
            .replace_instance_authorities(vec![arcbox_dns::registry::InstanceAuthority {
                root: "custom.local".parse().unwrap(),
                target: arcbox_dns::registry::InstanceTarget::Current,
            }]);
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(&inspect).unwrap();
        runtime.register_container_dns(&metadata).await.unwrap();
        let revision = dns_revision(&runtime, "custom.local").unwrap();
        for name in ["web.arcbox.local", "custom.local", "v1.api.local"] {
            assert_eq!(
                runtime.web_service_dns_address(name),
                Some("10.0.2.3".parse().unwrap())
            );
            assert_eq!(dns_revision(&runtime, name), Some(revision));
        }
        for name in ["custom.local.arcbox.local", "api.local"] {
            assert_eq!(runtime.web_service_dns_address(name), None);
            assert_eq!(dns_revision(&runtime, name), None);
        }
        let hosts = runtime.network_manager().local_hosts_table();
        assert!(hosts.read().unwrap().contains_key("custom.local"));
        assert!(
            !hosts
                .read()
                .unwrap()
                .contains_key("custom.local.arcbox.local")
        );
        runtime.register_container_dns(&metadata).await.unwrap();
        assert_eq!(dns_revision(&runtime, "custom.local"), Some(revision));

        inspect["NetworkSettings"]["Networks"] = serde_json::json!({});
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(&inspect).unwrap();
        runtime.register_container_dns(&metadata).await.unwrap();
        runtime.container_aliases.write().await.clear();
        assert!(
            runtime
                .registered_container_ids()
                .await
                .contains("container")
        );
        assert_eq!(runtime.web_service_dns_address("custom.local"), None);
        assert_eq!(dns_revision(&runtime, "custom.local"), None);
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
        assert_eq!(dns_revision(&runtime, name), None);
        let ip = "10.0.2.3".parse().unwrap();
        let aliases = [
            "web".into(),
            "web.demo.arcbox.local".into(),
            "absolute.arcbox.local".into(),
        ];
        runtime.register_dns("first", &aliases, ip).await.unwrap();
        let first = dns_revision(&runtime, name).unwrap();
        for alias in [name, "web.demo.arcbox.local", "absolute.arcbox.local"] {
            assert_eq!(dns_revision(&runtime, alias), Some(first));
        }
        assert_eq!(dns_revision(&runtime, "web.example.com"), None);

        runtime.register_dns("first", &aliases, ip).await.unwrap();
        let renewed = dns_revision(&runtime, name).unwrap();
        assert_eq!(renewed, first);
        runtime
            .register_dns("second", &["web".into()], ip)
            .await
            .unwrap();
        assert_eq!(dns_revision(&runtime, name), None);
        runtime.deregister_dns_by_id("second").await;
        for owner in ["sandbox:other", "machine:other", "system:host"] {
            runtime
                .register_dns(owner, &["web".into()], ip)
                .await
                .unwrap();
            assert_eq!(dns_revision(&runtime, name), None, "{owner}");
            runtime.deregister_dns_by_id(owner).await;
            assert_eq!(dns_revision(&runtime, name), Some(first));
        }
        assert_eq!(dns_revision(&runtime, name), Some(renewed));
        runtime
            .network_manager()
            .local_hosts_table()
            .write()
            .unwrap()
            .insert(name.into(), "10.0.2.4".parse().unwrap());
        assert_eq!(dns_revision(&runtime, name), Some(first));
        runtime.deregister_dns_by_id("first").await;
        assert_eq!(dns_revision(&runtime, name), None);
    }

    fn selected_container_services(
        routes: Vec<ContainerWebService>,
        hostname: Option<&Domain>,
        registry: &Registry,
    ) -> Result<Vec<WebService>> {
        selected_services(
            routes
                .into_iter()
                .map(|container| GuestWebService {
                    route: container.into(),
                    ..Default::default()
                })
                .collect(),
            hostname,
            registry,
            &IndexEndpoint {
                apex: &"arcbox.local".parse().unwrap(),
                gateway: "172.17.0.1".parse().unwrap(),
            },
        )
    }

    #[test]
    fn missing_or_invalid_typed_routes_are_rejected() {
        let index = IndexEndpoint {
            apex: &"arcbox.local".parse().unwrap(),
            gateway: "172.17.0.1".parse().unwrap(),
        };
        for route in [None, Some(Route::Index(Default::default()))] {
            let service = GuestWebService {
                route,
                ..Default::default()
            };
            assert!(selected_services(vec![service], None, &registry(), &index).is_err());
        }
    }

    #[test]
    fn system_guest_cannot_report_a_registered_machine_or_publish_partial_rows() {
        use arcbox_connect::v1::{MachineWebService, machine_web_service};
        use arcbox_dns::{NamePattern, claims::Claim};

        let hostname: Domain = "dev.machine.arcbox.local".parse().unwrap();
        let owner = "machine:dev";
        let address = "192.168.64.5";
        let mut registry = registry();
        registry
            .replace(
                owner.into(),
                vec![Claim::exclusive(
                    NamePattern::Exact(hostname.clone()),
                    owner.into(),
                    "machine".into(),
                )],
                Some(address.parse().unwrap()),
            )
            .unwrap();
        let machine = GuestWebService {
            route: MachineWebService {
                machine_name: "dev".into(),
                hostname: hostname.to_string(),
                ipv4_address: address.into(),
                http_port: 3000,
                https_backend: Some(machine_web_service::HttpsBackend::TerminatedHttpPort(3000)),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let index = IndexEndpoint {
            apex: &"arcbox.local".parse().unwrap(),
            gateway: "172.17.0.1".parse().unwrap(),
        };
        let container = GuestWebService {
            route: route().into(),
            ..Default::default()
        };
        assert_ne!(
            selected_services(vec![container.clone()], None, &registry, &index).unwrap(),
            Vec::<WebService>::new()
        );
        for (routes, requested) in [
            (vec![machine.clone()], None),
            (vec![machine.clone()], Some(&hostname)),
            (vec![container, machine], None),
        ] {
            let error = selected_services(routes, requested, &registry, &index).unwrap_err();
            assert!(
                matches!(error, CoreError::Vm(message) if message == "System VM reported a machine web route")
            );
        }
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
            .await
            .unwrap();
        let name = "web.one.dev.arcbox.local";
        let first = dns_revision(&runtime, name).unwrap();
        for name in [
            "web.arcbox.local",
            "web.two.dev.arcbox.local",
            "web.one.dev.arcbox.local.evil",
        ] {
            assert_eq!(dns_revision(&runtime, name), None, "{name}");
        }
        runtime
            .register_dns("container", &["web".into()], "10.0.2.3".parse().unwrap())
            .await
            .unwrap();
        let renewed = dns_revision(&runtime, name).unwrap();
        assert_eq!(renewed, first);
        runtime.deregister_dns_by_id("container").await;
        assert_eq!(dns_revision(&runtime, name), None);
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
        let services =
            selected_container_services(vec![route.clone()], Some(&name), &registry).unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].https_url, "https://first.api.local");
        assert_eq!(services[0].https_mode, HttpsMode::Managed);
        assert!(
            selected_container_services(vec![route.clone(), route.clone()], Some(&name), &registry)
                .is_err()
        );
        for hostnames in [
            vec![],
            vec!["other.api.local".into()],
            vec![name.to_string(), "other.api.local".into()],
        ] {
            let mut tampered = route.clone();
            tampered.hostnames = hostnames;
            assert!(selected_container_services(vec![tampered], Some(&name), &registry).is_err());
        }
        for (owner, address) in [("other", "10.0.2.3"), ("abc123", "10.0.2.4")] {
            let mut stale = route.clone();
            stale.container_id = owner.into();
            stale.ipv4_address = address.into();
            assert_eq!(
                selected_container_services(vec![stale], Some(&name), &registry).unwrap(),
                []
            );
        }
    }

    #[tokio::test]
    async fn index_signing_and_observation_require_the_exact_published_endpoint() {
        use arcbox_dns::{
            NamePattern,
            claims::Claim,
            registry::{InstanceAuthority, InstanceTarget},
        };
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        let apex = runtime.config.network.dns_domain.clone();
        let gateway = runtime.config.container.cidr.docker_bridge_gateway();
        let claim = Claim::exclusive(
            NamePattern::Exact(apex.clone()),
            "system:index".into(),
            "index".into(),
        );
        runtime.replace_dns_registration("system:index", vec![claim.clone()], Some(gateway.into()));
        assert_eq!(dns_revision(&runtime, apex.as_str()), None);
        assert_eq!(runtime.web_service_dns_address(apex.as_str()), None);
        runtime
            .dns_registry
            .write()
            .unwrap()
            .replace_instance_authorities(vec![InstanceAuthority {
                root: apex.clone(),
                target: InstanceTarget::Current,
            }]);
        let revision = dns_revision(&runtime, apex.as_str()).unwrap();
        assert_eq!(
            runtime.web_service_dns_address(apex.as_str()),
            Some(gateway.into())
        );
        runtime.replace_dns_registration("system:index", vec![claim.clone()], Some(gateway.into()));
        assert_eq!(dns_revision(&runtime, apex.as_str()), Some(revision));
        for name in [
            "host.arcbox.local",
            "other.arcbox.local",
            "child.arcbox.local",
        ] {
            assert_eq!(dns_revision(&runtime, name), None);
            assert_eq!(runtime.web_service_dns_address(name), None);
        }
        let extra = Claim::exclusive(
            NamePattern::Exact("extra.local".parse().unwrap()),
            "system:index".into(),
            "index".into(),
        );
        for (claims, address) in [
            (vec![claim.clone()], Some("172.17.0.2".parse().unwrap())),
            (vec![claim.clone(), extra], Some(gateway.into())),
            (vec![claim.clone()], None),
        ] {
            runtime.replace_dns_registration("system:index", claims, address);
            assert_eq!(dns_revision(&runtime, apex.as_str()), None);
            assert_eq!(runtime.web_service_dns_address(apex.as_str()), None);
        }
        runtime.replace_dns_registration("system:index", vec![claim], Some(gateway.into()));
        assert_ne!(dns_revision(&runtime, apex.as_str()).unwrap(), revision);
        runtime
            .dns_registry
            .write()
            .unwrap()
            .invalidate_instance_authorities();
        assert_eq!(dns_revision(&runtime, apex.as_str()), None);
        assert_eq!(runtime.web_service_dns_address(apex.as_str()), None);
    }

    #[test]
    fn index_urls_require_the_current_builtin_owner_and_configured_endpoint() {
        use arcbox_dns::claims::Claim;
        use arcbox_dns::registry::{InstanceAuthority, InstanceTarget};
        let apex: Domain = "one.arcbox.local".parse().unwrap();
        let gateway = "172.17.0.1".parse().unwrap();
        let index = IndexEndpoint {
            apex: &apex,
            gateway,
        };
        let mut registry = Registry::with_builtin_names(&apex).unwrap();
        let claim = Claim::exclusive(
            arcbox_dns::NamePattern::Exact(apex.clone()),
            "system:index".into(),
            "index".into(),
        );
        let route = IndexWebService {
            hostname: apex.to_string(),
            ipv4_address: gateway.to_string(),
            managed_https: true,
            ..Default::default()
        };
        assert_eq!(index.project(route.clone(), &registry).unwrap(), []);
        registry
            .replace(
                "system:index".into(),
                vec![claim.clone()],
                Some(gateway.into()),
            )
            .unwrap();
        assert_eq!(index.project(route.clone(), &registry).unwrap(), []);
        registry.replace_instance_authorities(vec![InstanceAuthority {
            root: apex.clone(),
            target: InstanceTarget::Current,
        }]);
        let service = index
            .project(route.clone(), &registry)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(service.owner, "system:index");
        assert_eq!(service.domains, [apex.as_str()]);
        assert_eq!(service.http_url, "http://one.arcbox.local");
        assert_eq!(service.https_url, "https://one.arcbox.local");
        assert_eq!(service.https_mode, HttpsMode::Managed);
        assert_eq!((service.http_port, service.https_port), (0, 0));
        let guest_route = GuestWebService {
            route: route.clone().into(),
            ..Default::default()
        };
        assert!(
            selected_services(
                vec![guest_route],
                Some(&"else.arcbox.local".parse().unwrap()),
                &registry,
                &index,
            )
            .is_err()
        );
        for target in [
            InstanceTarget::Unavailable,
            InstanceTarget::Other { port: 5353 },
        ] {
            registry.replace_instance_authorities(vec![InstanceAuthority {
                root: apex.clone(),
                target,
            }]);
            assert_eq!(index.project(route.clone(), &registry).unwrap(), []);
        }
        registry.replace_instance_authorities(vec![InstanceAuthority {
            root: apex.clone(),
            target: InstanceTarget::Current,
        }]);
        registry
            .replace(
                "system:index".into(),
                vec![claim],
                Some("172.17.0.2".parse().unwrap()),
            )
            .unwrap();
        assert_eq!(index.project(route.clone(), &registry).unwrap(), []);
        for (hostname, address) in [
            ("else.arcbox.local", "172.17.0.1"),
            ("one.arcbox.local", "172.17.0.2"),
            ("one.arcbox.local", "invalid"),
        ] {
            let invalid = IndexWebService {
                hostname: hostname.into(),
                ipv4_address: address.into(),
                ..route.clone()
            };
            assert!(index.project(invalid, &registry).is_err());
        }
    }

    #[test]
    fn index_projection_rejects_container_owners_and_wildcard_claims() {
        use arcbox_dns::NamePattern;
        use arcbox_dns::claims::Claim;
        use arcbox_dns::registry::{InstanceAuthority, InstanceTarget};
        let apex: Domain = "one.arcbox.local".parse().unwrap();
        let gateway = "172.17.0.1".parse().unwrap();
        let index = IndexEndpoint {
            apex: &apex,
            gateway,
        };
        for (owner, authority, pattern) in [
            ("container:a", "index", NamePattern::Exact(apex.clone())),
            (
                "system:index",
                "container",
                NamePattern::Exact(apex.clone()),
            ),
            (
                "system:index",
                "index",
                NamePattern::Descendants("arcbox.local".parse().unwrap()),
            ),
        ] {
            let mut registry = Registry::default();
            registry.replace_instance_authorities(vec![InstanceAuthority {
                root: apex.clone(),
                target: InstanceTarget::Current,
            }]);
            registry
                .replace(
                    owner.into(),
                    vec![Claim::exclusive(pattern, owner.into(), authority.into())],
                    Some(gateway.into()),
                )
                .unwrap();
            assert!(matches!(
                registry.resolve(&apex),
                Lookup::Matched {
                    publishable: true,
                    ..
                }
            ));
            let route = IndexWebService {
                hostname: apex.to_string(),
                ipv4_address: gateway.to_string(),
                managed_https: true,
                ..Default::default()
            };
            assert_eq!(index.project(route, &registry).unwrap(), []);
        }
    }

    #[test]
    fn index_projection_preserves_http_when_managed_https_is_unavailable() {
        use arcbox_dns::claims::Claim;
        use arcbox_dns::registry::{InstanceAuthority, InstanceTarget};
        for (hostname, managed, https) in [("arcbox.local", false, ""), ("arcbox.test", true, "")] {
            let apex: Domain = hostname.parse().unwrap();
            let gateway = "172.17.0.1".parse().unwrap();
            let index = IndexEndpoint {
                apex: &apex,
                gateway,
            };
            let mut registry = Registry::with_builtin_names(&apex).unwrap();
            registry.replace_instance_authorities(vec![InstanceAuthority {
                root: apex.clone(),
                target: InstanceTarget::Current,
            }]);
            registry
                .replace(
                    "system:index".into(),
                    vec![Claim::exclusive(
                        arcbox_dns::NamePattern::Exact(apex.clone()),
                        "system:index".into(),
                        "index".into(),
                    )],
                    Some(gateway.into()),
                )
                .unwrap();
            let route = IndexWebService {
                hostname: hostname.into(),
                ipv4_address: gateway.to_string(),
                managed_https: managed,
                ..Default::default()
            };
            let services = index.project(route, &registry).unwrap();
            assert_eq!(services.len(), 1);
            assert_eq!(services[0].http_url, format!("http://{hostname}"));
            assert_eq!(services[0].https_url, https);
            assert_eq!(services[0].https_mode, HttpsMode::None);
        }
    }
}
