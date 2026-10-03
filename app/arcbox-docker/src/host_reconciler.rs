//! Keeps host-side container networking in step with guest dockerd's live
//! container set.
//!
//! Host port forwarding, DNS and name aliases are set up by `start_container`
//! and torn down by the `stop`/`kill`/`remove` handlers. Neither path sees a
//! container that changes state without an API call through this proxy: a
//! natural exit, `--rm` auto-remove, `docker prune`, an OOM kill, a `docker
//! stop` issued inside the guest — and, after a System VM restart, every
//! container dockerd brings back under its restart policy. This task closes
//! both gaps. Every [`RECONCILE_INTERVAL`] it lists the guest's running
//! containers, tears down host state for a registered container that is gone,
//! and sets up host state for a running container it does not know, through
//! the same inspect-driven path `start_container` uses. It also follows the
//! System VM's lifecycle: when the VM goes down it retires every piece of
//! container state at once (a listener kept alive would hold its port while
//! injecting into a dead datapath), and when the VM is back it reconciles
//! promptly — retrying every [`RETURN_RETRY`] until dockerd answers — rather
//! than waiting out a whole interval.

use crate::api::AppState;
use crate::error::Result;
use crate::guest_query::list_running_container_ids;
use crate::handlers::setup_container_networking;
use crate::proxy::ProxyState;
use arcbox_core::{Runtime, VmLifecycleState};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How often the guest's container set is compared with the host's state.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Cadence after the VM comes back, until the first listing succeeds:
/// dockerd is still starting — and restarting its containers — then.
const RETURN_RETRY: Duration = Duration::from_secs(2);

/// Spawns the reconciler, cancelled via `shutdown`.
///
/// Shares the router's [`ProxyState`] so queries go through the same pooled
/// client, and observes the System VM restart generation before each listing
/// so a query right after a backend switch dials the fresh VM instead of
/// failing once on a stale pooled connection.
pub fn spawn(runtime: Arc<Runtime>, proxy: Arc<ProxyState>, shutdown: CancellationToken) {
    let vm = runtime.subscribe_system_vm_state();
    let guest = ProxyGuest {
        state: AppState {
            runtime: Arc::clone(&runtime),
            proxy,
        },
    };
    drop(tokio::spawn(async move {
        reconcile_loop(&runtime, &guest, vm, shutdown).await;
    }));
}

/// The guest side of a reconcile; a seam so the loop is testable without
/// dockerd.
trait Guest: Send + Sync {
    /// The IDs of the containers running in the guest.
    fn running(&self) -> impl Future<Output = Result<HashSet<String>>> + Send;

    /// Sets up host networking for a running container the host does not
    /// know.
    fn set_up(&self, container_id: &str) -> impl Future<Output = ()> + Send;
}

/// Guest dockerd through the Docker proxy's pooled client.
struct ProxyGuest {
    state: AppState,
}

impl Guest for ProxyGuest {
    async fn running(&self) -> Result<HashSet<String>> {
        self.state
            .proxy
            .reset_if_restarted(self.state.runtime.system_vm_restart_generation());
        list_running_container_ids(self.state.proxy.client()).await
    }

    async fn set_up(&self, container_id: &str) {
        setup_container_networking(&self.state, container_id).await;
    }
}

/// Reconciles while the VM is ready, retires the containers' host state when
/// it goes down, and reconciles again promptly when it is back.
async fn reconcile_loop(
    runtime: &Runtime,
    guest: &impl Guest,
    mut vm: watch::Receiver<VmLifecycleState>,
    shutdown: CancellationToken,
) {
    let mut ready = vm.borrow_and_update().is_ready();
    // The guest has containers the host has never seen whenever the VM has
    // just come up — at daemon start as much as after a restart.
    let mut returning = true;
    loop {
        let delay = if returning {
            RETURN_RETRY
        } else {
            RECONCILE_INTERVAL
        };
        tokio::select! {
            () = shutdown.cancelled() => break,
            // The sender lives as long as the runtime; an error means it is
            // gone and there is nothing left to reconcile.
            changed = vm.changed() => {
                if changed.is_err() {
                    break;
                }
                let now_ready = vm.borrow_and_update().is_ready();
                if ready && !now_ready {
                    runtime.retire_system_vm_container_networking().await;
                }
                if now_ready && !ready {
                    returning = true;
                }
                ready = now_ready;
            }
            () = tokio::time::sleep(delay), if ready => {
                match reconcile(runtime, guest).await {
                    Ok(()) => returning = false,
                    // Fail-safe: a listing error changes nothing this cycle
                    // rather than risk tearing down live containers.
                    Err(e) => tracing::debug!(error = %e, "host networking reconcile skipped"),
                }
            }
        }
    }
}

