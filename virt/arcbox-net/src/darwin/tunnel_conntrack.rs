//! Lightweight connection tracker for L3 tunnel (utun) return path.
//!
//! When `L3TunnelService` injects a host-originated IP packet into the guest
//! via `RoutePacket`, the reverse 5-tuple is registered here. When the guest
//! sends a reply, the datapath checks this table BEFORE dispatching to
//! SmoltcpDevice/proxy — if the packet matches, it is written back to the
//! utun FD instead of being proxied.
//!
//! This table is intentionally separate from the NAT engine conntrack:
//! it only tracks flows injected via `RoutePacket` and is much smaller.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Default timeouts per protocol.
const TCP_ESTABLISHED_TIMEOUT: Duration = Duration::from_secs(300);
const TCP_HALF_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const UDP_TIMEOUT: Duration = Duration::from_secs(60);
const ICMP_TIMEOUT: Duration = Duration::from_secs(10);

/// IP protocol numbers.
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

/// 5-tuple flow key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FiveTuple {
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    proto: u8,
}

/// Per-entry state.
struct ConnEntry {
    last_seen: Instant,
    /// Whether a reply has been seen (for TCP: SYN-ACK received).
    established: bool,
    protocol: u8,
}

impl ConnEntry {
    fn timeout(&self) -> Duration {
        match self.protocol {
            PROTO_TCP if self.established => TCP_ESTABLISHED_TIMEOUT,
            PROTO_TCP => TCP_HALF_OPEN_TIMEOUT,
            PROTO_UDP => UDP_TIMEOUT,
            PROTO_ICMP => ICMP_TIMEOUT,
            _ => UDP_TIMEOUT,
        }
    }

    fn is_expired(&self) -> bool {
        self.last_seen.elapsed() > self.timeout()
    }
}

/// Tunnel connection tracker.
pub struct TunnelConnTrack {
    /// Reverse 5-tuples of injected packets.
    /// Key = expected reply tuple (swapped src/dst from injected packet).
    entries: HashMap<FiveTuple, ConnEntry>,
}

impl TunnelConnTrack {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Called when a `RoutePacket` is injected into the guest.
    /// Parses the IP packet and registers the expected reverse flow.
    pub fn register_injected(&mut self, ip_packet: &[u8]) {
        let Some(fwd) = parse_five_tuple(ip_packet) else {
            return;
        };
        // Register the reverse tuple so we can match the reply.
        let rev = FiveTuple {
            src_ip: fwd.dst_ip,
            dst_ip: fwd.src_ip,
            src_port: fwd.dst_port,
            dst_port: fwd.src_port,
            proto: fwd.proto,
        };
        self.entries.insert(
            rev,
            ConnEntry {
                last_seen: Instant::now(),
                established: false,
                protocol: fwd.proto,
            },
        );
    }

    /// Called for every frame received from the guest (before proxy dispatch).
    ///
    /// `ip_packet` should be the IP payload (L2 header already stripped).
    /// Returns `true` if this is a reply to a tunneled flow.
    pub fn is_tunnel_return(&mut self, ip_packet: &[u8]) -> bool {
        let Some(tuple) = parse_five_tuple(ip_packet) else {
            return false;
        };
        if let Some(entry) = self.entries.get_mut(&tuple) {
            entry.last_seen = Instant::now();
            entry.established = true;
            return true;
        }
        false
    }

    /// Removes expired entries. Call periodically (e.g. every 10s).
    pub fn gc(&mut self) {
        self.entries.retain(|_, e| !e.is_expired());
    }

    /// Number of tracked flows (for diagnostics).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Extracts a 5-tuple from a raw IPv4 packet.
fn parse_five_tuple(ip: &[u8]) -> Option<FiveTuple> {
    if ip.len() < 20 {
        return None;
    }
    let version = ip[0] >> 4;
    if version != 4 {
        return None;
    }
    let ihl = ((ip[0] & 0x0F) as usize) * 4;
    let proto = ip[9];
    let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let dst_ip = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);

    let (src_port, dst_port) = match proto {
        PROTO_TCP | PROTO_UDP => {
            if ip.len() < ihl + 4 {
                return None;
            }
            let sp = u16::from_be_bytes([ip[ihl], ip[ihl + 1]]);
            let dp = u16::from_be_bytes([ip[ihl + 2], ip[ihl + 3]]);
            (sp, dp)
        }
        PROTO_ICMP => {
            // Use ICMP ID as "port" for matching echo request/reply.
            if ip.len() < ihl + 8 {
                return None;
            }
            let icmp_type = ip[ihl];
            // Echo request (8) / echo reply (0): use ID field.
            if icmp_type == 8 || icmp_type == 0 {
                let id = u16::from_be_bytes([ip[ihl + 4], ip[ihl + 5]]);
                (id, id)
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };

    Some(FiveTuple {
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        proto,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal IPv4 TCP SYN packet.
    fn build_tcp_packet(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
    ) -> Vec<u8> {
        let mut pkt = vec![0u8; 40]; // IP(20) + TCP(20)
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[2..4].copy_from_slice(&40u16.to_be_bytes()); // total length
        pkt[9] = PROTO_TCP;
        pkt[12..16].copy_from_slice(&src.octets());
        pkt[16..20].copy_from_slice(&dst.octets());
        pkt[20..22].copy_from_slice(&src_port.to_be_bytes());
        pkt[22..24].copy_from_slice(&dst_port.to_be_bytes());
        pkt[32] = 0x50; // data offset = 5
        pkt
    }

    #[test]
    fn register_and_match_tcp() {
        let mut ct = TunnelConnTrack::new();

        // Host sends SYN to container.
        let syn = build_tcp_packet(
            Ipv4Addr::new(192, 168, 1, 100),
            Ipv4Addr::new(172, 17, 0, 2),
            54321,
            80,
        );
        ct.register_injected(&syn);
        assert_eq!(ct.len(), 1);

        // Container replies with SYN-ACK.
        let syn_ack = build_tcp_packet(
            Ipv4Addr::new(172, 17, 0, 2),
            Ipv4Addr::new(192, 168, 1, 100),
            80,
            54321,
        );
        assert!(ct.is_tunnel_return(&syn_ack));
    }

    #[test]
    fn no_false_positive() {
        let mut ct = TunnelConnTrack::new();

        let syn = build_tcp_packet(
            Ipv4Addr::new(192, 168, 1, 100),
            Ipv4Addr::new(172, 17, 0, 2),
            54321,
            80,
        );
        ct.register_injected(&syn);

        // Different flow from guest (outbound, not a reply).
        let outbound = build_tcp_packet(
            Ipv4Addr::new(172, 17, 0, 2),
            Ipv4Addr::new(8, 8, 8, 8),
            45000,
            443,
        );
        assert!(!ct.is_tunnel_return(&outbound));
    }

    #[test]
    fn gc_removes_expired() {
        let mut ct = TunnelConnTrack::new();
        let syn = build_tcp_packet(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 88, 0, 2),
            1234,
            22,
        );
        ct.register_injected(&syn);
        assert_eq!(ct.len(), 1);

        // Manually expire the entry.
        for entry in ct.entries.values_mut() {
            entry.last_seen = Instant::now() - Duration::from_secs(600);
        }
        ct.gc();
        assert_eq!(ct.len(), 0);
    }
}
