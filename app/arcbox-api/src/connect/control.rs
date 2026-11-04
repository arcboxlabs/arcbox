//! Sandbox lifecycle service — control plane.

use std::sync::Arc;

use arcbox_connect::sandbox_v1 as pb;
use arcbox_connect::sandbox_v1::{KeepAlive, WatchEventsResponse, watch_events_response};
use buffa_types::google::protobuf::Empty;
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use tokio_stream::StreamExt as _;

use super::sandbox_route::SandboxRuntimes;
use crate::ApiError;
use arcbox_core::{HOST_SANDBOX_ROUTE, HostSandboxCapabilities};

use super::exposed_port;
use super::sandbox_resume;
use super::{ContextExt as _, port_protocol, with_keepalive};
use arcbox_computer::cleanup as sandbox_cleanup;
use arcbox_computer::locks::SandboxOperationLocks;
use arcbox_computer::ports;

/// Why Create refuses `RUN_LOCATION_HOST` and `GetCapabilities` reports
/// the host unavailable when the host can run VMs: the daemon composed no
/// host location (`[sandbox] enabled = false`, another platform, or its
/// composition failed at startup — the daemon log says why).
const HOST_UNAVAILABLE: &str = "this daemon does not run host sandboxes";

/// Why the host location is absent: the host's own reason when it cannot
/// run a VM at all (the System VM then runs on QEMU), [`HOST_UNAVAILABLE`]
/// otherwise.
fn host_unavailable_reason() -> &'static str {
    arcbox_engine::host_virtualization()
        .require()
        .err()
        .unwrap_or(HOST_UNAVAILABLE)
}

/// One entry per run location. The nested location answers what it always
/// has: it pauses whenever it runs, publishes ports through guest DNAT, and
/// takes no bind mounts. Its pool prewarm is the agent's configuration,
/// which the daemon cannot see, so it is reported off. The host location
/// reports its manager's own answer, which changes over time (a VZ pause
/// while the session is locked).
fn location_capabilities(
    nested: &arcbox_computer::NestedVirtCapability,
    host: Option<HostSandboxCapabilities>,
    host_unavailable: &str,
) -> Vec<pb::LocationCapability> {
    let host = host.map_or_else(
        || pb::LocationCapability {
            location: pb::RunLocation::Host.into(),
            unavailable_reason: host_unavailable.to_owned(),
            pause_unavailable_reason: host_unavailable.to_owned(),
            ..Default::default()
        },
        |host| pb::LocationCapability {
            location: pb::RunLocation::Host.into(),
            backend: host.backend.to_owned(),
            available: true,
            pause_resume: host.pause_resume,
            pause_available: host.pause_unavailable.is_none(),
            pause_unavailable_reason: host.pause_unavailable.unwrap_or_default(),
            port_publishing: host.port_publishing,
            bind_mounts: host.bind_mounts,
            warm_prespawn: host.warm_prespawn,
            ..Default::default()
        },
    );
    let unavailable = (!nested.supported).then(|| nested.reason.clone());
    let nested = pb::LocationCapability {
        location: pb::RunLocation::Nested.into(),
        backend: if nested.supported { "fc" } else { "" }.to_owned(),
        available: nested.supported,
        unavailable_reason: unavailable.clone().unwrap_or_default(),
        pause_resume: true,
        pause_available: nested.supported,
        pause_unavailable_reason: unavailable.unwrap_or_default(),
        port_publishing: true,
        ..Default::default()
    };
    vec![host, nested]
}

/// Sandbox lifecycle service implementation.
///
/// The local daemon handles sandbox lifecycle, events, and published ports.
/// Guest operations use the port-1024 vsock binary-frame protocol to reach
/// `arcbox-agent` inside the System VM.
pub struct SandboxServiceImpl {
    runtime: SandboxRuntimes,
    operations: Arc<SandboxOperationLocks>,
}

impl SandboxServiceImpl {
    /// Creates a new sandbox service with a deferred runtime.
    #[must_use]
    pub(super) fn new(runtime: SandboxRuntimes, operations: Arc<SandboxOperationLocks>) -> Self {
        Self {
            runtime,
            operations,
        }
    }
}

