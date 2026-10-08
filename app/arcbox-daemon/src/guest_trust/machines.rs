//! Owns independent trust deliveries until every machine connection and RPC has joined.

use std::collections::{HashMap, HashSet};

use anyhow::Context as _;
use arcbox_core::{event::Event, machine::MachineState};
use tokio::{
    sync::{broadcast, broadcast::error::RecvError},
    task::{JoinError, JoinSet},
};

use super::*;

pub(super) async fn run(
    runtime: Arc<Runtime>,
    authority: Authority,
    shutdown: CancellationToken,
) -> Result<()> {
    let events = runtime.event_bus().subscribe();
    let source = Arc::clone(&runtime);
    supervise(
        events,
        shutdown,
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

struct Workers<F> {
    start: F,
    shutdown: CancellationToken,
    active: HashMap<String, (CancellationToken, watch::Sender<()>)>,
    tasks: JoinSet<(String, Result<()>)>,
}

impl<F, Fut> Workers<F>
where
    F: Fn(String, watch::Receiver<()>, CancellationToken) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    fn sync(&mut self, names: HashSet<String>) {
        if self.shutdown.is_cancelled() {
            return;
        }
        for (name, (shutdown, changed)) in &self.active {
            if names.contains(name) {
                changed.send_replace(());
            } else {
                shutdown.cancel();
            }
        }
        for name in names {
            if self.active.contains_key(&name) {
                continue;
            }
            let shutdown = self.shutdown.child_token();
            let (changed, changes) = watch::channel(());
            let task = (self.start)(name.clone(), changes, shutdown.clone());
            self.active.insert(name.clone(), (shutdown, changed));
            self.tasks.spawn(async move { (name, task.await) });
        }
    }

    fn completed(
        &mut self,
        joined: std::result::Result<(String, Result<()>), JoinError>,
    ) -> Result<()> {
        let (name, result) = joined.context("machine trust worker panicked")?;
        self.active.remove(&name);
        result.with_context(|| format!("machine {name} trust worker failed"))
    }
}

async fn supervise<F, Fut>(
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
    names: impl Fn() -> HashSet<String> + Send + Sync,
    start: F,
) -> Result<()>
where
    F: Fn(String, watch::Receiver<()>, CancellationToken) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut workers = Workers {
        start,
        shutdown: shutdown.clone(),
        active: HashMap::new(),
        tasks: JoinSet::new(),
    };
    let mut rescan = tokio::time::interval_at(
        tokio::time::Instant::now() + IDENTITY_INTERVAL,
        IDENTITY_INTERVAL,
    );
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut result = async {
        workers.sync(names());
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                _ = rescan.tick() => workers.sync(names()),
                event = events.recv() => match event {
                    Ok(Event::MachineStopping { name }) => {
                        if let Some((cancelled, _)) = workers.active.get(&name) {
                            cancelled.cancel();
                        }
                        workers.sync(names());
                    },
                    Ok(Event::MachineStarted { .. }
                        | Event::MachineStopCancelled { .. }
                        | Event::MachineStopped { .. }
                        | Event::MachineRemoved { .. })
                    | Err(RecvError::Lagged(_)) => workers.sync(names()),
                    Ok(_) => {},
                    Err(RecvError::Closed) => anyhow::bail!("machine trust event source closed"),
                },
                joined = workers.tasks.join_next(), if !workers.tasks.is_empty() => {
                    workers.completed(joined.expect("task set was not empty"))?;
                    workers.sync(names());
                },
            }
        }
    }
    .await;
    shutdown.cancel();
    // A cancelled worker retains its name until native work finishes; replacement cannot overlap.
    while let Some(joined) = workers.tasks.join_next().await {
        if let Err(error) = workers.completed(joined) {
            if result.is_ok() {
                result = Err(error);
            } else {
                tracing::warn!(%error, "machine trust worker failed during cleanup");
            }
        }
    }
    result
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
