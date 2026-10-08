//! Keeps host-side container networking in step with guest dockerd's live
//! container set.
//!
//! Host port forwarding, DNS and name aliases are set up by `start_container`
//! and torn down by the `stop`/`kill`/`remove` handlers. Neither path sees a
//! container that changes state without an API call through this proxy: a
//! natural exit, `--rm` auto-remove, `docker prune`, an OOM kill, a `docker
//! stop` issued inside the guest — and, after a System VM restart, every
//! container dockerd brings back under its restart policy. This task closes
//! both gaps. Every [`RECONCILE_INTERVAL`] it inspects every running
//! container, then commits the complete host state if no concurrent mutation
//! invalidated the query. This also refreshes known containers. It follows the
//! System VM's physical identity on each notification and timer tick, so a
//! coalesced restart cannot retain the previous VM's container state. When
//! the VM returns, it retries every [`RETURN_RETRY`] until a snapshot commits.

use crate::api::AppState;
use crate::error::Result;
use crate::guest_query::running_container_snapshot;
use crate::proxy::ProxyState;
use arcbox_core::runtime::ContainerNetwork;
use arcbox_core::{Runtime, VmLifecycleState};
use arcbox_error::CommonError;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Steady-state check interval and deadline for one complete guest snapshot.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Cadence after the VM comes back, until the first snapshot commits:
/// dockerd is still starting — and restarting its containers — then.
const RETURN_RETRY: Duration = Duration::from_secs(2);

/// Runs the reconciler until `shutdown`; the server owns the task.
///
/// Shares the router's [`ProxyState`] so queries go through the same pooled
/// client, and observes the System VM restart generation before each listing
/// so a query right after a backend switch dials the fresh VM instead of
/// failing once on a stale pooled connection.
pub async fn run(runtime: Arc<Runtime>, proxy: Arc<ProxyState>, shutdown: CancellationToken) {
    let vm = runtime.subscribe_system_vm_state();
    let guest = ProxyGuest {
        state: AppState {
            runtime: Arc::clone(&runtime),
            proxy,
        },
    };
    reconcile_loop(&runtime, &guest, vm, shutdown).await;
}

/// The guest side of a reconcile; a seam so the loop is testable without
/// dockerd.
trait Guest: Send + Sync {
    /// Complete validated networking state for every running container.
    fn snapshot(&self) -> impl Future<Output = Result<Vec<ContainerNetwork>>> + Send;
}

/// Guest dockerd through the Docker proxy's pooled client.
struct ProxyGuest {
    state: AppState,
}

impl Guest for ProxyGuest {
    async fn snapshot(&self) -> Result<Vec<ContainerNetwork>> {
        self.state
            .proxy
            .reset_if_restarted(self.state.runtime.system_vm_restart_generation());
        running_container_snapshot(self.state.proxy.client()).await
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
    let mut retirement_pending = true;
    loop {
        let delay = if retirement_pending || (ready && returning) {
            RETURN_RETRY
        } else {
            RECONCILE_INTERVAL
        };
        let query_due = tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            // The sender lives as long as the runtime; an error means it is
            // gone and there is nothing left to reconcile.
            changed = vm.changed() => {
                if changed.is_err() {
                    break;
                }
                false
            }
            () = tokio::time::sleep(delay) => true,
        };
        let now_ready = vm.borrow_and_update().is_ready();
        if now_ready && !ready {
            returning = true;
        }
        ready = now_ready;
        match runtime.retire_system_vm_container_networking().await {
            Ok(retired) => {
                retirement_pending = false;
                returning |= retired;
            }
            Err(error) => {
                // Retry failed cleanup even when the lifecycle watch reports a stopped VM.
                retirement_pending = true;
                returning = true;
                tracing::debug!(%error, "container host networking retirement deferred");
                continue;
            }
        }
        if query_due && ready {
            // Keep the loop's receiver unread so the next iteration handles the notification.
            let mut query_vm = vm.clone();
            let interrupted = async {
                tokio::select! {
                    () = shutdown.cancelled() => {},
                    _ = query_vm.changed() => {},
                }
            };
            match reconcile(runtime, guest, interrupted).await {
                Ok(true) => returning = false,
                Ok(false) => {}
                // A failed guest query leaves the existing host state intact.
                Err(e) => tracing::debug!(error = %e, "host networking reconcile skipped"),
            }
        }
    }
}

