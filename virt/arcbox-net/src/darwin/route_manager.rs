//! Host-side route management for container subnet routing.
//!
//! Installs and removes routes so that host traffic destined for container
//! subnets (e.g. 172.16.0.0/12) is directed through the utun interface.
//!
//! Uses the privileged helper when available, falling back to direct
//! `/sbin/route` execution (requires root).

use std::io;
use std::process::Command;

/// Adds a route for `subnet` via `interface`.
///
/// Tries the privileged helper first, falls back to direct `/sbin/route`.
pub fn add_route(subnet: &str, interface: &str) -> io::Result<()> {
    if super::helper_client::is_available() {
        return super::helper_client::add_route(subnet, interface);
    }
    add_route_direct(subnet, interface)
}

/// Removes a route for `subnet` via `interface`.
///
/// Tries the privileged helper first, falls back to direct `/sbin/route`.
pub fn remove_route(subnet: &str, interface: &str) -> io::Result<()> {
    if super::helper_client::is_available() {
        return super::helper_client::remove_route(subnet, interface);
    }
    remove_route_direct(subnet, interface)
}

/// Direct route addition via `/sbin/route` (requires root).
fn add_route_direct(subnet: &str, interface: &str) -> io::Result<()> {
    let output = Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, "-interface", interface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, interface, "installed host route");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("File exists") {
            tracing::debug!(subnet, interface, "route already exists");
            Ok(())
        } else {
            tracing::warn!(subnet, interface, stderr = %stderr, "failed to add route");
            Err(io::Error::new(io::ErrorKind::Other, stderr.to_string()))
        }
    }
}

/// Direct route removal via `/sbin/route` (requires root).
fn remove_route_direct(subnet: &str, interface: &str) -> io::Result<()> {
    let output = Command::new("/sbin/route")
        .args(["-n", "delete", "-net", subnet, "-interface", interface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, interface, "removed host route");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(subnet, interface, stderr = %stderr, "route removal (may not exist)");
    }
    Ok(())
}