/// Makes the host's container state match the guest's running set: tears
/// down what is registered but gone, sets up what runs but is unknown.
///
/// When the listing fails the error propagates and nothing changes, so a
/// transient guest hiccup can't strip live containers.
async fn reconcile(runtime: &Runtime, guest: &impl Guest) -> Result<()> {
    let registered = runtime.registered_container_ids().await;
    let running = guest.running().await?;
    for id in registered.difference(&running) {
        tracing::info!(
            container_id = %id,
            "reconciler tearing down host networking for a container no longer running"
        );
        runtime.stop_port_forwarding_by_id(id).await;
        runtime.deregister_dns_by_id(id).await;
    }
    for id in running.difference(&registered) {
        tracing::info!(
            container_id = %id,
            "reconciler setting up host networking for a running container it did not know"
        );
        guest.set_up(id).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DockerError;
    use arcbox_core::{Config, Runtime, VmLifecycleConfig};
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn test_runtime() -> (Arc<Runtime>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let vlc = VmLifecycleConfig {
            skip_vm_check: true,
            ..Default::default()
        };
        let runtime = Arc::new(Runtime::with_vm_lifecycle_config(config, vlc).expect("runtime"));
        (runtime, tmp)
    }

    /// Answers every listing the same way and records what it is asked to
    /// set up.
    struct FakeGuest {
        running: std::result::Result<HashSet<String>, String>,
        set_up: Mutex<Vec<String>>,
    }

    impl FakeGuest {
        fn running(ids: &[&str]) -> Self {
            Self {
                running: Ok(ids.iter().map(|id| (*id).to_owned()).collect()),
                set_up: Mutex::new(Vec::new()),
            }
        }

        fn unreachable() -> Self {
            Self {
                running: Err("guest unreachable".to_owned()),
                set_up: Mutex::new(Vec::new()),
            }
        }

        fn set_up(&self) -> Vec<String> {
            self.set_up.lock().unwrap().clone()
        }
    }

    impl Guest for FakeGuest {
        async fn running(&self) -> Result<HashSet<String>> {
            self.running.clone().map_err(DockerError::Server)
        }

        async fn set_up(&self, container_id: &str) {
            self.set_up.lock().unwrap().push(container_id.to_owned());
        }
    }

    #[tokio::test]
    async fn reconcile_tears_down_only_orphans() {
        let (runtime, _tmp) = test_runtime();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        runtime
            .register_dns("alive", &["alive.local".into()], ip)
            .await;
        runtime
            .register_dns("dead", &["dead.local".into()], ip)
            .await;
        assert_eq!(runtime.registered_container_ids().await.len(), 2);

        // Only "alive" is running in the guest.
        let guest = FakeGuest::running(&["alive"]);
        reconcile(&runtime, &guest).await.unwrap();

        let remaining = runtime.registered_container_ids().await;
        assert!(remaining.contains("alive"));
        assert!(!remaining.contains("dead"));
        assert_eq!(guest.set_up(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn reconcile_reclaims_alias_only_containers() {
        let (runtime, _tmp) = test_runtime();
        // A container with a name alias but no DNS/port state (e.g. started
        // with --network none): without reclamation every ephemeral --rm run
        // would leak one alias for the life of the daemon.
        runtime
            .register_container_alias("ephemeral", "cafe1234")
            .await;
        assert!(
            runtime
                .registered_container_ids()
                .await
                .contains("cafe1234"),
            "alias-only containers must be visible to the reconciler"
        );

        reconcile(&runtime, &FakeGuest::running(&[])).await.unwrap();

        assert!(runtime.registered_container_ids().await.is_empty());
        assert_eq!(
            runtime.resolve_registered_container("ephemeral").await,
            None
        );
    }

    #[tokio::test]
    async fn reconcile_is_fail_safe_on_query_error() {
        let (runtime, _tmp) = test_runtime();
        runtime
            .register_dns("c", &["c.local".into()], IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await;

        let guest = FakeGuest::unreachable();
        let result = reconcile(&runtime, &guest).await;

        assert!(result.is_err());
        // Nothing torn down, nothing set up, on a query failure.
        assert!(runtime.registered_container_ids().await.contains("c"));
        assert_eq!(guest.set_up(), Vec::<String>::new());
    }

    /// A container the guest runs without the host knowing — started inside
    /// the guest, or brought back by dockerd's restart policy after a VM
    /// restart — gets its host networking set up.
    #[tokio::test]
    async fn reconcile_sets_up_running_containers_the_host_does_not_know() {
        let (runtime, _tmp) = test_runtime();
        runtime
            .register_dns(
                "known",
                &["known.local".into()],
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await;

        let guest = FakeGuest::running(&["known", "restarted"]);
        reconcile(&runtime, &guest).await.unwrap();

        assert_eq!(guest.set_up(), ["restarted"]);
        assert!(runtime.registered_container_ids().await.contains("known"));
    }

    /// Lets the loop run until it parks again, advancing paused time by
    /// `elapsed` on the way.
    async fn settle(elapsed: Duration) {
        tokio::time::sleep(elapsed).await;
        tokio::task::yield_now().await;
    }

    /// The lifecycle contract end to end: a VM departure retires the
    /// containers' host state at once, and its return is reconciled within
    /// the short retry rather than a whole interval.
    #[tokio::test(start_paused = true)]
    async fn a_vm_departure_retires_container_state_and_its_return_is_reconciled_promptly() {
        let (runtime, _tmp) = test_runtime();
        let guest = Arc::new(FakeGuest::running(&["web"]));
        runtime
            .register_dns(
                "web",
                &["web.local".into()],
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await;
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });

        // Steady state: the guest runs exactly what the host knows.
        settle(RECONCILE_INTERVAL * 2).await;
        assert_eq!(guest.set_up(), Vec::<String>::new());
        assert!(runtime.registered_container_ids().await.contains("web"));

        // The VM goes down: its containers' host state goes at once, and
        // nothing is reconciled while it is down.
        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        assert!(runtime.registered_container_ids().await.is_empty());
        settle(RECONCILE_INTERVAL * 2).await;
        assert_eq!(guest.set_up(), Vec::<String>::new());

        // The VM returns: the container dockerd brought back is set up within
        // the short retry, not after a whole interval.
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(RETURN_RETRY).await;
        assert_eq!(guest.set_up(), ["web"]);

        shutdown.cancel();
        task.await.unwrap();
    }
}
