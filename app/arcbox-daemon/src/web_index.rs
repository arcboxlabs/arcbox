//! Refreshes the guest index and withdraws it before a cancelled request can outlive the service.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use arcbox_core::{Runtime, VmLifecycleState};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::context::DaemonContext;

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn start(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    ctx.web_index_service
        .start(run(
            RuntimeSource(Arc::clone(runtime)),
            runtime.subscribe_system_vm_state(),
            ctx.shutdown.clone(),
        ))
        .await;
}

trait IndexSource: Send + Sync {
    fn refresh(&self, cancelled: &CancellationToken) -> impl Future<Output = Result<()>> + Send;
    fn withdraw(&self) -> impl Future<Output = ()> + Send;
}

struct RuntimeSource(Arc<Runtime>);

impl IndexSource for RuntimeSource {
    async fn refresh(&self, cancelled: &CancellationToken) -> Result<()> {
        Ok(self.0.refresh_web_index(cancelled).await?)
    }

    async fn withdraw(&self) {
        self.0.withdraw_web_index().await;
    }
}

async fn refresh(source: &impl IndexSource, cancelled: &CancellationToken) -> Result<()> {
    anyhow::ensure!(!cancelled.is_cancelled(), "index refresh cancelled");
    source.refresh(cancelled).await
}

async fn run(
    source: impl IndexSource,
    mut vm: watch::Receiver<VmLifecycleState>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            changed = vm.changed() => if changed.is_err() { break; },
            _ = interval.tick() => {}
        }
        if !vm.borrow_and_update().is_ready() {
            source.withdraw().await;
            continue;
        }
        let cancelled = shutdown.child_token();
        let operation = refresh(&source, &cancelled);
        tokio::pin!(operation);
        let result = tokio::select! {
            biased;
            () = cancelled.cancelled() => None,
            _ = vm.changed() => None,
            () = tokio::time::sleep(POLL_INTERVAL) => None,
            result = &mut operation => Some(result),
        };
        match result {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                source.withdraw().await;
                tracing::debug!(%error, "guest index refresh failed");
            }
            None => {
                cancelled.cancel();
                // The request may hold the commit lock; poll its cleanup while withdrawing.
                let (result, ()) = tokio::join!(operation, source.withdraw());
                if let Err(error) = result {
                    tracing::debug!(%error, "guest index refresh cancelled");
                }
            }
        }
    }
    source.withdraw().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::{Mutex, Notify};

    #[derive(Default)]
    struct Source {
        commit: Mutex<()>,
        started: Notify,
        cancelled: Notify,
        withdrawn: Notify,
        release: Notify,
        joined: AtomicBool,
        published: AtomicBool,
        attempts: AtomicUsize,
        fail: AtomicBool,
        pending: AtomicBool,
    }

    impl IndexSource for Arc<Source> {
        async fn refresh(&self, cancelled: &CancellationToken) -> Result<()> {
            let _commit = self.commit.lock().await;
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if self.pending.load(Ordering::SeqCst) {
                cancelled.cancelled().await;
                self.cancelled.notify_one();
                self.release.notified().await;
                self.joined.store(true, Ordering::SeqCst);
                anyhow::bail!("cancelled");
            }
            anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "source failed");
            self.published.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn withdraw(&self) {
            let _commit = self.commit.lock().await;
            self.published.store(false, Ordering::SeqCst);
            self.withdrawn.notify_one();
        }
    }

    #[tokio::test]
    async fn cancellation_before_the_first_poll_does_not_start_a_request() {
        let source = Arc::new(Source::default());
        let cancelled = CancellationToken::new();
        let operation = refresh(&source, &cancelled);
        cancelled.cancel();
        assert_eq!(
            operation.await.unwrap_err().to_string(),
            "index refresh cancelled"
        );
        assert_eq!(source.attempts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn source_failure_withdraws_and_the_next_snapshot_recovers() {
        let source = Arc::new(Source::default());
        source.published.store(true, Ordering::SeqCst);
        source.fail.store(true, Ordering::SeqCst);
        let (sender, vm) = watch::channel(VmLifecycleState::Stopped);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(source.clone(), vm, shutdown.clone()));
        source.withdrawn.notified().await;
        assert!(!source.published.load(Ordering::SeqCst));
        assert_eq!(source.attempts.load(Ordering::SeqCst), 0);
        sender.send_replace(VmLifecycleState::Running);
        source.started.notified().await;
        source.withdrawn.notified().await;
        assert!(!source.published.load(Ordering::SeqCst));
        source.fail.store(false, Ordering::SeqCst);
        tokio::time::advance(POLL_INTERVAL).await;
        source.started.notified().await;
        assert!(source.published.load(Ordering::SeqCst));
        sender.send_replace(VmLifecycleState::Stopped);
        source.withdrawn.notified().await;
        assert!(!source.published.load(Ordering::SeqCst));
        shutdown.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn interruptions_join_the_request_while_withdrawal_waits_for_its_lock() {
        for stop in ["shutdown", "vm", "timeout"] {
            let source = Arc::new(Source::default());
            source.pending.store(true, Ordering::SeqCst);
            source.published.store(true, Ordering::SeqCst);
            let (sender, vm) = watch::channel(VmLifecycleState::Running);
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(run(source.clone(), vm, shutdown.clone()));
            source.started.notified().await;
            match stop {
                "shutdown" => shutdown.cancel(),
                "vm" => {
                    sender.send_replace(VmLifecycleState::Stopped);
                }
                _ => tokio::time::advance(POLL_INTERVAL).await,
            }
            source.cancelled.notified().await;
            if stop == "timeout" {
                sender.send_replace(VmLifecycleState::Stopped);
            }
            assert!(!task.is_finished(), "{stop}");
            source.release.notify_one();
            source.withdrawn.notified().await;
            assert!(source.joined.load(Ordering::SeqCst), "{stop}");
            assert!(!source.published.load(Ordering::SeqCst), "{stop}");
            shutdown.cancel();
            task.await.unwrap().unwrap();
        }
    }
}
