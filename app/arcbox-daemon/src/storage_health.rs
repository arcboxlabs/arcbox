//! Mirrors guest storage observations without starting the VM or recording activity.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arcbox_api::SetupState;
use arcbox_connect::v1::StorageHealth;
use arcbox_core::{Runtime, VmLifecycleState};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let observer = RuntimeObserver(Arc::clone(runtime));
    let vm = runtime.subscribe_system_vm_state();
    let setup = Arc::clone(&ctx.setup_state);
    let shutdown = ctx.shutdown.clone();
    drop(tokio::spawn(async move {
        observe_loop(&observer, vm, &setup, &shutdown).await;
    }));
}

/// Keeps lifecycle policy testable without booting a VM or probing user disks.
trait Observer: Sync {
    fn generation(&self) -> u64;
    fn set_health(&self, generation: u64, health: Option<StorageHealth>);
    fn observe(
        &self,
        publish: impl Fn(StorageHealth) + Send + Sync,
    ) -> impl Future<Output = Result<()>> + Send;
}

struct RuntimeObserver(Arc<Runtime>);

impl Observer for RuntimeObserver {
    fn generation(&self) -> u64 {
        self.0.system_vm_restart_generation()
    }

    fn set_health(&self, generation: u64, health: Option<StorageHealth>) {
        self.0.set_system_storage_health(generation, health);
    }

    async fn observe(&self, publish: impl Fn(StorageHealth) + Send + Sync) -> Result<()> {
        let agent = self.0.connect_system_agent().await?;
        let mut stream = agent.watch_storage_health().await?;
        while let Some(snapshot) = stream.recv().await {
            publish(snapshot?);
        }
        anyhow::bail!("storage health watch ended");
    }
}

async fn observe_loop(
    observer: &impl Observer,
    mut vm: watch::Receiver<VmLifecycleState>,
    setup: &SetupState,
    shutdown: &CancellationToken,
) {
    loop {
        publish_health(observer, setup, observer.generation(), None);
        if vm.has_changed().is_err() {
            return;
        }
        if !vm.borrow_and_update().is_ready() {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = vm.changed() => if changed.is_err() { return },
            }
            continue;
        }
        let generation = observer.generation();
        let publish = |snapshot| {
            if observer.generation() == generation && vm.borrow().is_ready() {
                publish_health(observer, setup, generation, Some(snapshot));
            }
        };
        let result = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            () = guest_changed(observer, vm.clone(), generation) => continue,
            result = observer.observe(publish) => result,
        };
        publish_health(observer, setup, observer.generation(), None);
        if let Err(error) = result {
            tracing::warn!(%error, "storage health unavailable");
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = guest_changed(observer, vm.clone(), generation) => {},
            () = tokio::time::sleep(RECONNECT_DELAY) => {},
        }
    }
    publish_health(observer, setup, observer.generation(), None);
}

fn publish_health(
    observer: &impl Observer,
    setup: &SetupState,
    generation: u64,
    health: Option<StorageHealth>,
) {
    observer.set_health(generation, health.clone());
    setup.set_storage_health(health);
}

/// Running↔Idle does not open another connection or hold an activity lease.
async fn guest_changed(
    observer: &impl Observer,
    mut vm: watch::Receiver<VmLifecycleState>,
    generation: u64,
) {
    loop {
        if !vm.borrow_and_update().is_ready() || observer.generation() != generation {
            return;
        }
        if vm.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
