use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::error::{Result, VmmError};

/// Result of allocating network resources for a single VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkAllocation {
    /// TAP interface name (e.g. `vmtap0`).
    pub tap_name: String,
    /// IP address assigned to the guest.
    pub ip_address: Ipv4Addr,
    /// Network prefix length (e.g. 16 for /16).
    pub prefix_len: u8,
    /// Gateway IP.
    pub gateway: Ipv4Addr,
    /// MAC address (deterministic from VM ID).
    pub mac_address: String,
    /// DNS servers.
    pub dns_servers: Vec<String>,
}

impl NetworkAllocation {
    /// Return the subnet mask as an `Ipv4Addr` (e.g. prefix_len 16 → 255.255.0.0).
    pub fn netmask(&self) -> Ipv4Addr {
        if self.prefix_len == 0 {
            Ipv4Addr::new(0, 0, 0, 0)
        } else {
            Ipv4Addr::from(!0u32 << (32 - self.prefix_len))
        }
    }
}

/// Shared manager for TAP interfaces and guest IP addresses.
pub struct NetworkManager {
    /// Bridge interface name.
    #[allow(dead_code)]
    bridge: String,
    /// Base IP from which the pool starts (host-octet 2 onwards).
    base: Ipv4Addr,
    /// Network prefix length (e.g. 16 for /16).
    prefix_len: u8,
    /// Gateway IP.
    gateway: Ipv4Addr,
    /// DNS servers.
    dns: Vec<String>,
    /// Set of already-allocated guest IPs.
    allocated: Mutex<HashSet<u32>>,
}

impl NetworkManager {
    /// Create a new manager from the network configuration.
    ///
    /// `cidr` must be in `a.b.c.d/n` notation (e.g. `"172.20.0.0/16"`).
    pub fn new(bridge: &str, cidr: &str, gateway: &str, dns: Vec<String>) -> Result<Self> {
        let (base, prefix_len) = parse_cidr(cidr)?;
        if !(1..=30).contains(&prefix_len) {
            return Err(VmmError::Network(format!(
                "prefix length {prefix_len} out of range 1–30"
            )));
        }
        let gateway = gateway
            .parse::<Ipv4Addr>()
            .map_err(|e| VmmError::Network(format!("invalid gateway: {e}")))?;

        #[cfg(target_os = "linux")]
        if !bridge.is_empty() {
            ensure_bridge(bridge, gateway, prefix_len)?;
        }

        Ok(Self {
            bridge: bridge.to_owned(),
            base,
            prefix_len,
            gateway,
            dns,
            allocated: Mutex::new(HashSet::new()),
        })
    }

    /// Allocate a TAP interface and guest IP for `vm_id`.
    ///
    /// On Linux this creates a real TAP device and attaches it to the bridge.
    /// The call is best-effort on non-Linux platforms (tests / macOS CI).
    pub fn allocate(&self, vm_id: &str) -> Result<NetworkAllocation> {
        let ip = self.next_ip()?;
        let tap_name = tap_name_from_ip(ip);
        let mac = mac_from_vm_id(vm_id);

        info!(vm_id, tap = %tap_name, ip = %ip, "allocating network");

        #[cfg(target_os = "linux")]
        if let Err(e) = self.create_tap(&tap_name) {
            self.allocated.lock().unwrap().remove(&u32::from(ip));
            return Err(e);
        }

        Ok(NetworkAllocation {
            tap_name,
            ip_address: ip,
            prefix_len: self.prefix_len,
            gateway: self.gateway,
            mac_address: mac,
            dns_servers: self.dns.clone(),
        })
    }

