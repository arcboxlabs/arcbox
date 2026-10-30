//! Keeps the host listeners for Kubernetes LoadBalancer Services in step
//! with the cluster.
//!
//! While the System VM is ready and the daemon believes Kubernetes runs (the
//! lifecycle's Kubernetes hold), the guest's Service list is polled every
//! [`POLL_INTERVAL`] and applied with
//! [`Runtime::apply_kubernetes_load_balancers`]; the moment either goes away
//! every LoadBalancer listener closes, so a VM restart or a crashed k3s
//! leaves nothing bound. Polling a unary agent RPC rather than watching is
//! deliberate: the HV backend's blocking agent transport carries no streams.
//! The retained service drains cancelled polls and closes its listeners before Runtime stops.

use std::sync::Arc;
use std::time::Duration;

use arcbox_connect::v1::KubernetesLoadBalancersResponse;
use arcbox_core::{AgentClient, Runtime, VmLifecycleState};
use arcbox_engine::vm::VmIncarnation;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::context::DaemonContext;

/// How often the Service list is polled while Kubernetes runs. One listing
/// costs the guest ~55 ms of CPU (a `k3s kubectl` run, measured on k3s
/// v1.36), next to the ~70 ms per second k3s spends idle.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive failed listings after which the failure is logged as a
/// warning; a few are routine while k3s starts its API server.
const FAILURES_BEFORE_WARNING: u32 = 15;

/// Retains the reconcile loop for the System VM's cluster through daemon shutdown.
pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let vm = runtime.subscribe_system_vm_state();
    let hold = runtime.subscribe_kubernetes_hold();
    let shutdown = ctx.shutdown.clone();
    let source = AgentSource {
        runtime: Arc::clone(runtime),
        agent: None,
    };
    let runtime = Arc::clone(runtime);
    ctx.kubernetes_lb_service
        .start(async move {
            reconcile_loop(&runtime, vm, hold, shutdown, source).await;
            Ok(())
        })
        .await;
}

/// Where the loop gets its listings; a seam so the gating is testable
/// without a guest.
trait ServiceSource {
    /// Lists the cluster's LoadBalancer Services.
    async fn list(
        &mut self,
        cancelled: &CancellationToken,
    ) -> arcbox_core::Result<(VmIncarnation, KubernetesLoadBalancersResponse)>;

    /// Forgets any connection to a VM that is going away.
    fn disconnect(&mut self);
}

/// Lists through the System VM's agent over one kept connection, so a
/// poll does not pay for (and log) a new vsock connection every interval.
struct AgentSource {
    runtime: Arc<Runtime>,
    agent: Option<(VmIncarnation, AgentClient)>,
}

impl ServiceSource for AgentSource {
    async fn list(
        &mut self,
        cancelled: &CancellationToken,
    ) -> arcbox_core::Result<(VmIncarnation, KubernetesLoadBalancersResponse)> {
        let expected = self.runtime.container_network_incarnation()?;
        let agent = match self.agent.take() {
            Some((incarnation, agent)) if incarnation == expected => agent,
            _ => {
                self.runtime
                    .connect_system_agent_expected(expected.clone())
                    .await?
            }
        };
        let (agent, listing) = agent.list_kubernetes_load_balancers(cancelled).await?;
        self.agent = Some((expected.clone(), agent));
        Ok((expected, listing))
    }

    fn disconnect(&mut self) {
        self.agent = None;
    }
}