#[allow(
    refining_impl_trait,
    reason = "the trait returns `impl Encodable<M>`; naming the concrete body \
              type is strictly more informative and these impls are registered on a \
              Router rather than named by callers"
)]
impl pb::SandboxService for SandboxServiceImpl {
    async fn create(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::CreateSandboxRequest>,
    ) -> ServiceResult<pb::CreateSandboxResponse> {
        let req = request.to_owned_message();
        let sandbox_id = req.id.clone();
        // Request validation answers before any location's capability.
        if !sandbox_id.is_empty() {
            self.runtime
                .any()?
                .sandbox_dns_name(&sandbox_id)
                .map_err(|error| ConnectError::invalid_argument(error.to_string()))?;
        }
        // The host location by default (D2), the nested one on request.
        let (runtime, machine) = match req.run_location.as_known() {
            Some(pb::RunLocation::Unspecified) => self.runtime.default_location_for_write(&ctx)?,
            Some(pb::RunLocation::Host) => {
                ctx.sandbox_machine_id()?;
                let host = self
                    .runtime
                    .host()
                    .ok_or_else(|| ConnectError::failed_precondition(host_unavailable_reason()))?;
                (host, HOST_SANDBOX_ROUTE.to_owned())
            }
            Some(pb::RunLocation::Nested) => {
                let machine = ctx.sandbox_machine_id()?;
                let system = self.runtime.nested()?;
                system
                    .ensure_storage_writes_available(&machine)
                    .map_err(ApiError::from)?;
                (system, machine)
            }
            None => {
                return Err(ConnectError::invalid_argument(format!(
                    "unknown run_location {}",
                    req.run_location.to_i32()
                )));
            }
        };
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        // One id names one sandbox across both locations: they share the
        // sandbox DNS records.
        if !sandbox_id.is_empty()
            && machine != HOST_SANDBOX_ROUTE
            && runtime.owns_host_sandbox(&sandbox_id)
        {
            return Err(ConnectError::already_exists(format!(
                "sandbox {sandbox_id} already exists at the host location"
            )));
        }
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        // The RPC error otherwise reaches only the caller; the daemon log
        // must record a failed lifecycle mutation on its own (CORE-82).
        let resp = agent
            .sandbox_create(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox create failed");
            })
            .map_err(ApiError::from)?;
        // Register sandbox DNS so the host can resolve sandbox-id.sandbox.arcbox.local
        // (shared live-match discipline; see register_live_sandbox_dns).
        sandbox_cleanup::register_live_sandbox_dns(runtime, &machine, &resp.id, &resp.ip_address)
            .await
            .map_err(ApiError::from)?;

