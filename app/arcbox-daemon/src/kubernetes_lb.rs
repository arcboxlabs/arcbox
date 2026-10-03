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

use std::sync::Arc;
use std::time::Duration;

use arcbox_connect::v1::KubernetesLoadBalancersResponse;
use arcbox_core::{AgentClient, Runtime, VmLifecycleState};
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

/// Spawns the reconcile loop for the System VM's cluster.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let vm = runtime.subscribe_system_vm_state();
    let hold = runtime.subscribe_kubernetes_hold();
    let shutdown = ctx.shutdown.clone();
    let source = AgentSource {
        runtime: Arc::clone(runtime),
        agent: None,
    };
    let runtime = Arc::clone(runtime);
    drop(tokio::spawn(async move {
        reconcile_loop(&runtime, vm, hold, shutdown, source).await;
    }));
}

/// Where the loop gets its listings; a seam so the gating is testable
/// without a guest.
trait ServiceSource {
    /// Lists the cluster's LoadBalancer Services.
    async fn list(&mut self) -> arcbox_core::Result<KubernetesLoadBalancersResponse>;

    /// Forgets any connection to a VM that is going away.
    fn disconnect(&mut self);
}

/// Lists through the System VM's agent over one kept connection, so a
/// poll does not pay for (and log) a new vsock connection every interval.
struct AgentSource {
    runtime: Arc<Runtime>,
    agent: Option<AgentClient>,
}

impl ServiceSource for AgentSource {
    async fn list(&mut self) -> arcbox_core::Result<KubernetesLoadBalancersResponse> {
        let agent = match &mut self.agent {
            Some(agent) => agent,
            None => self
                .agent
                .insert(self.runtime.connect_system_agent().await?),
        };
        let listing = agent.list_kubernetes_load_balancers().await;
        if listing.is_err() {
            // The connection may be what failed; the next poll dials afresh.
            self.agent = None;
        }
        Ok(listing?)
    }

    fn disconnect(&mut self) {
        self.agent = None;
    }
}

/// Polls `source` and applies each listing while the VM is ready and the
/// hold is taken; closes the listeners whenever either is not.
///
/// A failed listing leaves the listeners as they were: a transient API
/// server hiccup must not unpublish every Service.
async fn reconcile_loop(
    runtime: &Runtime,
    mut vm: watch::Receiver<VmLifecycleState>,
    mut hold: watch::Receiver<bool>,
    shutdown: CancellationToken,
    mut source: impl ServiceSource,
) {
    let mut failures = 0u32;
    loop {
        let active = vm.borrow_and_update().is_ready() && *hold.borrow_and_update();
        if active {
            match source.list().await {
                Ok(listing) => {
                    failures = 0;
                    runtime.apply_kubernetes_load_balancers(&listing).await;
                }
                Err(e) => {
                    failures += 1;
                    if failures == FAILURES_BEFORE_WARNING {
                        warn!(error = %e, failures, "cannot list Kubernetes Services; LoadBalancer ports are left as they were");
                    } else {
                        debug!(error = %e, "Kubernetes Service listing failed");
                    }
                }
            }
        } else {
            failures = 0;
            source.disconnect();
            runtime.close_kubernetes_load_balancers().await;
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            // The senders live as long as the runtime; an error means it is
            // gone and there is nothing left to reconcile.
            changed = vm.changed() => if changed.is_err() { break },
            changed = hold.changed() => if changed.is_err() { break },
            () = tokio::time::sleep(POLL_INTERVAL), if active => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use arcbox_core::Config;

    use super::*;

    /// Counts listings and answers each with an empty cluster.
    struct CountingSource(Arc<AtomicUsize>);

    impl ServiceSource for CountingSource {
        async fn list(&mut self) -> arcbox_core::Result<KubernetesLoadBalancersResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(KubernetesLoadBalancersResponse::default())
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
            .unwrap(),
        );
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let (hold_tx, hold) = watch::channel(false);
        let shutdown = CancellationToken::new();
        let polls = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn({
            let (runtime, shutdown) = (Arc::clone(&runtime), shutdown.clone());
            let source = CountingSource(Arc::clone(&polls));
            async move { reconcile_loop(&runtime, vm, hold, shutdown, source).await }
        });

        settle(POLL_INTERVAL * 3).await;
        assert_eq!(polls.load(Ordering::SeqCst), 0, "no poll without the hold");

        hold_tx.send_replace(true);
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
