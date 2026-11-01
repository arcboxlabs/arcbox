//! Which run location a Sandbox API request goes to (ADR 0010).
//!
//! The nested location is the System VM's agent, reached through the
//! runtime the System stage admits; the host location is the daemon's own
//! manager, reached through the runtime host admission publishes, as the
//! agent of [`HOST_SANDBOX_ROUTE`]. A request about an existing sandbox
//! goes where that sandbox lives; anything else goes to the default
//! location, the host when it runs here (D2).
//!
//! The nested location is reached only where the System VM can nest
//! (CORE-13): a route that would land there otherwise refuses with
//! `NESTED_VIRT_UNSUPPORTED` before dialing the guest, unless the host
//! location can take the request instead.

use std::sync::Arc;

use arcbox_core::{HOST_SANDBOX_ROUTE, Runtime};
use connectrpc::{ConnectError, RequestContext};

use super::sandbox_errors::SandboxCapabilityExt as _;
use super::{ContextExt as _, SharedRuntime};
use crate::ApiError;

/// The runtime handles behind the two locations.
#[derive(Clone)]
pub struct SandboxRuntimes {
    system: SharedRuntime,
    host: SharedRuntime,
}

/// A location a request was routed to: the runtime to reach it through and
/// the route its agent is named by.
pub(super) type Route<'a> = (&'a Arc<Runtime>, String);

impl SandboxRuntimes {
    /// The handles: `system` once the System VM's services are admitted,
    /// `host` once host admission is published.
    #[must_use]
    pub const fn new(system: SharedRuntime, host: SharedRuntime) -> Self {
        Self { system, host }
    }

    /// The System-admitted runtime; what the nested location needs.
    pub(super) fn system(&self) -> Result<&Arc<Runtime>, ConnectError> {
        self.system
            .get()
            .ok_or_else(|| ConnectError::unavailable("daemon is starting, runtime not ready yet"))
    }

    /// The System-admitted runtime, when its backend and this host let the
    /// nested location run sandboxes.
    pub(super) fn nested(&self) -> Result<&Arc<Runtime>, ConnectError> {
        self.system()?.sandbox_capable()
    }

    /// Whichever runtime is admitted, for answers about the daemon itself.
    pub(super) fn any(&self) -> Result<&Arc<Runtime>, ConnectError> {
        self.system().or_else(|error| self.host.get().ok_or(error))
    }

    /// The runtime, when the host location is composed in it.
    pub(super) fn host(&self) -> Option<&Arc<Runtime>> {
        self.host
            .get()
            .filter(|runtime| runtime.host_sandbox_capabilities().is_some())
    }

    /// The location sandbox `id` lives at. An id neither location knows
    /// goes to the nested location while it can run sandboxes, so its own
    /// not-found answers, and to the host otherwise.
    pub(super) fn route(&self, ctx: &RequestContext, id: &str) -> Result<Route<'_>, ConnectError> {
        let machine = ctx.sandbox_machine_id()?;
        if let Some(host) = self.host()
            && host.owns_host_sandbox(id)
        {
            return Ok((host, HOST_SANDBOX_ROUTE.to_owned()));
        }
        match (self.nested(), self.host()) {
            (Ok(system), _) => Ok((system, machine)),
            (Err(_), Some(host)) => Ok((host, HOST_SANDBOX_ROUTE.to_owned())),
            (Err(error), None) => Err(error),
        }
    }

    /// [`Self::route`], admitting a write to the location's storage.
    pub(super) fn route_for_write(
        &self,
        ctx: &RequestContext,
        id: &str,
    ) -> Result<Route<'_>, ConnectError> {
        let (runtime, machine) = self.route(ctx, id)?;
        runtime
            .ensure_storage_writes_available(&machine)
            .map_err(ApiError::from)?;
        Ok((runtime, machine))
    }

    /// The location of a checkpoint.
    pub(super) fn route_snapshot(
        &self,
        ctx: &RequestContext,
        snapshot_id: &str,
    ) -> Result<Route<'_>, ConnectError> {
        if let Some(host) = self.host()
            && host.owns_host_snapshot(snapshot_id)
        {
            ctx.sandbox_machine_id()?;
            return Ok((host, HOST_SANDBOX_ROUTE.to_owned()));
        }
        self.route(ctx, "")
    }

    /// The default location: the host when it runs here (D2), the nested
    /// location otherwise.
    pub(super) fn default_location(&self, ctx: &RequestContext) -> Result<Route<'_>, ConnectError> {
        let machine = ctx.sandbox_machine_id()?;
        if let Some(host) = self.host() {
            return Ok((host, HOST_SANDBOX_ROUTE.to_owned()));
        }
        Ok((self.nested()?, machine))
    }

    /// [`Self::default_location`], admitting a write to its storage.
    pub(super) fn default_location_for_write(
        &self,
        ctx: &RequestContext,
    ) -> Result<Route<'_>, ConnectError> {
        let (runtime, machine) = self.default_location(ctx)?;
        runtime
            .ensure_storage_writes_available(&machine)
            .map_err(ApiError::from)?;
        Ok((runtime, machine))
    }
}