    /// Release the TAP interface and guest IP associated with `vm_id`.
    pub fn release(&self, alloc: &NetworkAllocation) {
        let ip_int = u32::from(alloc.ip_address);
        self.allocated.lock().unwrap().remove(&ip_int);

        debug!(tap = %alloc.tap_name, ip = %alloc.ip_address, "releasing network");

        #[cfg(target_os = "linux")]
        destroy_tap(&alloc.tap_name);
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    fn next_ip(&self) -> Result<Ipv4Addr> {
        // prefix_len is validated to 1..=30 in new(), so shifts are safe.
        let host_bits = 32 - u32::from(self.prefix_len);
        let mask = !((1u32 << host_bits) - 1);
        let host_max = (1u32 << host_bits) - 2; // excludes network address (0) and broadcast

        // Mask away any host bits so arithmetic stays within the subnet.
        let network_base = u32::from(self.base) & mask;

        let mut allocated = self.allocated.lock().unwrap();
        // offset 0 = network address, 1 = gateway; start at 2.
        for offset in 2..=host_max {
            let candidate = network_base + offset;
            if !allocated.contains(&candidate) {
                allocated.insert(candidate);
                return Ok(Ipv4Addr::from(candidate));
            }
        }
        Err(VmmError::Network("IP pool exhausted".into()))
    }

    #[cfg(target_os = "linux")]
    fn create_tap(&self, tap_name: &str) -> Result<()> {
        use std::os::fd::FromRawFd;
        use std::os::unix::io::AsRawFd;

        // Remove any stale TAP left over from a previous crashed run.
        destroy_tap(tap_name);

        let name_bytes = tap_name.as_bytes();
        if name_bytes.len() >= libc::IFNAMSIZ {
            return Err(VmmError::Network(format!("TAP name too long: {tap_name}")));
        }

        // 1. Create persistent TAP device via /dev/net/tun.
        let tun = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")
            .map_err(|e| VmmError::Network(format!("open /dev/net/tun: {e}")))?;

        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        // SAFETY: name_bytes.len() < IFNAMSIZ checked above; ifr is zero-initialized.
        unsafe {
            std::ptr::copy_nonoverlapping(
                name_bytes.as_ptr(),
                ifr.ifr_name.as_mut_ptr().cast::<u8>(),
                name_bytes.len(),
            );
            ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI) as i16;
        }

        // ioctl request codes — defined as c_ulong, cast via `as _` to match
        // the platform's ioctl signature (c_ulong on glibc, c_int on musl).
        const TUNSETIFF: libc::c_ulong = 0x400454ca;
        const TUNSETPERSIST: libc::c_ulong = 0x400454cb;

        // SAFETY: tun fd is valid, ifr is initialized with name and flags.
        if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETIFF as _, &ifr) } < 0 {
            return Err(VmmError::Network(format!(
                "TUNSETIFF {tap_name}: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Make persistent so Firecracker can reopen the TAP by name after we
        // close this fd.
        // SAFETY: tun fd is attached to the TAP device after TUNSETIFF.
        if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETPERSIST as _, 1i32) } < 0 {
            return Err(VmmError::Network(format!(
                "TUNSETPERSIST {tap_name}: {}",
                std::io::Error::last_os_error()
            )));
        }
        drop(tun);

        // 2. Bring interface up via ioctl on a helper socket.
        // SAFETY: standard socket creation.
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            destroy_tap(tap_name);
            return Err(VmmError::Network(format!(
                "socket: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: sock is a valid fd returned by socket().
        let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(sock) };

        // SAFETY: sock and ifr.ifr_name are valid; kernel writes ifr_flags.
        if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS as _, &ifr) } < 0 {
            destroy_tap(tap_name);
            return Err(VmmError::Network(format!(
                "SIOCGIFFLAGS {tap_name}: {}",
                std::io::Error::last_os_error()
            )));
        }
        unsafe { ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16 };
        // SAFETY: sock and ifr are valid; kernel reads ifr_name and ifr_flags.
        if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
            destroy_tap(tap_name);
            return Err(VmmError::Network(format!(
                "SIOCSIFFLAGS UP {tap_name}: {}",
                std::io::Error::last_os_error()
            )));
        }

        // 3. Attach to bridge via SIOCBRADDIF.
        if !self.bridge.is_empty() {
            // SAFETY: sock and ifr.ifr_name are valid.
            if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFINDEX as _, &ifr) } < 0 {
                destroy_tap(tap_name);
                return Err(VmmError::Network(format!(
                    "SIOCGIFINDEX {tap_name}: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let if_index = unsafe { ifr.ifr_ifru.ifru_ifindex };

            let bridge_bytes = self.bridge.as_bytes();
            if bridge_bytes.len() >= libc::IFNAMSIZ {
                destroy_tap(tap_name);
                return Err(VmmError::Network(format!(
                    "bridge name too long: {}",
                    self.bridge
                )));
            }
            let mut br_ifr: libc::ifreq = unsafe { std::mem::zeroed() };
            // SAFETY: bridge_bytes.len() < IFNAMSIZ checked above.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bridge_bytes.as_ptr(),
                    br_ifr.ifr_name.as_mut_ptr().cast::<u8>(),
                    bridge_bytes.len(),
                );
                br_ifr.ifr_ifru.ifru_ifindex = if_index;
            }

            const SIOCBRADDIF: libc::c_ulong = 0x89a2;
            // SAFETY: sock is valid; br_ifr has bridge name and TAP ifindex.
            if unsafe { libc::ioctl(sock.as_raw_fd(), SIOCBRADDIF as _, &br_ifr) } < 0 {
                destroy_tap(tap_name);
                return Err(VmmError::Network(format!(
                    "SIOCBRADDIF {tap_name} -> {}: {}",
                    self.bridge,
                    std::io::Error::last_os_error()
                )));
            }
        }

        Ok(())
    }
}

