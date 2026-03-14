//! Host-side route management for container subnet routing.
//!
//! Installs and removes routes via `/sbin/route` so that host traffic
//! destined for container subnets (e.g. 172.17.0.0/16) is directed
//! through the utun interface.

use std::io;
use std::process::Command;

/// Adds a route for `subnet` (e.g. "172.17.0.0/16") via `interface` (e.g. "utun5").
///
/// Runs: `/sbin/route -n add -net <subnet> -interface <interface>`
///
/// Idempotent: if the route already exists, the error is logged but not propagated.
pub fn add_route(subnet: &str, interface: &str) -> io::Result<()> {
    let output = Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, "-interface", interface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, interface, "installed host route");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "File exists" means the route is already installed — not an error.
        if stderr.contains("File exists") {
            tracing::debug!(subnet, interface, "route already exists");
            Ok(())
        } else {
            tracing::warn!(subnet, interface, stderr = %stderr, "failed to add route");
            Err(io::Error::new(io::ErrorKind::Other, stderr.to_string()))
        }
    }
}

/// Removes a route for `subnet` via `interface`.
///
/// Runs: `/sbin/route -n delete -net <subnet> -interface <interface>`
///
/// Best-effort: failure is logged but not propagated.
pub fn remove_route(subnet: &str, interface: &str) -> io::Result<()> {
    let output = Command::new("/sbin/route")
        .args(["-n", "delete", "-net", subnet, "-interface", interface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, interface, "removed host route");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(subnet, interface, stderr = %stderr, "route removal failed (may not exist)");
    }
    Ok(())
}
