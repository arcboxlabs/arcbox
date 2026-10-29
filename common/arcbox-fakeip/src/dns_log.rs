//! DNS resolution log: maps IP addresses back to domain names.
//!
//! When the DNS forwarder receives an upstream response containing A or AAAA records,
//! it records (IP → domain) entries here. The TcpBridge consults this log to
//! determine the original domain name for a given destination IP, enabling
//! proxy-aware connection strategies (HTTP CONNECT / SOCKS5 use the domain
//! rather than the raw IP).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// TTL for resolution log entries. After this, entries are eligible for eviction.
const ENTRY_TTL: Duration = Duration::from_mins(5);

/// Maximum number of entries before LRU eviction kicks in.
const MAX_ENTRIES: usize = 4096;

/// A single resolution entry: domain name + timestamp.
struct Entry {
    domain: String,
    recorded_at: Instant,
}

/// Thread-safe DNS resolution log shared between the datapath DNS handler
/// and the TcpBridge.
///
/// Records are written on DNS response (datapath task) and read on TCP SYN
/// (TcpBridge). Both happen on the same tokio runtime but potentially
/// different tasks, so interior mutability via `Mutex` is required.
#[derive(Clone)]
pub struct DnsResolutionLog {
    inner: Arc<Mutex<LogInner>>,
}

struct LogInner {
    /// IP → (domain, recorded_at). Most recent resolution wins.
    entries: HashMap<IpAddr, Entry>,
}

impl DnsResolutionLog {
    /// Creates a new empty resolution log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogInner {
                entries: HashMap::with_capacity(256),
            })),
        }
    }

    /// Records that `domain` resolved to `ips` via DNS.
    ///
    /// Called by the datapath DNS handler after receiving an upstream response.
    /// Overwrites previous entries for the same IPs (latest resolution wins).
    pub fn record<T: Copy + Into<IpAddr>>(&self, domain: &str, ips: &[T]) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        // Evict expired entries if we're near capacity.
        if inner.entries.len() >= MAX_ENTRIES {
            let now = Instant::now();
            inner
                .entries
                .retain(|_, e| now.duration_since(e.recorded_at) < ENTRY_TTL);
        }

        let now = Instant::now();
        for &ip in ips {
            inner.entries.insert(
                ip.into(),
                Entry {
                    domain: domain.to_string(),
                    recorded_at: now,
                },
            );
        }
    }

    /// Looks up the domain name for an IP address.
    ///
    /// Returns `None` if the IP wasn't seen in a recent DNS response or if
    /// the entry has expired.
    #[must_use]
    pub fn lookup(&self, ip: impl Into<IpAddr>) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        let entry = inner.entries.get(&ip.into())?;
        if entry.recorded_at.elapsed() < ENTRY_TTL {
            Some(entry.domain.clone())
        } else {
            None
        }
    }

    /// Removes the entry for `ip`, returning the domain it mapped to (if any).
    ///
    /// For targeted invalidation (e.g. a single re-pointed mapping). A full flush
    /// should use [`clear`](Self::clear) so the reverse log can't be left
    /// partially in sync with a fake-IP allocator.
    pub fn remove(&self, ip: impl Into<IpAddr>) -> Option<String> {
        let mut inner = self.inner.lock().ok()?;
        inner.entries.remove(&ip.into()).map(|e| e.domain)
    }

    /// Clears every IP → domain entry — a full DNS-resolution-log flush.
    ///
    /// Pairs with the consumer's fake-IP allocator reset: clearing the reverse log
    /// without the allocator (or vice versa) can desync them, so a flush clears
    /// both together.
    pub fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.entries.clear();
        }
    }

    /// Number of live entries (recorded, not yet evicted). For diagnostics and
    /// post-flush assertions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map_or(0, |i| i.entries.len())
    }

    /// Whether the log holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for DnsResolutionLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse A record IPs and the query domain from a raw DNS response.
///
/// Returns `(domain, [ips])` or `None` if the response can't be parsed.
/// Minimal parser — only extracts the question name and A record addresses.
pub fn parse_dns_response_a_records(data: &[u8]) -> Option<(String, Vec<Ipv4Addr>)> {
    let (domain, addresses) = parse_dns_response_addresses(data)?;
    let ips: Vec<_> = addresses
        .into_iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .collect();
    (!ips.is_empty()).then_some((domain, ips))
}

