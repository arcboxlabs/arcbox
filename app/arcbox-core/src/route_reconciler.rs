//! Route lifecycle management for L3 direct routing (macOS).
//!
//! The daemon owns all route intelligence:
//! - Bridge discovery via `ifbridge` (kernel FDB, no text parsing)
//! - Route state decisions (add, replace, remove)
//!
//! `arcbox-helper` is a pure mutation executor — the daemon tells it
//! exactly which interface to add/remove a route for. No MAC resolution
//! or route status queries happen in the helper.

use std::fmt;
use std::process::Command;
use std::time::Duration;

use crate::bridge_discovery;

/// Container subnet to route.
const CONTAINER_SUBNET: &str = "172.16.0.0/12";

/// Maximum retry attempts for transient route installation failures.
const MAX_ROUTE_ATTEMPTS: u32 = 5;

/// Delay between retry attempts.
const ROUTE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Errors from route installation.
///
/// All variants are retryable — the caller decides whether to retry via
/// [`ensure_route_with_retry`].
#[derive(Debug)]
pub enum RouteError {
    /// Bridge MAC not found in kernel FDB. Retryable — the bridge/FDB may
    /// not have stabilized yet (VM just started, vmenet member not learned).
    BridgeNotReady,
    /// Helper CLI failed (not running, XPC timeout, etc). Retryable.
    HelperUnavailable(String),
    /// `/sbin/route` returned an unexpected error. Retryable.
    RouteFailed(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BridgeNotReady => write!(f, "bridge not found in kernel FDB"),
            Self::HelperUnavailable(msg) => write!(f, "helper unavailable: {msg}"),
            Self::RouteFailed(msg) => write!(f, "route command failed: {msg}"),
        }
    }
}

impl std::error::Error for RouteError {}

/// Resolves the path to `arcbox-helper`.
///
/// Search order:
/// 1. `ARCBOX_HELPER_PATH` env override (for dev/testing)
/// 2. Sibling of current exe: `arcbox-helper`
/// 3. `~/.arcbox/bin/arcbox-helper`
/// 4. `/usr/local/libexec/arcbox-helper`
/// 5. `/usr/local/bin/arcbox-helper`
/// 6. Bare `arcbox-helper` (rely on PATH)
fn helper_path() -> String {
    if let Ok(path) = std::env::var("ARCBOX_HELPER_PATH") {
        return path;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = dir.join("arcbox-helper");
            if helper.exists() {
                return helper.to_string_lossy().to_string();
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        let helper = home.join(".arcbox/bin/arcbox-helper");
        if helper.exists() {
            return helper.to_string_lossy().to_string();
        }
    }

    for path in [
        "/usr/local/libexec/arcbox-helper",
        "/usr/local/bin/arcbox-helper",
    ] {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }

    "arcbox-helper".to_string()
}

/// Extracts the error message from `arcbox-helper` JSON stdout.
///
/// Output format: `{"ok":false,"error":"..."}`.
fn helper_error_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            return err.to_string();
        }
    }
    stdout.trim().to_string()
}

/// Ensures the container subnet route points to the correct bridge.
///
/// 1. Resolves bridge MAC → bridge interface via kernel FDB (`ifbridge`)
/// 2. Calls helper to add/replace the route
///
/// Returns `Ok(())` on success (including "already installed").
/// Returns a typed error on failure — all variants are retryable.
///
/// Called on: VM ready, VM recovery, daemon cold-start reconcile.
pub fn ensure_route(bridge_mac: &str) -> Result<(), RouteError> {
    // Step 1: Resolve MAC → bridge via kernel FDB (typed API, no text parsing).
    let bridge =
        bridge_discovery::resolve_bridge_by_mac(bridge_mac).ok_or(RouteError::BridgeNotReady)?;

    // Step 2: Tell helper to add the route (pure mutation, no intelligence).
    let ctl = helper_path();
    match Command::new(&ctl)
        .args([
            "route",
            "add",
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
            Ok(())
        }
        Ok(ref output) => Err(RouteError::RouteFailed(helper_error_message(output))),
        Err(e) => Err(RouteError::HelperUnavailable(e.to_string())),
    }
}

/// Ensures the container subnet route with automatic retry on transient failures.
///
/// All [`RouteError`] variants are treated as retryable. Retries up to 5 times
/// with 2-second intervals (~10s total). This covers:
/// - Bridge FDB not yet populated after VM start (~1-2s to learn MAC)
/// - Helper XPC daemon not yet ready (registration in progress)
/// - Helper mid-restart during app update (unregister → register window)
pub async fn ensure_route_with_retry(bridge_mac: &str) -> Result<(), RouteError> {
    let mac = bridge_mac.to_string();
    for attempt in 1..=MAX_ROUTE_ATTEMPTS {
        let mac_clone = mac.clone();
        let result = tokio::task::spawn_blocking(move || ensure_route(&mac_clone))
            .await
            .unwrap_or_else(|_| Err(RouteError::HelperUnavailable("task panicked".into())));

        match result {
            Ok(()) => return Ok(()),
            Err(ref e) if attempt < MAX_ROUTE_ATTEMPTS => {
                tracing::debug!(
                    attempt,
                    max_attempts = MAX_ROUTE_ATTEMPTS,
                    error = %e,
                    "route install failed, retrying"
                );
                tokio::time::sleep(ROUTE_RETRY_INTERVAL).await;
            }
            Err(e) => {
                tracing::warn!(
                    attempt,
                    error = %e,
                    "route install failed after all attempts"
                );
                return Err(e);
            }
        }
    }
    unreachable!()
}

/// Ensures the container subnet route using a known bridge interface name.
///
/// When vmnet.framework creates the bridge, we know the interface immediately —
/// no need to scan the kernel FDB. Only retries for helper XPC readiness.
#[cfg(feature = "vmnet")]
pub async fn ensure_route_for_bridge(bridge_name: &str) -> Result<(), RouteError> {
    let name = bridge_name.to_string();
    for attempt in 1..=2 {
        let name_clone = name.clone();
        let result = tokio::task::spawn_blocking(move || {
            let ctl = helper_path();
            match std::process::Command::new(&ctl)
                .args([
                    "route",
                    "add",
                    "--subnet",
                    CONTAINER_SUBNET,
                    "--iface",
                    &name_clone,
                ])
                .output()
            {
                Ok(output) if output.status.success() => {
                    tracing::info!(
                        subnet = CONTAINER_SUBNET,
                        bridge = %name_clone,
                        "route ensured (vmnet direct)"
                    );
                    Ok(())
                }
                Ok(ref output) => Err(RouteError::RouteFailed(helper_error_message(output))),
                Err(e) => Err(RouteError::HelperUnavailable(e.to_string())),
            }
        })
        .await
        .unwrap_or_else(|_| Err(RouteError::HelperUnavailable("task panicked".into())));

        match result {
            Ok(()) => return Ok(()),
            Err(ref e) if attempt < 2 => {
                tracing::debug!(attempt, error = %e, "vmnet route install retry");
                tokio::time::sleep(ROUTE_RETRY_INTERVAL).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
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
        Ok(output) if output.status.success() => {
            tracing::info!(subnet = CONTAINER_SUBNET, "route removed");
        }
        Ok(ref output) => {
            let msg = helper_error_message(output);
            if msg.contains("not in table") {
                tracing::debug!(subnet = CONTAINER_SUBNET, "route already absent");
            } else {
                tracing::debug!(
                    subnet = CONTAINER_SUBNET,
                    error = %msg,
                    "route remove: non-zero exit"
                );
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "arcbox-helper not available for route cleanup");
        }
    }
}
