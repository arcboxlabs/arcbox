//! DNS packet parsing and response building.
//!
//! Platform-independent, no-std-compatible (except `std::net` types).
//! Compiles under `aarch64-unknown-linux-musl` for use in both host and guest.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::RwLock;

/// Shared local-hosts table used by both host-side and VMM-side DNS forwarders.
pub type LocalHostsTable = RwLock<HashMap<String, IpAddr>>;

/// Default DNS port.
pub const DNS_PORT: u16 = 53;

/// Default response TTL (seconds).
pub const DEFAULT_TTL: u32 = 300;

// ── Errors ──────────────────────────────────────────────────────────────────

/// DNS processing errors.
#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("packet too short: need {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },

    #[error("invalid name at offset {offset}")]
    InvalidName { offset: usize },

    #[error("query truncated at offset {offset}")]
    Truncated { offset: usize },

    #[error("unsupported query type: {0}")]
    UnsupportedType(u16),

    #[error("unsupported query class: {0}")]
    UnsupportedClass(u16),
}

// ── Record types ────────────────────────────────────────────────────────────

/// DNS record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum DnsRecordType {
    A = 1,
    Cname = 5,
    Ptr = 12,
    Mx = 15,
    Txt = 16,
    Aaaa = 28,
    Srv = 33,
}

impl TryFrom<u16> for DnsRecordType {
    type Error = u16;

    fn try_from(value: u16) -> Result<Self, u16> {
        match value {
            1 => Ok(Self::A),
            5 => Ok(Self::Cname),
            12 => Ok(Self::Ptr),
            15 => Ok(Self::Mx),
            16 => Ok(Self::Txt),
            28 => Ok(Self::Aaaa),
            33 => Ok(Self::Srv),
            other => Err(other),
        }
    }
}

/// DNS response code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DnsResponseCode {
    NoError = 0,
    FormErr = 1,
    ServFail = 2,
    NxDomain = 3,
    NotImp = 4,
    Refused = 5,
}

// ── Parsed query ────────────────────────────────────────────────────────────

/// A parsed DNS query (question section only).
///
/// Preserves raw header and question bytes for fast response serialization
/// (copy header, flip flags, append answer).
#[derive(Debug)]
pub struct DnsQuery {
    /// Decoded query name (e.g. `"my-nginx.arcbox.local"`).
    pub name: String,
    /// Query type (A, AAAA, etc.).
    pub qtype: DnsRecordType,
    /// Raw 12-byte DNS header.
    pub raw_header: Vec<u8>,
    /// Raw question section (name + qtype + qclass), excluding the header.
    pub raw_question: Vec<u8>,
}

const HEADER_LEN: usize = 12;

impl DnsQuery {
    /// Parses a DNS query from raw bytes.
    ///
    /// Only the first question is parsed (QDCOUNT >= 1). Additional questions
    /// and any trailing OPT/EDNS records are ignored.
    pub fn parse(data: &[u8]) -> Result<Self, DnsError> {
        if data.len() < HEADER_LEN {
            return Err(DnsError::TooShort {
                need: HEADER_LEN,
                got: data.len(),
            });
        }

        let raw_header = data[..HEADER_LEN].to_vec();

        // Walk the question name labels.
        let mut offset = HEADER_LEN;
        let mut name_parts: Vec<String> = Vec::new();

        loop {
            if offset >= data.len() {
                return Err(DnsError::Truncated { offset });
            }
            let len = data[offset] as usize;
            if len == 0 {
                offset += 1; // skip root label
                break;
            }
            if offset + 1 + len > data.len() {
                return Err(DnsError::InvalidName { offset });
            }
            let label = String::from_utf8_lossy(&data[offset + 1..offset + 1 + len]);
            name_parts.push(label.into_owned());
            offset += 1 + len;
        }

        // Need 4 more bytes for QTYPE + QCLASS.
        if offset + 4 > data.len() {
            return Err(DnsError::Truncated { offset });
        }

        let qtype_raw = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let qclass_raw = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);

        let qtype =
            DnsRecordType::try_from(qtype_raw).map_err(DnsError::UnsupportedType)?;

        if qclass_raw != 1 {
            return Err(DnsError::UnsupportedClass(qclass_raw));
        }

        let raw_question = data[HEADER_LEN..offset + 4].to_vec();
        let name = name_parts.join(".");

        Ok(Self {
            name,
            qtype,
            raw_header,
            raw_question,
        })
    }
}

// ── Response builders ───────────────────────────────────────────────────────

/// Builds an A or AAAA response for a local-resolution hit.
///
/// Accepts raw query bytes. Returns a complete DNS response packet.
pub fn build_response_ip(query_bytes: &[u8], ip: IpAddr, ttl: u32) -> Result<Vec<u8>, DnsError> {
    let query = DnsQuery::parse(query_bytes)?;

    let mut r = Vec::with_capacity(HEADER_LEN + query.raw_question.len() + 28);
    r.extend_from_slice(&query.raw_header);

    // Flags: QR=1, RD=1, RA=1, RCODE=0
    r[2] = 0x81;
    r[3] = 0x80;
    // ANCOUNT = 1
    r[6] = 0x00;
    r[7] = 0x01;
    // NSCOUNT = 0, ARCOUNT = 0
    r[8] = 0x00;
    r[9] = 0x00;
    r[10] = 0x00;
    r[11] = 0x00;

    // Question section
    r.extend_from_slice(&query.raw_question);

    // Answer: name pointer to offset 0x0c (question name)
    r.extend_from_slice(&[0xc0, 0x0c]);

    let ttl_bytes = ttl.to_be_bytes();
    match ip {
        IpAddr::V4(v4) => {
            r.extend_from_slice(&[0x00, 0x01]); // TYPE A
            r.extend_from_slice(&[0x00, 0x01]); // CLASS IN
            r.extend_from_slice(&ttl_bytes);
            r.extend_from_slice(&[0x00, 0x04]); // RDLENGTH
            r.extend_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            r.extend_from_slice(&[0x00, 0x1c]); // TYPE AAAA
            r.extend_from_slice(&[0x00, 0x01]); // CLASS IN
            r.extend_from_slice(&ttl_bytes);
            r.extend_from_slice(&[0x00, 0x10]); // RDLENGTH
            r.extend_from_slice(&v6.octets());
        }
    }

    Ok(r)
}

