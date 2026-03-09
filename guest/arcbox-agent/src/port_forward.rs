//! Port forwarding manager for sandbox access.
//!
//! Sets up iptables DNAT rules to forward a port on the guest VM to a port
//! inside a Firecracker sandbox.  The guest VM's vmnet IP is directly
//! reachable from macOS, so the returned address needs no macOS-side
//! coordination.
//!
//! Host ports are allocated from the reserved range [`PORT_RANGE_START`] –
//! [`PORT_RANGE_END`] and are managed exclusively by this module.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::process::Command;
use std::str::FromStr;

const PORT_RANGE_START: u16 = 40000;
const PORT_RANGE_END: u16 = 49999;

// =============================================================================
// Entry
// =============================================================================

struct ForwardEntry {
    sandbox_ip: Ipv4Addr,
    sandbox_port: u16,
    host_port: u16,
    protocol: String,
}

// =============================================================================
// Manager
// =============================================================================

/// Manages iptables DNAT rules that expose sandbox ports on the guest VM.
///
/// Each rule forwards traffic arriving at `guest_vm_ip:host_port` to
/// `sandbox_ip:sandbox_port` via kernel-level packet rewriting.
pub struct PortForwardManager {
    /// Active rules keyed by (sandbox_id, sandbox_port, protocol).
    allocations: HashMap<(String, u16, String), ForwardEntry>,
    /// Next candidate host port in `[PORT_RANGE_START, PORT_RANGE_END]`.
    next_port: u16,
    /// Cached guest VM IP (vmnet address reachable from macOS).
    guest_ip: Option<Ipv4Addr>,
}

impl PortForwardManager {
    pub fn new() -> Self {
        Self {
            allocations: HashMap::new(),
            next_port: PORT_RANGE_START,
            guest_ip: None,
        }
    }

    /// Add an iptables DNAT rule forwarding `sandbox_id:sandbox_port`.
    ///
    /// Returns the guest VM socket address (e.g. `"192.168.64.2:40001"`)
    /// that callers can connect to from macOS.  If a rule for the same
    /// `(sandbox_id, sandbox_port, protocol)` already exists the existing
    /// address is returned without creating a duplicate rule.
    pub fn add(
        &mut self,
        sandbox_id: &str,
        sandbox_ip: Ipv4Addr,
        sandbox_port: u16,
        protocol: &str,
    ) -> Result<String, String> {
        let key = (sandbox_id.to_string(), sandbox_port, protocol.to_string());
        if let Some(host_port) = self.allocations.get(&key).map(|e| e.host_port) {
            let guest_ip = self.guest_ip()?;
            return Ok(format!("{}:{}", guest_ip, host_port));
        }

        let host_port = self.alloc_port()?;
        let guest_ip = self.guest_ip()?;

        self.ensure_ip_forward()?;
        self.add_dnat(protocol, host_port, sandbox_ip, sandbox_port)?;

        self.allocations.insert(
            key,
            ForwardEntry {
                sandbox_ip,
                sandbox_port,
                host_port,
                protocol: protocol.to_string(),
            },
        );

        tracing::info!(
            sandbox_id,
            %sandbox_ip,
            sandbox_port,
            host_port,
            protocol,
            "sandbox port forward added"
        );

        Ok(format!("{}:{}", guest_ip, host_port))
    }

    /// Remove the iptables DNAT rule for `sandbox_id:sandbox_port`.
    ///
    /// No-ops if the rule does not exist.
    pub fn remove(
        &mut self,
        sandbox_id: &str,
        sandbox_port: u16,
        protocol: &str,
    ) -> Result<(), String> {
        let key = (sandbox_id.to_string(), sandbox_port, protocol.to_string());
        let entry = match self.allocations.get(&key) {
            Some(e) => e,
            None => return Ok(()),
        };

        self.remove_dnat(
            &entry.protocol,
            entry.host_port,
            entry.sandbox_ip,
            entry.sandbox_port,
        )?;

        let entry = self.allocations.remove(&key).expect("entry was just get");
        tracing::info!(
            sandbox_id,
            sandbox_port,
            host_port = entry.host_port,
            protocol,
            "sandbox port forward removed"
        );

        Ok(())
    }

