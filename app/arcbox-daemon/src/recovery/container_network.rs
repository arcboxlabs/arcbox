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

    let client = GuestHttpClient::new(Arc::new(VsockConnector::new(Arc::clone(runtime))));
    let running = guest_query::running_container_snapshot(&client).await?;

    let mut recovered_dns = 0u32;
    let mut recovered_ports = 0u32;
    for inspect in &running {
        let id = &inspect.metadata.id;
        runtime.register_container_dns(&inspect.metadata).await;
        if inspect.metadata.primary_ipv4().is_some() {
            recovered_dns += 1;
        }

        let bindings = arcbox_docker::port_bindings::parse_port_bindings(&inspect.body);
        if !bindings.is_empty() {
            let rules: Vec<_> = bindings
                .iter()
                .map(|b| {
                    (
                        b.host_ip.clone(),
                        b.host_port,
                        b.container_port,
                        b.protocol.clone(),
                    )
                })
                .collect();
            match runtime
                .start_port_forwarding_for(runtime.default_machine_name(), id, &rules)
                .await
            {
                Ok(()) => recovered_ports += 1,
                Err(e) => tracing::warn!("Failed to recover port forwarding for {id}: {e}"),
            }
        }
    }

    if recovered_dns > 0 || recovered_ports > 0 {
        info!(
            dns = recovered_dns,
            ports = recovered_ports,
            "Recovered networking for running containers"
        );
    }
    Ok(())
}
