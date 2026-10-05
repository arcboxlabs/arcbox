//! Observes one runtime start attempt for a readiness watch.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use arcbox_connect::v1::readiness_event::Kind;
use arcbox_connect::v1::{ReadinessEvent, RuntimeEnsureResponse, RuntimeStatusResponse};

use super::ensure_runtime::{RuntimeGuard, RuntimeState, ensure_runtime};

/// Starts or joins one attempt, then observes its state without retrying failures.
pub(super) async fn watch_runtime<S, SFut, P, PFut>(
    guard: Arc<RuntimeGuard>,
    timeout: Duration,
    start: S,
    mut probe: P,
) -> ReadinessEvent
where
    S: FnOnce() -> SFut,
    SFut: Future<Output = RuntimeEnsureResponse> + Send + 'static,
    P: FnMut() -> PFut,
    PFut: Future<Output = RuntimeStatusResponse>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    ensure_runtime(Arc::clone(&guard), true, start, || async {
        unreachable!("a start request never calls the probe-only path")
    })
    .await;

    loop {
        let state = guard.state.lock().await.clone();
        let status = probe().await;
        let timed_out = tokio::time::Instant::now() >= deadline;
        let (kind, endpoint, detail) = match state {
            // A completed start includes post-start routing. The independent
            // probe confirms Docker remains reachable before publishing ready.
            RuntimeState::Ready { endpoint, message } if status.docker_ready => {
                (Kind::RuntimeReady, endpoint, message)
            }
            RuntimeState::Failed { message } => (Kind::RuntimeFailed, String::new(), message),
            RuntimeState::Ready { message, .. } if timed_out => {
                (Kind::RuntimeFailed, String::new(), message)
            }
            _ if timed_out => (
                Kind::RuntimeFailed,
                String::new(),
                "runtime start in progress".to_owned(),
            ),
            _ => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        return ReadinessEvent {
            kind: kind.into(),
            endpoint: if endpoint.is_empty() {
                status.endpoint
            } else {
                endpoint
            },
            detail: if detail.is_empty() {
                status.detail
            } else {
                detail
            },
            services: status.services,
            ..Default::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn docker_status(ready: bool) -> RuntimeStatusResponse {
        RuntimeStatusResponse {
            docker_ready: ready,
            endpoint: "vsock:2375".to_owned(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn failed_attempt_ends_the_watch_and_a_new_watch_can_retry() {
        let guard = Arc::new(RuntimeGuard::new());
        let starts = AtomicUsize::new(0);
        let failed = tokio::time::timeout(
            Duration::from_secs(1),
            watch_runtime(
                Arc::clone(&guard),
                Duration::from_secs(30),
                || {
                    starts.fetch_add(1, Ordering::SeqCst);
                    async {
                        RuntimeEnsureResponse {
                            message: "metadata volume setup failed: read-only filesystem"
                                .to_owned(),
                            ..Default::default()
                        }
                    }
                },
                || async { docker_status(false) },
            ),
        )
        .await
        .expect("a failed attempt must finish before the watch timeout");
        assert_eq!(failed.kind, Kind::RuntimeFailed);
        assert_eq!(
            failed.detail,
            "metadata volume setup failed: read-only filesystem"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            *guard.state.lock().await,
            RuntimeState::Failed { .. }
        ));

        let retried = watch_runtime(
            guard,
            Duration::from_secs(1),
            || {
                starts.fetch_add(1, Ordering::SeqCst);
                async {
                    RuntimeEnsureResponse {
                        ready: true,
                        ..Default::default()
                    }
                }
            },
            || async { docker_status(true) },
        )
        .await;
        assert_eq!(retried.kind, Kind::RuntimeReady);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ready_requires_both_the_start_result_and_a_live_docker_probe() {
        let guard = Arc::new(RuntimeGuard::new());
        let (complete_start, start_completed) = tokio::sync::oneshot::channel();
        let mut complete_start = Some(complete_start);
        let mut probes = 0;
        let event = watch_runtime(
            guard,
            Duration::from_secs(1),
            || async move {
                start_completed.await.unwrap();
                RuntimeEnsureResponse {
                    ready: true,
                    endpoint: "vsock:2375".to_owned(),
                    message: "runtime and routing ready".to_owned(),
                    ..Default::default()
                }
            },
            || {
                probes += 1;
                if let Some(complete_start) = complete_start.take() {
                    complete_start.send(()).unwrap();
                }
                let docker_ready = probes != 2;
                async move { docker_status(docker_ready) }
            },
        )
        .await;
        assert_eq!(event.kind, Kind::RuntimeReady);
        assert_eq!(event.endpoint, "vsock:2375");
        assert_eq!(event.detail, "runtime and routing ready");
        assert_eq!(probes, 3);
    }
}
