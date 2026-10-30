//! Exact host source addresses for guest services restricted to the Mac.

use std::net::{IpAddr, Ipv4Addr};

use anyhow::{Context, Result};

/// Lists the host's own IPv4 addresses on the guest bridge's network.
pub fn host_addresses_on_link(bridge: Ipv4Addr) -> Result<Vec<IpAddr>> {
    Ok(nix::ifaddrs::getifaddrs()
        .context("enumerating host network interfaces")?
        .filter_map(|interface| {
            let address = interface.address?.as_sockaddr_in()?.ip();
            let netmask = interface.netmask?.as_sockaddr_in()?.ip();
            same_link(address, bridge, netmask).then_some(IpAddr::V4(address))
        })
        .collect())
}

fn same_link(a: Ipv4Addr, b: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let mask = u32::from(netmask);
    u32::from(a) & mask == u32::from(b) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_is_shared_under_the_interface_netmask() {
        let mask: Ipv4Addr = "255.255.255.0".parse().unwrap();
        let host: Ipv4Addr = "192.168.64.1".parse().unwrap();
        assert!(same_link(host, "192.168.64.7".parse().unwrap(), mask));
        assert!(!same_link(host, "192.168.65.7".parse().unwrap(), mask));
        assert!(!same_link(host, "10.0.2.15".parse().unwrap(), mask));
    }

    #[test]
    fn loopback_is_never_on_a_machines_link() {
        let peers = host_addresses_on_link("192.168.64.7".parse().unwrap()).unwrap();
        assert!(!peers.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }
}
