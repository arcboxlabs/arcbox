//! Keeps the System VM's durable sandbox cleanup stream connected while a
//! guest that runs sandboxes is up.
//!
//! The stream is a streaming RPC. The HV backend's blocking agent transport
//! cannot carry one, and a guest without nested virtualization runs no
//! sandboxes, so it has nothing to replay. The watch therefore follows the
//! VM lifecycle instead of retrying blindly: it connects once the VM is
//! ready on a backend that nests, reconnects while that holds (the guest
//! replays every unfinalized marker on connect), and stays idle otherwise.
//! A VZ→HV switch at runtime thus ends the watch, rather than leaving it to
//! fail every second against the new transport.

use std::sync::Arc;
use std::time::Duration;

use arcbox_computer::cleanup;
use arcbox_core::{Runtime, VmBackend, VmLifecycleState};
use arcbox_engine::EngineError;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;

/// How long a broken stream waits before reconnecting to a guest that is
/// still up.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Spawns the watch for the System VM.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let vm = runtime.subscribe_system_vm_state();
    let shutdown = ctx.shutdown.clone();
    let stream = RuntimeStream {
        runtime: Arc::clone(runtime),
    };
    drop(tokio::spawn(async move {
        reconcile_loop(&stream, vm, shutdown).await;
    }));
}

/// Where the loop gets its stream; a seam so the gating is testable
/// without a guest.
trait CleanupStream: Send + Sync {
    /// The backend the System VM runs on right now.
    fn backend(&self) -> VmBackend;

    /// Serves one connected cleanup stream until it ends, returning why.
    fn watch(&self) -> impl Future<Output = EngineError> + Send;
}

/// The System VM's stream, through the runtime's `SandboxHost` seam.
struct RuntimeStream {
    runtime: Arc<Runtime>,
}

impl CleanupStream for RuntimeStream {
    fn backend(&self) -> VmBackend {
        self.runtime.system_vm_backend()
    }

    async fn watch(&self) -> EngineError {
        cleanup::watch(self.runtime.as_ref()).await
    }
}

/// Serves `stream` while the VM is ready on a backend that runs sandboxes;
/// idles — asking the guest nothing — whenever it is not.
///
/// A guest that answers that it runs no sandboxes is left alone until the
/// VM restarts: the answer cannot change within one incarnation.
async fn reconcile_loop(
    stream: &impl CleanupStream,
    mut vm: watch::Receiver<VmLifecycleState>,
    shutdown: CancellationToken,
) {
    // Set once this guest incarnation is known to run no sandboxes; cleared
    // when the VM goes down, so the next one is asked afresh.
    let mut idle = false;
    loop {
        let ready = vm.borrow_and_update().is_ready();
        let mut reconnect = false;
        if !ready {
            idle = false;
        } else if !idle {
            let backend = stream.backend();
            if backend.supports_nested_virt() {
                let error = tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    error = stream.watch() => error,
                };
                if cleanup::sandbox_unavailable(&error) {
                    info!(%error, "sandbox cleanup watch idle: the guest runs no sandboxes");
                    idle = true;
                } else if vm.borrow().is_ready() {
                    warn!(%error, "sandbox cleanup watch disconnected; reconnecting");
                    reconnect = true;
                } else {
                    debug!(%error, "sandbox cleanup watch ended with the VM");
                }
            } else {
                info!(
                    backend = backend.as_str(),
                    "sandbox cleanup watch idle: the backend runs no sandboxes"
                );
                idle = true;
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            // The sender lives as long as the runtime; an error means it is
            // gone and there is nothing left to watch.
            changed = vm.changed() => if changed.is_err() { break },
            () = tokio::time::sleep(RECONNECT_DELAY), if reconnect => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts connections and ends every stream the same way.
    struct FakeStream {
        backend: Mutex<VmBackend>,
        connections: AtomicUsize,
        end: fn() -> EngineError,
    }

    impl FakeStream {
        fn new(backend: VmBackend, end: fn() -> EngineError) -> Self {
            Self {
                backend: Mutex::new(backend),
                connections: AtomicUsize::new(0),
                end,
            }
        }

        fn set_backend(&self, backend: VmBackend) {
            *self.backend.lock().unwrap() = backend;
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    impl CleanupStream for FakeStream {
        fn backend(&self) -> VmBackend {
            *self.backend.lock().unwrap()
        }

        async fn watch(&self) -> EngineError {
            self.connections.fetch_add(1, Ordering::SeqCst);
            (self.end)()
        }
    }

    fn disconnected() -> EngineError {
        EngineError::Machine("stream reset".into())
    }

    fn no_sandboxes() -> EngineError {
        EngineError::Agent {
            code: 412,
            message: "nested virtualization unavailable".into(),
        }
    }

    /// Lets the loop run until it parks again, advancing paused time by
    /// `elapsed` on the way.
    async fn settle(elapsed: Duration) {
        tokio::time::sleep(elapsed).await;
        tokio::task::yield_now().await;
    }

    fn spawn_loop(
        stream: &Arc<FakeStream>,
        vm: watch::Receiver<VmLifecycleState>,
        shutdown: &CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let (stream, shutdown) = (Arc::clone(stream), shutdown.clone());
        tokio::spawn(async move { reconcile_loop(stream.as_ref(), vm, shutdown).await })
    }

    #[tokio::test(start_paused = true)]
    async fn watches_only_while_the_vm_is_ready_on_a_nesting_backend() {
        let stream = Arc::new(FakeStream::new(VmBackend::Hv, disconnected));
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = spawn_loop(&stream, vm, &shutdown);

        // HV runs no sandboxes: a ready VM is not asked, and nothing retries.
        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(
            stream.connections(),
            0,
            "no stream on a backend without sandboxes"
        );

        // The VM restarts on VZ: the watch connects, and reconnects after
        // every break for as long as the guest stays up.
        stream.set_backend(VmBackend::Vz);
        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        assert_eq!(stream.connections(), 0, "no stream while the VM is down");
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(RECONNECT_DELAY * 3).await;
        let while_up = stream.connections();
        assert!(
            while_up >= 3,
            "reconnects every {RECONNECT_DELAY:?} while the guest is up, got {while_up}"
        );

        // Down again: the break is the VM's, so no reconnect is attempted.
        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        let at_stop = stream.connections();
        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(
            stream.connections(),
            at_stop,
            "no stream while the VM is down"
        );

        // Back up after a switch to HV: idle, with no retry loop.
        stream.set_backend(VmBackend::Hv);
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(
            stream.connections(),
            at_stop,
            "no stream after switching to a backend without sandboxes"
        );

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_without_sandboxes_is_asked_once_per_incarnation() {
        let stream = Arc::new(FakeStream::new(VmBackend::Vz, no_sandboxes));
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = spawn_loop(&stream, vm, &shutdown);

        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(stream.connections(), 1, "asked once, then left alone");

        // Still the same guest: an idle-state change asks nothing new.
        vm_tx.send_replace(VmLifecycleState::Idle);
        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(stream.connections(), 1);

        // A restart is a new guest, asked afresh.
        vm_tx.send_replace(VmLifecycleState::Stopping);
        settle(Duration::ZERO).await;
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(RECONNECT_DELAY * 3).await;
        assert_eq!(stream.connections(), 2);

        shutdown.cancel();
        task.await.unwrap();
    }
}