    /// Remove all iptables DNAT rules associated with `sandbox_id`.
    ///
    /// Called automatically when a sandbox is stopped or removed.
    pub fn remove_all_for_sandbox(&mut self, sandbox_id: &str) {
        let keys: Vec<_> = self
            .allocations
            .keys()
            .filter(|(id, _, _)| id == sandbox_id)
            .cloned()
            .collect();

        for key in keys {
            if let Some(entry) = self.allocations.get(&key) {
                if let Err(e) = self.remove_dnat(
                    &entry.protocol,
                    entry.host_port,
                    entry.sandbox_ip,
                    entry.sandbox_port,
                ) {
                    tracing::warn!(
                        sandbox_id,
                        sandbox_port = entry.sandbox_port,
                        error = %e,
                        "failed to remove port forward on sandbox cleanup"
                    );
                } else {
                    self.allocations.remove(&key);
                }
            }
        }
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Return the cached guest VM IP, discovering it on first call.
    fn guest_ip(&mut self) -> Result<Ipv4Addr, String> {
        if let Some(ip) = self.guest_ip {
            return Ok(ip);
        }
        let ip = discover_guest_ip()?;
        self.guest_ip = Some(ip);
        Ok(ip)
    }

    /// Find the next free port in `[PORT_RANGE_START, PORT_RANGE_END]`.
    fn alloc_port(&mut self) -> Result<u16, String> {
        let used: std::collections::HashSet<u16> =
            self.allocations.values().map(|e| e.host_port).collect();

        let start = self.next_port;
        let mut port = start;
        loop {
            if !used.contains(&port) {
                self.next_port = if port == PORT_RANGE_END {
                    PORT_RANGE_START
                } else {
                    port + 1
                };
                return Ok(port);
            }
            port = if port == PORT_RANGE_END {
                PORT_RANGE_START
            } else {
                port + 1
            };
            if port == start {
                return Err("port forward range exhausted".into());
            }
        }
    }

    /// Enable kernel IP forwarding (idempotent).
    fn ensure_ip_forward(&self) -> Result<(), String> {
        run_cmd(
            "sysctl",
            &["-w", "net.ipv4.ip_forward=1"],
            "enable ip_forward",
        )
    }

    /// Add the PREROUTING DNAT rule and FORWARD ACCEPT rule.
    fn add_dnat(
        &self,
        protocol: &str,
        host_port: u16,
        sandbox_ip: Ipv4Addr,
        sandbox_port: u16,
    ) -> Result<(), String> {
        let dest = format!("{}:{}", sandbox_ip, sandbox_port);
        run_cmd(
            "iptables",
            &[
                "-t",
                "nat",
                "-A",
                "PREROUTING",
                "-p",
                protocol,
                "--dport",
                &host_port.to_string(),
                "-j",
                "DNAT",
                "--to-destination",
                &dest,
            ],
            "add PREROUTING DNAT",
        )?;
        run_cmd(
            "iptables",
            &[
                "-A",
                "FORWARD",
                "-p",
                protocol,
                "-d",
                &sandbox_ip.to_string(),
                "--dport",
                &sandbox_port.to_string(),
                "-j",
                "ACCEPT",
            ],
            "add FORWARD ACCEPT",
        )
    }

    /// Remove the PREROUTING DNAT rule and FORWARD ACCEPT rule.
    fn remove_dnat(
        &self,
        protocol: &str,
        host_port: u16,
        sandbox_ip: Ipv4Addr,
        sandbox_port: u16,
    ) -> Result<(), String> {
        let dest = format!("{}:{}", sandbox_ip, sandbox_port);
        run_cmd(
            "iptables",
            &[
                "-t",
                "nat",
                "-D",
                "PREROUTING",
                "-p",
                protocol,
                "--dport",
                &host_port.to_string(),
                "-j",
                "DNAT",
                "--to-destination",
                &dest,
            ],
            "remove PREROUTING DNAT",
        )?;
        run_cmd(
            "iptables",
            &[
                "-D",
                "FORWARD",
                "-p",
                protocol,
                "-d",
                &sandbox_ip.to_string(),
                "--dport",
                &sandbox_port.to_string(),
                "-j",
                "ACCEPT",
            ],
            "remove FORWARD ACCEPT",
        )
    }
}

// =============================================================================
// Platform helpers
// =============================================================================

/// Discover the guest VM's IP on the default route interface.
///
/// Parses `ip route get 8.8.8.8` output and extracts the `src` field.
fn discover_guest_ip() -> Result<Ipv4Addr, String> {
    let output = Command::new("ip")
        .args(["route", "get", "8.8.8.8"])
        .output()
        .map_err(|e| format!("ip route get: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output example:
    //   8.8.8.8 via 192.168.64.1 dev eth0 src 192.168.64.2 uid 0
    let mut tokens = stdout.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "src" {
            if let Some(ip_str) = tokens.next() {
                return Ipv4Addr::from_str(ip_str)
                    .map_err(|e| format!("invalid src IP '{ip_str}': {e}"));
            }
        }
    }

    Err(format!(
        "could not find src IP in `ip route get 8.8.8.8` output: {stdout}"
    ))
}

/// Run a command, returning an error if it fails.
fn run_cmd(bin: &str, args: &[&str], desc: &str) -> Result<(), String> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| format!("{desc}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{desc} failed: exit {status}"))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_port_sequential() {
        let mut mgr = PortForwardManager::new();
        let p1 = mgr.alloc_port().unwrap();
        let p2 = mgr.alloc_port().unwrap();
        assert_eq!(p1, PORT_RANGE_START);
        assert_eq!(p2, PORT_RANGE_START + 1);
    }

