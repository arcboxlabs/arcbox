//! Integration tests for DNS response builders.

use std::net::Ipv4Addr;

/// Builds a DNS query packet.
fn query(name: &str, qtype: u16) -> Vec<u8> {
    let mut pkt = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00];
    pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    for label in name.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0x00);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
    pkt
}

#[test]
fn a_record_response() {
    let q = query("test.arcbox.local", 1); // A record
    let resp = arcbox_dns::build_response_a(&q, Ipv4Addr::new(172, 17, 0, 2), 300).unwrap();

    // ID preserved.
    assert_eq!(resp[0], 0x12);
    assert_eq!(resp[1], 0x34);
    // QR=1, RCODE=0.
    assert_ne!(resp[2] & 0x80, 0);
    assert_eq!(resp[3] & 0x0F, 0);
    // ANCOUNT=1.
    assert_eq!(resp[7], 1);
    // Answer contains the IP.
    assert!(resp.windows(4).any(|w| w == [172, 17, 0, 2]));
}

#[test]
fn nxdomain_clears_all_counts() {
    let q = query("missing.arcbox.local", 1);
    let resp = arcbox_dns::build_nxdomain(&q).unwrap();

    assert_eq!(resp[3] & 0x0F, 3); // RCODE=NXDOMAIN
    assert_eq!(resp[7], 0); // ANCOUNT=0
    assert_eq!(resp[9], 0); // NSCOUNT=0
    assert_eq!(resp[11], 0); // ARCOUNT=0
}

#[test]
fn servfail_response() {
    let q = query("fail.example.com", 1);
    let resp = arcbox_dns::build_servfail(&q).unwrap();
    assert_eq!(resp[3] & 0x0F, 2); // RCODE=SERVFAIL
}

#[test]
fn unsupported_qtype_returns_error() {
    // HTTPS record (type 65) is not in our enum.
    let q = query("example.com", 65);
    let result = arcbox_dns::DnsQuery::parse(&q);
    assert!(
        result.is_err(),
        "unsupported type should error so caller can forward"
    );
}

#[test]
fn parse_rejects_truncated_packet() {
    assert!(arcbox_dns::DnsQuery::parse(&[0; 4]).is_err());
}

#[test]
fn parse_rejects_empty() {
    assert!(arcbox_dns::DnsQuery::parse(&[]).is_err());
}