/// Shorthand: build an A record response.
pub fn build_response_a(
    query_bytes: &[u8],
    ip: Ipv4Addr,
    ttl: u32,
) -> Result<Vec<u8>, DnsError> {
    build_response_ip(query_bytes, IpAddr::V4(ip), ttl)
}

/// Builds an NXDOMAIN response.
///
/// Zeroes ANCOUNT/NSCOUNT/ARCOUNT so EDNS(0) queries (ARCOUNT=1 in request)
/// don't produce malformed responses with missing additional records.
pub fn build_nxdomain(query_bytes: &[u8]) -> Result<Vec<u8>, DnsError> {
    let query = DnsQuery::parse(query_bytes)?;
    let mut r = Vec::with_capacity(HEADER_LEN + query.raw_question.len());
    r.extend_from_slice(&query.raw_header);

    // QR=1, AA=1, RD=1
    r[2] = 0x85;
    // RA=1, RCODE=3 (NXDOMAIN)
    r[3] = 0x83;
    // ANCOUNT = 0
    r[6] = 0x00;
    r[7] = 0x00;
    // NSCOUNT = 0
    r[8] = 0x00;
    r[9] = 0x00;
    // ARCOUNT = 0
    r[10] = 0x00;
    r[11] = 0x00;

    r.extend_from_slice(&query.raw_question);
    Ok(r)
}

/// Builds a SERVFAIL response.
pub fn build_servfail(query_bytes: &[u8]) -> Result<Vec<u8>, DnsError> {
    let query = DnsQuery::parse(query_bytes)?;
    let mut r = Vec::with_capacity(HEADER_LEN + query.raw_question.len());
    r.extend_from_slice(&query.raw_header);

    // QR=1, RD=1
    r[2] = 0x81;
    // RA=1, RCODE=2 (SERVFAIL)
    r[3] = 0x82;
    r[6] = 0x00;
    r[7] = 0x00;
    r[8] = 0x00;
    r[9] = 0x00;
    r[10] = 0x00;
    r[11] = 0x00;

    r.extend_from_slice(&query.raw_question);
    Ok(r)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal DNS A query packet for testing.
    fn build_test_query(name: &str) -> Vec<u8> {
        let mut pkt = Vec::with_capacity(64);
        // Header: ID=0xABCD, flags=0x0100 (RD=1), QDCOUNT=1
        pkt.extend_from_slice(&[0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00]);
        pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        for label in name.split('.') {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0x00); // root label
        pkt.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
        pkt.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
        pkt
    }

    #[test]
    fn parse_simple_query() {
        let data = build_test_query("my-nginx.arcbox.local");
        let q = DnsQuery::parse(&data).unwrap();
        assert_eq!(q.name, "my-nginx.arcbox.local");
        assert_eq!(q.qtype, DnsRecordType::A);
    }

    #[test]
    fn parse_too_short() {
        assert!(DnsQuery::parse(&[0u8; 4]).is_err());
    }

    #[test]
    fn response_a_preserves_id() {
        let data = build_test_query("test.arcbox.local");
        let resp = build_response_a(&data, Ipv4Addr::new(172, 17, 0, 2), 300).unwrap();
        // Transaction ID preserved.
        assert_eq!(resp[0], 0xAB);
        assert_eq!(resp[1], 0xCD);
        // QR=1
        assert_ne!(resp[2] & 0x80, 0);
        // RCODE=0
        assert_eq!(resp[3] & 0x0F, 0);
        // ANCOUNT=1
        assert_eq!(resp[7], 1);
    }

    #[test]
    fn nxdomain_clears_counts() {
        let data = build_test_query("missing.arcbox.local");
        let resp = build_nxdomain(&data).unwrap();
        assert_eq!(resp[3] & 0x0F, 3); // RCODE=NXDOMAIN
        assert_eq!(resp[7], 0); // ANCOUNT=0
        assert_eq!(resp[9], 0); // NSCOUNT=0
        assert_eq!(resp[11], 0); // ARCOUNT=0
    }

    #[test]
    fn servfail_rcode() {
        let data = build_test_query("fail.example.com");
        let resp = build_servfail(&data).unwrap();
        assert_eq!(resp[3] & 0x0F, 2); // RCODE=SERVFAIL
    }

    #[test]
    fn response_ipv6() {
        let data = build_test_query("v6.arcbox.local");
        let ip = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);
        let resp = build_response_ip(&data, ip, 60).unwrap();
        assert_eq!(resp[7], 1); // ANCOUNT=1
    }
}