// =============================================================================
// Platform helpers
// =============================================================================

/// Creates a Linux bridge if it does not already exist, assigns the gateway IP,
/// and brings it up.
#[cfg(target_os = "linux")]
fn ensure_bridge(name: &str, gateway: Ipv4Addr, prefix_len: u8) -> Result<()> {
    use std::os::fd::FromRawFd;
    use std::os::unix::io::AsRawFd;

    let name_bytes = name.as_bytes();
    if name_bytes.len() >= libc::IFNAMSIZ {
        return Err(VmmError::Network(format!("bridge name too long: {name}")));
    }

    // SAFETY: standard socket creation.
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(VmmError::Network(format!(
            "socket: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: sock is a valid fd returned by socket().
    let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(sock) };

    // 1. Create bridge (SIOCBRADDBR). Ignore EEXIST — bridge may already exist.
    const SIOCBRADDBR: libc::c_ulong = 0x89a0;
    let mut br_name = [0u8; libc::IFNAMSIZ];
    br_name[..name_bytes.len()].copy_from_slice(name_bytes);

    // SAFETY: sock is valid; br_name is a null-terminated interface name.
    let ret = unsafe { libc::ioctl(sock.as_raw_fd(), SIOCBRADDBR as _, br_name.as_ptr()) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(VmmError::Network(format!("SIOCBRADDBR {name}: {err}")));
        }
        debug!(bridge = name, "bridge already exists, reusing");
    } else {
        info!(bridge = name, "created bridge");
    }

    // 2. Assign gateway IP + netmask via SIOCSIFADDR / SIOCSIFNETMASK.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    // SAFETY: name_bytes.len() < IFNAMSIZ checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            ifr.ifr_name.as_mut_ptr().cast::<u8>(),
            name_bytes.len(),
        );
    }

    // Build sockaddr_in for gateway IP.
    let mut addr_in: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    addr_in.sin_family = libc::AF_INET as libc::sa_family_t;
    addr_in.sin_addr.s_addr = u32::from(gateway).to_be();

    // SAFETY: ifr and addr_in are properly initialized, sizes match.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &addr_in as *const libc::sockaddr_in as *const u8,
            &mut ifr.ifr_ifru as *mut _ as *mut u8,
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }
    // SAFETY: sock and ifr are valid.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFADDR as _, &ifr) } < 0 {
        return Err(VmmError::Network(format!(
            "SIOCSIFADDR {name} {gateway}: {}",
            std::io::Error::last_os_error()
        )));
    }

    // Build netmask from prefix_len.
    let mask = if prefix_len == 0 {
        0u32
    } else {
        !((1u32 << (32 - prefix_len)) - 1)
    };
    addr_in.sin_addr.s_addr = mask.to_be();
    // SAFETY: same layout copy as above.
    unsafe {
        std::ptr::copy_nonoverlapping(
            &addr_in as *const libc::sockaddr_in as *const u8,
            &mut ifr.ifr_ifru as *mut _ as *mut u8,
            std::mem::size_of::<libc::sockaddr_in>(),
        );
    }
    // SAFETY: sock and ifr are valid.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFNETMASK as _, &ifr) } < 0 {
        return Err(VmmError::Network(format!(
            "SIOCSIFNETMASK {name}: {}",
            std::io::Error::last_os_error()
        )));
    }

    // 3. Bring bridge UP.
    // SAFETY: sock and ifr are valid; kernel writes ifr_flags.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS as _, &ifr) } < 0 {
        return Err(VmmError::Network(format!(
            "SIOCGIFFLAGS {name}: {}",
            std::io::Error::last_os_error()
        )));
    }
    unsafe { ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16 };
    // SAFETY: sock and ifr are valid.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as _, &ifr) } < 0 {
        return Err(VmmError::Network(format!(
            "SIOCSIFFLAGS UP {name}: {}",
            std::io::Error::last_os_error()
        )));
    }

    info!(bridge = name, %gateway, prefix_len, "bridge ready");
    Ok(())
}