        Response::ok(resp)
    }

    async fn stop(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::StopSandboxRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let sandbox_id = req.id.clone();
        let (runtime, machine) = self.runtime.route(&ctx, &sandbox_id)?;
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let response = agent
            .sandbox_stop(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox stop failed");
            })
            .map_err(ApiError::from)?;
        if let Some(ticket) = response.ticket.as_option() {
            sandbox_cleanup::complete(runtime, &mut agent, ticket)
                .await
                .map_err(ApiError::from)?;
        }

        Response::ok(Empty::default())
    }

    async fn remove(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::RemoveSandboxRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let sandbox_id = req.id.clone();
        let (runtime, machine) = self.runtime.route_for_write(&ctx, &sandbox_id)?;
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let response = agent
            .sandbox_remove(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox remove failed");
            })
            .map_err(ApiError::from)?;
        if let Some(ticket) = response.ticket.as_option() {
            sandbox_cleanup::complete(runtime, &mut agent, ticket)
                .await
                .map_err(ApiError::from)?;
        }

        Response::ok(Empty::default())
    }

    /// Pause: checkpoint in the guest, release the VM, then complete the
    /// host half of the network release — the guest quarantines the TAP+IP
    /// exactly like Stop and hands back the same durable cleanup ticket
    /// (which also drops the sandbox's host DNS entry).
    async fn pause(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::PauseSandboxRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let sandbox_id = req.id.clone();
        let (runtime, machine) = self.runtime.route_for_write(&ctx, &sandbox_id)?;
        let _operation = self.operations.lock(&machine, &sandbox_id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        // Same reason as create/stop/remove: a failed lifecycle mutation must
        // reach the daemon log on its own, not only the caller (CORE-82).
        let response = agent
            .sandbox_pause(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, sandbox_id = %sandbox_id, %error, "sandbox pause failed");
            })
            .map_err(ApiError::from)?;
        if let Some(ticket) = response.ticket.as_option() {
            sandbox_cleanup::complete(runtime, &mut agent, ticket)
                .await
                .map_err(ApiError::from)?;
        }

        Response::ok(Empty::default())
    }

    /// Explicit resume; data-plane RPCs resume transparently through the
    /// same routine (`sandbox_resume`), differing only in the event reason.
    async fn resume(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ResumeSandboxRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.route(&ctx, &req.id)?;
        sandbox_resume::resume(
            runtime,
            &self.operations,
            &machine,
            &req.id,
            sandbox_resume::REASON_RESUME,
        )
        .await?;
        Response::ok(Empty::default())
    }

    /// Replace lifecycle deadlines: `ttl_seconds` re-arms the hard cap
    /// from now (CORE-60), `idle_timeout_seconds`/`on_idle` replace the
    /// idle knobs (CORE-21). Absent fields are unchanged; works on paused
    /// sandboxes too (the TTL keeps applying to them).
    async fn set_lifecycle(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::SetLifecycleRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.route_for_write(&ctx, &req.id)?;
        let _operation = self.operations.lock(&machine, &req.id).await;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        agent
            .sandbox_set_lifecycle(req)
            .await
            .map_err(ApiError::from)?;
        Response::ok(Empty::default())
    }

    /// Report what this daemon can do (CORE-13): version, sandbox protocol
    /// level, feature flags, and whether nested virtualization is available
    /// on the *current* backend and hardware — answered host-side without
    /// touching the guest, so the SDK handshake works before any sandbox
    /// exists.
    async fn get_capabilities(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, pb::GetCapabilitiesRequest>,
    ) -> ServiceResult<pb::GetCapabilitiesResponse> {
        let runtime = self.runtime.any()?;
        let nested = runtime.sandbox_nested_virt();
        Response::ok(pb::GetCapabilitiesResponse {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: arcbox_constants::sandbox::SANDBOX_API_PROTOCOL,
            features: arcbox_constants::sandbox::SANDBOX_FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            locations: location_capabilities(
                &nested,
                runtime.host_sandbox_capabilities(),
                host_unavailable_reason(),
            ),
            nested_virt: pb::NestedVirtCapability {
                supported: nested.supported,
                reason: nested.reason,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
    }

    async fn inspect(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::InspectSandboxRequest>,
    ) -> ServiceResult<pb::SandboxInfo> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.route(&ctx, &req.id)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let info = agent.sandbox_inspect(req).await.map_err(ApiError::from)?;
        Response::ok(info)
    }

    async fn list(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ListSandboxesRequest>,
    ) -> ServiceResult<pb::ListSandboxesResponse> {
        let machine = ctx.sandbox_machine_id()?;
        let mut req = request.to_owned_message();
        let Some(host) = self.runtime.host() else {
            let mut agent = self
                .runtime
                .nested()?
                .get_agent(&machine)
                .map_err(ApiError::from)?;
            let resp = agent.sandbox_list(req).await.map_err(ApiError::from)?;
            return Response::ok(resp);
        };
        // Both locations, host first: the page token says which one a page
        // continues, and the host's last page hands over to the nested
        // location while the System VM runs.
        let (page, token) = ListPage::parse(&req.page_token);
        req.page_token = token.to_owned();
        let (runtime, route) = match page {
            ListPage::Host => (host, HOST_SANDBOX_ROUTE.to_owned()),
            ListPage::Nested => (self.runtime.nested()?, machine),
        };
        let mut agent = runtime.get_agent(&route).map_err(ApiError::from)?;
        let mut resp = agent.sandbox_list(req).await.map_err(ApiError::from)?;
        resp.next_page_token = match (page, resp.next_page_token.is_empty()) {
            (_, false) => format!("{}{}", page.prefix(), resp.next_page_token),
            (ListPage::Host, true) if self.runtime.nested().is_ok() => {
                ListPage::Nested.prefix().to_owned()
            }
            _ => String::new(),
        };
        Response::ok(resp)
    }

    async fn expose_port(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ExposePortRequest>,
    ) -> ServiceResult<pb::ExposePortResponse> {
        let req = request.to_owned_message();
        let sandbox_port = u16::try_from(req.sandbox_port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| ConnectError::invalid_argument("sandbox_port must be 1-65535"))?;
        let host_port = u16::try_from(req.host_port)
            .map_err(|_| ConnectError::invalid_argument("host_port must be 0-65535"))?;
        let protocol = port_protocol(req.protocol.as_known().unwrap_or_default());
        let (runtime, machine) = self.runtime.route(&ctx, &req.id)?;
        let _operation = self.operations.lock(&machine, &req.id).await;

        let exposed = ports::expose(
            runtime,
            &machine,
            &req.id,
            sandbox_port,
            host_port,
            protocol,
        )
        .await
        .map_err(|error| match error {
            ports::ExposePortError::Raced => ConnectError::unavailable(
                "sandbox host cleanup raced port exposure; retry to confirm the result",
            ),
            ports::ExposePortError::Engine(e) => ConnectError::from(ApiError::from(e)),
        })?;

        let resp = pb::ExposePortResponse {
            host_port: u32::from(exposed.host_port),
            guest_port: u32::from(exposed.guest_port),
            ..Default::default()
        };
        Response::ok(resp)
    }

    async fn list_exposed_ports(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ListExposedPortsRequest>,
    ) -> ServiceResult<pb::ListExposedPortsResponse> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.route(&ctx, &req.id)?;
        let _operation = self.operations.lock(&machine, &req.id).await;
        let mappings =
            ports::list(runtime, &machine, &req.id)
                .await
                .map_err(|error| match error {
                    ports::ListExposedPortsError::Sandbox(e) => {
                        ConnectError::from(ApiError::from(e))
                    }
                    ports::ListExposedPortsError::Unavailable(e) => {
                        ConnectError::unavailable(format!("sandbox state unavailable: {e}"))
                    }
                    ports::ListExposedPortsError::Unstable => ConnectError::unavailable(
                        "sandbox cleanup prevented a stable exposed-port snapshot; retry",
                    ),
                })?;
        let listed = mappings.into_iter().map(exposed_port).collect();
        Response::ok(pb::ListExposedPortsResponse {
            ports: listed,
            ..Default::default()
        })
    }

    async fn unexpose_port(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::UnexposePortRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let sandbox_port = u16::try_from(req.sandbox_port)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| ConnectError::invalid_argument("sandbox_port must be 1-65535"))?;
        let protocol = port_protocol(req.protocol.as_known().unwrap_or_default());
        let (runtime, machine) = self.runtime.route(&ctx, &req.id)?;
        let _operation = self.operations.lock(&machine, &req.id).await;
        ports::unexpose(runtime, &machine, &req.id, sandbox_port, protocol)
            .await
            .map_err(ApiError::from)?;

        Response::ok(Empty::default())
    }

    async fn events(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::SandboxEventsRequest>,
    ) -> ServiceResult<ServiceStream<WatchEventsResponse>> {
        let req = request.to_owned_message();
        let (runtime, machine) = if req.sandbox_id.is_empty() {
            self.runtime.default_location(&ctx)?
        } else {
            self.runtime.route(&ctx, &req.sandbox_id)?
        };
        let agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let rx = agent
            .sandbox_events(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, %error, "sandbox events subscribe failed");
            })
            .map_err(ApiError::from)?;
        let stream = rx.map(|r| {
            r.map(|event| WatchEventsResponse {
                payload: Some(watch_events_response::Payload::from(event)),
                ..Default::default()
            })
            .map_err(|e| ConnectError::from(ApiError::from(e)))
        });
        let stream = with_keepalive(stream, || WatchEventsResponse {
            payload: Some(watch_events_response::Payload::from(KeepAlive::default())),
            ..Default::default()
        });
        Response::ok(Box::pin(stream))
    }
}

