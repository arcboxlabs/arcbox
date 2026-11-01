//! Sandbox template catalog service — control plane (CORE-107).
//!
//! Pure forwarders to the default location's agent, mirroring
//! `snapshot.rs`: the catalog, every artifact it references, and the build
//! pipeline live with the location — the host when it runs here (D2), the
//! System VM otherwise.

use arcbox_connect::sandbox_v1 as pb;
use buffa_types::google::protobuf::Empty;
use connectrpc::{RequestContext, Response, ServiceRequest, ServiceResult};

use super::sandbox_route::SandboxRuntimes;
use crate::ApiError;

/// Template service implementation.
pub struct TemplateServiceImpl {
    runtime: SandboxRuntimes,
}

impl TemplateServiceImpl {
    /// Creates a new template service with a deferred runtime.
    #[must_use]
    pub(super) fn new(runtime: SandboxRuntimes) -> Self {
        Self { runtime }
    }
}

#[allow(
    refining_impl_trait,
    reason = "the trait returns `impl Encodable<M>`; naming the concrete body \
              type is strictly more informative and these impls are registered on a \
              Router rather than named by callers"
)]
impl pb::TemplateService for TemplateServiceImpl {
    /// Blocks for the whole build (image export, rootfs conversion). Safe on
    /// this transport: sandbox RPCs ride the async vsock arm, which applies
    /// no read deadline.
    async fn build(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::BuildTemplateRequest>,
    ) -> ServiceResult<pb::Template> {
        let req = request.to_owned_message();
        let name = req.name.clone();
        let (runtime, machine) = self.runtime.default_location_for_write(&ctx)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        // The RPC error otherwise reaches only the caller; the daemon log
        // must record a failed catalog mutation on its own (CORE-82).
        let resp = agent
            .sandbox_template_build(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, template = %name, %error, "template build failed");
            })
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn publish(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::PublishTemplateRequest>,
    ) -> ServiceResult<pb::Template> {
        let req = request.to_owned_message();
        let name = req.name.clone();
        let (runtime, machine) = self.runtime.default_location_for_write(&ctx)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        // The RPC error otherwise reaches only the caller; the daemon log
        // must record a failed catalog mutation on its own (CORE-82).
        let resp = agent
            .sandbox_template_publish(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, template = %name, %error, "template publish failed");
            })
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn get(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::GetTemplateRequest>,
    ) -> ServiceResult<pb::Template> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.default_location(&ctx)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let resp = agent
            .sandbox_template_get(req)
            .await
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn list(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::ListTemplatesRequest>,
    ) -> ServiceResult<pb::ListTemplatesResponse> {
        let req = request.to_owned_message();
        let (runtime, machine) = self.runtime.default_location(&ctx)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        let resp = agent
            .sandbox_template_list(req)
            .await
            .map_err(ApiError::from)?;
        Response::ok(resp)
    }

    async fn delete(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, pb::DeleteTemplateRequest>,
    ) -> ServiceResult<Empty> {
        let req = request.to_owned_message();
        let reference = req.reference.clone();
        let (runtime, machine) = self.runtime.default_location_for_write(&ctx)?;
        let mut agent = runtime.get_agent(&machine).map_err(ApiError::from)?;
        agent
            .sandbox_template_delete(req)
            .await
            .inspect_err(|error| {
                tracing::warn!(machine = %machine, template = %reference, %error, "template delete failed");
            })
            .map_err(ApiError::from)?;
        Response::ok(Empty::default())
    }
}
