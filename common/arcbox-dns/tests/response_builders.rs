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
fn unknown_qtypes_preserve_names_and_questions_in_empty_responses() {
    for qtype in [64, 65, 65_280] {
        let q = query("Web.arcbox.local", qtype);
        let parsed = arcbox_dns::DnsQuery::parse(&q).unwrap();
        assert_eq!(parsed.name, "Web.arcbox.local");
        assert_eq!(parsed.qtype, arcbox_dns::DnsRecordType::Unknown(qtype));
        assert_eq!(u16::from(parsed.qtype), qtype);
        assert_eq!(parsed.raw_question, q[12..]);
        for (response, rcode) in [
            (arcbox_dns::build_nodata(&q).unwrap(), 0),
            (arcbox_dns::build_servfail(&q).unwrap(), 2),
            (arcbox_dns::build_nxdomain(&q).unwrap(), 3),
        ] {
            assert_eq!(&response[..2], &q[..2]);
            assert_ne!(response[2] & 0x80, 0);
            assert_eq!(response[3] & 0x0f, rcode);
            assert_eq!(&response[4..6], &[0, 1]);
            assert_eq!(&response[6..12], &[0; 6]);
            assert_eq!(&response[12..], &q[12..]);
        }
    }
}

#[test]
fn parse_rejects_truncated_packet() {
    assert!(arcbox_dns::DnsQuery::parse(&[0; 4]).is_err());
}

#[test]
fn parse_rejects_empty() {
    assert!(arcbox_dns::DnsQuery::parse(&[]).is_err());
}
