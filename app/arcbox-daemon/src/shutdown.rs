//! Graceful shutdown: signal handling, connection drain, cleanup.

use std::time::Duration;

use anyhow::{Context, Result};
use arcbox_docker::DockerContextManager;
use tokio::signal;
use tracing::{info, warn};

use crate::context::{DaemonContext, ServiceHandles};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits for a shutdown signal, drains services, and cleans up.
pub async fn run(ctx: DaemonContext, mut handles: ServiceHandles) -> Result<()> {
    wait_for_signal().await;
    info!("Shutdown signal received, draining connections...");
    ctx.shutdown.cancel();

    drain(&mut handles).await;

    // Remove container subnet route before stopping the VM.
    #[cfg(target_os = "macos")]
    {
        arcbox_core::route_reconciler::remove_route().await;
    }

    if let Some(runtime) = ctx.shared_runtime.get() {
        runtime
            .shutdown()
            .await
            .context("Failed to shutdown runtime")?;
    }

    if ctx.docker_integration {
        if let Ok(ctx_manager) = DockerContextManager::new(ctx.socket_path.clone()) {
            let _ = ctx_manager.disable();
        }
    }

    cleanup_files(&ctx);

    info!("ArcBox daemon stopped");
    Ok(())
}

async fn wait_for_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

async fn drain(handles: &mut ServiceHandles) {
    if tokio::time::timeout(DRAIN_TIMEOUT, async {
        let _ = tokio::join!(&mut handles.dns, &mut handles.docker, &mut handles.grpc);
    })
    .await
    .is_err()
    {
        warn!(
            "Server drain timed out after {}s, aborting remaining tasks",
            DRAIN_TIMEOUT.as_secs()
        );
        handles.dns.abort();
        handles.docker.abort();
        handles.grpc.abort();
    }
}

fn cleanup_files(ctx: &DaemonContext) {
    for path in [&ctx.socket_path, &ctx.grpc_socket] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove socket {}: {}", path.display(), e);
            }
        }
    }

    if let Err(e) = std::fs::remove_file(&ctx.pid_file) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(
                "Failed to remove PID file {}: {}",
                ctx.pid_file.display(),
                e
            );
        }
    }
}
