//! Sandbox error-registry attachment (CORE-58 phase 2).
//!
//! Daemon-originated sandbox errors attach the registry detail directly
//! (see [`SandboxCapabilityExt`]); guest agent errors are classified
//! centrally in `crate::error`'s `ApiError → ConnectError` conversion,
//! which every sandbox handler already funnels through. The shared
//! `ErrorInfo` builder lives in `crate::error::error_info`.

use std::sync::Arc;

use arcbox_connect::sandbox_v1 as pb;
use arcbox_core::Runtime;
use connectrpc::{ConnectError, ErrorCode as ConnectCode};

use crate::error::error_info;

/// The CORE-13 fail-fast check every guest-reaching sandbox RPC makes.
///
/// Without `/dev/kvm` the guest refuses every sandbox message, but its
/// reason describes the guest (it advises the VZ backend even on a Mac
/// that cannot run VZ) and reaches the client without a `reason` context,
/// or as a retryable `UNAVAILABLE` through `ListExposedPorts`. The daemon
/// knows why from the backend and the host, so it refuses before dialing.
pub(super) trait SandboxCapabilityExt {
    /// Returns the runtime when sandboxes can run on the System VM's
    /// current backend and this host.
    ///
    /// # Errors
    ///
    /// `FAILED_PRECONDITION` carrying `NESTED_VIRT_UNSUPPORTED` with the
    /// daemon's authoritative reason, exactly what `GetCapabilities` reports.
    fn sandbox_capable(&self) -> Result<&Self, ConnectError>;
}

impl SandboxCapabilityExt for Arc<Runtime> {
    fn sandbox_capable(&self) -> Result<&Self, ConnectError> {
        let capability = self.sandbox_nested_virt();
        if capability.supported {
            return Ok(self);
        }
        let mut error = ConnectError::new(
            ConnectCode::FailedPrecondition,
            format!("sandboxes cannot run on this host: {}", capability.reason),
        );
        error.details.push(error_info(
            pb::ErrorCode::NestedVirtUnsupported,
            "check `SandboxService.GetCapabilities` for this host's sandbox support",
            &[("reason", capability.reason.as_str())],
        ));
        Err(error)
    }
}
