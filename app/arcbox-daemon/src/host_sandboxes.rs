//! Composes the host sandbox location (ADR 0010) and keeps its cleanup
//! stream served.
//!
//! The location is part of the Machine host, not of the System VM: it is
//! composed before host admission, so the Sandbox API can reach it while
//! the System VM is still booting, stopped, or disabled. A composition that
//! fails is logged and leaves the location unavailable — `GetCapabilities`
//! reports it so — rather than failing the daemon.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use arcbox_computer::cleanup;
use arcbox_core::{HOST_SANDBOX_ROUTE, Runtime, RuntimeSandboxNames, VmBackend};
use arcbox_engine::EngineError;
use arcbox_host_sandbox::{Backend, HostSandboxOptions, HostSandboxes};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::context::DaemonContext;

/// The VMM binary the HV backend spawns per sandbox; it ships and is signed
/// next to the daemon.
const HV_VMM_BINARY: &str = "arcbox-hv-vmm";

/// How long a broken cleanup stream waits before reconnecting.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Composes the location into `runtime` and starts serving its cleanup
/// stream. Never fails the daemon.
pub fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let host = match compose(runtime) {
        Ok(host) => Arc::new(host),
        Err(error) => {
            warn!(error = %format!("{error:#}"), "host sandboxes are unavailable");
            return;
        }
    };
    if !runtime.set_host_sandboxes(host) {
        warn!("host sandboxes were composed twice; keeping the first");
        return;
    }
    let runtime = Arc::clone(runtime);
    let shutdown = ctx.shutdown.clone();
    drop(tokio::spawn(async move {
        serve_cleanup(&runtime, shutdown).await;
    }));
}

/// Hands the host computers over before the runtime stops.
pub async fn shutdown(ctx: &DaemonContext) {
    if let Some(host) = ctx
        .early_runtime
        .get()
        .and_then(|runtime| runtime.host_sandboxes())
    {
        host.shutdown().await;
    }
}

fn compose(runtime: &Arc<Runtime>) -> anyhow::Result<HostSandboxes> {
    let config = runtime.config();
    let boot = config.boot_asset_config();
    let kernel = boot
        .custom_kernel
        .clone()
        .unwrap_or_else(|| boot.version_cache_dir().join("kernel"));
    let backend = match config.sandbox.backend {
        VmBackend::Hv => Backend::Hv,
        VmBackend::Vz => Backend::Vz,
        VmBackend::Qemu => {
            anyhow::bail!("[sandbox] backend must be \"hv\" or \"vz\"; QEMU runs no host sandboxes")
        }
    };
    let system_vm = runtime.subscribe_system_vm_state();
    let options = HostSandboxOptions {
        data_dir: config.data_dir.clone(),
        backend,
        kernel,
        hv_vmm: hv_vmm_path()?,
        domain: config.network.dns_domain.clone(),
        names: Arc::new(RuntimeSandboxNames::new(runtime)),
        docker_socket: config.docker.socket_path.clone(),
        system_docker_ready: Arc::new(move || system_vm.borrow().is_ready()),
    };
    info!(?backend, "composing host sandboxes");
    HostSandboxes::start(options)
}

/// `arcbox-hv-vmm` next to this executable.
fn hv_vmm_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locate the daemon executable")?;
    Ok(exe.with_file_name(HV_VMM_BINARY))
}

/// Completes the host location's cleanup tickets — the ones idle and TTL
/// expiry raise with no API call to complete them — for the life of the
/// daemon. The protocol is the nested location's (`cleanup::complete`),
/// except for a startup ticket: that one clears every sandbox's host state
/// on the nested location's behalf, so here it is only finalized. (The host
/// network has no startup token, so none is expected.)
async fn serve_cleanup(runtime: &Arc<Runtime>, shutdown: CancellationToken) {
    loop {
        let error = tokio::select! {
            () = shutdown.cancelled() => return,
            error = watch(runtime) => error,
        };
        warn!(%error, "host sandbox cleanup stream ended; reconnecting");
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
    }
}

async fn watch(runtime: &Arc<Runtime>) -> EngineError {
    let watcher = match runtime.get_agent(HOST_SANDBOX_ROUTE) {
        Ok(agent) => agent,
        Err(error) => return EngineError::Machine(error.to_string()),
    };
    let mut events = match watcher.sandbox_cleanup_events().await {
        Ok(events) => events,
        Err(error) => return error,
    };
    while let Some(event) = events.recv().await {
        let ticket = match event {
            Ok(ticket) => ticket,
            Err(error) => return error,
        };
        let mut agent = match runtime.get_agent(HOST_SANDBOX_ROUTE) {
            Ok(agent) => agent,
            Err(error) => return EngineError::Machine(error.to_string()),
        };
        let completed = if ticket.startup {
            match agent.sandbox_cleanup_prepare(&ticket).await {
                Ok(()) => agent.sandbox_cleanup_finalize(&ticket).await,
                Err(error) => Err(error),
            }
        } else {
            cleanup::complete(runtime.as_ref(), &mut agent, &ticket).await
        };
        if let Err(error) = completed {
            warn!(sandbox_id = %ticket.id, %error, "host sandbox cleanup failed");
        }
    }
    EngineError::Machine("host sandbox cleanup stream ended".into())
}
