//! Relays leased guest HTTPS observations to an active Desktop authorization client.

use std::{net::IpAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use arcbox_api::HttpsAccess;
use arcbox_connect::v1::{ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse};
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime, VmLifecycleState};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::context::DaemonContext;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RPC_TIMEOUT: Duration = Duration::from_millis(750);

/// Starts observation without gating VM or Docker readiness on certificate trust.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    drop(tokio::spawn(observe(
        RuntimeSource(Arc::clone(runtime)),
        ctx.setup_state.https_access.clone(),
        runtime.subscribe_system_vm_state(),
        ctx.shutdown.clone(),
    )));
}

trait AccessSource {
    fn host_addresses(&self) -> Result<Vec<String>>;
    fn resolve(&self, hostname: &str) -> Option<IpAddr>;
    async fn reconcile(
        &mut self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse>;
}

struct RuntimeSource(Arc<Runtime>);

impl AccessSource for RuntimeSource {
    fn host_addresses(&self) -> Result<Vec<String>> {
        let bridge = self
            .0
            .machine_manager()
            .get(DEFAULT_MACHINE_NAME)
            .and_then(|machine| machine.bridge_ip_address)
            .context("System VM has no bridge address")?
            .parse()
            .context("invalid System VM bridge address")?;
        let addresses = crate::host_network::host_addresses_on_link(bridge)?;
        ensure!(
            !addresses.is_empty(),
            "host has no address on the VM bridge"
        );
        Ok(addresses
            .into_iter()
            .map(|address| address.to_string())
            .collect())
    }

    fn resolve(&self, hostname: &str) -> Option<IpAddr> {
        self.0
            .network_manager()
            .local_hosts_table()
            .read()
            .expect("local DNS hosts lock poisoned")
            .get(hostname)
            .copied()
    }

    async fn reconcile(
        &mut self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        Ok(self.0.reconcile_https_access(request, cancelled).await?)
    }
}

async fn observe(
    mut source: impl AccessSource,
    access: HttpsAccess,
    mut vm: watch::Receiver<VmLifecycleState>,
    shutdown: CancellationToken,
) {
    let mut active = None;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            changed = vm.changed() => if changed.is_err() { break; },
            _ = poll.tick() => {}
        }
        let ready = vm.borrow_and_update().is_ready();
        let cycle = ready.then(|| access.observing_cycle()).flatten();
        if active != cycle {
            disable(&mut source, active.take()).await;
        }
        let Some(observation_id) = cycle else {
            continue;
        };
        let host_addresses = match source.host_addresses() {
            Ok(addresses) => addresses,
            Err(error) => {
                debug!(%error, "cannot observe Mac HTTPS access");
                disable(&mut source, active.take()).await;
                continue;
            }
        };
        active = Some(observation_id.clone());
        let response = reconcile(
            &mut source,
            ReconcileHttpsAccessRequest {
                observation_id: observation_id.clone(),
                host_addresses,
                enabled: true,
                ..Default::default()
            },
            &shutdown,
        )
        .await;
        match response {
            Ok(response)
                if response.sequence > 0 && vm.borrow().is_ready() && !shutdown.is_cancelled() =>
            {
                if response
                    .destination_ip
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| source.resolve(&response.hostname) == Some(ip))
                {
                    access.record(&response.hostname, &observation_id);
                } else {
                    debug!(hostname = %response.hostname, "ignored HTTPS access without current DNS ownership");
                }
            }
            Ok(_) => {}
            Err(error) => {
                debug!(%error, "HTTPS access observation failed; guest lease will expire");
            }
        }
    }
    disable(&mut source, active).await;
}

async fn reconcile(
    source: &mut impl AccessSource,
    request: ReconcileHttpsAccessRequest,
    shutdown: &CancellationToken,
) -> Result<ReconcileHttpsAccessResponse> {
    let cancelled = shutdown.child_token();
    let operation = source.reconcile(request, &cancelled);
    tokio::pin!(operation);
    tokio::select! {
        biased;
        result = &mut operation => return result,
        () = cancelled.cancelled() => {},
        () = tokio::time::sleep(RPC_TIMEOUT) => cancelled.cancel(),
    }
    // Joining after socket shutdown prevents a late enable after the next disable.
    operation.await?;
    bail!("HTTPS access observation cancelled")
}

async fn disable(source: &mut impl AccessSource, observation_id: Option<String>) {
    let Some(observation_id) = observation_id else {
        return;
    };
    if let Err(error) = reconcile(
        source,
        ReconcileHttpsAccessRequest {
            observation_id,
            enabled: false,
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    {
        debug!(%error, "HTTPS access disable failed; guest lease will expire");
    }
}

#[cfg(test)]
mod tests;
