//! Route management via `/sbin/route`.
//!
//! Adds or removes a host route for the container subnet through a given
//! bridge interface. This is the only way the daemon modifies the host
//! routing table — all intelligence lives in the daemon's route_reconciler.

use std::process::Command;

use arcbox_helper::validate::{BridgeIface, Subnet};

/// Adds a route for `subnet` via `iface`.
///
/// Invokes `/sbin/route -n add -net <subnet> -interface <iface>`.
/// Idempotent: returns Ok if the route already exists.
pub fn add(subnet: &str, iface: &str) -> Result<(), String> {
    let subnet: Subnet = subnet.parse()?;
    let iface: BridgeIface = iface.parse()?;

    let output = Command::new("/sbin/route")
        .args(["-n", "add", "-net", &subnet.to_string(), "-interface", iface.as_str()])
        .output()
        .map_err(|e| format!("failed to execute /sbin/route: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // "File exists" means the route is already installed — idempotent success.
    if stderr.contains("File exists") {
        return Ok(());
    }

    Err(stderr.trim().to_string())
}

/// Removes the route for `subnet`.
///
/// Invokes `/sbin/route -n delete -net <subnet>`.
/// Idempotent: returns Ok if the route is already absent.
pub fn remove(subnet: &str) -> Result<(), String> {
    let subnet: Subnet = subnet.parse()?;

    let output = Command::new("/sbin/route")
        .args(["-n", "delete", "-net", &subnet.to_string()])
        .output()
        .map_err(|e| format!("failed to execute /sbin/route: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    // "not in table" means the route is already absent — idempotent success.
    if stderr.contains("not in table") {
        return Ok(());
    }

    Err(stderr.trim().to_string())
}
