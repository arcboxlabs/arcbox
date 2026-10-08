//! Concrete DNS projection and complete Registry change notification for host consumers.

use std::collections::HashMap;

use arcbox_dns::{
    NamePattern,
    registry::{Lookup, Registry},
};

use crate::{DnsChange, NetworkManager};

impl NetworkManager {
    /// Replaces mDNS records with the registry's granted exact `.local` claims.
    ///
    /// Hold the registry mutation lock through this call. Wildcards remain in the registry;
    /// conflicts and unavailable owners produce no concrete record. Names are already canonical
    /// and never receive another suffix. The configured suffix does not limit custom names.
    /// The latest Registry epoch is published even when no concrete records change.
    pub fn replace_dns_projection(&self, registry: &Registry) {
        let mut projected = HashMap::new();
        for owner in registry.owners() {
            let registration = registry.get(owner).expect("registry owners are registered");
            for candidate in &registration.claims {
                let NamePattern::Exact(name) = candidate.pattern() else {
                    continue;
                };
                if name
                    .as_str()
                    .rsplit_once('.')
                    .is_none_or(|(_, tld)| tld != "local")
                {
                    continue;
                }
                if let Lookup::Matched {
                    claim,
                    address,
                    publishable: true,
                    ..
                } = registry.resolve(name)
                    && matches!(claim.pattern(), NamePattern::Exact(_))
                {
                    projected.insert(name.as_str().to_owned(), address);
                }
            }
        }

        let forwarder = &self.dns_forwarder;
        let table = forwarder.local_hosts_table();
        let mut current = table.write().expect("local DNS hosts lock poisoned");
        let previous = std::mem::replace(&mut *current, projected);
        for name in previous.keys() {
            if !current.contains_key(name) {
                let _ = self
                    .dns_changes
                    .send(DnsChange::Removed { fqdn: name.clone() });
            }
        }
        for (name, ip) in current.iter() {
            if previous.get(name) != Some(ip) {
                let _ = self.dns_changes.send(DnsChange::Registered {
                    fqdn: name.clone(),
                    ip: *ip,
                });
            }
        }
        self.dns_registry_epoch.send_if_modified(|epoch| {
            if *epoch == registry.epoch() {
                return false;
            }
            *epoch = registry.epoch();
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use arcbox_dns::{
        claims::{Boundary, Claim},
        registry::{InstanceAuthority, InstanceTarget},
    };

    use super::*;
    use crate::NetConfig;

    fn claim(owner: &str, name: &str) -> Claim {
        let pattern = match name.strip_prefix("*.") {
            Some(root) => NamePattern::Descendants(root.parse().unwrap()),
            None => NamePattern::Exact(name.parse().unwrap()),
        };
        Claim::exclusive(pattern, owner.into(), "containers".into())
    }

    fn ip(last: u8) -> IpAddr {
        std::net::Ipv4Addr::new(10, 0, 2, last).into()
    }

    fn grant(registry: &mut Registry, roots: &[&str]) {
        registry.replace_instance_authorities(
            roots
                .iter()
                .map(|root| InstanceAuthority {
                    root: root.parse().unwrap(),
                    target: InstanceTarget::Current,
                })
                .collect(),
        );
    }

    #[test]
    fn registry_watch_includes_changes_without_concrete_records() {
        let manager = NetworkManager::new(NetConfig::default());
        let registry = manager.dns_registry();
        let mut registry = registry.write().unwrap();
        let mut epochs = manager.subscribe_dns_registry();
        let mut records = manager.subscribe_dns_changes();
        for (name, address) in [
            ("*.custom.local", None),
            ("*.custom.local", Some(ip(2))),
            ("*.custom.local", None),
            ("exact.custom.local", None),
            ("exact.custom.local", Some(ip(2))),
        ] {
            let claims = vec![claim("resource", name)];
            let revision = registry
                .replace("resource".into(), claims.clone(), address)
                .unwrap();
            manager.replace_dns_projection(&registry);
            assert!(epochs.has_changed().unwrap());
            assert_eq!(*epochs.borrow_and_update(), revision);
            assert!(records.try_recv().is_err());

            registry
                .replace("resource".into(), claims, address)
                .unwrap();
            manager.replace_dns_projection(&registry);
            assert!(!epochs.has_changed().unwrap());
        }

        drop(epochs);
        registry.remove("resource");
        manager.replace_dns_projection(&registry);
        let epochs = manager.subscribe_dns_registry();
        assert_eq!(*epochs.borrow(), registry.epoch());
        assert!(manager.local_hosts_table().read().unwrap().is_empty());
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
        grant(&mut registry, &["arcbox.local", "example.local"]);
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
            manager
                .local_dns_entries()
                .into_iter()
                .collect::<HashMap<_, _>>(),
            HashMap::from([
                ("web.arcbox.local".into(), ip(2)),
                ("site.example.local".into(), ip(2))
            ])
        );
    }

    #[test]
    fn projection_emits_only_effective_changes_and_clears_unavailable_records() {
        let manager = NetworkManager::new(NetConfig::default());
        let mut changes = manager.subscribe_dns_changes();
        let mut registry = Registry::default();
        grant(&mut registry, &["arcbox.local"]);
        let claims = vec![
            claim("web", "web.arcbox.local"),
            claim("web", "web.example.test"),
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
        grant(&mut registry, &["arcbox.local"]);
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

    #[test]
    fn grants_control_custom_local_names_and_apex_independently_of_configured_suffix() {
        let manager = NetworkManager::new(NetConfig {
            dns: crate::dns::DnsConfig::default().with_local_domain("containers.test"),
            ..Default::default()
        });
        let mut registry = Registry::default();
        let claims = [
            "custom.local",
            "deep.custom.local",
            "*.wild.local",
            "web.containers.test",
        ]
        .into_iter()
        .map(|name| claim("web", name))
        .collect();
        registry.replace("web".into(), claims, Some(ip(2))).unwrap();
        let mut changes = manager.subscribe_dns_changes();
        manager.replace_dns_projection(&registry);
        assert_eq!(manager.local_dns_entries(), [] as [(String, IpAddr); 0]);
        assert!(changes.try_recv().is_err());

        for target in [
            InstanceTarget::Current,
            InstanceTarget::Other { port: 5554 },
            InstanceTarget::Unavailable,
            InstanceTarget::Current,
        ] {
            registry.replace_instance_authorities(vec![
                InstanceAuthority {
                    root: "containers.test".parse().unwrap(),
                    target: InstanceTarget::Current,
                },
                InstanceAuthority {
                    root: "custom.local".parse().unwrap(),
                    target,
                },
                InstanceAuthority {
                    root: "wild.local".parse().unwrap(),
                    target: InstanceTarget::Current,
                },
            ]);
            manager.replace_dns_projection(&registry);
            let mut events = Vec::new();
            while let Ok(event) = changes.try_recv() {
                events.push(event);
            }
            let expected = if target == InstanceTarget::Current {
                vec![
                    ("custom.local".into(), ip(2)),
                    ("deep.custom.local".into(), ip(2)),
                ]
            } else {
                vec![]
            };
            let mut actual = manager.local_dns_entries();
            actual.sort();
            assert_eq!(actual, expected);
            assert_eq!(
                events.len(),
                if target == InstanceTarget::Unavailable {
                    0
                } else {
                    2
                }
            );
            for event in events {
                assert_eq!(
                    matches!(event, DnsChange::Registered { .. }),
                    target == InstanceTarget::Current
                );
            }
        }
        grant(&mut registry, &[]);
        manager.replace_dns_projection(&registry);
        assert_eq!(manager.local_dns_entries(), [] as [(String, IpAddr); 0]);
        assert!(matches!(
            changes.try_recv().unwrap(),
            DnsChange::Removed { .. }
        ));
        assert!(matches!(
            changes.try_recv().unwrap(),
            DnsChange::Removed { .. }
        ));
        assert!(changes.try_recv().is_err());
    }
}
