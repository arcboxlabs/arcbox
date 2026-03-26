//! Integration test: shared DNS hosts table between NetworkManager and DnsForwarder.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use arcbox_dns::LocalHostsTable;
use arcbox_net::dns::{DnsConfig, DnsForwarder};

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
fn shared_table_visible_to_both_forwarders() {
    // Simulate: NetworkManager and VMM datapath share the same LocalHostsTable.
    let shared = Arc::new(LocalHostsTable::new(Default::default()));

    let config1 = DnsConfig::default();
    let config2 = DnsConfig::default();

    // Host-side forwarder (like NetworkManager's).
    let forwarder1 = DnsForwarder::with_shared_hosts(config1, Arc::clone(&shared));
    // VMM-side forwarder (like datapath's).
    let forwarder2 = DnsForwarder::with_shared_hosts(config2, Arc::clone(&shared));

    // Register via forwarder1 (simulates runtime.register_dns()).
    let ip = IpAddr::V4(Ipv4Addr::new(172, 17, 0, 2));
    forwarder1.add_local_host("my-nginx", ip);

    // Both forwarders should resolve it.
    assert_eq!(forwarder1.resolve_local("my-nginx"), Some(ip));
    assert_eq!(forwarder2.resolve_local("my-nginx"), Some(ip));

    // FQDN should also work.
    assert_eq!(forwarder1.resolve_local("my-nginx.arcbox.local"), Some(ip));
    assert_eq!(forwarder2.resolve_local("my-nginx.arcbox.local"), Some(ip));

    // Forwarder2 should return a DNS response for a query.
    let query = build_query("my-nginx.arcbox.local");
    let resp = forwarder2.try_resolve_locally(&query);
    assert!(
        resp.is_some(),
        "VMM forwarder should resolve from shared table"
    );

    // NXDOMAIN for unregistered local name.
    let query = build_query("missing.arcbox.local");
    let resp = forwarder2.try_resolve_locally_or_nxdomain(&query);
    assert!(resp.is_some());
    // Check RCODE=3 (NXDOMAIN).
    let r = resp.unwrap();
    assert_eq!(r[3] & 0x0F, 3, "should be NXDOMAIN");

    // Remove via forwarder1, both should see it gone.
    forwarder1.remove_local_host("my-nginx");
    assert_eq!(forwarder1.resolve_local("my-nginx"), None);
    assert_eq!(forwarder2.resolve_local("my-nginx"), None);
}

#[test]
fn separate_tables_are_independent() {
    let config1 = DnsConfig::default();
    let config2 = DnsConfig::default();

    // Two forwarders with separate tables (not shared).
    let f1 = DnsForwarder::new(config1);
    let f2 = DnsForwarder::new(config2);

    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    f1.add_local_host("only-in-f1", ip);

    assert_eq!(f1.resolve_local("only-in-f1"), Some(ip));
    assert_eq!(f2.resolve_local("only-in-f1"), None);
}
