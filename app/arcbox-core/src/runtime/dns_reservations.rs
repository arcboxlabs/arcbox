//! Built-in DNS names remain reserved independently of resource addresses.

use arcbox_constants::dns::Domain;
use arcbox_dns::registry::Registry;

use crate::error::{CoreError, Result};

pub(super) fn registry(domain: &Domain) -> Result<Registry> {
    Registry::with_builtin_names(domain).map_err(CoreError::config)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use arcbox_dns::{NamePattern, claims::Claim, registry::Lookup};
    use arcbox_net::dns::{DnsConfig, DnsForwarder};

    use super::*;
    use crate::{Config, Runtime};

    #[tokio::test]
    async fn runtime_reserves_system_names_before_resources_and_after_address_loss() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        };
        config.network.dns_domain = "dev.arcbox.local".parse().unwrap();
        let runtime = Runtime::new(config).unwrap();
        let manager = runtime.network_manager();
        let shared = manager.shared_dns();
        let forwarder = DnsForwarder::with_shared_registry(
            DnsConfig {
                local_domain: shared.local_domain,
                ..Default::default()
            },
            shared.registry,
        );
        let host_ip: IpAddr = "10.0.2.1".parse().unwrap();
        let container_ip: IpAddr = "172.17.0.2".parse().unwrap();
        let host_names =
            ["host", "host.docker.internal", "gateway.docker.internal"].map(str::to_owned);
        runtime
            .register_dns("container", &host_names, container_ip)
            .await;
        {
            let mut claims = runtime
                .dns_registry
                .read()
                .unwrap()
                .get("container")
                .unwrap()
                .claims
                .clone();
            for name in ["*.dev.arcbox.local", "dev.arcbox.local"] {
                claims.push(Claim::exclusive(
                    NamePattern::parse_custom(name).unwrap(),
                    "container".into(),
                    "container".into(),
                ));
            }
            runtime.replace_dns_registration("container", claims, Some(container_ip));
        }
        for name in [
            "host",
            "host.dev.arcbox.local",
            "host.docker.internal",
            "gateway.docker.internal",
            "dev.arcbox.local",
        ] {
            assert_eq!(forwarder.resolve_local(name), None, "{name}");
            assert!(matches!(
                runtime
                    .dns_registry
                    .read()
                    .unwrap()
                    .resolve(&name.parse().unwrap()),
                Lookup::Reserved(_)
            ));
        }
        assert_eq!(
            forwarder.resolve_local("other.dev.arcbox.local"),
            Some(container_ip)
        );
        assert_eq!(
            forwarder.resolve_local("child.host.dev.arcbox.local"),
            Some(container_ip)
        );

        runtime.register_host_dns(&host_names, host_ip).await;
        for name in [
            "host",
            "host.dev.arcbox.local",
            "host.docker.internal",
            "gateway.docker.internal",
        ] {
            assert_eq!(forwarder.resolve_local(name), Some(host_ip), "{name}");
        }
        assert_eq!(forwarder.resolve_local("dev.arcbox.local"), None);

        let host_claims = runtime
            .dns_registry
            .read()
            .unwrap()
            .get(super::super::HOST_DNS_OWNER)
            .unwrap()
            .claims
            .clone();
        runtime.replace_dns_registration(super::super::HOST_DNS_OWNER, host_claims, None);
        assert_eq!(forwarder.resolve_local("host.dev.arcbox.local"), None);
        assert!(matches!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .resolve(&"host.dev.arcbox.local".parse().unwrap()),
            Lookup::Reserved(_)
        ));
        let index = Claim::exclusive(
            NamePattern::Exact("dev.arcbox.local".parse().unwrap()),
            "system:index".into(),
            "index".into(),
        );
        runtime.replace_dns_registration("system:index", vec![index], Some(host_ip));
        assert_eq!(forwarder.resolve_local("dev.arcbox.local"), Some(host_ip));
    }

    #[test]
    fn suffix_must_not_replace_a_system_name() {
        for suffix in ["host", "host.docker.internal", "gateway.docker.internal"] {
            let error = registry(&suffix.parse().unwrap()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("conflicts with a built-in host name"),
                "{suffix}: {error}"
            );
        }
        assert!(registry(&"host.arcbox.local".parse().unwrap()).is_ok());
    }

    #[test]
    fn suffix_must_leave_room_for_the_host_alias() {
        let domain = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61),
        ]
        .join(".")
        .parse()
        .unwrap();
        assert!(registry(&domain).is_err());
    }
}
