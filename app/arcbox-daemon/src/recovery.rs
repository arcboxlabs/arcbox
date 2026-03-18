//! Post-startup recovery: re-establish networking for surviving containers
//! and reconcile host routes.

use std::sync::Arc;

use arcbox_core::Runtime;
use tracing::info;

use crate::context::DaemonContext;
use crate::self_setup;

/// Runs all recovery and best-effort setup tasks.
///
/// - Recovers DNS and port forwarding for containers that survived a daemon restart
/// - Re-installs the container subnet route (cold-start reconcile)
/// - Installs DNS resolver and Docker socket via helper (best-effort)
pub async fn run(ctx: &DaemonContext) {
    recover_container_networking(&ctx.runtime).await;

    // Cold-start route reconcile (non-blocking).
    #[cfg(target_os = "macos")]
    {
        use arcbox_core::DEFAULT_MACHINE_NAME;
        if let Some(mac) = ctx
            .runtime
            .machine_manager()
            .bridge_mac(DEFAULT_MACHINE_NAME)
        {
            drop(tokio::spawn(async move {
                if let Err(e) = arcbox_core::route_reconciler::ensure_route_with_retry(&mac).await {
                    tracing::warn!(error = %e, "failed to install container route on cold start");
                }
            }));
        }
    }

    // Best-effort self-setup (non-blocking).
    let dns_task = self_setup::DnsResolver {
        domain: ctx.dns_domain.clone(),
        port: ctx.dns_port,
    };
    let socket_task = self_setup::DockerSocket {
        target: ctx.socket_path.clone(),
    };
    tokio::spawn(async move {
        self_setup::run(&[&dns_task, &socket_task]).await;
    });
}

/// Re-registers DNS entries and port forwarding for all running containers.
async fn recover_container_networking(runtime: &Arc<Runtime>) {
    use axum::http::{HeaderMap, Method};
    use bytes::Bytes;

    let resp = match arcbox_docker::proxy::proxy_to_guest(
        runtime,
        Method::GET,
        "/containers/json",
        &HeaderMap::new(),
        Bytes::new(),
    )
    .await
    {
        Ok(resp) if resp.status().is_success() => resp,
        Ok(resp) => {
            tracing::debug!("Container list returned status {}", resp.status());
            return;
        }
        Err(e) => {
            tracing::debug!("Failed to list containers for networking recovery: {}", e);
            return;
        }
    };

    let body_bytes = match http_body_util::BodyExt::collect(resp.into_body()).await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::debug!("Failed to read container list body: {}", e);
            return;
        }
    };

    let containers: Vec<serde_json::Value> = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("Failed to parse container list JSON: {}", e);
            return;
        }
    };

    let mut recovered_dns = 0u32;
    let mut recovered_ports = 0u32;
    for container in &containers {
        let Some(id) = container.get("Id").and_then(|v| v.as_str()) else {
            continue;
        };

        let inspect_path = format!("/containers/{id}/json");
        let inspect_resp = match arcbox_docker::proxy::proxy_to_guest(
            runtime,
            Method::GET,
            &inspect_path,
            &HeaderMap::new(),
            Bytes::new(),
        )
        .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            _ => continue,
        };

        let inspect_body = match http_body_util::BodyExt::collect(inspect_resp.into_body()).await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => continue,
        };

        if let Some((name, ip)) = arcbox_docker::handlers::extract_container_dns_info(&inspect_body)
        {
            runtime.register_dns(id, &name, ip).await;
            recovered_dns += 1;
        }

        let bindings = arcbox_docker::proxy::parse_port_bindings(&inspect_body);
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
}
