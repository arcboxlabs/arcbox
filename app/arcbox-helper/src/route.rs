//! Route management via `/sbin/route`.
//!
//! Adds or removes a host route for the container subnet through a given
//! bridge interface. This is the only way the daemon modifies the host
//! routing table — all intelligence lives in the daemon's route_reconciler.

use std::process::Command;

use crate::validate;

/// Adds a route for `subnet` via `iface`.
///
/// Invokes `/sbin/route -n add -net <subnet> -interface <iface>`.
/// Idempotent: returns Ok if the route already exists.
pub fn add(subnet: &str, iface: &str) -> Result<(), String> {
    validate::validate_subnet(subnet)?;
    validate::validate_iface(iface)?;

    let output = Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, "-interface", iface])
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
    validate::validate_subnet(subnet)?;

    let output = Command::new("/sbin/route")
        .args(["-n", "delete", "-net", subnet])
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
