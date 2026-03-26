//! Graceful shutdown: signal handling, connection drain, cleanup.
//!
//! First signal triggers graceful shutdown with visible feedback.
//! Second signal force-quits: skips the VM graceful stop but still runs
//! full cleanup (port forwarding, network manager, routes, sockets).

use std::time::Duration;

use anyhow::Result;
use arcbox_docker::DockerContextManager;
use tokio::signal;
use tracing::{info, warn};

use crate::context::{DaemonContext, ServiceHandles};

const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits for a shutdown signal, drains services, and cleans up.
pub async fn run(ctx: DaemonContext, mut handles: ServiceHandles) -> Result<()> {
    wait_for_signal().await;
    println!("Shutting down guest VM... (press Ctrl+C again to force quit)");
    info!("Shutdown signal received, draining connections...");
    ctx.shutdown.cancel();

    drain(&mut handles).await;

    // runtime.shutdown() calls graceful_stop() which blocks synchronously,
    // so it must run in a separate task — otherwise select! can't poll the
    // signal branch while the blocking call holds the current task.
    let forced = if let Some(runtime) = ctx.shared_runtime.get() {
        let runtime = runtime.clone();
        let shutdown_task = tokio::spawn(async move { runtime.shutdown().await });

        tokio::select! {
            result = shutdown_task => {
                match result {
                    Ok(Err(e)) => warn!("Runtime shutdown error: {e}"),
                    Err(e) => warn!("Runtime shutdown task panicked: {e}"),
                    _ => {}
                }
                false
            }
            () = wait_for_signal() => {
                println!("Force shutting down...");
                warn!("Second signal received, forcing shutdown");
                true
            }
        }
    } else {
        false
    };

    // Force path: shutdown_force does everything except the graceful VM
    // stop (port forwarding, force VM kill, other machines, network manager).
    if forced {
        if let Some(runtime) = ctx.shared_runtime.get() {
            let _ = runtime.shutdown_force().await;
        }
    }

    cleanup(&ctx).await;
    info!("ArcBox daemon stopped");

    if forced {
        // The spawned graceful shutdown task is still blocked in the sync
        // request_stop() call (no locks held, but still occupies a tokio
        // worker thread). process::exit avoids hanging in runtime drop.
        std::process::exit(0);
    }

    Ok(())
}

async fn cleanup(ctx: &DaemonContext) {
    #[cfg(target_os = "macos")]
    {
        arcbox_core::route_reconciler::remove_route().await;
    }

    if ctx.docker_integration {
        if let Ok(ctx_manager) = DockerContextManager::new(ctx.socket_path.clone()) {
            let _ = ctx_manager.disable();
        }
    }

    remove_sockets(ctx);
}

fn remove_sockets(ctx: &DaemonContext) {
    for path in [&ctx.socket_path, &ctx.grpc_socket] {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove socket {}: {}", path.display(), e);
            }
        }
    }

    // daemon.lock is deliberately kept on disk — the flock is released
    // automatically when the process exits, and the next daemon reuses the
    // file. Removing it would race with a concurrent startup.
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