/// Destroys a persistent TAP device by clearing its persist flag.
///
/// Re-attaches to the named TAP via `/dev/net/tun`, clears `TUNSETPERSIST`,
/// then closes the fd — the kernel removes the interface automatically.
#[cfg(target_os = "linux")]
fn destroy_tap(tap_name: &str) {
    use std::os::unix::io::AsRawFd;

    let name_bytes = tap_name.as_bytes();
    if name_bytes.len() >= libc::IFNAMSIZ {
        return;
    }

    let Ok(tun) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
    else {
        return;
    };

    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    // SAFETY: name_bytes.len() < IFNAMSIZ checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            ifr.ifr_name.as_mut_ptr().cast::<u8>(),
            name_bytes.len(),
        );
        ifr.ifr_ifru.ifru_flags = (libc::IFF_TAP | libc::IFF_NO_PI) as i16;
    }

    const TUNSETIFF: libc::c_ulong = 0x400454ca;
    const TUNSETPERSIST: libc::c_ulong = 0x400454cb;

    // SAFETY: tun fd is valid, ifr is properly initialized.
    if unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETIFF as _, &ifr) } < 0 {
        return;
    }
    // SAFETY: tun fd is attached to the TAP device.
    let _ = unsafe { libc::ioctl(tun.as_raw_fd(), TUNSETPERSIST as _, 0i32) };
    // Drop closes the fd → kernel destroys the non-persistent TAP.
}

fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(VmmError::Network(format!("invalid CIDR: {cidr}")));
    }
    let addr = parts[0]
        .parse::<Ipv4Addr>()
        .map_err(|e| VmmError::Network(format!("invalid CIDR address: {e}")))?;
    let prefix: u8 = parts[1]
        .parse()
        .map_err(|e| VmmError::Network(format!("invalid prefix length: {e}")))?;
    Ok((addr, prefix))
}

fn tap_name_from_ip(ip: Ipv4Addr) -> String {
    let octets = ip.octets();
    // Encode last two octets with a delimiter to keep name short and unambiguous.
    format!("vmtap{}-{}", octets[2], octets[3])
}

