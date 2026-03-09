//! DHCP server with lease management.
//!
//! Implements the DHCP state machine (DISCOVER/OFFER/REQUEST/ACK/RELEASE/DECLINE)
//! with IP allocation and lease tracking.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::allocator::IpAllocator;
use crate::config::DhcpConfig;
use crate::error::{DhcpError, Result};
use crate::packet::{DhcpMessageType, DhcpPacket};

/// DHCP lease information.
#[derive(Debug, Clone)]
pub struct DhcpLease {
    /// Client MAC address.
    pub mac: [u8; 6],
    /// Assigned IP address.
    pub ip: Ipv4Addr,
    /// Client hostname (if provided).
    pub hostname: Option<String>,
    /// Lease start time.
    pub lease_start: Instant,
    /// Lease duration.
    pub lease_duration: Duration,
}

impl DhcpLease {
    /// Checks if the lease has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.lease_start.elapsed() >= self.lease_duration
    }

    /// Returns the time remaining on the lease.
    #[must_use]
    pub fn time_remaining(&self) -> Duration {
        let elapsed = self.lease_start.elapsed();
        if elapsed >= self.lease_duration {
            Duration::ZERO
        } else {
            self.lease_duration.checked_sub(elapsed).unwrap()
        }
    }
}

/// DHCP server.
///
/// Provides IP addresses to clients via the DHCP protocol.
pub struct DhcpServer {
    /// Server configuration.
    config: DhcpConfig,
    /// IP address allocator.
    allocator: IpAllocator,
    /// Active leases (MAC -> Lease).
    leases: HashMap<[u8; 6], DhcpLease>,
    /// IP reservations (MAC -> IP).
    reservations: HashMap<[u8; 6], Ipv4Addr>,
}

impl DhcpServer {
    /// Creates a new DHCP server.
    #[must_use]
    pub fn new(config: DhcpConfig) -> Self {
        let allocator = IpAllocator::new(config.pool_start, config.pool_end);

        Self {
            config,
            allocator,
            leases: HashMap::new(),
            reservations: HashMap::new(),
        }
    }

    /// Returns the server IP.
    #[must_use]
    pub fn server_ip(&self) -> Ipv4Addr {
        self.config.server_ip
    }

    /// Returns the configuration.
    #[must_use]
    pub fn config(&self) -> &DhcpConfig {
        &self.config
    }

    /// Returns active leases.
    #[must_use]
    pub fn leases(&self) -> &HashMap<[u8; 6], DhcpLease> {
        &self.leases
    }

    /// Reserves an IP address for a specific MAC.
    pub fn reserve_ip(&mut self, mac: [u8; 6], ip: Ipv4Addr) {
        self.reservations.insert(mac, ip);
        self.allocator.allocate_specific(ip);
    }

    /// Removes an IP reservation.
    pub fn remove_reservation(&mut self, mac: &[u8; 6]) {
        if let Some(ip) = self.reservations.remove(mac) {
            self.allocator.release(ip);
        }
    }

