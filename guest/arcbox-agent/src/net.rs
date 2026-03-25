//! Guest network interface detection.
//!
//! Shared helpers for identifying the primary and bridge NICs inside the guest VM.
//! Used by both `init` (to configure DHCP) and `agent` (to report `bridge_ip`).

use std::fs;

/// Skippable interface prefixes (loopback, virtual, container-created).
const SKIP_PREFIXES: &[&str] = &["dummy", "veth", "br-", "docker", "vmtap", "sit"];

fn is_virtual(name: &str) -> bool {
    name == "lo" || SKIP_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Detects the primary (first alphabetical) non-virtual network interface.
///
/// This is typically `eth0` — the NAT datapath connected through the VMM
/// socketpair/virtio-net backend.
pub fn detect_primary_interface() -> Option<String> {
    let entries = fs::read_dir("/sys/class/net").ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_virtual(&name) {
            continue;
        }
        candidates.push(name);
    }
    candidates.sort();
    candidates.into_iter().next()
}

/// Detects the bridge NIC — the non-primary, non-virtual interface connected
/// to Apple's vmnet bridge (`bridge100`).
///
/// Returns `None` when there is only a single NIC (no bridge configured).
pub fn detect_bridge_interface() -> Option<String> {
    let primary = detect_primary_interface();
    let entries = fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if is_virtual(&name) || primary.as_deref() == Some(&name) {
            continue;
        }
        return Some(name);
    }
    None
}

/// Returns the IPv4 address assigned to the bridge NIC, if any.
pub fn detect_bridge_ip() -> Option<String> {
    let bridge = detect_bridge_interface()?;
    ipv4_of_interface(&bridge)
}

/// Looks up the first IPv4 address on a given interface name via `getifaddrs`.
fn ipv4_of_interface(iface: &str) -> Option<String> {
    let addrs = nix::ifaddrs::getifaddrs().ok()?;
    for ifaddr in addrs {
        if ifaddr.interface_name != iface {
            continue;
        }
        let Some(address) = ifaddr.address else {
            continue;
        };
        if let Some(v4) = address.as_sockaddr_in() {
            let ip = v4.ip();
            if !ip.is_loopback() && !ip.is_multicast() && !ip.is_unspecified() {
                return Some(ip.to_string());
            }
        }
    }
    None
}
