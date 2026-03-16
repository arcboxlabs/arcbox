//! Route lifecycle management for L3 direct routing.
//!
//! The daemon owns the route lifecycle. It calls `arcbox-helperctl` (a thin
//! Swift CLI that bridges to the XPC helper) to ensure/remove host routes.
//!
//! The helper resolves bridge MAC → vmenet → bridge interface internally.
//! The daemon only needs to know the subnet and the VM's bridge MAC.

use std::process::Command;

/// Container subnet to route.
const CONTAINER_SUBNET: &str = "172.16.0.0/12";

/// Path to the ArcBoxHelper binary (used in CLI mode with `route` subcommand).
/// The same binary serves as XPC daemon (no args) and CLI bridge (`route ...`).
fn helper_path() -> String {
    let candidates = [
        // Production: inside the app bundle's HelperTools.
        "/Applications/ArcBox Desktop.app/Contents/Library/HelperTools/ArcBoxHelper",
    ];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }
    // Development: look next to the daemon binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = dir.join("ArcBoxHelper");
            if helper.exists() {
                return helper.to_string_lossy().to_string();
            }
        }
    }
    // Last resort: hope it's in PATH.
    "ArcBoxHelper".to_string()
}

/// Ensures the container subnet route exists, pointing to the bridge
/// identified by the VM's bridge NIC MAC address.
///
/// Called on: VM ready, VM recovery, daemon startup reconcile.
pub fn ensure_route(bridge_mac: &str) {
    let ctl = helper_path();
    match Command::new(&ctl)
        .args([
            "route",
            "ensure",
            "--subnet",
            CONTAINER_SUBNET,
            "--bridge-mac",
            bridge_mac,
        ])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.success() {
                tracing::info!(subnet = CONTAINER_SUBNET, %bridge_mac, "route ensured: {}", stdout.trim());
            } else {
                tracing::warn!(
                    subnet = CONTAINER_SUBNET,
                    %bridge_mac,
                    stdout = %stdout.trim(),
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "route ensure failed"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to run arcbox-helperctl (is ArcBox Desktop installed?)"
            );
        }
    }
}

/// Removes the container subnet route.
///
/// Called on: VM stop, daemon shutdown.
pub fn remove_route() {
    let ctl = helper_path();
    match Command::new(&ctl)
        .args(["route", "remove", "--subnet", CONTAINER_SUBNET])
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if output.status.success() {
                tracing::info!(
                    subnet = CONTAINER_SUBNET,
                    "route removed: {}",
                    stdout.trim()
                );
            } else {
                tracing::debug!(subnet = CONTAINER_SUBNET, "route remove: {}", stdout.trim());
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "helperctl not available for route cleanup");
        }
    }
}

/// Queries current route status.
#[allow(dead_code)]
pub fn route_status() -> Option<String> {
    let ctl = helper_path();
    let output = Command::new(&ctl)
        .args(["route", "status", "--subnet", CONTAINER_SUBNET])
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}
