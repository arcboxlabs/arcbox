//! Route management via PF_ROUTE routing socket.
//!
//! Adds or removes a host route for the container subnet through a given
//! bridge interface. This is the only way the daemon modifies the host
//! routing table — all intelligence lives in the daemon's route_reconciler.

use arcbox_helper::validate::{BridgeIface, Subnet};
use arcbox_route::Ipv4Net;

/// Adds a route for `subnet` via `iface`.
///
/// Uses the PF_ROUTE routing socket with RTM_ADD. If the route already
/// exists, atomically replaces it via RTM_CHANGE (fixes the "File exists"
/// bug where a stale route pointing to a different gateway was kept).
///
/// Idempotent: re-adding the same route succeeds.
pub fn add(subnet: &Subnet, iface: &BridgeIface) -> Result<(), String> {
    let net = to_ipv4net(subnet)?;
    arcbox_route::add(net, iface.as_str())
}

/// Removes the route for `subnet`.
///
/// Uses the PF_ROUTE routing socket with RTM_DELETE.
/// Idempotent: returns Ok if the route is already absent.
pub fn remove(subnet: &Subnet) -> Result<(), String> {
    let net = to_ipv4net(subnet)?;
    arcbox_route::remove(net)
}

/// Converts a helper `Subnet` to an `arcbox_route::Ipv4Net`.
///
/// `Subnet` already guarantees valid CIDR + no host bits + private range,
/// so this conversion is infallible in practice.
fn to_ipv4net(subnet: &Subnet) -> Result<Ipv4Net, String> {
    let inner = subnet.network();
    Ipv4Net::new(inner.ip(), inner.prefix()).map_err(|e| format!("invalid subnet: {e}"))
}
