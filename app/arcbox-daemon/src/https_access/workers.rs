//! Runs independent System and machine observers through joined cancellation.

use std::{future::Future, sync::Arc};

use anyhow::Result;
use arcbox_api::HttpsAccess;
use arcbox_core::Runtime;
use arcbox_engine::machine::MachineState;
use arcbox_engine::vm_lifecycle::VmLifecycleState;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{POLL_INTERVAL, RuntimeSource, observe};
use crate::{
    context::DaemonContext,
    guest_workers::{join_services, supervise_machines},
};

/// Starts observation without gating guest readiness on certificate trust.
pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let shutdown = ctx.shutdown.child_token();
    let access = ctx.setup_state.https_access.clone();
    let system = observe(
        RuntimeSource(Arc::clone(runtime), runtime.default_machine_name().into()),
        access.clone(),
        runtime.subscribe_system_vm_state(),
        VmLifecycleState::is_ready,
        shutdown.clone(),
    );
    let machines = machines(Arc::clone(runtime), access, shutdown.clone());
    ctx.https_access_service
        .start(run(system, machines, shutdown))
        .await;
}

async fn machines(
    runtime: Arc<Runtime>,
    access: HttpsAccess,
    shutdown: CancellationToken,
) -> Result<()> {
    let events = runtime.event_bus().subscribe();
    let names = Arc::clone(&runtime);
    supervise_machines(
        events,
        shutdown,
        POLL_INTERVAL,
        move || {
            names
                .machine_manager()
                .list()
                .into_iter()
                .filter(|machine| {
                    machine.state == MachineState::Running
                        && machine.distro.is_some()
                        && machine.name != names.default_machine_name()
                })
                .map(|machine| machine.name)
                .collect()
        },
        move |name, changed, shutdown| {
            let observation = observe(
                RuntimeSource(Arc::clone(&runtime), name),
                access.clone(),
                changed,
                |()| true,
                shutdown,
            );
            async move {
                observation.await;
                Ok(())
            }
        },
    )
    .await
}

pub(super) async fn run(
    system: impl Future<Output = ()> + Send + 'static,
    machines: impl Future<Output = Result<()>> + Send + 'static,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        system.await;
        Ok(())
    });
    tasks.spawn(machines);
    join_services(tasks, shutdown).await
}
