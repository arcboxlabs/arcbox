//! Relays leased guest HTTPS observations to an active Desktop authorization client.

use std::{net::IpAddr, sync::Arc, time::Duration};

use anyhow::{Result, bail, ensure};
use arcbox_api::HttpsAccess;
use arcbox_connect::v1::{ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse};
use arcbox_core::{Runtime, runtime::CapturedWebGuest};
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::debug;

mod workers;
pub use workers::start;

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RPC_TIMEOUT: Duration = Duration::from_millis(750);

trait AccessSource {
    type Vm: Clone + Eq + Send + Sync;
    fn incarnation(&self) -> Result<Self::Vm>;
    fn with_claim<T>(
        &self,
        expected: &Self::Vm,
        hostname: &str,
        publish: impl FnOnce(IpAddr) -> T,
    ) -> Result<Option<T>>;
    fn host_addresses(&self, expected: &Self::Vm) -> Result<Vec<String>>;
    async fn reconcile(
        &mut self,
        expected: &Self::Vm,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse>;
}

struct RuntimeSource(Arc<Runtime>, String);

impl AccessSource for RuntimeSource {
    type Vm = CapturedWebGuest;

    fn incarnation(&self) -> Result<Self::Vm> {
        Ok(self.0.capture_web_guest(&self.1)?)
    }

    fn with_claim<T>(
        &self,
        expected: &Self::Vm,
        hostname: &str,
        publish: impl FnOnce(IpAddr) -> T,
    ) -> Result<Option<T>> {
        Ok(self
            .0
            .with_web_guest_claim(expected, hostname, |address, _| publish(address))?)
    }

    fn host_addresses(&self, expected: &Self::Vm) -> Result<Vec<String>> {
        let bridge = self.0.web_guest_bridge_address(expected)?;
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

    async fn reconcile(
        &mut self,
        expected: &Self::Vm,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        Ok(self
            .0
            .reconcile_https_access(expected, request, cancelled)
            .await?)
    }
}

async fn observe<T: Send + Sync>(
    mut source: impl AccessSource,
    access: HttpsAccess,
    mut vm: watch::Receiver<T>,
    is_ready: fn(&T) -> bool,
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
        let ready = is_ready(&vm.borrow_and_update());
        let cycle = ready.then(|| access.observing_cycle()).flatten();
        let target = match cycle {
            Some(cycle) => match source.incarnation() {
                Ok(expected) => Some((cycle, expected)),
                Err(error) => {
                    debug!(%error, "cannot capture HTTPS observation VM");
                    None
                }
            },
            None => None,
        };
        if active != target {
            disable(&mut source, active.take(), &mut vm).await;
        }
        let Some((observation_id, expected)) = target else {
            continue;
        };
        let host_addresses = match source.host_addresses(&expected) {
            Ok(addresses) => addresses,
            Err(error) => {
                debug!(%error, "cannot observe Mac HTTPS access");
                disable(&mut source, active.take(), &mut vm).await;
                continue;
            }
        };
        active = Some((observation_id.clone(), expected.clone()));
        let response = reconcile(
            &mut source,
            &expected,
            ReconcileHttpsAccessRequest {
                observation_id: observation_id.clone(),
                host_addresses,
                enabled: true,
                ..Default::default()
            },
            &shutdown,
            &mut vm,
        )
        .await;
        match response {
            Ok(response) if response.sequence > 0 => {
                let ready = is_ready(&vm.borrow());
                if ready && !shutdown.is_cancelled() {
                    let recorded = source.with_claim(&expected, &response.hostname, |address| {
                        if !shutdown.is_cancelled()
                            && response.destination_ip.parse::<IpAddr>() == Ok(address)
                        {
                            access.record(&response.hostname, &observation_id);
                        }
                    });
                    if let Err(error) = recorded {
                        debug!(%error, "ignored HTTPS access from a retired VM");
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                debug!(%error, "HTTPS access observation failed; guest lease will expire");
            }
        }
    }
    disable(&mut source, active, &mut vm).await;
}

async fn reconcile<S: AccessSource, T: Send + Sync>(
    source: &mut S,
    expected: &S::Vm,
    request: ReconcileHttpsAccessRequest,
    shutdown: &CancellationToken,
    vm: &mut watch::Receiver<T>,
) -> Result<ReconcileHttpsAccessResponse> {
    let cancelled = shutdown.child_token();
    let operation = async {
        ensure!(
            !cancelled.is_cancelled(),
            "HTTPS access observation cancelled before connection"
        );
        source.reconcile(expected, request, &cancelled).await
    };
    tokio::pin!(operation);
    tokio::select! {
        biased;
        () = cancelled.cancelled() => {},
        _ = vm.changed() => {},
        () = tokio::time::sleep(RPC_TIMEOUT) => {},
        result = &mut operation => return result,
    }
    cancelled.cancel();
    // Joining after socket shutdown prevents a late enable after the next disable.
    operation.await?;
    bail!("HTTPS access observation cancelled")
}

async fn disable<S: AccessSource, T: Send + Sync>(
    source: &mut S,
    active: Option<(String, S::Vm)>,
    vm: &mut watch::Receiver<T>,
) {
    let Some((observation_id, expected)) = active else {
        return;
    };
    if let Err(error) = reconcile(
        source,
        &expected,
        ReconcileHttpsAccessRequest {
            observation_id,
            enabled: false,
            ..Default::default()
        },
        &CancellationToken::new(),
        vm,
    )
    .await
    {
        debug!(%error, "HTTPS access disable failed; guest lease will expire");
    }
}

#[cfg(test)]
mod tests;
