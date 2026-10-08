//! Reconciles the current public CA into one physical System VM and joins each request.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use arcbox_connect::v1::{
    reconcile_guest_trust_request::Desired, reconcile_guest_trust_response::Applied,
};
use arcbox_core::{Runtime, VmLifecycleState};
use arcbox_engine::{agent_client::AgentClient, vm::VmIncarnation};
use arcbox_local_ca::{KeychainAuthority, PublicAuthority};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const IDENTITY_INTERVAL: Duration = Duration::from_secs(30);

/// Starts guest trust independently of Docker readiness without changing macOS trust.
pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    ctx.guest_trust_service
        .start(run(
            Arc::clone(runtime),
            ctx.local_ca.subscribe(),
            ctx.shutdown.clone(),
        ))
        .await;
}

type Authority = watch::Receiver<Option<Arc<KeychainAuthority>>>;
type Identity = (VmIncarnation, String);

struct Target {
    identity: Identity,
    pem: Vec<u8>,
}

async fn run(
    runtime: Arc<Runtime>,
    mut authority: Authority,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut vm = runtime.subscribe_system_vm_state();
    let mut acknowledged = None;
    while !shutdown.is_cancelled() {
        authority.borrow_and_update();
        vm.borrow_and_update();
        let delay = match reconcile(&runtime, &authority, &vm, &shutdown, &mut acknowledged).await {
            Ok(true) => continue,
            Ok(false) => IDENTITY_INTERVAL,
            Err(error) => {
                acknowledged = None;
                tracing::warn!(%error, "guest trust reconciliation failed");
                RETRY_INTERVAL
            }
        };
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(()),
            changed = authority.changed() => changed?,
            changed = vm.changed() => changed?,
            () = tokio::time::sleep(delay) => {}
        }
    }
    Ok(())
}

async fn reconcile(
    runtime: &Runtime,
    authority: &Authority,
    vm: &watch::Receiver<VmLifecycleState>,
    shutdown: &CancellationToken,
    acknowledged: &mut Option<Identity>,
) -> Result<bool> {
    let Some(target) = capture(runtime, authority, vm)? else {
        // An unavailable CA preserves the guest root until a published CA returns.
        *acknowledged = None;
        return Ok(false);
    };
    if acknowledged.as_ref() == Some(&target.identity) {
        return Ok(false);
    }
    *acknowledged = None;
    let current = || {
        Ok(capture(runtime, authority, vm)?
            .is_some_and(|active| active.identity == target.identity))
    };
    let applied = attempt(
        async {
            Ok(runtime
                .connect_system_agent_expected(target.identity.0.clone())
                .await?)
        },
        &target.pem,
        &target.identity.1,
        shutdown,
        &current,
        source_changed(authority.clone(), vm.clone(), &current),
    )
    .await?;
    if applied {
        *acknowledged = Some(target.identity);
    }
    Ok(!applied)
}

fn capture(
    runtime: &Runtime,
    authority: &Authority,
    vm: &watch::Receiver<VmLifecycleState>,
) -> Result<Option<Target>> {
    if !vm.borrow().is_ready() {
        return Ok(None);
    }
    let Some(authority) = authority.borrow().clone() else {
        return Ok(None);
    };
    let incarnation = runtime.container_network_incarnation()?;
    let pem = authority.certificate_pem().into_bytes();
    let fingerprint = PublicAuthority::from_pem(&pem)?.fingerprint_sha256();
    Ok(Some(Target {
        identity: (incarnation, fingerprint),
        pem,
    }))
}

async fn source_changed<A: Send + Sync, V: Send + Sync>(
    mut authority: watch::Receiver<A>,
    mut vm: watch::Receiver<V>,
    current: &(impl Fn() -> Result<bool> + Sync),
) -> Result<()> {
    loop {
        authority.borrow_and_update();
        vm.borrow_and_update();
        if !current()? {
            return Ok(());
        }
        tokio::select! {
            changed = authority.changed() => changed?,
            changed = vm.changed() => changed?,
            () = tokio::time::sleep(IDENTITY_INTERVAL) => {}
        }
    }
}

async fn attempt(
    connecting: impl Future<Output = Result<AgentClient>>,
    pem: &[u8],
    fingerprint: &str,
    shutdown: &CancellationToken,
    current: &(impl Fn() -> Result<bool> + Sync),
    changed: impl Future<Output = Result<()>>,
) -> Result<bool> {
    if shutdown.is_cancelled() || !current()? {
        return Ok(false);
    }
    // Native connect owns a blocking worker. Finish it before shutdown or replacement.
    let client = connecting.await?;
    if shutdown.is_cancelled() || !current()? {
        return Ok(false);
    }
    let cancelled = shutdown.child_token();
    let request = client.reconcile_guest_trust(Desired::CertificatePem(pem.to_vec()), &cancelled);
    tokio::pin!(request);
    let applied = tokio::select! {
        result = &mut request => result,
        change = changed => {
            cancelled.cancel();
            let result = request.await;
            change?;
            result
        }
    }?;
    ensure!(
        matches!(applied, Applied::InstalledSha256(installed) if installed == fingerprint),
        "guest acknowledged a different CA fingerprint"
    );
    Ok(!shutdown.is_cancelled() && current()?)
}

#[cfg(test)]
mod tests;
