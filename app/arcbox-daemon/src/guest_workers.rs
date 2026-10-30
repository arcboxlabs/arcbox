//! Retains guest services and per-machine workers through cancellation and native cleanup.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

use anyhow::{Context as _, Result};
use arcbox_engine::event::Event;
use tokio::{
    sync::{broadcast, broadcast::error::RecvError, watch},
    task::{JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// Cancels peer services after the first exit and joins every service before returning.
pub async fn join_services(
    mut tasks: JoinSet<Result<()>>,
    shutdown: CancellationToken,
) -> Result<()> {
    let first = tasks
        .join_next()
        .await
        .expect("guest services must not be empty");
    shutdown.cancel();
    let mut result = first
        .context("guest service panicked")
        .and_then(std::convert::identity);
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined
            .context("guest service panicked")
            .and_then(std::convert::identity)
        {
            if result.is_ok() {
                result = Err(error);
            } else {
                tracing::warn!(%error, "guest service failed during cleanup");
            }
        }
    }
    result
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
        let (name, result) = joined.context("machine worker panicked")?;
        self.active.remove(&name);
        result.with_context(|| format!("machine {name} worker failed"))
    }
}

/// Retains each machine worker until cancellation cleanup finishes before starting its replacement.
pub async fn supervise_machines<F, Fut>(
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
    rescan_interval: Duration,
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
        tokio::time::Instant::now() + rescan_interval,
        rescan_interval,
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
                    Err(RecvError::Closed) => anyhow::bail!("machine event source closed"),
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
                tracing::warn!(%error, "machine worker failed during cleanup");
            }
        }
    }
    result
}
