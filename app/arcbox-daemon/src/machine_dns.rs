//! Keeps `<machine>.arcbox.local` in step with the machines that run.
//!
//! A machine's name is published at its bridge NIC address when it reaches
//! readiness and withdrawn when it stops or is removed; the System VM is
//! `default.arcbox.local`, its address recorded by the lifecycle's own
//! readiness. The loop follows the runtime's event bus and, on every machine
//! event, re-derives the answer from the machine's current record rather
//! than from the event's kind — so a missed event (a lagging receiver) is
//! repaired by one pass over every machine, and an event for a machine
//! without a bridge address (a guest whose bridge NIC got no lease, a VM
//! booted with the bridge NIC disabled) withdraws rather than skips. The
//! same pass runs once at start: the System VM boots in `boot_runtime`,
//! before this loop exists, so its `MachineStarted` is never received.

use std::net::IpAddr;
use std::sync::Arc;

use arcbox_core::Runtime;
use arcbox_core::event::Event;
use arcbox_core::machine::MachineState;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

/// Spawns the loop for the daemon's lifetime.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let events = runtime.event_bus().subscribe();
    let shutdown = ctx.shutdown.clone();
    let runtime = Arc::clone(runtime);
    drop(tokio::spawn(async move {
        run(&runtime, events, shutdown).await;
    }));
}

async fn run(
    runtime: &Runtime,
    mut events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
) {
    sync_all(runtime).await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            event = events.recv() => match event {
                Ok(
                    Event::MachineStarted { name }
                    | Event::MachineStopped { name }
                    | Event::MachineRemoved { name },
                ) => sync(runtime, &name).await,
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => sync_all(runtime).await,
                Err(RecvError::Closed) => break,
            },
        }
    }
}

/// Brings `name`'s DNS entry in line with its machine record.
async fn sync(runtime: &Runtime, name: &str) {
    let machine = runtime.machine_manager().get(name);
    let target = machine
        .as_ref()
        .and_then(|machine| published_address(machine.state, machine.bridge_ip_address.as_deref()));
    match target {
        Some(ip) => runtime.register_machine_dns(name, ip).await,
        None => runtime.deregister_machine_dns(name).await,
    }
}

/// Every machine the daemon knows or still publishes, for the lagged case:
/// a machine removed during the lag is only in the second set.
async fn sync_all(runtime: &Runtime) {
    let mut names: Vec<String> = runtime
        .machine_manager()
        .list()
        .into_iter()
        .map(|machine| machine.name)
        .collect();
    names.extend(runtime.registered_machine_dns_names().await);
    names.sort_unstable();
    names.dedup();
    for name in names {
        sync(runtime, &name).await;
    }
}

/// The address a machine in `state` is published at: its bridge address
/// while it runs, nothing otherwise. A bridge address that does not parse
/// is treated as absent rather than published as garbage.
fn published_address(state: MachineState, bridge_ip: Option<&str>) -> Option<IpAddr> {
    if state != MachineState::Running {
        return None;
    }
    bridge_ip?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arcbox_core::{Config, event::EventBus};

    use super::*;

    #[test]
    fn only_a_running_machine_with_a_bridge_address_is_published() {
        let ip: IpAddr = "192.168.64.5".parse().unwrap();
        assert_eq!(
            published_address(MachineState::Running, Some("192.168.64.5")),
            Some(ip)
        );
        assert_eq!(
            published_address(MachineState::Stopped, Some("192.168.64.5")),
            None
        );
        assert_eq!(published_address(MachineState::Running, None), None);
        assert_eq!(
            published_address(MachineState::Running, Some("not-an-address")),
            None
        );
    }

    async fn wait_until(mut condition: impl AsyncFnMut() -> bool) -> bool {
        for _ in 0..100 {
            if condition().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// The loop works from records, not events: a name published for a
    /// machine that no longer exists is withdrawn by the initial pass before
    /// any event arrives (the System VM's start predates the loop), and a
    /// started machine without a bridge address (the mock here) is not
    /// published.
    #[tokio::test]
    async fn the_loop_follows_machine_records_through_the_event_bus() {
        let tmp = tempfile::TempDir::new().unwrap();
        let runtime = Arc::new(
            Runtime::new(Config {
                data_dir: tmp.path().to_path_buf(),
                ..Default::default()
            })
            .expect("runtime"),
        );
        let bus: &EventBus = runtime.event_bus();
        runtime
            .register_machine_dns("gone", "192.168.64.9".parse().unwrap())
            .await;
        let shutdown = CancellationToken::new();
        let task = {
            let runtime = Arc::clone(&runtime);
            let events = bus.subscribe();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { run(&runtime, events, shutdown).await })
        };
        assert!(
            wait_until(async || runtime.registered_machine_dns_names().await.is_empty()).await,
            "the initial pass keeps a name no machine owns"
        );

        runtime
            .machine_manager()
            .register_mock_machine("no-bridge", 3)
            .unwrap();
        bus.publish(Event::MachineStarted {
            name: "no-bridge".into(),
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            runtime.registered_machine_dns_names().await,
            Vec::<String>::new()
        );

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("loop exits on shutdown")
            .expect("loop task panicked");
    }
}