    #[test]
    fn alloc_port_skips_in_use() {
        let mut mgr = PortForwardManager::new();
        // Manually insert a ForwardEntry to simulate an in-use port.
        mgr.allocations.insert(
            ("s".into(), 80, "tcp".into()),
            ForwardEntry {
                sandbox_ip: Ipv4Addr::new(172, 16, 0, 2),
                sandbox_port: 80,
                host_port: PORT_RANGE_START,
                protocol: "tcp".into(),
            },
        );
        let p = mgr.alloc_port().unwrap();
        assert_eq!(p, PORT_RANGE_START + 1);
    }

    #[test]
    fn remove_all_for_sandbox_clears_entries() {
        let mut mgr = PortForwardManager::new();
        let ip = Ipv4Addr::new(172, 16, 0, 2);
        mgr.allocations.insert(
            ("sandbox-1".into(), 80, "tcp".into()),
            ForwardEntry {
                sandbox_ip: ip,
                sandbox_port: 80,
                host_port: 40000,
                protocol: "tcp".into(),
            },
        );
        mgr.allocations.insert(
            ("sandbox-1".into(), 443, "tcp".into()),
            ForwardEntry {
                sandbox_ip: ip,
                sandbox_port: 443,
                host_port: 40001,
                protocol: "tcp".into(),
            },
        );
        mgr.allocations.insert(
            ("sandbox-2".into(), 8080, "tcp".into()),
            ForwardEntry {
                sandbox_ip: ip,
                sandbox_port: 8080,
                host_port: 40002,
                protocol: "tcp".into(),
            },
        );
        // remove_dnat will fail (no iptables in test), but entries are cleared first.
        mgr.remove_all_for_sandbox("sandbox-1");
        assert!(
            !mgr.allocations
                .contains_key(&("sandbox-1".into(), 80, "tcp".into()))
        );
        assert!(
            !mgr.allocations
                .contains_key(&("sandbox-1".into(), 443, "tcp".into()))
        );
        assert!(
            mgr.allocations
                .contains_key(&("sandbox-2".into(), 8080, "tcp".into()))
        );
    }
}
