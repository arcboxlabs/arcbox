//! EnsureRuntime state machine (platform-independent, testable).

use std::sync::OnceLock;

// STATUS_STARTED is used in the linux-only runtime start path and tests.
#[allow(unused_imports)]
pub use arcbox_constants::status::{
    RUNTIME_FAILED as STATUS_FAILED, RUNTIME_REUSED as STATUS_REUSED,
    RUNTIME_STARTED as STATUS_STARTED,
};
use arcbox_protocol::agent::RuntimeEnsureResponse;
use tokio::sync::{Mutex, Notify};

/// Runtime lifecycle state.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum RuntimeState {
    /// No ensure has been attempted yet.
    NotStarted,
    /// An ensure operation is in progress (first caller drives it).
    Starting,
    /// Runtime is confirmed ready.
    Ready { endpoint: String, message: String },
    /// Last ensure attempt failed; may retry on next start_if_needed=true.
    Failed { message: String },
}

/// Global singleton guard that serializes EnsureRuntime attempts and caches
/// the outcome so that repeated / concurrent calls are idempotent.
#[allow(dead_code)]
pub struct RuntimeGuard {
    pub state: Mutex<RuntimeState>,
    /// Notified when a Starting -> Ready/Failed transition completes so
    /// that concurrent waiters can proceed.
    pub notify: Notify,
}

impl RuntimeGuard {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RuntimeState::NotStarted),
            notify: Notify::new(),
        }
    }
}

/// Returns the global RuntimeGuard singleton.
#[allow(dead_code)]
pub fn runtime_guard() -> &'static RuntimeGuard {
    static GUARD: OnceLock<RuntimeGuard> = OnceLock::new();
    GUARD.get_or_init(RuntimeGuard::new)
}

/// Platform-independent, idempotent EnsureRuntime handler.
///
/// - First caller with `start_if_needed=true` transitions NotStarted -> Starting -> Ready/Failed.
/// - Concurrent callers wait for the first caller to finish and share the result.
/// - After Ready, subsequent calls return "reused" immediately.
/// - After Failed, a new `start_if_needed=true` call retries.
/// - `start_if_needed=false` only probes without attempting to start.
///
/// `start_fn` is invoked only by the driver; it performs the actual start sequence.
/// `probe_fn` is invoked for start_if_needed=false to report current status.
#[allow(dead_code)]
pub async fn ensure_runtime<F, P>(
    guard: &RuntimeGuard,
    start_if_needed: bool,
    start_fn: F,
    probe_fn: P,
) -> RuntimeEnsureResponse
where
    F: std::future::Future<Output = RuntimeEnsureResponse>,
    P: std::future::Future<Output = RuntimeEnsureResponse>,
{
    // Fast path: if already Ready, return immediately.
    {
        let state = guard.state.lock().await;
        if let RuntimeState::Ready { endpoint, message } = &*state {
            return RuntimeEnsureResponse {
                ready: true,
                endpoint: endpoint.clone(),
                message: message.clone(),
                status: STATUS_REUSED.to_string(),
            };
        }
    }

    // Probe-only mode: do not attempt to start.
    if !start_if_needed {
        return probe_fn.await;
    }

    // Attempt to become the driver of the start sequence.
    let i_am_driver = {
        let mut state = guard.state.lock().await;
        match &*state {
            RuntimeState::Ready { endpoint, message } => {
                // Another caller finished while we waited for the lock.
                return RuntimeEnsureResponse {
                    ready: true,
                    endpoint: endpoint.clone(),
                    message: message.clone(),
                    status: STATUS_REUSED.to_string(),
                };
            }
            RuntimeState::Starting => false,
            RuntimeState::NotStarted | RuntimeState::Failed { .. } => {
                *state = RuntimeState::Starting;
                true
            }
        }
    };

    if i_am_driver {
        // We are the driver: perform the actual start sequence.
        let response = start_fn.await;

        // Publish outcome to the state machine.
        let mut state = guard.state.lock().await;
        if response.ready {
            *state = RuntimeState::Ready {
                endpoint: response.endpoint.clone(),
                message: response.message.clone(),
            };
        } else {
            *state = RuntimeState::Failed {
                message: response.message.clone(),
            };
        }
        // Wake all waiters.
        guard.notify.notify_waiters();

        return response;
    }

    // We are a waiter: wait for the driver to finish.
    loop {
        // Register for notification BEFORE checking state to prevent lost
        // wakeups.  If the driver calls notify_waiters() between our state
        // check and the await, the future is already enabled and will
        // resolve immediately.
        let notified = guard.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let state = guard.state.lock().await;
        match &*state {
            RuntimeState::Ready { endpoint, message } => {
                return RuntimeEnsureResponse {
                    ready: true,
                    endpoint: endpoint.clone(),
                    message: message.clone(),
                    status: STATUS_REUSED.to_string(),
                };
            }
            RuntimeState::Failed { message } => {
                return RuntimeEnsureResponse {
                    ready: false,
                    endpoint: String::new(),
                    message: message.clone(),
                    status: STATUS_FAILED.to_string(),
                };
            }
            RuntimeState::Starting => {
                // Release lock before waiting.
                drop(state);
                notified.await;
            }
            RuntimeState::NotStarted => {
                // Should not happen, but treat as failed.
                return RuntimeEnsureResponse {
                    ready: false,
                    endpoint: String::new(),
                    message: "unexpected state: NotStarted after notify".to_string(),
                    status: STATUS_FAILED.to_string(),
                };
            }
        }
    }
}