/// Extracts A and AAAA answer addresses with the original question name.
pub fn parse_dns_response_addresses(data: &[u8]) -> Option<(String, Vec<IpAddr>)> {
    // Minimum DNS header (12 bytes) + at least 1 question.
    if data.len() < 12 {
        return None;
    }

    // Check QR bit (must be a response).
    if data[2] & 0x80 == 0 {
        return None;
    }

    let qdcount = u16::from_be_bytes([data[4], data[5]]) as usize;
    let ancount = u16::from_be_bytes([data[6], data[7]]) as usize;
    if qdcount == 0 || ancount == 0 {
        return None;
    }

    // Parse question name.
    let mut offset = 12;
    let mut name_parts: Vec<String> = Vec::new();
    while offset < data.len() {
        let len = data[offset] as usize;
        if len == 0 {
            offset += 1;
            break;
        }
        if len >= 0xC0 {
            // Compression pointer — skip for question name parsing.
            offset += 2;
            break;
        }
        if offset + 1 + len > data.len() {
            return None;
        }
        name_parts.push(String::from_utf8_lossy(&data[offset + 1..offset + 1 + len]).to_string());
        offset += 1 + len;
    }

    // Skip QTYPE + QCLASS (4 bytes).
    if offset + 4 > data.len() {
        return None;
    }
    offset += 4;

    let domain = name_parts.join(".");
    if domain.is_empty() {
        return None;
    }

    // Parse answer section for A and AAAA records.
    let mut ips = Vec::new();
    for _ in 0..ancount {
        if offset + 2 > data.len() {
            break;
        }

        // Skip name (may be a compression pointer).
        if data[offset] >= 0xC0 {
            offset += 2;
        } else {
            while offset < data.len() {
                let len = data[offset] as usize;
                if len == 0 {
                    offset += 1;
                    break;
                }
                if len >= 0xC0 {
                    offset += 2;
                    break;
                }
                if offset + 1 + len > data.len() {
                    return None;
                }
                offset += 1 + len;
            }
        }

        if offset + 10 > data.len() {
            break;
        }

        let rtype = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let rdlength = u16::from_be_bytes([data[offset + 8], data[offset + 9]]) as usize;
        offset += 10;

        if offset + rdlength > data.len() {
            break;
        }

        // Type A (1) with 4-byte RDATA.
        if rtype == 1 && rdlength == 4 {
            ips.push(
                Ipv4Addr::new(
                    data[offset],
                    data[offset + 1],
                    data[offset + 2],
                    data[offset + 3],
                )
                .into(),
            );
        } else if rtype == 28 && rdlength == 16 {
            let octets: [u8; 16] = data[offset..offset + 16].try_into().ok()?;
            ips.push(Ipv6Addr::from(octets).into());
        }

        offset += rdlength;
    }

    if ips.is_empty() {
        None
    } else {
        Some((domain, ips))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_remain_distinct_and_share_expiry_and_removal() {
        let log = DnsResolutionLog::new();
        let ipv4 = Ipv4Addr::new(192, 0, 2, 1);
        let mapped = ipv4.to_ipv6_mapped();
        let native: Ipv6Addr = "2001:db8::1".parse().unwrap();
        log.record("v4.test", &[ipv4]);
        log.record("mapped.test", &[mapped]);
        log.record("native.test", &[native]);
        assert_eq!(log.lookup(ipv4).as_deref(), Some("v4.test"));
        assert_eq!(log.lookup(mapped).as_deref(), Some("mapped.test"));
        assert_eq!(log.remove(native).as_deref(), Some("native.test"));
        assert_eq!(log.lookup(native), None);
        log.inner
            .lock()
            .unwrap()
            .entries
            .get_mut(&mapped.into())
            .unwrap()
            .recorded_at = Instant::now().checked_sub(ENTRY_TTL).unwrap();
        assert_eq!(log.lookup(mapped), None);
        assert_eq!(log.lookup(ipv4).as_deref(), Some("v4.test"));
    }

    #[test]
    fn aaaa_extraction_preserves_the_ipv4_only_entry_point() {
        let ipv6: Ipv6Addr = "2001:db8::1234".parse().unwrap();
        let mut response = vec![0, 1, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        response.extend_from_slice(b"\x01v\x04test\0\0\x1c\0\x01");
        response.extend_from_slice(&[0xc0, 12, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        response.extend_from_slice(&ipv6.octets());
        assert_eq!(
            parse_dns_response_addresses(&response),
            Some(("v.test".into(), vec![ipv6.into()]))
        );
        assert_eq!(parse_dns_response_a_records(&response), None);
        assert_eq!(
            parse_dns_response_addresses(&response[..response.len() - 1]),
            None
        );
        response[7] = 2;
        response.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 1]);
        assert_eq!(
            parse_dns_response_addresses(&response).unwrap().1,
            vec![IpAddr::V6(ipv6), IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))]
        );
        assert_eq!(
            parse_dns_response_a_records(&response).unwrap().1,
            vec![Ipv4Addr::new(192, 0, 2, 1)]
        );
    }

    #[test]
    fn record_and_lookup() {
        let log = DnsResolutionLog::new();
        let ip = Ipv4Addr::new(198, 18, 2, 17);
        log.record("registry-1.docker.io", &[ip]);
        assert_eq!(log.lookup(ip).as_deref(), Some("registry-1.docker.io"));
    }

    #[test]
    fn lookup_unknown_returns_none() {
        let log = DnsResolutionLog::new();
        assert!(log.lookup(Ipv4Addr::new(1, 2, 3, 4)).is_none());
    }

    #[test]
    fn latest_resolution_wins() {
        let log = DnsResolutionLog::new();
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        log.record("old.example.com", &[ip]);
        log.record("new.example.com", &[ip]);
        assert_eq!(log.lookup(ip).as_deref(), Some("new.example.com"));
    }

    #[test]
    fn remove_drops_a_single_entry() {
        let log = DnsResolutionLog::new();
        let a = Ipv4Addr::new(10, 0, 0, 1);
        let b = Ipv4Addr::new(10, 0, 0, 2);
        log.record("a.example.com", &[a]);
        log.record("b.example.com", &[b]);
        assert_eq!(log.remove(a).as_deref(), Some("a.example.com"));
        assert!(log.lookup(a).is_none(), "removed entry is gone");
        assert_eq!(
            log.lookup(b).as_deref(),
            Some("b.example.com"),
            "other kept"
        );
        assert!(log.remove(a).is_none(), "removing again is a no-op");
    }

    #[test]
    fn clear_empties_the_log() {
        let log = DnsResolutionLog::new();
        log.record("a.example.com", &[Ipv4Addr::new(10, 0, 0, 1)]);
        log.record("b.example.com", &[Ipv4Addr::new(10, 0, 0, 2)]);
        assert_eq!(log.len(), 2);
        assert!(!log.is_empty());
        log.clear();
        assert!(log.is_empty(), "flush empties the log");
        assert!(log.lookup(Ipv4Addr::new(10, 0, 0, 1)).is_none());
    }

    #[test]
    fn parse_response_basic() {
        // Minimal DNS response: registry-1.docker.io → 52.7.198.84
        let mut pkt = Vec::new();
        // Header: QR=1, QDCOUNT=1, ANCOUNT=1
        pkt.extend_from_slice(&[0xAB, 0xCD, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01]);
        pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // Question: registry-1.docker.io
        for label in ["registry-1", "docker", "io"] {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0x00); // root
        pkt.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // QTYPE=A, QCLASS=IN
        // Answer: compression pointer + A record
        pkt.extend_from_slice(&[0xC0, 0x0C]); // name pointer
        pkt.extend_from_slice(&[0x00, 0x01]); // TYPE=A
        pkt.extend_from_slice(&[0x00, 0x01]); // CLASS=IN
        pkt.extend_from_slice(&[0x00, 0x00, 0x01, 0x2C]); // TTL=300
        pkt.extend_from_slice(&[0x00, 0x04]); // RDLENGTH=4
        pkt.extend_from_slice(&[52, 7, 198, 84]); // RDATA

        let (domain, ips) = parse_dns_response_a_records(&pkt).unwrap();
        assert_eq!(domain, "registry-1.docker.io");
        assert_eq!(ips, vec![Ipv4Addr::new(52, 7, 198, 84)]);
    }
}
