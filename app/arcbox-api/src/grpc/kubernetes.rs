//! Kubernetes service gRPC implementation.

use arcbox_grpc::v1::kubernetes_service_server;
use arcbox_protocol::v1::{
    KubernetesDeleteRequest, KubernetesDeleteResponse, KubernetesKubeconfigRequest,
    KubernetesKubeconfigResponse, KubernetesStartRequest, KubernetesStartResponse,
    KubernetesStatusRequest, KubernetesStatusResponse, KubernetesStopRequest,
    KubernetesStopResponse,
};
use tonic::{Request, Response, Status};

use super::{SharedRuntime, SharedRuntimeExt};

/// Kubernetes service implementation.
pub struct KubernetesServiceImpl {
    runtime: SharedRuntime,
}

impl KubernetesServiceImpl {
    /// Creates a new Kubernetes service with a deferred runtime.
    #[must_use]
    pub fn new(runtime: SharedRuntime) -> Self {
        Self { runtime }
    }
}

#[tonic::async_trait]
impl kubernetes_service_server::KubernetesService for KubernetesServiceImpl {
    async fn start(
        &self,
        _request: Request<KubernetesStartRequest>,
    ) -> Result<Response<KubernetesStartResponse>, Status> {
        let runtime = self.runtime.ready()?;
        let response = runtime
            .start_kubernetes()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
    }

    async fn stop(
        &self,
        _request: Request<KubernetesStopRequest>,
    ) -> Result<Response<KubernetesStopResponse>, Status> {
        let runtime = self.runtime.ready()?;
        let response = runtime
            .stop_kubernetes()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
    }

    async fn delete(
        &self,
        _request: Request<KubernetesDeleteRequest>,
    ) -> Result<Response<KubernetesDeleteResponse>, Status> {
        let runtime = self.runtime.ready()?;
        let response = runtime
            .delete_kubernetes()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
    }

    async fn status(
        &self,
        _request: Request<KubernetesStatusRequest>,
    ) -> Result<Response<KubernetesStatusResponse>, Status> {
        let runtime = self.runtime.ready()?;
        let response = runtime
            .kubernetes_status()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
    }

    async fn get_kubeconfig(
        &self,
        _request: Request<KubernetesKubeconfigRequest>,
    ) -> Result<Response<KubernetesKubeconfigResponse>, Status> {
        let runtime = self.runtime.ready()?;
        let response = runtime
            .kubernetes_kubeconfig()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(response))
    }
}
