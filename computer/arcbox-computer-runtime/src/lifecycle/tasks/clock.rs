//! Pushing the host's wall clock into a guest after a boot, restore or
//! resume.
//!
//! A guest wakes at the kernel default epoch on a cold boot, and at the
//! moment it was saved after a restore: its clock is frozen across a
//! checkpoint. Whether the push must finish before the computer is handed
//! back depends on the location ([`ClockPush`]).

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::agent::{ClockSync, GuestAgent};

/// Cap on one push: `sync_clock`'s internal connect loop can retry for up
/// to 30 s, pointlessly long for an agent that has just answered.
const CLOCK_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a push is awaited or fired off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockPush {
    /// Fire the push and return. The nested location: vm-agent re-syncs
    /// itself from `ptp_kvm` on every accepted exec connection, so correct
    /// wall time does not depend on this RPC, and awaiting it put the
    /// post-resume vsock connect settle (~57 ms) back on the critical path
    /// (CORE-80).
    Detached,
    /// Finish the push first. The host location: VZ and HV offer no
    /// `ptp_kvm`, so this RPC is the only thing that moves the guest's
    /// clock, and a command run before it lands sees the saved time (or
    /// 1970 after an HV cold boot) — TLS certificates read as not yet
    /// valid, `make` sees the future.
    Awaited,
}

/// Push the host's wall clock into `agent`'s guest. A failure is a warn
/// either way: the guest is up, and a skewed clock is not a reason to
/// fail the boot or resume that reached it.
pub async fn push(id: &str, agent: &Arc<dyn GuestAgent>, mode: ClockPush, after: &'static str) {
    let push = {
        let id = id.to_owned();
        let agent = Arc::clone(agent);
        async move {
            match tokio::time::timeout(CLOCK_SYNC_TIMEOUT, agent.sync_clock()).await {
                Ok(Ok(ClockSync::Synced)) => {}
                Ok(Ok(ClockSync::AgentError(code))) => {
                    warn!(sandbox_id = %id, code, "agent could not set the clock after {after}");
                }
                Ok(Err(error)) => {
                    warn!(sandbox_id = %id, %error, "clock sync after {after} failed");
                }
                Err(_) => warn!(sandbox_id = %id, "clock sync after {after} timed out"),
            }
        }
    };
    match mode {
        ClockPush::Detached => drop(tokio::spawn(push)),
        ClockPush::Awaited => push.await,
    }
}
