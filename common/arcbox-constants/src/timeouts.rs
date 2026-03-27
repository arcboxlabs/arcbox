//! Timeout constants.

/// Grace period (seconds) for the guest agent to SIGTERM processes and
/// let them exit cleanly before escalating to SIGKILL. Must be long
/// enough for dockerd to stop all running containers and flush state.
pub const GUEST_SHUTDOWN_GRACE_SECS: u32 = 25;

/// Total host-side wait for VM to stop after shutdown RPC.
///
/// Must be greater than [`GUEST_SHUTDOWN_GRACE_SECS`] to account for
/// SIGKILL, sync, and PSCI overhead.
pub const HOST_SHUTDOWN_TIMEOUT_SECS: u64 = 30;