/// Commits a complete guest query only while its captured generation remains current.
/// Query failures preserve host state; concurrent mutations cause a skipped commit.
async fn reconcile(
    runtime: &Runtime,
    guest: &impl Guest,
    interrupted: impl Future<Output = ()> + Send,
) -> Result<bool> {
    let Some(snapshot) = runtime.capture_container_snapshot().await? else {
        return Ok(false);
    };
    let containers = tokio::select! {
        biased;
        () = interrupted => return Ok(false),
        containers = tokio::time::timeout(RECONCILE_INTERVAL, guest.snapshot()) => {
            containers.map_err(|_| CommonError::timeout("container networking snapshot timed out"))??
        },
    };
    snapshot.commit(&containers).await.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DockerError;
    use arcbox_core::runtime::{ContainerChange, NetworkFixture};
    use arcbox_core::{Config, Runtime, VmLifecycleConfig};
    use arcbox_dns::container::ContainerMetadata;
    use std::future::pending;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::sync::Notify;

    fn test_runtime() -> (Arc<Runtime>, NetworkFixture, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let vlc = VmLifecycleConfig {
            skip_vm_check: true,
            ..Default::default()
        };
        let fixture = NetworkFixture::default();
        let runtime = Arc::new(
            Runtime::with_vm_lifecycle_config(config, vlc)
                .expect("runtime")
                .with_network_fixture(fixture.clone()),
        );
        (runtime, fixture, tmp)
    }

    fn network(id: &str, name: &str, ip: &str) -> ContainerNetwork {
        ContainerNetwork {
            metadata: ContainerMetadata::from_inspect(&serde_json::json!({
                "Id": id, "Name": name,
                "NetworkSettings": {"Networks": {"bridge": {"IPAddress": ip}}}
            }))
            .unwrap(),
            bindings: Vec::new(),
        }
    }

    /// Returns a complete snapshot, optionally pausing before the query finishes.
    struct FakeGuest {
        snapshot: std::result::Result<Vec<ContainerNetwork>, String>,
        queries: AtomicUsize,
        active_queries: AtomicUsize,
        query_finished: Notify,
        query_gate: Option<(Notify, Notify)>,
    }

    impl FakeGuest {
        fn running(ids: &[&str]) -> Self {
            Self {
                snapshot: Ok(ids.iter().map(|id| network(id, id, "172.17.0.2")).collect()),
                queries: AtomicUsize::new(0),
                active_queries: AtomicUsize::new(0),
                query_finished: Notify::new(),
                query_gate: None,
            }
        }

        fn unreachable() -> Self {
            Self {
                snapshot: Err("guest unreachable".to_owned()),
                ..Self::running(&[])
            }
        }

        fn queries(&self) -> usize {
            self.queries.load(Ordering::Relaxed)
        }
    }

    struct ActiveQuery<'a>(&'a FakeGuest);

    impl Drop for ActiveQuery<'_> {
        fn drop(&mut self) {
            self.0.active_queries.fetch_sub(1, Ordering::Relaxed);
            self.0.query_finished.notify_one();
        }
    }

    impl Guest for FakeGuest {
        async fn snapshot(&self) -> Result<Vec<ContainerNetwork>> {
            self.queries.fetch_add(1, Ordering::Relaxed);
            self.active_queries.fetch_add(1, Ordering::Relaxed);
            let _query = ActiveQuery(self);
            if let Some((entered, release)) = &self.query_gate {
                entered.notify_one();
                release.notified().await;
            }
            self.snapshot.clone().map_err(DockerError::Server)
        }
    }

    #[tokio::test]
    async fn reconcile_tears_down_only_orphans() {
        let (runtime, _fixture, _tmp) = test_runtime();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        runtime
            .register_dns("alive", &["alive.local".into()], ip)
            .await
            .unwrap();
        runtime
            .register_dns("dead", &["dead.local".into()], ip)
            .await
            .unwrap();
        assert_eq!(runtime.registered_container_ids().await.len(), 2);

        // Only "alive" is running in the guest.
        let guest = FakeGuest::running(&["alive"]);
        assert!(reconcile(&runtime, &guest, pending()).await.unwrap());

        let remaining = runtime.registered_container_ids().await;
        assert!(remaining.contains("alive"));
        assert!(!remaining.contains("dead"));
        assert_eq!(guest.queries(), 1);
    }

    #[tokio::test]
    async fn reconcile_reclaims_alias_only_containers() {
        let (runtime, _fixture, _tmp) = test_runtime();
        // A container with a name alias but no DNS/port state (e.g. started
        // with --network none): without reclamation every ephemeral --rm run
        // would leak one alias for the life of the daemon.
        runtime
            .register_container_alias("ephemeral", "cafe1234")
            .await
            .unwrap();
        assert!(
            runtime
                .registered_container_ids()
                .await
                .contains("cafe1234"),
            "alias-only containers must be visible to the reconciler"
        );

        assert!(
            reconcile(&runtime, &FakeGuest::running(&[]), pending())
                .await
                .unwrap()
        );

        assert!(runtime.registered_container_ids().await.is_empty());
        assert_eq!(
            runtime.resolve_registered_container("ephemeral").await,
            None
        );
    }

    #[tokio::test]
    async fn reconcile_is_fail_safe_on_query_error() {
        let (runtime, _fixture, _tmp) = test_runtime();
        runtime
            .register_dns("c", &["c.local".into()], IpAddr::V4(Ipv4Addr::LOCALHOST))
            .await
            .unwrap();

        let guest = FakeGuest::unreachable();
        let result = reconcile(&runtime, &guest, pending()).await;

        assert!(result.is_err());
        // Nothing torn down, nothing set up, on a query failure.
        assert!(runtime.registered_container_ids().await.contains("c"));
        assert_eq!(
            runtime.web_service_dns_address("c.local"),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        assert_eq!(guest.queries(), 1);
    }

    #[tokio::test]
    async fn reconcile_refreshes_known_containers_and_registers_unknown_containers() {
        let (runtime, _fixture, _tmp) = test_runtime();
        runtime
            .register_container_dns(&network("known", "old", "172.17.0.2").metadata)
            .await
            .unwrap();

        let mut guest = FakeGuest::running(&["known", "restarted"]);
        guest.snapshot.as_mut().unwrap()[0] = network("known", "renamed", "172.17.0.3");
        assert!(reconcile(&runtime, &guest, pending()).await.unwrap());

        assert_eq!(runtime.resolve_registered_container("old").await, None);
        assert_eq!(
            runtime
                .resolve_registered_container("renamed")
                .await
                .as_deref(),
            Some("known")
        );
        assert_eq!(runtime.web_service_dns_address("old.arcbox.local"), None);
        assert_eq!(
            runtime.web_service_dns_address("renamed.arcbox.local"),
            Some("172.17.0.3".parse().unwrap())
        );
        assert!(
            runtime
                .registered_container_ids()
                .await
                .contains("restarted")
        );
    }

    #[tokio::test]
    async fn a_mutation_during_a_query_prevents_stale_publication_or_removal() {
        let (runtime, _fixture, _tmp) = test_runtime();
        let guest = Arc::new(FakeGuest {
            snapshot: Ok(vec![network("canonical", "stale", "172.17.0.2")]),
            query_gate: Some((Notify::new(), Notify::new())),
            ..FakeGuest::running(&[])
        });
        let task = tokio::spawn({
            let (runtime, guest) = (Arc::clone(&runtime), Arc::clone(&guest));
            async move { reconcile(&runtime, guest.as_ref(), pending()).await }
        });
        let (entered, release) = guest.query_gate.as_ref().unwrap();
        entered.notified().await;
        let mutation = runtime
            .begin_container_mutation(runtime.container_network_incarnation().unwrap())
            .await
            .unwrap();
        let skipped = FakeGuest::running(&[]);
        assert!(!reconcile(&runtime, &skipped, pending()).await.unwrap());
        assert_eq!(skipped.queries(), 0);
        for container in [
            network("canonical", "current", "172.17.0.3"),
            network("created", "new", "172.17.0.4"),
        ] {
            mutation
                .commit(ContainerChange::Publish(
                    &container.metadata,
                    &container.bindings,
                ))
                .await
                .unwrap();
        }
        drop(mutation);
        release.notify_one();
        assert!(!task.await.unwrap().unwrap());
        assert_eq!(runtime.resolve_registered_container("stale").await, None);
        for (name, id, ip) in [
            ("current", "canonical", "172.17.0.3"),
            ("new", "created", "172.17.0.4"),
        ] {
            assert_eq!(
                runtime.resolve_registered_container(name).await.as_deref(),
                Some(id)
            );
            assert_eq!(
                runtime.web_service_dns_address(&format!("{name}.arcbox.local")),
                Some(ip.parse().unwrap())
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_and_server_drop_cancel_a_pending_query_without_publication() {
        for cancel_shutdown in [true, false] {
            let (runtime, _fixture, _tmp) = test_runtime();
            let current = network("current", "current", "172.17.0.2");
            let mutation = runtime
                .begin_container_mutation(runtime.container_network_incarnation().unwrap())
                .await
                .unwrap();
            mutation
                .commit(ContainerChange::Publish(&current.metadata, &[]))
                .await
                .unwrap();
            drop(mutation);
            let guest = Arc::new(FakeGuest {
                query_gate: Some((Notify::new(), Notify::new())),
                ..FakeGuest::running(&["replacement"])
            });
            let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
            let shutdown = CancellationToken::new();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn({
                let (runtime, guest, shutdown) =
                    (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
                async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
            });
            let (entered, _) = guest.query_gate.as_ref().unwrap();
            tokio::time::timeout(RETURN_RETRY * 2, entered.notified())
                .await
                .expect("the query starts");
            assert_eq!(guest.active_queries.load(Ordering::Relaxed), 1);

            if cancel_shutdown {
                shutdown.cancel();
                tokio::time::timeout(Duration::from_secs(1), tasks.join_next())
                    .await
                    .expect("shutdown joins without a guest response")
                    .unwrap()
                    .unwrap();
            }
            drop(tasks);
            tokio::time::timeout(Duration::from_secs(1), guest.query_finished.notified())
                .await
                .expect("the pending query is dropped with its owner");
            assert_eq!(guest.active_queries.load(Ordering::Relaxed), 0);
            assert_eq!(guest.queries(), 1);
            assert_eq!(
                runtime
                    .resolve_registered_container("current")
                    .await
                    .as_deref(),
                Some("current")
            );
            assert_eq!(
                runtime.web_service_dns_address("current.arcbox.local"),
                Some("172.17.0.2".parse().unwrap())
            );
            assert_eq!(
                runtime.resolve_registered_container("replacement").await,
                None
            );
        }
    }

    /// Lets the loop run until it parks again, advancing paused time by
    /// `elapsed` on the way.
    async fn settle(elapsed: Duration) {
        tokio::time::sleep(elapsed).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_coalesced_restart_cancels_the_query_and_retires_the_old_generation() {
        let (runtime, fixture, _tmp) = test_runtime();
        assert!(
            reconcile(&runtime, &FakeGuest::running(&["old"]), pending())
                .await
                .unwrap()
        );
        let guest = Arc::new(FakeGuest {
            query_gate: Some((Notify::new(), Notify::new())),
            ..FakeGuest::running(&["replacement"])
        });
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });
        let (entered, release) = guest.query_gate.as_ref().unwrap();
        tokio::time::timeout(RETURN_RETRY * 2, entered.notified())
            .await
            .expect("the old generation query starts");

        fixture.restart();
        vm_tx.send_replace(VmLifecycleState::Stopping);
        vm_tx.send_replace(VmLifecycleState::Running);
        tokio::time::timeout(Duration::from_secs(1), guest.query_finished.notified())
            .await
            .expect("the coalesced notification cancels the old query");
        settle(Duration::ZERO).await;
        assert_eq!(guest.active_queries.load(Ordering::Relaxed), 0);
        assert_eq!(runtime.resolve_registered_container("old").await, None);
        assert_eq!(runtime.web_service_dns_address("old.arcbox.local"), None);
        assert_eq!(
            runtime.resolve_registered_container("replacement").await,
            None
        );

        release.notify_one();
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), 2);
        assert_eq!(
            runtime
                .resolve_registered_container("replacement")
                .await
                .as_deref(),
            Some("replacement")
        );
        assert_eq!(
            runtime.web_service_dns_address("replacement.arcbox.local"),
            Some("172.17.0.2".parse().unwrap())
        );
        assert_eq!(runtime.resolve_registered_container("old").await, None);

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown joins the reconciler")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_checks_retire_a_physical_vm_without_a_lifecycle_notification() {
        let (runtime, fixture, _tmp) = test_runtime();
        let guest = Arc::new(FakeGuest::running(&["web"]));
        let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), 1);
        assert_eq!(
            runtime.resolve_registered_container("web").await.as_deref(),
            Some("web")
        );

        fixture.stop();
        settle(RECONCILE_INTERVAL).await;
        assert_eq!(guest.queries(), 1);
        assert_eq!(runtime.resolve_registered_container("web").await, None);
        assert_eq!(runtime.web_service_dns_address("web.arcbox.local"), None);

        fixture.restart();
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), 2);
        assert_eq!(
            runtime.resolve_registered_container("web").await.as_deref(),
            Some("web")
        );
        assert_eq!(
            runtime.web_service_dns_address("web.arcbox.local"),
            Some("172.17.0.2".parse().unwrap())
        );

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown joins the reconciler")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_query_deadline_allows_retirement_after_an_unannounced_physical_restart() {
        let (runtime, fixture, _tmp) = test_runtime();
        assert!(
            reconcile(&runtime, &FakeGuest::running(&["old"]), pending())
                .await
                .unwrap()
        );
        let guest = Arc::new(FakeGuest {
            query_gate: Some((Notify::new(), Notify::new())),
            ..FakeGuest::running(&["replacement"])
        });
        let (_vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });
        let (entered, release) = guest.query_gate.as_ref().unwrap();
        tokio::time::timeout(RETURN_RETRY * 2, entered.notified())
            .await
            .expect("the old generation query starts");
        fixture.restart();
        tokio::time::timeout(
            RECONCILE_INTERVAL + RETURN_RETRY,
            guest.query_finished.notified(),
        )
        .await
        .expect("the deadline drops the unanswered guest query");
        assert_eq!(guest.active_queries.load(Ordering::Relaxed), 0);
        assert_eq!(guest.queries(), 1);
        assert_eq!(
            runtime.resolve_registered_container("old").await.as_deref(),
            Some("old")
        );

        tokio::time::timeout(RETURN_RETRY * 2, entered.notified())
            .await
            .expect("the next tick queries the replacement VM");
        assert_eq!(runtime.resolve_registered_container("old").await, None);
        assert_eq!(runtime.web_service_dns_address("old.arcbox.local"), None);
        assert_eq!(guest.queries(), 2);
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), guest.query_finished.notified())
            .await
            .expect("the replacement query finishes");
        settle(Duration::ZERO).await;
        assert_eq!(
            runtime
                .resolve_registered_container("replacement")
                .await
                .as_deref(),
            Some("replacement")
        );

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown joins the reconciler")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_vm_is_rechecked_at_the_regular_interval() {
        let (runtime, fixture, _tmp) = test_runtime();
        fixture.stop();
        let guest = Arc::new(FakeGuest::running(&[]));
        let (_vm_tx, vm) = watch::channel(VmLifecycleState::Stopping);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });
        settle(RETURN_RETRY).await;
        runtime
            .register_container_alias("stale", "stale")
            .await
            .unwrap();
        settle(RETURN_RETRY).await;
        assert_eq!(
            runtime
                .resolve_registered_container("stale")
                .await
                .as_deref(),
            Some("stale")
        );
        settle(RECONCILE_INTERVAL.checked_sub(RETURN_RETRY).unwrap()).await;
        assert_eq!(runtime.resolve_registered_container("stale").await, None);
        assert_eq!(guest.queries(), 0);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("shutdown joins the reconciler")
            .unwrap();
    }

    /// The lifecycle contract end to end: a VM departure retires the
    /// containers' host state at once, and its return is reconciled within
    /// the short retry rather than a whole interval.
    #[tokio::test(start_paused = true)]
    async fn a_vm_departure_retires_container_state_and_its_return_is_reconciled_promptly() {
        let (runtime, fixture, _tmp) = test_runtime();
        let guest = Arc::new(FakeGuest::running(&["web"]));
        runtime
            .register_dns(
                "web",
                &["web.local".into()],
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap();
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let mutation = runtime
            .begin_container_mutation(runtime.container_network_incarnation().unwrap())
            .await
            .unwrap();
        let task = tokio::spawn({
            let (runtime, guest, shutdown) =
                (Arc::clone(&runtime), Arc::clone(&guest), shutdown.clone());
            async move { reconcile_loop(&runtime, guest.as_ref(), vm, shutdown).await }
        });

        // A skipped commit must retain the short return cadence.
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), 0);
        drop(mutation);
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), 1);

        // Steady state: the guest runs exactly what the host knows.
        settle(RECONCILE_INTERVAL * 2).await;
        let queries = guest.queries();
        assert!(queries >= 2);
        assert!(runtime.registered_container_ids().await.contains("web"));

        // The VM goes down: its containers' host state goes at once, and
        // nothing is reconciled while it is down.
        fixture.stop();
        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        assert!(runtime.registered_container_ids().await.is_empty());
        settle(RECONCILE_INTERVAL * 2).await;
        assert_eq!(guest.queries(), queries);

        // The VM returns: the container dockerd brought back is set up within
        // the short retry, not after a whole interval.
        fixture.restart();
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(RETURN_RETRY).await;
        assert_eq!(guest.queries(), queries + 1);
        assert!(runtime.registered_container_ids().await.contains("web"));

        shutdown.cancel();
        task.await.unwrap();
    }
}
