//! Returns freed guest disk space to the host without anyone asking.
//!
//! Every guest mounts its Btrfs data disk with `discard=async`, so an extent
//! the filesystem frees whole reaches the host's sparse image on its own —
//! measured at 15 s after `rm` inside a machine and ~3 min inside the System
//! VM, where Btrfs's block-group delay and its 1000 discards/s cap stretch it.
//! What that never returns is the free space left inside a block group that
//! is still partly used, and whatever was queued when a disk was unmounted.
//! A full `FITRIM` gets those, so this module asks each guest for one:
//!
//! - the System VM whenever the lifecycle marks it idle (five minutes with
//!   no Docker traffic), so the trim never competes with a workload, and
//!   at most once per [`SYSTEM_VM_MIN_INTERVAL`] while it stays idle;
//! - every running distro machine on a fixed [`MACHINE_INTERVAL`], because
//!   machines have no idle signal (a machine's agent serves RPC only, so the
//!   trim is host-driven rather than a loop inside the guest).
//!
//! The System VM's virtio path and HVC fast path both hole-punch on DISCARD,
//! so a trim costs the host nothing beyond the punches themselves (APFS
//! punches a range that is already a hole in microseconds), and the guest
//! side of `FITRIM` on a fresh disk measured under 1 ms. Failures are logged
//! and retried on the next tick: a guest that cannot trim right now is not a
//! reason to stop trying.

use std::sync::Arc;
use std::time::Duration;

use arcbox_core::machine::MachineState;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime, VmLifecycleState};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;

/// Shortest gap between two System VM trims while it stays idle. The idle
/// lifecycle state is entered once per idle period, so this is what bounds
/// a VM that flaps between idle and active.
const SYSTEM_VM_MIN_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How often every running distro machine is trimmed.
const MACHINE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How long after the daemon starts the first machine sweep runs; the
/// machines it finds running were recovered from a previous daemon and may
/// hold months of unreclaimed space.
const MACHINE_INITIAL_DELAY: Duration = Duration::from_secs(60);

/// Spawns both trim loops.
pub fn spawn(ctx: &DaemonContext, runtime: &Arc<Runtime>) {
    let source = RuntimeTrimmer(Arc::clone(runtime));
    let vm = runtime.subscribe_system_vm_state();
    drop(tokio::spawn(idle_system_vm_loop(
        source.clone(),
        vm,
        ctx.shutdown.clone(),
    )));
    drop(tokio::spawn(machine_loop(source, ctx.shutdown.clone())));
}

/// Where trims go; a seam so the scheduling is testable without a guest.
trait Trimmer: Clone + Send + 'static {
    /// Trims the System VM's data filesystems.
    fn trim_system_vm(&self) -> impl Future<Output = arcbox_core::Result<u64>> + Send;

    /// Names of the running distro machines.
    fn running_machines(&self) -> Vec<String>;

    /// Trims one distro machine's data disk.
    fn trim_machine(&self, name: &str) -> impl Future<Output = arcbox_core::Result<u64>> + Send;
}

#[derive(Clone)]
struct RuntimeTrimmer(Arc<Runtime>);

impl Trimmer for RuntimeTrimmer {
    async fn trim_system_vm(&self) -> arcbox_core::Result<u64> {
        self.0.trim_machine_disk(DEFAULT_MACHINE_NAME).await
    }

    fn running_machines(&self) -> Vec<String> {
        self.0
            .machine_manager()
            .list()
            .into_iter()
            .filter(|m| m.distro.is_some() && m.state == MachineState::Running)
            .map(|m| m.name)
            .collect()
    }

    async fn trim_machine(&self, name: &str) -> arcbox_core::Result<u64> {
        self.0.trim_machine_disk(name).await
    }
}

