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

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_api::{SetupPhase, SetupState};
    use arcbox_core::Config;
    use clap::Parser as _;
    use std::{pin::pin, task::Poll, time::Duration};
    use tokio::sync::oneshot;

    async fn fixture(autostart: bool) -> (tempfile::TempDir, Arc<DaemonContext>, Arc<Runtime>) {
        let directory = tempfile::tempdir().unwrap();
        let args = crate::DaemonArgs::parse_from([
            "arcbox-daemon",
            "--profile",
            "development",
            "--data-dir",
            directory.path().to_str().unwrap(),
            "--no-mount-nfs",
        ]);
        let handles = crate::context::StartupHandles::new(Arc::new(SetupState::new()));
        let early = super::super::init_early(args, handles).await.unwrap();
        let ctx = Arc::new(super::super::acquire_lock(early).await.unwrap());
        let mut config = Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        };
        config.vm.autostart = autostart;
        let runtime = Arc::new(Runtime::new(config).unwrap());
        runtime.prepare_host().await.unwrap();
        assert!(ctx.host_runtime.set(Arc::clone(&runtime)).is_ok());
        ctx.setup_state
            .set_phase(SetupPhase::Ready, "Machine host ready");
        (directory, ctx, runtime)
    }

    #[tokio::test]
    async fn disabled_system_preserves_host_admission() {
        let (_directory, ctx, runtime) = fixture(false).await;
        supervise(&ctx, &runtime, || async {
            panic!("disabled System VM must not initialize")
        })
        .await
        .unwrap();
        assert_eq!(
            ctx.setup_state.current().system_runtime.state,
            State::Disabled
        );
        assert_eq!(ctx.setup_state.current().phase, SetupPhase::Ready);
        assert!(ctx.host_runtime.get().is_some());
        assert!(ctx.shared_runtime.get().is_none());
    }

    #[tokio::test]
    async fn failed_system_keeps_host_ready_and_reports_its_error() {
        let (_directory, ctx, runtime) = fixture(true).await;
        let mut task = pin!(supervise(&ctx, &runtime, || async {
            anyhow::bail!("injected System startup failure")
        }));
        assert!(matches!(futures::poll!(&mut task), Poll::Pending));
        let status = ctx.setup_state.current();
        assert_eq!(status.system_runtime.state, State::Failed);
        assert!(
            status
                .system_runtime
                .reason
                .contains("injected System startup failure")
        );
        assert_eq!(status.phase, SetupPhase::Ready);
        assert_eq!(status.error, "");
        assert!(ctx.host_runtime.get().is_some());
        assert!(ctx.shared_runtime.get().is_none());
        ctx.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn system_waits_for_a_running_operation_before_initializing() {
        let (_directory, ctx, runtime) = fixture(true).await;
        let operation = runtime.reserve_system_startup().await;
        let mut task = pin!(supervise(&ctx, &runtime, || async {
            Ok(SystemServiceHandles {
                docker: None,
                kubernetes_proxy: None,
                route_guard: None,
            })
        }));
        assert!(matches!(futures::poll!(&mut task), Poll::Pending));
        assert_eq!(
            ctx.setup_state.current().system_runtime.state,
            State::Starting
        );
        drop(operation);
        assert!(matches!(futures::poll!(&mut task), Poll::Pending));
        assert_eq!(ctx.setup_state.current().system_runtime.state, State::Ready);
        assert_eq!(ctx.setup_state.current().phase, SetupPhase::Ready);
        ctx.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_system_admission_without_releasing_another_operation() {
        let (_directory, ctx, runtime) = fixture(true).await;
        let operation = runtime.reserve_system_startup().await;
        let mut task = pin!(supervise(&ctx, &runtime, || async {
            panic!("System startup must not acquire another operation's reservation")
        }));
        assert!(matches!(futures::poll!(&mut task), Poll::Pending));
        ctx.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ctx.setup_state.current().system_runtime.state,
            State::Starting
        );
        assert!(ctx.shared_runtime.get().is_none());
        drop(operation);
    }

    #[tokio::test]
    async fn only_a_new_successful_recovery_retries_system() {
        use arcbox_connect::v1::StorageRecoveryProgress;
        use storage_recovery_progress::Phase;

        let (_directory, ctx, runtime) = fixture(true).await;
        let (sender, updates) = broadcast::channel(4);
        let mut waiting = pin!(wait_for_recovery(
            &ctx,
            &runtime,
            Some("previous".into()),
            updates
        ));
        for progress in [
            StorageRecoveryProgress {
                operation_id: "previous".into(),
                phase: Phase::Complete.into(),
                ..Default::default()
            },
            StorageRecoveryProgress {
                operation_id: "check-only".into(),
                phase: Phase::Complete.into(),
                storage_protected: true,
                ..Default::default()
            },
            StorageRecoveryProgress {
                operation_id: "failed".into(),
                phase: Phase::Failed.into(),
                ..Default::default()
            },
        ] {
            sender.send(progress).unwrap();
            assert!(matches!(futures::poll!(&mut waiting), Poll::Pending));
        }
        sender
            .send(StorageRecoveryProgress {
                operation_id: "new-recovery".into(),
                phase: Phase::Complete.into(),
                ..Default::default()
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn pending_system_retains_cleanup_after_host_shutdown() {
        let (_directory, ctx, runtime) = fixture(true).await;
        let task_ctx = Arc::clone(&ctx);
        let (entered, started) = oneshot::channel();
        let (release, cleanup) = oneshot::channel();
        let mut input = Some((entered, cleanup));
        ctx.system_service
            .start(async move {
                supervise(&task_ctx, &runtime, || {
                    let (entered, cleanup) = input.take().unwrap();
                    async move {
                        entered.send(()).unwrap();
                        cleanup.await.unwrap();
                        anyhow::bail!("cancelled initialization")
                    }
                })
                .await
            })
            .await;
        started.await.unwrap();
        assert_eq!(ctx.setup_state.current().phase, SetupPhase::Ready);
        assert_eq!(
            ctx.setup_state.current().system_runtime.state,
            State::Starting
        );
        assert!(ctx.host_runtime.get().is_some());
        assert!(ctx.shared_runtime.get().is_none());
        ctx.shutdown.cancel();
        let mut joining = std::pin::pin!(ctx.system_service.join());
        assert!(matches!(
            futures::poll!(&mut joining),
            std::task::Poll::Pending
        ));
        release.send(()).unwrap();
        joining.await.unwrap();
    }
}
