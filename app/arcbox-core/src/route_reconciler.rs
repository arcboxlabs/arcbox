//! Route lifecycle management for L3 direct routing (macOS).
//!
//! The daemon owns all route intelligence:
//! - Bridge discovery via `ifbridge` (kernel FDB, no text parsing)
//! - Route state decisions (add, replace, remove)
//!
//! The ArcBoxHelper is a pure mutation executor — the daemon tells it
//! exactly which interface to add/remove a route for. No MAC resolution
//! or route status queries happen in the helper.

use std::process::Command;

use crate::bridge_discovery;

/// Container subnet to route.
const CONTAINER_SUBNET: &str = "172.16.0.0/12";

/// Path to the ArcBoxHelper binary.
fn helper_path() -> String {
    let candidates = ["/Applications/ArcBox Desktop.app/Contents/Library/HelperTools/ArcBoxHelper"];
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = dir.join("ArcBoxHelper");
            if helper.exists() {
                return helper.to_string_lossy().to_string();
            }
        }
    }
    "ArcBoxHelper".to_string()
}

/// Ensures the container subnet route points to the correct bridge.
///
/// 1. Resolves bridge MAC → bridge interface via kernel FDB (`ifbridge`)
/// 2. Calls ArcBoxHelper to add/replace the route
///
/// Called on: VM ready, VM recovery, daemon cold-start reconcile.
pub fn ensure_route(bridge_mac: &str) {
    // Step 1: Resolve MAC → bridge via kernel FDB (typed API, no text parsing).
    let bridge = match bridge_discovery::resolve_bridge_by_mac(bridge_mac) {
        Some(b) => b,
        None => {
            tracing::warn!(
                %bridge_mac,
                "no bridge found for VM MAC in kernel FDB"
            );
            return;
        }
    };

    // Step 2: Tell helper to add the route (pure mutation, no intelligence).
    let ctl = helper_path();
    match Command::new(&ctl)
        .args([
            "route",
            "add-interface",
            "--subnet",
            CONTAINER_SUBNET,
            "--iface",
            &bridge.name,
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            tracing::info!(
                subnet = CONTAINER_SUBNET,
                bridge = %bridge.name,
                %bridge_mac,
                "route ensured"
            );
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "File exists" means the route is already installed — not an error.
            if stderr.contains("File exists") {
                tracing::info!(
                    subnet = CONTAINER_SUBNET,
                    bridge = %bridge.name,
                    "route already installed"
                );
            } else {
                tracing::warn!(
                    subnet = CONTAINER_SUBNET,
                    bridge = %bridge.name,
                    stderr = %stderr.trim(),
                    "route add failed"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to run ArcBoxHelper (is ArcBox Desktop installed?)"
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
        .args(["route", "remove-interface", "--subnet", CONTAINER_SUBNET])
        .output()
    {
        Ok(output) if output.status.success() => {
            tracing::info!(subnet = CONTAINER_SUBNET, "route removed");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not in table") {
                tracing::debug!(subnet = CONTAINER_SUBNET, "route already absent");
            } else {
                tracing::debug!(
                    subnet = CONTAINER_SUBNET,
                    stderr = %stderr.trim(),
                    "route remove: non-zero exit"
                );
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "ArcBoxHelper not available for route cleanup");
        }
    }
}
