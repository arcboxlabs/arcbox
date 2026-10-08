//! Runs one retained signing connection worker for each running machine.

use arcbox_core::machine::MachineState;

use crate::guest_workers::supervise_machines;

use super::*;

pub(super) async fn run(
    runtime: Arc<Runtime>,
    authority: watch::Receiver<Option<Arc<KeychainAuthority>>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let events = runtime.event_bus().subscribe();
    let source = Arc::clone(&runtime);
    supervise_machines(
        events,
        shutdown,
        IDENTITY_INTERVAL,
        move || {
            source
                .machine_manager()
                .list()
                .into_iter()
                .filter(|machine| {
                    machine.state == MachineState::Running
                        && machine.name != source.default_machine_name()
                })
                .map(|machine| machine.name)
                .collect()
        },
        move |name, changed, shutdown| {
            run_machine(
                Arc::clone(&runtime),
                authority.clone(),
                name,
                changed,
                shutdown,
            )
        },
    )
    .await
}

async fn run_machine(
    runtime: Arc<Runtime>,
    authority: watch::Receiver<Option<Arc<KeychainAuthority>>>,
    name: String,
    changed: watch::Receiver<()>,
    shutdown: CancellationToken,
) -> Result<()> {
    while !shutdown.is_cancelled() {
        let result = serve_once(&runtime, &authority, &name, changed.clone(), &shutdown).await;
        if shutdown.is_cancelled() {
            break;
        }
        if let Err(error) = result {
            tracing::warn!(machine = name, %error, "machine TLS signing connection failed");
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(RETRY_INTERVAL) => {}
            }
        }
    }
    Ok(())
}