/// Cancels and drains each poll before the next VM or Kubernetes hold can publish.
async fn reconcile_loop(
    runtime: &Runtime,
    mut vm: watch::Receiver<VmLifecycleState>,
    mut hold: watch::Receiver<bool>,
    shutdown: CancellationToken,
    mut source: impl ServiceSource,
) {
    let mut failures = 0u32;
    while !shutdown.is_cancelled() {
        let ready = vm.borrow_and_update().is_ready();
        let held = *hold.borrow_and_update();
        let active = ready && held;
        if active {
            let cancelled = shutdown.child_token();
            let outcome = {
                let operation = poll(runtime, &mut source, &cancelled);
                tokio::pin!(operation);
                let outcome = tokio::select! {
                    biased;
                    () = shutdown.cancelled() => PollOutcome::Stop,
                    changed = vm.changed() => if changed.is_ok() { PollOutcome::Changed } else { PollOutcome::Stop },
                    changed = hold.changed() => if changed.is_ok() { PollOutcome::Changed } else { PollOutcome::Stop },
                    result = &mut operation => PollOutcome::Applied(result),
                };
                if !matches!(outcome, PollOutcome::Applied(_)) {
                    cancelled.cancel();
                    // Apply can hold the port lock. Poll cleanup and the cancelled operation together.
                    let (result, ()) =
                        tokio::join!(operation, runtime.close_kubernetes_load_balancers());
                    if let Err(error) = result {
                        debug!(%error, "Kubernetes Service poll cancelled and drained");
                    }
                }
                outcome
            };
            match outcome {
                PollOutcome::Changed => {
                    failures = 0;
                    source.disconnect();
                    continue;
                }
                PollOutcome::Stop => break,
                PollOutcome::Applied(Ok(())) => failures = 0,
                PollOutcome::Applied(Err(error)) => {
                    source.disconnect();
                    runtime.close_kubernetes_load_balancers().await;
                    failures += 1;
                    if failures == FAILURES_BEFORE_WARNING {
                        warn!(%error, failures, "cannot list Kubernetes Services; LoadBalancer ports were withdrawn");
                    } else {
                        debug!(%error, "Kubernetes Service poll failed; LoadBalancer ports were withdrawn");
                    }
                }
            }
        } else {
            failures = 0;
            source.disconnect();
            runtime.close_kubernetes_load_balancers().await;
        }
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            changed = vm.changed() => if changed.is_err() { break },
            changed = hold.changed() => if changed.is_err() { break },
            () = tokio::time::sleep(POLL_INTERVAL), if active => {}
        }
    }
    source.disconnect();
    runtime.close_kubernetes_load_balancers().await;
}

async fn poll(
    runtime: &Runtime,
    source: &mut impl ServiceSource,
    cancelled: &CancellationToken,
) -> arcbox_core::Result<()> {
    if cancelled.is_cancelled() {
        return Err(arcbox_core::CoreError::invalid_state(
            "Kubernetes Service poll cancelled before connection",
        ));
    }
    let (expected, listing) = source.list(cancelled).await?;
    runtime
        .apply_kubernetes_load_balancers(&expected, &listing, cancelled)
        .await
}

enum PollOutcome {
    Applied(arcbox_core::Result<()>),
    Changed,
    Stop,
}

#[cfg(test)]
mod cancellation_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use arcbox_core::Config;

    use super::*;

    /// Counts listings and answers each with an empty cluster.
    struct CountingSource(Arc<AtomicUsize>, Arc<Runtime>);

    impl ServiceSource for CountingSource {
        async fn list(
            &mut self,
            _cancelled: &CancellationToken,
        ) -> arcbox_core::Result<(VmIncarnation, KubernetesLoadBalancersResponse)> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok((
                self.1.container_network_incarnation()?,
                KubernetesLoadBalancersResponse::default(),
            ))
        }

        fn disconnect(&mut self) {}
    }

    /// Lets the loop run until it parks again, advancing paused time by
    /// `elapsed` on the way.
    async fn settle(elapsed: Duration) {
        tokio::time::sleep(elapsed).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn polls_only_while_the_vm_is_ready_and_kubernetes_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            Runtime::new(Config {
                data_dir: dir.path().to_path_buf(),
                ..Default::default()
            })
            .unwrap()
            .with_network_fixture(arcbox_core::runtime::NetworkFixture::default()),
        );
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let hold = runtime.subscribe_kubernetes_hold();
        let shutdown = CancellationToken::new();
        let polls = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn({
            let (runtime, shutdown) = (Arc::clone(&runtime), shutdown.clone());
            let source = CountingSource(Arc::clone(&polls), Arc::clone(&runtime));
            async move { reconcile_loop(&runtime, vm, hold, shutdown, source).await }
        });

        settle(POLL_INTERVAL * 3).await;
        assert_eq!(polls.load(Ordering::SeqCst), 0, "no poll without the hold");

        runtime.vm_lifecycle().set_kubernetes_hold(true).await;
        settle(POLL_INTERVAL * 3).await;
        let while_held = polls.load(Ordering::SeqCst);
        assert!(
            while_held >= 3,
            "polls every interval while held, got {while_held}"
        );

        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        let at_stop = polls.load(Ordering::SeqCst);
        settle(POLL_INTERVAL * 3).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            at_stop,
            "no poll while the VM is down"
        );

        vm_tx.send_replace(VmLifecycleState::Running);
        settle(POLL_INTERVAL).await;
        assert!(
            polls.load(Ordering::SeqCst) > at_stop,
            "polling resumes with the VM"
        );

        shutdown.cancel();
        task.await.unwrap();
    }
}
