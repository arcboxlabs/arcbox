//! Recovers container DNS and listeners from a complete guest snapshot.

use std::sync::Arc;

use anyhow::Result;
use arcbox_core::Runtime;
use tracing::info;

/// Re-registers DNS entries and port forwarding for all running containers.
///
/// Reports nothing about VM liveness: `services::vm_running_loop` owns
/// `SetupStatus.vm_running` and follows the lifecycle across restarts, which
/// a one-shot observation made here could not.
pub(super) async fn recover_container_networking(runtime: &Arc<Runtime>) -> Result<()> {
    use arcbox_docker::guest_query;
    use arcbox_docker::proxy::{GuestHttpClient, VsockConnector};

    let Some(snapshot) = runtime.capture_container_snapshot().await? else {
        return Ok(());
    };
    let client = GuestHttpClient::new(Arc::new(VsockConnector::new(Arc::clone(runtime))));
    let running = guest_query::running_container_snapshot(&client).await?;
    if snapshot.commit(&running).await? {
        info!(
            containers = running.len(),
            "Recovered networking for running containers"
        );
    }
    Ok(())
}
