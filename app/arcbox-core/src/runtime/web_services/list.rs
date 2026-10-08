//! Owned web queries retain captured VMs until native and RPC cancellation completes.

use std::{future::Future, sync::Arc, time::Duration};

use arcbox_connect::v1::{ListGuestWebServicesRequest, ListGuestWebServicesResponse};
use tokio_util::sync::CancellationToken;

use super::{
    CapturedWebGuest, CoreError, Domain, IndexEndpoint, Result, Runtime, WebService,
    selected_services,
};

#[cfg(test)]
mod ownership_tests;

impl Runtime {
    /// Lists guest web services with their current DNS names and installed routes.
    ///
    /// # Errors
    /// Returns an error if a guest query fails or the captured identity changes.
    pub async fn list_web_services(
        self: &Arc<Self>,
        hostname: Option<Domain>,
    ) -> Result<Vec<WebService>> {
        let runtime = Arc::clone(self);
        owned_query(move |cancelled| async move {
            let guest = runtime.capture_web_guest(runtime.default_machine_name())?;
            let response = runtime
                .query_web_services(&guest, hostname.as_ref(), &cancelled)
                .await?;
            runtime.with_host_network_incarnation(&guest.machine, &guest.incarnation, || {
                let registry = runtime
                    .dns_registry
                    .read()
                    .expect("DNS registry lock poisoned");
                selected_services(
                    response.services,
                    hostname.as_ref(),
                    &registry,
                    &IndexEndpoint {
                        apex: &runtime.config.network.dns_domain,
                        gateway: runtime.config.container.cidr.docker_bridge_gateway(),
                    },
                )
            })?
        })
        .await
    }

    async fn query_web_services(
        self: &Arc<Self>,
        guest: &CapturedWebGuest,
        hostname: Option<&Domain>,
        cancelled: &CancellationToken,
    ) -> Result<ListGuestWebServicesResponse> {
        let query_cancelled = cancelled.child_token();
        let query = async {
            let (agent, _hold) = self
                .connect_web_guest_query(guest, &query_cancelled)
                .await?;
            let request = ListGuestWebServicesRequest {
                hostname: hostname.map_or_else(String::new, ToString::to_string),
                ..Default::default()
            };
            Ok(agent
                .list_guest_web_services(request, &query_cancelled)
                .await?)
        };
        query_deadline(query, &query_cancelled).await
    }
}

async fn owned_query<T: Send + 'static, F: Future<Output = Result<T>> + Send + 'static>(
    operation: impl FnOnce(CancellationToken) -> F,
) -> Result<T> {
    let cancelled = CancellationToken::new();
    let _cancel_on_drop = cancelled.clone().drop_guard();
    tokio::spawn(operation(cancelled))
        .await
        .map_err(|error| CoreError::Vm(format!("web query owner: {error}")))?
}

async fn query_deadline<T>(
    query: impl Future<Output = Result<T>>,
    cancelled: &CancellationToken,
) -> Result<T> {
    tokio::pin!(query);
    tokio::select! {
        result = &mut query => result,
        () = tokio::time::sleep(Duration::from_secs(5)) => {
            cancelled.cancel();
            let _ = query.await;
            Err(CoreError::Vm("web service query timed out".into()))
        }
    }
}