/// Which location a `ListSandboxes` page token continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListPage {
    Host,
    Nested,
}

impl ListPage {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Host => "h:",
            Self::Nested => "n:",
        }
    }

    /// The location a token continues and the location's own token. A
    /// token without a prefix is the host's first page.
    fn parse(token: &str) -> (Self, &str) {
        if let Some(rest) = token.strip_prefix(Self::Nested.prefix()) {
            (Self::Nested, rest)
        } else {
            (
                Self::Host,
                token.strip_prefix(Self::Host.prefix()).unwrap_or(""),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unsupported nested location says so with the same reason the
    /// legacy `nested_virt` field carries, and the host stays unavailable
    /// until a host manager is composed.
    #[test]
    fn locations_follow_nested_support() {
        let unsupported = arcbox_computer::NestedVirtCapability {
            supported: false,
            reason: "M1 has no nested virtualization".to_owned(),
        };
        let [host, nested] = location_capabilities(&unsupported, None, HOST_UNAVAILABLE)
            .try_into()
            .unwrap();
        assert_eq!(host.location, pb::RunLocation::Host);
        assert!(!host.available && !host.pause_available);
        assert_eq!(host.unavailable_reason, HOST_UNAVAILABLE);
        assert_eq!(nested.location, pb::RunLocation::Nested);
        assert!(!nested.available && !nested.pause_available);
        assert_eq!(nested.unavailable_reason, unsupported.reason);
        assert!(nested.backend.is_empty());

        let supported = arcbox_computer::NestedVirtCapability {
            supported: true,
            reason: String::new(),
        };
        let [_, nested] = location_capabilities(&supported, None, HOST_UNAVAILABLE)
            .try_into()
            .unwrap();
        assert!(nested.available && nested.pause_available && nested.port_publishing);
        assert_eq!(nested.backend, "fc");
        assert!(nested.unavailable_reason.is_empty() && !nested.bind_mounts);
    }

    /// A composed host location reports its manager's answer, including a
    /// pause that cannot run right now (VZ while the session is locked).
    #[test]
    fn a_composed_host_location_reports_its_manager() {
        let nested = arcbox_computer::NestedVirtCapability {
            supported: false,
            reason: "no nested virtualization".to_owned(),
        };
        let locked = HostSandboxCapabilities {
            backend: "vz",
            pause_resume: true,
            pause_unavailable: Some("the console session is locked".to_owned()),
            warm_prespawn: false,
            port_publishing: false,
            bind_mounts: false,
        };
        let [host, _] = location_capabilities(&nested, Some(locked), HOST_UNAVAILABLE)
            .try_into()
            .unwrap();
        assert!(host.available && host.pause_resume && !host.pause_available);
        assert_eq!(host.backend, "vz");
        assert_eq!(
            host.pause_unavailable_reason,
            "the console session is locked"
        );
        assert!(host.unavailable_reason.is_empty());
    }

    /// Host pages first; a token says which location it continues, and an
    /// unprefixed token starts at the host.
    #[test]
    fn list_tokens_name_the_location_they_continue() {
        assert_eq!(ListPage::parse(""), (ListPage::Host, ""));
        assert_eq!(ListPage::parse("h:abc"), (ListPage::Host, "abc"));
        assert_eq!(ListPage::parse("n:"), (ListPage::Nested, ""));
        assert_eq!(ListPage::parse("n:42"), (ListPage::Nested, "42"));
        assert_eq!(ListPage::parse("legacy"), (ListPage::Host, ""));
    }
}
