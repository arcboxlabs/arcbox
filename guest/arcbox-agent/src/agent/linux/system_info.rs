//! `GetSystemInfo` RPC handler and the underlying guest-state collector.

use std::net::IpAddr;

use arcbox_connect::v1::SystemInfo;

use crate::rpc::RpcResponse;

/// Whether the distro's own init is still running its boot sequence.
///
/// Reads the sentinel `machine_init` arranged for (see [`crate::boot_done`]);
/// it does not inspect the init system's runtime state, which is an
/// implementation detail that reports "settled" both before that init starts
/// and after it finishes.
///
/// "No hook installed" means no signal is coming — an image whose init we do
/// not recognize, or an install that failed — and readiness must not block on
/// a signal nothing will send.
fn distro_init_pending() -> bool {
    crate::boot_done::hook_installed() && !crate::boot_done::boot_complete()
}

/// Handles a GetSystemInfo request.
pub(super) async fn handle_get_system_info() -> RpcResponse {
    let info = collect_system_info();
    RpcResponse::SystemInfo(info)
}

/// Every address on the guest's interfaces, in interface order, minus
/// loopback and IPv6 link-local — the set `hostname -I` prints.
///
/// Read from the kernel rather than from a `hostname` binary: a distro image
/// need not ship one (NixOS and Oracle Linux do not, and machine readiness
/// then never saw an address), and BusyBox's `hostname -i` resolves the host
/// *name* instead of listing interfaces — through a proxy's fake-IP DNS it
/// reported 198.18.19.141 for a machine whose only address was 10.0.2.2.
fn interface_addresses() -> Vec<String> {
    let interfaces = match nix::ifaddrs::getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(e) => {
            tracing::warn!(error = %e, "getifaddrs failed; reporting no addresses");
            return Vec::new();
        }
    };
    let mut ips = Vec::new();
    for address in interfaces.filter_map(|interface| interface.address) {
        let ip = if let Some(v4) = address.as_sockaddr_in() {
            IpAddr::V4(v4.ip())
        } else if let Some(v6) = address.as_sockaddr_in6() {
            IpAddr::V6(v6.ip())
        } else {
            continue;
        };
        let link_local = matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local());
        if ip.is_loopback() || link_local {
            continue;
        }
        let ip = ip.to_string();
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips
}

/// Collects system information from the guest.
fn collect_system_info() -> SystemInfo {
    let mut info = SystemInfo::default();

    // Kernel version
    if let Ok(uname) = nix::sys::utsname::uname() {
        info.kernel_version = uname.release().to_string_lossy().to_string();
        info.os_name = uname.sysname().to_string_lossy().to_string();
        info.os_version = uname.version().to_string_lossy().to_string();
        info.arch = uname.machine().to_string_lossy().to_string();
        info.hostname = uname.nodename().to_string_lossy().to_string();
    }

    // Memory info
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(kb) = line.split_whitespace().nth(1) {
                    if let Ok(kb_val) = kb.parse::<u64>() {
                        info.total_memory = kb_val * 1024;
                    }
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(kb) = line.split_whitespace().nth(1) {
                    if let Ok(kb_val) = kb.parse::<u64>() {
                        info.available_memory = kb_val * 1024;
                    }
                }
            }
        }
    }

    // CPU count
    info.cpu_count = std::thread::available_parallelism()
        .map(|p| p.get() as u32)
        .unwrap_or(1);

    // Load average
    if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = loadavg.split_whitespace().collect();
        if parts.len() >= 3 {
            if let Ok(load1) = parts[0].parse::<f64>() {
                info.load_average.push(load1);
            }
            if let Ok(load5) = parts[1].parse::<f64>() {
                info.load_average.push(load5);
            }
            if let Ok(load15) = parts[2].parse::<f64>() {
                info.load_average.push(load15);
            }
        }
    }

    // Uptime
    if let Ok(uptime) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = uptime.split_whitespace().next() {
            if let Ok(secs_val) = secs.parse::<f64>() {
                info.uptime = secs_val as u64;
            }
        }
    }

    info.ip_addresses = interface_addresses();
    info.distro_init_pending = distro_init_pending();

    info
}
