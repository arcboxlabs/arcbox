//! Delivers captured machine policies and public roots through retained guest connections.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{MachineWebPorts, ReplaceMachineWebRequest};
use arcbox_core::{Runtime, machine::MachineState, runtime::CapturedMachineWeb};
use arcbox_engine::agent_client::{AgentClient, MachineWebSession};
use arcbox_local_ca::KeychainAuthority;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{context::DaemonContext, guest_workers::supervise_machines};

const RESCAN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Updates<A> {
    policies: watch::Receiver<()>,
    dns: watch::Receiver<u64>,
    authority: watch::Receiver<A>,
    certificate: fn(&A) -> Option<Vec<u8>>,
}

pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let runtime = Arc::clone(runtime);
    let names = Arc::clone(&runtime);
    let authority = ctx.local_ca.subscribe();
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
                let updates = Updates {
                    policies: changed,
                    dns: runtime.network_manager().subscribe_dns_registry(),
                    authority: authority.clone(),
                    certificate: |authority| {
                        authority
                            .as_ref()
                            .map(|ca| ca.certificate_pem().into_bytes())
                    },
                };
                run_machine(Arc::clone(&runtime), name, updates, shutdown)
            },
        ))
        .await;
}

async fn run_machine(
    runtime: Arc<Runtime>,
    name: String,
    mut updates: Updates<Option<Arc<KeychainAuthority>>>,
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
            connected(client, snapshot, updates.clone(), &shutdown, || {
                Ok(runtime.capture_machine_web(&name)?)
            })
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
            update = updates.policies.changed() => update?,
            update = updates.authority.changed() => update?,
            () = tokio::time::sleep(RESCAN_INTERVAL) => {}
        }
    }
    Ok(())
}

async fn connected<A: Clone + Send + Sync>(
    client: AgentClient,
    initial: CapturedMachineWeb,
    updates: Updates<A>,
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
    let mut changed = updates.policies.clone();
    let mut dns = updates.dns.clone();
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
        result = drive(&mut session, updates, &initial, &capture) => result,
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

async fn drive<A: Send + Sync>(
    session: &mut MachineWebSession,
    mut updates: Updates<A>,
    initial: &CapturedMachineWeb,
    capture: &(impl Fn() -> Result<CapturedMachineWeb> + Sync),
) -> Result<()> {
    session.opened().await?;
    let mut acknowledged = None;
    let mut source_revision = 0_u64;
    loop {
        updates.policies.borrow_and_update();
        let certificate = (updates.certificate)(&updates.authority.borrow_and_update());
        let snapshot = capture()?;
        ensure!(
            initial.same_identity(&snapshot),
            "machine web identity changed before policy delivery"
        );
        let desired = (snapshot.ports, certificate);
        if acknowledged.as_ref() != Some(&desired) {
            source_revision = source_revision
                .checked_add(1)
                .context("machine web source revision exhausted")?;
            session
                .replace(ReplaceMachineWebRequest {
                    source_revision,
                    ports: MachineWebPorts::from(snapshot.ports).into(),
                    ca_certificate_pem: desired.1.clone(),
                    ..Default::default()
                })
                .await?;
            ensure!(
                initial.same_identity(&capture()?),
                "machine web identity changed before policy acknowledgment"
            );
            acknowledged = Some(desired);
            continue;
        }
        tokio::select! {
            changed = updates.policies.changed() => changed?,
            changed = updates.authority.changed() => changed?,
        }
    }
}

#[cfg(test)]
mod tests;
