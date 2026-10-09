//! Retain System VM startup without blocking the Machine host.

use std::{future::Future, sync::Arc};

use anyhow::{Context, Result};
use arcbox_connect::v1::{storage_recovery_progress, system_runtime_status::State};
use arcbox_core::{InitProgress, Runtime};
use tokio::sync::broadcast;

use crate::context::{DaemonContext, SystemServiceHandles};
use crate::{recovery, services};

pub(super) async fn run(
    ctx: Arc<DaemonContext>,
    runtime: Arc<Runtime>,
    assets: super::assets::PreparedAssets,
) -> Result<()> {
    supervise(&ctx, &runtime, || initialize(&ctx, &runtime, assets)).await
}

async fn supervise<F: Future<Output = Result<SystemServiceHandles>>>(
    ctx: &DaemonContext,
    runtime: &Arc<Runtime>,
    mut initialize_system: impl FnMut() -> F,
) -> Result<()> {
    if !runtime.config().vm.autostart {
        ctx.setup_state
            .set_system_runtime(State::Disabled, "System VM is disabled");
        return Ok(());
    }
    loop {
        if ctx.shutdown.is_cancelled() {
            return Ok(());
        }
        ctx.setup_state
            .set_system_runtime(State::Starting, "Preparing the System VM");
        let (result, recovery_updates, previous_operation) = {
            let _operation = tokio::select! {
                biased;
                () = ctx.shutdown.cancelled() => return Ok(()),
                operation = runtime.reserve_system_startup() => operation,
            };
            let (previous, updates) = runtime.subscribe_storage_recovery();
            let result = initialize_system().await;
            if result.is_ok() && !ctx.shutdown.is_cancelled() {
                ctx.setup_state
                    .set_system_runtime(State::Ready, "Docker runtime ready");
            }
            (
                result,
                updates,
                previous.map(|progress| progress.operation_id),
            )
        };
        match result {
            Ok(mut handles) => {
                ctx.shutdown.cancelled().await;
                crate::shutdown::drain_system(&mut handles).await;
                return Ok(());
            }
            Err(error) => {
                if ctx.shutdown.is_cancelled() {
                    return Ok(());
                }
                tracing::error!(%error, "System startup failed; Machine host remains available");
                ctx.setup_state
                    .set_system_runtime(State::Failed, &format!("{error:#}"));
            }
        }
        if !wait_for_recovery(ctx, runtime, previous_operation, recovery_updates).await {
            return Ok(());
        }
    }
}

async fn wait_for_recovery(
    ctx: &DaemonContext,
    runtime: &Runtime,
    previous_operation: Option<String>,
    mut updates: broadcast::Receiver<arcbox_connect::v1::StorageRecoveryProgress>,
) -> bool {
    loop {
        let progress = tokio::select! {
            biased;
            () = ctx.shutdown.cancelled() => return false,
            update = updates.recv() => match update {
                Ok(progress) => Some(progress),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (latest, receiver) = runtime.subscribe_storage_recovery();
                    updates = receiver;
                    latest
                }
                Err(broadcast::error::RecvError::Closed) => return false,
            },
        };
        // A prior recovery can publish completion after releasing the startup operation lock.
        if progress.is_some_and(|progress| {
            previous_operation.as_deref() != Some(progress.operation_id.as_str())
                && progress.phase == storage_recovery_progress::Phase::Complete
                && !progress.storage_protected
        }) {
            return true;
        }
    }
}

async fn initialize(
    ctx: &DaemonContext,
    runtime: &Arc<Runtime>,
    assets: super::assets::PreparedAssets,
) -> Result<SystemServiceHandles> {
    assets
        .prepare_system(runtime.config(), &ctx.setup_state)
        .await?;
    if ctx.container_network_lease_slot.get().is_none()
        && let Some(lease) = super::ContainerNetworkLease::acquire(runtime.config().container.cidr)?
    {
        ctx.container_network_lease_slot
            .set(lease)
            .map_err(|_| anyhow::anyhow!("container network lease already configured"))?;
    }
    runtime
        .init_system(|milestone| {
            let (state, reason) = match milestone {
                InitProgress::SystemVmStarting => (State::VmStarting, "Starting the System VM"),
                InitProgress::SystemVmReady => {
                    (State::VmReady, "System VM ready; waiting for Docker")
                }
            };
            ctx.setup_state.set_system_runtime(state, reason);
        })
        .await
        .context("Failed to initialize System VM")?;
    anyhow::ensure!(
        !runtime.machine_manager().storage_is_held()?,
        "System VM storage is protected; use storage recovery before starting Docker"
    );
    anyhow::ensure!(!ctx.shutdown.is_cancelled(), "System startup cancelled");

    if runtime.system_vm_backend().supports_nested_virt() {
        arcbox_computer::cleanup::initialize(runtime.as_ref())
            .await
            .context("Failed to initialize sandbox cleanup")?;
    }
    recovery::run_system(ctx, runtime).await?;
    anyhow::ensure!(!ctx.shutdown.is_cancelled(), "System startup cancelled");
    let handles = services::start_system_services(ctx, runtime).await?;
    crate::web_index::start(ctx, runtime).await;
    crate::kubernetes_lb::start(ctx, runtime).await;
    crate::sandbox_cleanup::spawn(ctx, runtime);
    services::enable_docker_integration(ctx);
    Ok(handles)
}
