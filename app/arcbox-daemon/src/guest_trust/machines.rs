//! Owns independent trust deliveries until every machine connection and RPC has joined.

use anyhow::Context as _;
use arcbox_core::machine::MachineState;

use crate::guest_workers::supervise_machines;

use super::*;

pub(super) async fn run(
    runtime: Arc<Runtime>,
    authority: Authority,
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
            Machine {
                runtime: Arc::clone(&runtime),
                name,
                authority: authority.clone(),
                changed,
                shutdown,
            }
            .run()
        },
    )
    .await
}

struct Machine {
    runtime: Arc<Runtime>,
    name: String,
    authority: Authority,
    changed: watch::Receiver<()>,
    shutdown: CancellationToken,
}

impl Machine {
    async fn run(mut self) -> Result<()> {
        let mut acknowledged = None;
        while !self.shutdown.is_cancelled() {
            self.authority.borrow_and_update();
            self.changed.borrow_and_update();
            let delay = match self.reconcile(&mut acknowledged).await {
                Ok(true) => continue,
                Ok(false) => IDENTITY_INTERVAL,
                Err(error) => {
                    acknowledged = None;
                    tracing::warn!(machine = self.name, %error, "machine trust reconciliation failed");
                    RETRY_INTERVAL
                }
            };
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Ok(()),
                update = self.authority.changed() => update?,
                update = self.changed.changed() => update?,
                () = tokio::time::sleep(delay) => {}
            }
        }
        Ok(())
    }

    async fn reconcile(&self, acknowledged: &mut Option<Identity>) -> Result<bool> {
        let Some(target) = self.capture()? else {
            *acknowledged = None;
            return Ok(false);
        };
        if acknowledged.as_ref() == Some(&target.identity) {
            return Ok(false);
        }
        *acknowledged = None;
        let current = || {
            Ok(self
                .capture()?
                .is_some_and(|active| active.identity == target.identity))
        };
        let manager = Arc::clone(self.runtime.machine_manager());
        let _hold = manager.host_hold_expected(&self.name, &target.identity.0)?;
        let expected = target.identity.0.clone();
        let connection_name = self.name.clone();
        let applied = attempt(
            async move {
                Ok(tokio::task::spawn_blocking(move || {
                    manager.connect_agent_expected(&connection_name, &expected)
                })
                .await
                .context("machine trust connection worker panicked")??)
            },
            &target.pem,
            &target.identity.1,
            &self.shutdown,
            &current,
            source_changed(self.authority.clone(), self.changed.clone(), &current),
        )
        .await?;
        if applied {
            *acknowledged = Some(target.identity);
        }
        Ok(!applied)
    }

    fn capture(&self) -> Result<Option<Target>> {
        if !self
            .runtime
            .machine_manager()
            .get(&self.name)
            .is_some_and(|machine| machine.state == MachineState::Running)
        {
            return Ok(None);
        }
        let Some(authority) = self.authority.borrow().clone() else {
            return Ok(None);
        };
        let incarnation = self
            .runtime
            .machine_manager()
            .network_incarnation(&self.name)?;
        let pem = authority.certificate_pem().into_bytes();
        let fingerprint = PublicAuthority::from_pem(&pem)?.fingerprint_sha256();
        Ok(Some(Target {
            identity: (incarnation, fingerprint),
            pem,
        }))
    }
}

#[cfg(test)]
mod tests;
