//! Discovers the guest VM's bridge NIC IP address.
//!
//! After the VM boots with a VZNATNetworkDeviceAttachment, Apple's vmnet
//! framework creates a bridge interface (bridge100, bridge101, etc.) and
//! assigns the guest an IP via DHCP. This module finds that IP by parsing
//! the system DHCP lease database.

use std::net::Ipv4Addr;

/// Path to the macOS vmnet DHCP lease database.
const DHCP_LEASES_PATH: &str = "/var/db/dhcpd_leases";

/// The primary subnet used by the socketpair datapath (192.168.64.0/24).
/// IPs in this subnet are NOT bridge NIC IPs.
const PRIMARY_SUBNET_PREFIX: [u8; 3] = [192, 168, 64];

/// Discovers the guest's bridge NIC IP by reading vmnet DHCP leases.
///
/// Returns the first lease IP that is NOT in the primary 192.168.64.0/24
/// subnet (which belongs to the socketpair datapath, not the bridge).
///
/// Typical result: 192.168.65.2 (on bridge100).
pub fn discover_bridge_ip() -> Option<Ipv4Addr> {
    let content = std::fs::read_to_string(DHCP_LEASES_PATH).ok()?;
    parse_bridge_ip_from_leases(&content)
}

/// Parses the vmnet DHCP lease file for a bridge NIC IP.
///
/// Selects the lease with the most recent timestamp (highest `lease=0x...`)
/// to handle stale entries from previous VM boots.
fn parse_bridge_ip_from_leases(content: &str) -> Option<Ipv4Addr> {
    // Lease file format: brace-delimited blocks with key=value lines.
    // Each block has ip_address=... and lease=0x... (epoch timestamp).
    let mut best_ip: Option<Ipv4Addr> = None;
    let mut best_lease: u64 = 0;
    let mut current_ip: Option<Ipv4Addr> = None;
    let mut current_lease: u64 = 0;

    for line in content.lines() {
        let line = line.trim();

        if line == "{" {
            current_ip = None;
            current_lease = 0;
        } else if line == "}" {
            if let Some(ip) = current_ip {
                if current_lease >= best_lease {
                    best_ip = Some(ip);
                    best_lease = current_lease;
                }
            }
        } else if let Some(ip_str) = line.strip_prefix("ip_address=") {
            if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
                let octets = ip.octets();
                // Skip the primary subnet (192.168.64.x).
                if octets[0] == PRIMARY_SUBNET_PREFIX[0]
                    && octets[1] == PRIMARY_SUBNET_PREFIX[1]
                    && octets[2] == PRIMARY_SUBNET_PREFIX[2]
                {
                    continue;
                }
                // Skip loopback and link-local.
                if octets[0] == 127 || octets[0] == 169 {
                    continue;
                }
                current_ip = Some(ip);
            }
        } else if let Some(lease_str) = line.strip_prefix("lease=") {
            current_lease =
                u64::from_str_radix(lease_str.strip_prefix("0x").unwrap_or(lease_str), 16)
                    .unwrap_or(0);
        }
    }

    best_ip
}

/// Finds which bridge interface the guest is connected to.
///
/// Looks for bridge100, bridge101, etc. with an IP in the same /24 as
/// the discovered guest IP. Returns the bridge interface name.
pub fn find_bridge_interface(guest_ip: Ipv4Addr) -> Option<String> {
    let guest_octets = guest_ip.octets();

    // Check bridge100 through bridge109.
    for i in 100..110 {
        let name = format!("bridge{i}");
        if let Some(bridge_ip) = get_interface_ipv4(&name) {
            let b = bridge_ip.octets();
            // Same /24 subnet?
            if b[0] == guest_octets[0] && b[1] == guest_octets[1] && b[2] == guest_octets[2] {
                return Some(name);
            }
        }
    }
    None
}

/// Gets the IPv4 address of a network interface using `ifconfig`.
fn get_interface_ipv4(iface: &str) -> Option<Ipv4Addr> {
    let output = std::process::Command::new("ifconfig")
        .arg(iface)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse "inet X.X.X.X" from ifconfig output.
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inet ") {
            let ip_str = rest.split_whitespace().next()?;
            return ip_str.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lease_file() {
        let content = r#"
{
	ip_address=192.168.65.2
	hw_address=1,da:aa:af:bf:26:df
	identifier=1,da:aa:af:bf:26:df
	lease=0x69b6d69c
}
{
	ip_address=192.168.64.55
	hw_address=1,e:63:74:dc:4e:5
	identifier=1,e:63:74:dc:4e:5
	lease=0x69b5351c
}
"#;
        let ip = parse_bridge_ip_from_leases(content);
        assert_eq!(ip, Some(Ipv4Addr::new(192, 168, 65, 2)));
    }

    #[test]
    fn skip_primary_subnet() {
        let content = "{\nip_address=192.168.64.2\nlease=0x99999999\n}\n";
        assert_eq!(parse_bridge_ip_from_leases(content), None);
    }

    #[test]
    fn picks_most_recent_lease() {
        let content = r#"
{
	ip_address=192.168.65.2
	lease=0x10000000
}
{
	ip_address=192.168.65.4
	lease=0x20000000
}
{
	ip_address=192.168.64.55
	lease=0x30000000
}
"#;
        // 192.168.65.4 has higher lease timestamp than 192.168.65.2
        let ip = parse_bridge_ip_from_leases(content);
        assert_eq!(ip, Some(Ipv4Addr::new(192, 168, 65, 4)));
    }
}