/// Trims the System VM each time it enters `Idle`, no more often than
/// [`SYSTEM_VM_MIN_INTERVAL`].
async fn idle_system_vm_loop<T: Trimmer>(
    trimmer: T,
    mut vm: watch::Receiver<VmLifecycleState>,
    shutdown: CancellationToken,
) {
    let mut last_trim: Option<tokio::time::Instant> = None;
    loop {
        let idle = *vm.borrow_and_update() == VmLifecycleState::Idle;
        let due = last_trim.is_none_or(|at| at.elapsed() >= SYSTEM_VM_MIN_INTERVAL);
        if idle && due {
            last_trim = Some(tokio::time::Instant::now());
            match trimmer.trim_system_vm().await {
                Ok(bytes) => info!(bytes_trimmed = bytes, "trimmed the idle System VM's disks"),
                Err(e) => {
                    warn!(error = %e, "System VM disk trim failed; retrying on the next idle period");
                }
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            // The sender lives as long as the runtime; an error means the
            // daemon is going away.
            changed = vm.changed() => if changed.is_err() { break },
        }
    }
}

/// Trims every running distro machine on [`MACHINE_INTERVAL`].
async fn machine_loop<T: Trimmer>(trimmer: T, shutdown: CancellationToken) {
    let mut next = tokio::time::Instant::now() + MACHINE_INITIAL_DELAY;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep_until(next) => {}
        }
        next = tokio::time::Instant::now() + MACHINE_INTERVAL;
        for name in trimmer.running_machines() {
            match trimmer.trim_machine(&name).await {
                Ok(bytes) => {
                    info!(machine = %name, bytes_trimmed = bytes, "trimmed the machine's data disk");
                }
                Err(e) => {
                    debug!(machine = %name, error = %e, "machine disk trim failed; retrying next interval");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Counts System VM trims and records which machines were trimmed.
    #[derive(Clone, Default)]
    struct Recording {
        system_vm: Arc<AtomicUsize>,
        machines: Arc<Mutex<Vec<String>>>,
        running: Arc<Mutex<Vec<String>>>,
    }

    impl Trimmer for Recording {
        async fn trim_system_vm(&self) -> arcbox_core::Result<u64> {
            self.system_vm.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }

        fn running_machines(&self) -> Vec<String> {
            self.running.lock().unwrap().clone()
        }

        async fn trim_machine(&self, name: &str) -> arcbox_core::Result<u64> {
            self.machines.lock().unwrap().push(name.to_owned());
            Ok(0)
        }
    }

    /// Lets a loop run until it parks again, advancing paused time by
    /// `elapsed` on the way.
    async fn settle(elapsed: Duration) {
        tokio::time::sleep(elapsed).await;
        tokio::task::yield_now().await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_system_vm_is_trimmed_on_each_idle_entry_but_not_more_often_than_the_floor() {
        let trimmer = Recording::default();
        let (vm_tx, vm) = watch::channel(VmLifecycleState::Running);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(idle_system_vm_loop(trimmer.clone(), vm, shutdown.clone()));

        settle(Duration::from_secs(1)).await;
        assert_eq!(
            trimmer.system_vm.load(Ordering::SeqCst),
            0,
            "a running VM is not trimmed"
        );

        vm_tx.send_replace(VmLifecycleState::Idle);
        settle(Duration::ZERO).await;
        assert_eq!(
            trimmer.system_vm.load(Ordering::SeqCst),
            1,
            "idle entry trims"
        );

        // Flapping back into idle within the floor does not trim again.
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(Duration::ZERO).await;
        vm_tx.send_replace(VmLifecycleState::Idle);
        settle(Duration::ZERO).await;
        assert_eq!(
            trimmer.system_vm.load(Ordering::SeqCst),
            1,
            "inside the floor"
        );

        // Past the floor, the next idle entry trims.
        vm_tx.send_replace(VmLifecycleState::Running);
        settle(SYSTEM_VM_MIN_INTERVAL).await;
        vm_tx.send_replace(VmLifecycleState::Idle);
        settle(Duration::ZERO).await;
        assert_eq!(
            trimmer.system_vm.load(Ordering::SeqCst),
            2,
            "past the floor"
        );

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn running_machines_are_trimmed_every_interval() {
        let trimmer = Recording::default();
        trimmer.running.lock().unwrap().push("m1".into());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(machine_loop(trimmer.clone(), shutdown.clone()));

        settle(MACHINE_INITIAL_DELAY / 2).await;
        assert!(
            trimmer.machines.lock().unwrap().is_empty(),
            "not before the initial delay"
        );

        settle(MACHINE_INITIAL_DELAY / 2).await;
        assert_eq!(*trimmer.machines.lock().unwrap(), vec!["m1"]);

        trimmer.running.lock().unwrap().push("m2".into());
        settle(MACHINE_INTERVAL).await;
        assert_eq!(*trimmer.machines.lock().unwrap(), vec!["m1", "m1", "m2"]);

        shutdown.cancel();
        task.await.unwrap();
    }
}
