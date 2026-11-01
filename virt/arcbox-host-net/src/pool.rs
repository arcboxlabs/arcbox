//! The address pool host sandboxes take their external identity from.

use std::net::{IpAddr, Ipv4Addr};

use arcbox_vm_driver::{Error, MacAddr, Result};

/// Host numbers below this are the network address and the gateway.
const FIRST_HOST: u32 = 2;

/// An IPv4 subnet whose `.1` is the gateway and whose hosts from `.2` up to
/// the last address before broadcast are leased, one per VM.
///
/// The address names a VM to the rest of the host — DNS, the sandbox API's
/// `ip_address` — and is never routed: each guest sits alone behind its own
/// datapath on the fixed link `HostNetwork` reports as its identity, so
/// nothing on the host sends packets to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    network: u32,
    prefix_len: u8,
}

impl Pool {
    /// The pool over `network/prefix_len`. `network` may carry host bits;
    /// they are masked off.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] for a prefix too long to hold a gateway and one
    /// guest (`/31`, `/32`) or longer than 32.
    pub fn new(network: Ipv4Addr, prefix_len: u8) -> Result<Self> {
        if prefix_len > 30 {
            return Err(Error::Network(format!(
                "pool {network}/{prefix_len} holds no guest address; use /30 or shorter"
            )));
        }
        let mask = u32::MAX
            .checked_shl(32 - u32::from(prefix_len))
            .unwrap_or(0);
        Ok(Self {
            network: u32::from(network) & mask,
            prefix_len,
        })
    }

    /// The subnet prefix length every lease carries.
    pub const fn prefix_len(self) -> u8 {
        self.prefix_len
    }

    /// The gateway every lease carries: the subnet's `.1`.
    pub fn gateway(self) -> Ipv4Addr {
        Ipv4Addr::from(self.network + 1)
    }

    /// The host numbers a lease can take, lowest first.
    pub fn hosts(self) -> std::ops::Range<u32> {
        let size = 1u64 << (32 - u32::from(self.prefix_len));
        // The last address is the broadcast address. `size` is at least 4
        // (prefix <= 30), so the range is never empty.
        FIRST_HOST..u32::try_from(size - 1).unwrap_or(u32::MAX)
    }

    /// The address of host number `host`.
    pub fn address(self, host: u32) -> Ipv4Addr {
        Ipv4Addr::from(self.network + host)
    }

    /// The host number of `ip`, when `ip` is one this pool leases.
    pub fn host(self, ip: IpAddr) -> Option<u32> {
        let IpAddr::V4(v4) = ip else { return None };
        let host = u32::from(v4).checked_sub(self.network)?;
        self.hosts().contains(&host).then_some(host)
    }

    /// The MAC of host number `host`: locally administered, unicast, and
    /// unique within the pool because the low 24 bits are the host number
    /// (a pool is at most a /8 in practice; a larger one would only repeat
    /// MACs across guests that never share a link).
    pub fn mac(host: u32) -> MacAddr {
        let [_, a, b, c] = host.to_be_bytes();
        MacAddr::new([0x02, 0xab, 0x5b, a, b, c])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(cidr: &str) -> Pool {
        let (ip, len) = cidr.split_once('/').unwrap();
        Pool::new(ip.parse().unwrap(), len.parse().unwrap()).unwrap()
    }

    #[test]
    fn a_30_holds_exactly_one_guest_behind_its_gateway() {
        let p = pool("10.9.0.4/30");
        assert_eq!(p.gateway(), Ipv4Addr::new(10, 9, 0, 5));
        assert_eq!(p.hosts().collect::<Vec<_>>(), vec![2]);
        assert_eq!(p.address(2), Ipv4Addr::new(10, 9, 0, 6));
        // Network, gateway and broadcast are not leasable.
        for ip in ["10.9.0.4", "10.9.0.5", "10.9.0.7", "10.9.0.8", "10.9.0.3"] {
            assert_eq!(p.host(ip.parse().unwrap()), None, "{ip}");
        }
        assert_eq!(p.host("10.9.0.6".parse().unwrap()), Some(2));
        assert_eq!(p.host("::1".parse().unwrap()), None);
    }

    #[test]
    fn host_bits_are_masked_and_too_long_prefixes_are_refused() {
        assert_eq!(pool("10.200.7.9/16"), pool("10.200.0.0/16"));
        assert!(Pool::new(Ipv4Addr::new(10, 0, 0, 0), 31).is_err());
        assert!(Pool::new(Ipv4Addr::new(10, 0, 0, 0), 33).is_err());
        let all = Pool::new(Ipv4Addr::UNSPECIFIED, 0).unwrap();
        assert_eq!(all.hosts().next(), Some(2));
        assert_eq!(
            all.host("255.255.255.254".parse().unwrap()),
            Some(u32::MAX - 1)
        );
        assert_eq!(all.host("255.255.255.255".parse().unwrap()), None);
    }

    #[test]
    fn macs_are_unicast_and_distinct_per_host() {
        let a = Pool::mac(2);
        let b = Pool::mac(0x0001_0002);
        assert!(a.is_unicast() && b.is_unicast());
        assert_ne!(a, b);
    }
}