    /// Handles an incoming DHCP packet.
    ///
    /// Returns the response packet if one should be sent.
    ///
    /// # Errors
    ///
    /// Returns an error if packet processing fails.
    pub fn handle_packet(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>> {
        let packet = DhcpPacket::parse(data)?;

        // Only handle BOOTREQUEST (client -> server)
        if packet.op != 1 {
            return Ok(None);
        }

        let response = match packet.message_type {
            Some(DhcpMessageType::Discover) => Some(self.handle_discover(&packet)?),
            Some(DhcpMessageType::Request) => Some(self.handle_request(&packet)?),
            Some(DhcpMessageType::Release) => {
                self.handle_release(&packet);
                None
            }
            Some(DhcpMessageType::Decline) => {
                self.handle_decline(&packet);
                None
            }
            _ => None,
        };

        Ok(response)
    }

    /// Handles DHCPDISCOVER.
    fn handle_discover(&mut self, packet: &DhcpPacket) -> Result<Vec<u8>> {
        let mac = packet.client_mac();

        // Clean up expired leases
        self.cleanup_expired_leases();

        // Check for reservation
        let ip = if let Some(&reserved_ip) = self.reservations.get(&mac) {
            reserved_ip
        } else if let Some(lease) = self.leases.get(&mac) {
            // Existing lease
            lease.ip
        } else if let Some(requested) = packet.requested_ip {
            // Try to honor requested IP
            if self.allocator.is_available(requested) {
                self.allocator.allocate_specific(requested);
                requested
            } else {
                self.allocator
                    .allocate()
                    .ok_or(DhcpError::PoolExhausted)?
            }
        } else {
            // Allocate new IP
            self.allocator
                .allocate()
                .ok_or(DhcpError::PoolExhausted)?
        };

        // Record a pending lease so handle_request() can validate.
        let lease = DhcpLease {
            mac,
            ip,
            hostname: packet.hostname.clone(),
            lease_start: Instant::now(),
            lease_duration: self.config.lease_duration,
        };
        self.leases.insert(mac, lease);

        // Create OFFER response
        let mut response = DhcpPacket::new();
        response.op = 2; // BOOTREPLY
        response.xid = packet.xid;
        response.yiaddr = ip;
        response.siaddr = self.config.server_ip;
        response.flags = packet.flags;
        response.chaddr = packet.chaddr;
        response.message_type = Some(DhcpMessageType::Offer);

        tracing::debug!(
            "DHCPOFFER: {} -> {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            ip,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );

        Ok(response.serialize(&self.config))
    }

    /// Handles DHCPREQUEST.
    fn handle_request(&mut self, packet: &DhcpPacket) -> Result<Vec<u8>> {
        let mac = packet.client_mac();
        let requested_ip = packet
            .requested_ip
            .or_else(|| {
                if packet.ciaddr != Ipv4Addr::UNSPECIFIED {
                    Some(packet.ciaddr)
                } else {
                    None
                }
            })
            .ok_or_else(|| DhcpError::Protocol("no IP requested".to_string()))?;

        // Verify the IP is available or already leased to this client
        let valid = if let Some(lease) = self.leases.get(&mac) {
            lease.ip == requested_ip
        } else if let Some(&reserved) = self.reservations.get(&mac) {
            reserved == requested_ip
        } else {
            self.allocator.is_available(requested_ip) || {
                // Check if we can allocate it
                self.allocator.allocate_specific(requested_ip)
            }
        };

        if !valid {
            // Send NAK
            let mut response = DhcpPacket::new();
            response.op = 2;
            response.xid = packet.xid;
            response.siaddr = self.config.server_ip;
            response.flags = packet.flags;
            response.chaddr = packet.chaddr;
            response.message_type = Some(DhcpMessageType::Nak);

            tracing::debug!(
                "DHCPNAK: {} denied for {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                requested_ip,
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );

            return Ok(response.serialize(&self.config));
        }

        // Create or update lease
        let lease = DhcpLease {
            mac,
            ip: requested_ip,
            hostname: packet.hostname.clone(),
            lease_start: Instant::now(),
            lease_duration: self.config.lease_duration,
        };
        self.leases.insert(mac, lease);

        // Create ACK response
        let mut response = DhcpPacket::new();
        response.op = 2;
        response.xid = packet.xid;
        response.yiaddr = requested_ip;
        response.siaddr = self.config.server_ip;
        response.flags = packet.flags;
        response.chaddr = packet.chaddr;
        response.message_type = Some(DhcpMessageType::Ack);

        tracing::debug!(
            "DHCPACK: {} -> {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            requested_ip,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );

        Ok(response.serialize(&self.config))
    }

    /// Handles DHCPRELEASE.
    fn handle_release(&mut self, packet: &DhcpPacket) {
        let mac = packet.client_mac();

        if let Some(lease) = self.leases.remove(&mac) {
            // Don't release reserved IPs
            if !self.reservations.contains_key(&mac) {
                self.allocator.release(lease.ip);
            }

            tracing::debug!(
                "DHCPRELEASE: {} from {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                lease.ip,
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );
        }
    }

    /// Handles DHCPDECLINE.
    fn handle_decline(&mut self, packet: &DhcpPacket) {
        // Client is saying the IP is already in use
        // We should mark it as unavailable
        if let Some(ip) = packet.requested_ip {
            self.allocator.allocate_specific(ip);

            tracing::warn!("DHCPDECLINE: {} reported as in use", ip);
        }
    }

    /// Cleans up expired leases.
    pub fn cleanup_expired_leases(&mut self) {
        let expired: Vec<[u8; 6]> = self
            .leases
            .iter()
            .filter(|(mac, lease)| lease.is_expired() && !self.reservations.contains_key(*mac))
            .map(|(mac, _)| *mac)
            .collect();

        for mac in expired {
            if let Some(lease) = self.leases.remove(&mac) {
                self.allocator.release(lease.ip);
                tracing::debug!("Expired lease for {}", lease.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dhcp_config_new() {
        let config = DhcpConfig::new(
            Ipv4Addr::new(192, 168, 64, 1),
            Ipv4Addr::new(255, 255, 255, 0),
        );

        assert_eq!(config.server_ip, Ipv4Addr::new(192, 168, 64, 1));
        assert_eq!(config.gateway, Ipv4Addr::new(192, 168, 64, 1));
        assert_eq!(config.pool_start, Ipv4Addr::new(192, 168, 64, 2));
    }

    #[test]
    fn test_dhcp_message_type_conversion() {
        assert_eq!(DhcpMessageType::try_from(1), Ok(DhcpMessageType::Discover));
        assert_eq!(DhcpMessageType::try_from(5), Ok(DhcpMessageType::Ack));
        assert!(DhcpMessageType::try_from(100).is_err());
    }

    #[test]
    fn test_dhcp_lease_expiration() {
        let lease = DhcpLease {
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            ip: Ipv4Addr::new(192, 168, 64, 2),
            hostname: None,
            lease_start: Instant::now(),
            lease_duration: Duration::from_secs(1),
        };

        assert!(!lease.is_expired());
        assert!(lease.time_remaining() > Duration::ZERO);
    }

    #[test]
    fn test_dhcp_server_reservation() {
        let config = DhcpConfig::new(
            Ipv4Addr::new(192, 168, 64, 1),
            Ipv4Addr::new(255, 255, 255, 0),
        );
        let mut server = DhcpServer::new(config);

        let mac = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        let ip = Ipv4Addr::new(192, 168, 64, 100);

        server.reserve_ip(mac, ip);
        // Reservation should prevent double-allocation
        assert!(!server.allocator.is_available(ip));

        server.remove_reservation(&mac);
        assert!(server.allocator.is_available(ip));
    }
}
