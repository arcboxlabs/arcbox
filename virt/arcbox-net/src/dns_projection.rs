//! Concrete DNS records projected from current resource claims for host and mDNS consumers.

use std::collections::HashMap;

use arcbox_dns::{
    NamePattern,
    registry::{Lookup, Registry},
};

use crate::{DnsChange, NetworkManager};

impl NetworkManager {
    /// Replaces concrete DNS records with the registry's selected exact claims.
    ///
    /// Hold the registry mutation lock through this call. Wildcards remain in the registry;
    /// conflicts and unavailable owners produce no concrete record. Names are already canonical
    /// and never receive another suffix. mDNS changes cover only the configured local domain.
    pub fn replace_dns_projection(&self, registry: &Registry) {
        let mut projected = HashMap::new();
        for owner in registry.owners() {
            let registration = registry.get(owner).expect("registry owners are registered");
            for candidate in &registration.claims {
                let NamePattern::Exact(name) = candidate.pattern() else {
                    continue;
                };
                if let Lookup::Matched { claim, address, .. } = registry.resolve(name)
                    && matches!(claim.pattern(), NamePattern::Exact(_))
                {
                    projected.insert(name.as_str().to_owned(), address);
                }
            }
        }

        let forwarder = &self.dns_forwarder;
        let suffix = forwarder
            .config()
            .local_domain
            .as_ref()
            .map(|domain| format!(".{domain}"));
        let announced = |name: &str| suffix.as_ref().is_some_and(|suffix| name.ends_with(suffix));
        let table = forwarder.local_hosts_table();
        let mut current = table.write().expect("local DNS hosts lock poisoned");
        let previous = std::mem::replace(&mut *current, projected);
        for name in previous.keys() {
            if announced(name) && !current.contains_key(name) {
                let _ = self
                    .dns_changes
                    .send(DnsChange::Removed { fqdn: name.clone() });
            }
        }
        for (name, ip) in current.iter() {
            if announced(name) && previous.get(name) != Some(ip) {
                let _ = self.dns_changes.send(DnsChange::Registered {
                    fqdn: name.clone(),
                    ip: *ip,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use arcbox_dns::claims::{Boundary, Claim};

    use super::*;
    use crate::NetConfig;

    fn claim(owner: &str, name: &str) -> Claim {
        Claim::exclusive(
            NamePattern::parse_custom(name).unwrap(),
            owner.into(),
            "containers".into(),
        )
    }

    fn ip(last: u8) -> IpAddr {
        std::net::Ipv4Addr::new(10, 0, 2, last).into()
    }

    #[test]
    fn projection_publishes_only_selected_exact_names_without_suffix_expansion() {
        let manager = NetworkManager::new(NetConfig::default());
        let mut registry = Registry::new(vec![
            Boundary::ReservedName {
                name: "admin.arcbox.local".parse().unwrap(),
                authority: "system".into(),
            },
            Boundary::DelegatedTree {
                root: "remote.arcbox.local".parse().unwrap(),
                instance: "remote".into(),
            },
        ]);
        registry
            .replace(
                "first".into(),
                vec![
                    claim("first", "web.arcbox.local"),
                    claim("first", "site.example.local"),
                    claim("first", "*.wild.arcbox.local"),
                    claim("first", "conflict.arcbox.local"),
                    claim("first", "admin.arcbox.local"),
                    claim("first", "remote.arcbox.local"),
                ],
                Some(ip(2)),
            )
            .unwrap();
        registry
            .replace(
                "second".into(),
                vec![claim("second", "conflict.arcbox.local")],
                Some(ip(3)),
            )
            .unwrap();
        registry
            .replace(
                "offline".into(),
                vec![
                    claim("offline", "offline.arcbox.local"),
                    claim("offline", "offline.wild.arcbox.local"),
                ],
                None,
            )
            .unwrap();

        manager.replace_dns_projection(&registry);

        assert_eq!(
            *manager.local_hosts_table().read().unwrap(),
            HashMap::from([
                ("web.arcbox.local".into(), ip(2)),
                ("site.example.local".into(), ip(2)),
            ])
        );
        assert_eq!(
            manager.local_dns_entries(),
            vec![("web.arcbox.local".into(), ip(2))]
        );
    }

    #[test]
    fn projection_emits_only_effective_changes_and_clears_unavailable_records() {
        let manager = NetworkManager::new(NetConfig::default());
        let mut changes = manager.subscribe_dns_changes();
        let mut registry = Registry::default();
        let claims = vec![
            claim("web", "web.arcbox.local"),
            claim("web", "web.example.local"),
        ];
        registry
            .replace("web".into(), claims.clone(), Some(ip(2)))
            .unwrap();
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: "web.arcbox.local".into(),
                ip: ip(2)
            }
        );
        assert!(changes.try_recv().is_err());

        manager.replace_dns_projection(&registry);
        assert!(
            changes.try_recv().is_err(),
            "an unchanged snapshot must not reannounce records"
        );

        registry
            .replace("web".into(), claims.clone(), Some(ip(3)))
            .unwrap();
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: "web.arcbox.local".into(),
                ip: ip(3)
            }
        );

        registry.replace("web".into(), claims, None).unwrap();
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Removed {
                fqdn: "web.arcbox.local".into()
            }
        );
        assert!(manager.local_hosts_table().read().unwrap().is_empty());
        assert_eq!(manager.local_dns_entries(), [] as [(String, IpAddr); 0]);
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn projection_follows_the_selected_replica_and_withdraws_conflicts() {
        let manager = NetworkManager::new(NetConfig::default());
        let mut changes = manager.subscribe_dns_changes();
        let mut registry = Registry::default();
        let name = "web.project.arcbox.local";
        for (owner, address) in [("z", ip(3)), ("a", ip(2))] {
            let claims = Claim::compose_service(
                name.parse().unwrap(),
                owner.into(),
                "containers".into(),
                ("project".into(), "web".into()),
            );
            registry
                .replace(owner.into(), claims.into(), Some(address))
                .unwrap();
        }
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: name.into(),
                ip: ip(2)
            }
        );

        registry.remove("a");
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: name.into(),
                ip: ip(3)
            }
        );

        registry
            .replace(
                "exclusive".into(),
                vec![claim("exclusive", name)],
                Some(ip(4)),
            )
            .unwrap();
        manager.replace_dns_projection(&registry);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Removed { fqdn: name.into() }
        );
        assert_eq!(manager.local_dns_entries(), [] as [(String, IpAddr); 0]);

        registry.remove("exclusive");
        manager.replace_dns_projection(&registry);
        assert_eq!(manager.local_dns_entries(), vec![(name.into(), ip(3))]);
        assert_eq!(
            changes.try_recv().unwrap(),
            DnsChange::Registered {
                fqdn: name.into(),
                ip: ip(3)
            }
        );
        assert!(changes.try_recv().is_err());
    }
}
