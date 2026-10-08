//! Delivers captured machine web configuration and drains every retained guest connection.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{MachineWebPorts, ReplaceMachineWebRequest};
use arcbox_core::{Runtime, machine::MachineState, runtime::CapturedMachineWeb};
use arcbox_engine::agent_client::{AgentClient, MachineWebSession};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{context::DaemonContext, guest_workers::supervise_machines};

const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let runtime = Arc::clone(runtime);
    let names = Arc::clone(&runtime);
    ctx.machine_web_service
        .start(supervise_machines(
            runtime.event_bus().subscribe(),
            ctx.shutdown.child_token(),
            RESCAN_INTERVAL,
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
                run_machine(Arc::clone(&runtime), name, changed, shutdown)
            },
        ))
        .await;
}

async fn run_machine(
    runtime: Arc<Runtime>,
    name: String,
    mut changed: watch::Receiver<()>,
    shutdown: CancellationToken,
) -> Result<()> {
    while !shutdown.is_cancelled() {
        let result = async {
            let snapshot = runtime.capture_machine_web(&name)?;
            let _hold = runtime.hold_web_guest(&snapshot.guest)?;
            let connecting = Arc::clone(&runtime);
            let guest = snapshot.guest.clone();
            let cancelled = shutdown.clone();
            // Await native connection completion before releasing the captured machine hold.
            let client = tokio::task::spawn_blocking(move || -> Result<_> {
                ensure!(
                    !cancelled.is_cancelled(),
                    "machine web connection cancelled"
                );
                Ok(connecting.connect_web_guest_agent(&guest)?)
            })
            .await
            .context("machine web connection task failed")??;
            connected(
                client,
                snapshot,
                changed.clone(),
                runtime.network_manager().subscribe_dns_registry(),
                &shutdown,
                || Ok(runtime.capture_machine_web(&name)?),
            )
            .await
        }
        .await;
        if shutdown.is_cancelled() {
            break;
        }
        if let Err(error) = result {
            tracing::warn!(machine = name, %error, "machine web configuration session ended");
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            update = changed.changed() => update?,
            () = tokio::time::sleep(RESCAN_INTERVAL) => {}
        }
    }
    Ok(())
}

async fn connected(
    client: AgentClient,
    initial: CapturedMachineWeb,
    mut changed: watch::Receiver<()>,
    mut dns: watch::Receiver<u64>,
    shutdown: &CancellationToken,
    capture: impl Fn() -> Result<CapturedMachineWeb> + Send + Sync,
) -> Result<()> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    ensure!(
        initial.same_identity(&capture()?),
        "machine web identity changed during connection"
    );
    let (mut session, mut coordinator) = client.machine_web(initial.open.clone())?;
    let policy_changes = changed.clone();
    let mut completed = false;
    let result = tokio::select! {
        biased;
        () = shutdown.cancelled() => Ok(()),
        result = &mut coordinator => {
            completed = true;
            result.context("machine web coordinator failed")?.map_err(Into::into)
        },
        result = async {
            loop {
                changed.borrow_and_update();
                dns.borrow_and_update();
                ensure!(initial.same_identity(&capture()?), "machine web identity changed");
                tokio::select! {
                    update = changed.changed() => update?,
                    update = dns.changed() => update?,
                }
            }
        } => result,
        result = drive(&mut session, policy_changes, &initial, &capture) => result,
    };
    drop(session);
    if !completed {
        let drained = coordinator
            .await
            .context("machine web cleanup failed")
            .and_then(|result| result.map_err(Into::into));
        if result.is_err()
            && let Err(error) = &drained
        {
            tracing::warn!(%error, "machine web cleanup also failed");
        }
        return result.and(drained);
    }
    result
}

async fn drive(
    session: &mut MachineWebSession,
    mut changed: watch::Receiver<()>,
    initial: &CapturedMachineWeb,
    capture: &(impl Fn() -> Result<CapturedMachineWeb> + Sync),
) -> Result<()> {
    session.opened().await?;
    let mut acknowledged = None;
    let mut source_revision = 0_u64;
    loop {
        changed.borrow_and_update();
        let snapshot = capture()?;
        ensure!(
            initial.same_identity(&snapshot),
            "machine web identity changed before policy delivery"
        );
        if acknowledged != Some(snapshot.ports) {
            source_revision = source_revision
                .checked_add(1)
                .context("machine web source revision exhausted")?;
            session
                .replace(ReplaceMachineWebRequest {
                    source_revision,
                    ports: MachineWebPorts::from(snapshot.ports).into(),
                    ..Default::default()
                })
                .await?;
            ensure!(
                initial.same_identity(&capture()?),
                "machine web identity changed before policy acknowledgment"
            );
            acknowledged = Some(snapshot.ports);
            continue;
        }
        changed.changed().await?;
    }
}

#[cfg(test)]
mod tests;