fn mac_from_vm_id(vm_id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    vm_id.hash(&mut hasher);
    let h = hasher.finish();
    // Locally administered, unicast: set bit 1 of first octet, clear bit 0.
    let b: [u8; 6] = [
        0x02 | (((h >> 40) & 0xfe) as u8),
        ((h >> 32) & 0xff) as u8,
        ((h >> 24) & 0xff) as u8,
        ((h >> 16) & 0xff) as u8,
        ((h >> 8) & 0xff) as u8,
        (h & 0xff) as u8,
    ];
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns true when the process effective UID is 0.
    /// TAP creation requires root on Linux; tests that call `allocate()` skip
    /// when this returns false.
    #[cfg(target_os = "linux")]
    fn is_root() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .map(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(2))
                    .map(|uid| uid == "0")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    #[test]
    fn test_allocate_sequential_ips() {
        #[cfg(target_os = "linux")]
        if !is_root() {
            eprintln!("SKIP test_allocate_sequential_ips — requires root (TAP creation)");
            return;
        }
        let mgr = NetworkManager::new("fcvmm0", "172.20.0.0/16", "172.20.0.1", vec![]).unwrap();
        let a1 = mgr.allocate("vm-1").unwrap();
        let a2 = mgr.allocate("vm-2").unwrap();
        assert_ne!(a1.ip_address, a2.ip_address);
    }

    #[test]
    fn test_release_returns_ip_to_pool() {
        #[cfg(target_os = "linux")]
        if !is_root() {
            eprintln!("SKIP test_release_returns_ip_to_pool — requires root (TAP creation)");
            return;
        }
        let mgr = NetworkManager::new("fcvmm0", "172.20.0.0/16", "172.20.0.1", vec![]).unwrap();
        let a1 = mgr.allocate("vm-1").unwrap();
        let first_ip = a1.ip_address;
        mgr.release(&a1);
        let a2 = mgr.allocate("vm-1").unwrap();
        assert_eq!(a2.ip_address, first_ip);
    }

    #[test]
    fn test_mac_deterministic() {
        assert_eq!(mac_from_vm_id("abc"), mac_from_vm_id("abc"));
        assert_ne!(mac_from_vm_id("abc"), mac_from_vm_id("xyz"));
    }

    #[test]
    fn test_invalid_prefix_len_rejected() {
        assert!(NetworkManager::new("br0", "10.0.0.0/0", "10.0.0.1", vec![]).is_err());
        assert!(NetworkManager::new("br0", "10.0.0.0/31", "10.0.0.1", vec![]).is_err());
        assert!(NetworkManager::new("br0", "10.0.0.0/32", "10.0.0.1", vec![]).is_err());
        assert!(NetworkManager::new("br0", "10.0.0.0/24", "10.0.0.1", vec![]).is_ok());
    }

    #[test]
    fn test_next_ip_respects_subnet_boundary() {
        #[cfg(target_os = "linux")]
        if !is_root() {
            eprintln!("SKIP test_next_ip_respects_subnet_boundary — requires root (TAP creation)");
            return;
        }
        // /30 has exactly 2 host addresses (.1 gateway, .2 first usable)
        let mgr = NetworkManager::new("br0", "10.0.0.0/30", "10.0.0.1", vec![]).unwrap();
        let a = mgr.allocate("vm-1").unwrap();
        assert_eq!(a.ip_address, "10.0.0.2".parse::<Ipv4Addr>().unwrap());
        // Pool is now exhausted
        assert!(mgr.allocate("vm-2").is_err());
    }

    #[test]
    fn test_pool_exhaustion_on_slash29() {
        #[cfg(target_os = "linux")]
        if !is_root() {
            eprintln!("SKIP test_pool_exhaustion_on_slash29 — requires root (TAP creation)");
            return;
        }
        // /29 has 6 usable addresses; gateway takes offset 1, leaving 5 for VMs.
        let mgr = NetworkManager::new("br0", "10.0.0.0/29", "10.0.0.1", vec![]).unwrap();
        for i in 0..5 {
            mgr.allocate(&format!("vm-{i}")).unwrap();
        }
        assert!(mgr.allocate("vm-overflow").is_err());
    }

    #[test]
    fn test_mac_unicast_and_locally_administered_bits() {
        let mac = mac_from_vm_id("test-vm");
        let first_byte = u8::from_str_radix(&mac[..2], 16).unwrap();
        // Bit 1 set → locally administered; bit 0 clear → unicast.
        assert_eq!(
            first_byte & 0x02,
            0x02,
            "locally administered bit must be set"
        );
        assert_eq!(first_byte & 0x01, 0x00, "multicast bit must be clear");
    }

    #[test]
    fn test_tap_name_encodes_last_two_octets() {
        let ip: Ipv4Addr = "172.20.3.17".parse().unwrap();
        assert_eq!(tap_name_from_ip(ip), "vmtap3-17");

        let ip2: Ipv4Addr = "10.0.255.1".parse().unwrap();
        assert_eq!(tap_name_from_ip(ip2), "vmtap255-1");
    }
}
