//! Delivers complete instance authority to one physical System VM and joins each connection.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use arcbox_connect::v1::{
    InstanceAuthorityRoot, ReplaceInstanceAuthorityRequest, instance_authority_root::Target,
};
use arcbox_core::Runtime;
use arcbox_dns::registry::InstanceTarget;
use arcbox_engine::agent_client::{AgentClient, InstanceAuthoritySession};
use arcbox_engine::vm_lifecycle::VmLifecycleState;
use arcbox_net::NetworkManager;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const IDENTITY_CHECK_INTERVAL: Duration = Duration::from_secs(30);

pub(super) async fn run(runtime: Arc<Runtime>, shutdown: CancellationToken) {
    let mut vm = runtime.subscribe_system_vm_state();
    while !shutdown.is_cancelled() {
        let delay = if let Ok(expected) = runtime.container_network_incarnation() {
            // Dropping this future detaches the native connect task, so await it during cancellation.
            let result = match runtime
                .connect_system_agent_expected(expected.clone())
                .await
            {
                Ok(client) if !shutdown.is_cancelled() => {
                    connected(
                        client,
                        runtime.network_manager(),
                        &shutdown,
                        vm.clone(),
                        || {
                            ensure!(
                                runtime.container_network_incarnation()? == expected,
                                "System VM changed during authority delivery"
                            );
                            Ok(())
                        },
                    )
                    .await
                }
                Ok(_) => return,
                Err(error) => Err(error.into()),
            };
            if let Err(error) = result {
                tracing::warn!(%error, "guest authority connection ended; retrying");
            }
            Duration::from_secs(2)
        } else {
            IDENTITY_CHECK_INTERVAL
        };
        tokio::select! {
            () = shutdown.cancelled() => return,
            changed = vm.changed() => if changed.is_err() { return; },
            () = tokio::time::sleep(delay) => {}
        }
    }
}

async fn connected(
    client: AgentClient,
    manager: &NetworkManager,
    shutdown: &CancellationToken,
    mut vm: watch::Receiver<VmLifecycleState>,
    current: impl Fn() -> Result<()> + Send + Sync,
) -> Result<()> {
    current()?;
    let (mut session, mut coordinator) = client.instance_authority()?;
    let mut completed = false;
    let result = tokio::select! {
        biased;
        () = shutdown.cancelled() => Ok(()),
        result = &mut coordinator => {
            completed = true;
            result.context("guest authority coordinator failed")?.map_err(Into::into)
        }
        result = async {
            loop {
                current()?;
                tokio::select! {
                    changed = vm.changed() => changed?,
                    () = tokio::time::sleep(IDENTITY_CHECK_INTERVAL) => {}
                }
            }
        } => result,
        result = drive(&mut session, manager, &current) => result,
    };
    drop(session);
    if !completed {
        coordinator
            .await
            .context("guest authority cleanup failed")??;
    }
    result
}

async fn drive(
    session: &mut InstanceAuthoritySession,
    manager: &NetworkManager,
    current: &(impl Fn() -> Result<()> + Sync),
) -> Result<()> {
    let mut changes = manager.subscribe_dns_registry();
    session.opened().await?;
    current()?;
    let mut acknowledged = None;
    loop {
        changes.borrow_and_update();
        let snapshot = snapshot(manager);
        if acknowledged != Some(snapshot.authority_revision) {
            current()?;
            let revision = snapshot.authority_revision;
            session.replace(snapshot).await?;
            current()?;
            acknowledged = Some(revision);
            continue;
        }
        changes.changed().await?;
    }
}

fn snapshot(manager: &NetworkManager) -> ReplaceInstanceAuthorityRequest {
    let registry = manager.dns_registry();
    let registry = registry.read().expect("DNS registry lock poisoned");
    let (authority_revision, roots) = registry.instance_authorities();
    ReplaceInstanceAuthorityRequest {
        authority_revision,
        roots: roots
            .iter()
            .map(|root| {
                let (target, dns_port) = match root.target {
                    InstanceTarget::Current => (Target::Current, 0),
                    InstanceTarget::Other { port } => (Target::Other, u32::from(port)),
                    InstanceTarget::Unavailable => (Target::Unavailable, 0),
                };
                InstanceAuthorityRoot {
                    root: root.root.to_string(),
                    target: target.into(),
                    dns_port,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
