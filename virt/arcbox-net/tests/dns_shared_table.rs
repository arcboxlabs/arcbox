//! Host and VM datapaths resolve against the same live DNS registry.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, RwLock};

use arcbox_dns::{
    NamePattern,
    claims::{Boundary, Claim},
    registry::Registry,
};
use arcbox_net::dns::{DnsConfig, DnsForwarder};
use arcbox_net::{NetConfig, NetworkManager};

/// Builds a minimal DNS A query for testing.
fn build_query(name: &str) -> Vec<u8> {
    let mut pkt = vec![0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00];
    pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    for label in name.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0x00);
    pkt.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A, IN
    pkt
}

#[test]
fn shared_registry_visible_to_both_forwarders() {
    let manager = NetworkManager::new(NetConfig {
        dns: DnsConfig::default().with_local_domain("containers.test"),
        ..Default::default()
    });
    let dns = manager.shared_dns();
    let shared = dns.registry;
    let config1 = manager.config().dns.clone();
    let config2 = DnsConfig {
        local_domain: dns.local_domain,
        ..Default::default()
    };

    // Host-side forwarder (like NetworkManager's).
    let forwarder1 = DnsForwarder::with_shared_registry(config1, Arc::clone(&shared));
    // VMM-side forwarder (like datapath's).
    let forwarder2 = DnsForwarder::with_shared_registry(config2, Arc::clone(&shared));

    let ip = IpAddr::V4(Ipv4Addr::new(172, 17, 0, 2));
    let claims = Claim::generated(
        "my-nginx.containers.test".parse().unwrap(),
        "nginx".into(),
        "container".into(),
    );
    shared
        .write()
        .unwrap()
        .replace("nginx".into(), claims.into(), Some(ip))
        .unwrap();

    // Both forwarders should resolve it.
    assert_eq!(forwarder1.resolve_local("my-nginx"), Some(ip));
    assert_eq!(forwarder2.resolve_local("my-nginx"), Some(ip));

    // FQDN should also work.
    assert_eq!(
        forwarder1.resolve_local("my-nginx.containers.test"),
        Some(ip)
    );
    assert_eq!(
        forwarder2.resolve_local("my-nginx.containers.test"),
        Some(ip)
    );
    assert_eq!(
        forwarder2.resolve_local("deep.my-nginx.containers.test"),
        Some(ip)
    );
    assert_eq!(forwarder2.resolve_local("deep.my-nginx"), None);

    // Forwarder2 should return a DNS response for a query.
    let query = build_query("my-nginx.containers.test");
    let resp = forwarder2.try_resolve_locally(&query);
    assert!(
        resp.is_some(),
        "VMM forwarder should resolve from shared registry"
    );

    // NODATA for an unregistered local name.
    let query = build_query("missing.containers.test");
    let resp = forwarder2.try_resolve_locally_or_nodata(&query);
    assert!(resp.is_some());
    let r = resp.unwrap();
    assert_eq!(r[3] & 0x0F, 0, "should be NOERROR");
    assert_eq!(r[7], 0, "should carry no answer");

    shared.write().unwrap().remove("nginx");
    assert_eq!(forwarder1.resolve_local("my-nginx"), None);
    assert_eq!(forwarder2.resolve_local("my-nginx"), None);
}

#[test]
fn ownership_boundaries_stop_projection_and_suffix_fallback() {
    let mut registry = Registry::new(vec![
        Boundary::ReservedName {
            name: "reserved.arcbox.local".parse().unwrap(),
            authority: "system".into(),
        },
        Boundary::DelegatedTree {
            root: "remote.arcbox.local".parse().unwrap(),
            instance: "other".into(),
        },
    ]);
    let ip = IpAddr::V4(Ipv4Addr::new(172, 17, 0, 2));
    for (owner, names) in [
        (
            "a",
            vec![
                "*.arcbox.local",
                "api.arcbox.local",
                "conflict.arcbox.local",
            ],
        ),
        ("b", vec!["conflict.arcbox.local"]),
    ] {
        let claims = names
            .into_iter()
            .map(|name| {
                Claim::exclusive(
                    NamePattern::parse_custom(name).unwrap(),
                    owner.into(),
                    "container".into(),
                )
            })
            .collect();
        registry.replace(owner.into(), claims, Some(ip)).unwrap();
    }
    let shared = Arc::new(RwLock::new(registry));
    let forwarder = DnsForwarder::with_shared_registry(
        DnsConfig::default().with_upstream(vec![]),
        Arc::clone(&shared),
    );
    forwarder
        .local_hosts_table()
        .write()
        .unwrap()
        .insert("conflict.arcbox.local".into(), ip);
    assert_eq!(forwarder.resolve_local("API.ARCBOX.LOCAL"), Some(ip));
    assert_eq!(forwarder.resolve_local("a.b.arcbox.local"), Some(ip));
    assert_eq!(forwarder.resolve_local("example.com"), None);
    for qtype in [1_u16, 28, 64, 65, 65_280] {
        for (name, code) in [("conflict.arcbox.local", 2), ("reserved.arcbox.local", 3)] {
            let mut query = build_query(name);
            let offset = query.len() - 4;
            query[offset..offset + 2].copy_from_slice(&qtype.to_be_bytes());
            let reply = forwarder.try_resolve_locally(&query).unwrap();
            assert_eq!(reply[3] & 15, code, "{name}/{qtype}");
            assert_eq!(&reply[6..12], &[0; 6]);
            let authoritative = forwarder.try_resolve_locally_or_nodata(&query).unwrap();
            assert_eq!(authoritative[3] & 15, if code == 3 { 0 } else { code });
            assert_eq!(&authoritative[6..12], &[0; 6]);
            assert_eq!(forwarder.handle_query(&query).unwrap(), reply);
        }
    }
    let delegated = build_query("remote.arcbox.local");
    assert!(forwarder.try_resolve_locally(&delegated).is_none());
    assert!(
        forwarder
            .try_resolve_locally_or_nodata(&delegated)
            .is_none()
    );
}

#[test]
fn separate_registries_are_independent() {
    let config1 = DnsConfig::default();
    let config2 = DnsConfig::default();

    let registry = Arc::new(RwLock::new(Registry::default()));
    let f1 = DnsForwarder::with_shared_registry(config1, Arc::clone(&registry));
    let f2 = DnsForwarder::new(config2);

    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let claims = Claim::generated(
        "only-in-f1.arcbox.local".parse().unwrap(),
        "one".into(),
        "container".into(),
    );
    registry
        .write()
        .unwrap()
        .replace("one".into(), claims.into(), Some(ip))
        .unwrap();

    assert_eq!(f1.resolve_local("only-in-f1"), Some(ip));
    assert_eq!(f2.resolve_local("only-in-f1"), None);
}
