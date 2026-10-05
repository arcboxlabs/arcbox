//! Guest shutdown sequence.
//!
//! When the host requests a graceful shutdown over vsock, the agent calls
//! [`poweroff`]. How the VM is powered off depends on who PID 1 is:
//!
//! - Under **busybox init** (the System VM) the agent is a supervised child,
//!   so it asks PID 1 to power off (`SIGUSR2`); busybox init then stops all
//!   children, syncs, and halts the VM. A direct `reboot(POWER_OFF)` is the
//!   fallback if PID 1 does not power off in time.
//! - In a **distro machine** PID 1 is the distro's own init — systemd,
//!   OpenRC on busybox, sysvinit, runit — and only its own `poweroff`
//!   command knows how to ask it. `SIGUSR2` is not that: systemd answers it
//!   by dumping its unit state to the journal and keeps running, so ubuntu
//!   sat through the whole grace period before the fallback halted it
//!   (~25 s per `machine stop`, measured 2026-10-04). The agent runs the
//!   distro's `poweroff`, with the same fallback.
//! - In **legacy standalone** boot (the agent is run directly as PID 1, e.g. the
//!   e2e harness) the agent drives shutdown itself: terminate every process, sync,
//!   and `reboot(LINUX_REBOOT_CMD_POWER_OFF)` (PSCI SYSTEM_OFF on ARM64).

use crate::agent::Guest;
#[cfg(not(target_os = "linux"))]
use std::time::Duration;

/// How to power the VM off, selected by PID 1's identity.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownStrategy {
    /// The agent is PID 1 (legacy standalone boot): drive the whole shutdown
    /// here — terminate all processes, sync, and `reboot(POWER_OFF)`.
    DirectReboot,
    /// busybox init is PID 1: ask it to power off (`SIGUSR2`) so it stops children
    /// gracefully and syncs, falling back to a direct reboot if it does not.
    SignalInit,
    /// The distro's init is PID 1: run the distro's `poweroff`, which asks
    /// that init in its own way, falling back to a direct reboot if the
    /// machine is still up after the grace period.
    DistroInit,
}

/// Chooses the [`ShutdownStrategy`] from the agent's PID and the guest it
/// serves.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const fn shutdown_strategy(pid: u32, guest: Guest) -> ShutdownStrategy {
    match guest {
        _ if pid == 1 => ShutdownStrategy::DirectReboot,
        Guest::DistroMachine => ShutdownStrategy::DistroInit,
        Guest::SystemVm | Guest::StorageRecovery => ShutdownStrategy::SignalInit,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Guest, ShutdownStrategy, shutdown_strategy};
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// Where a distro keeps its `poweroff`: the sbin/bin pairs, and the
    /// NixOS system profile, which has nothing under `/sbin`.
    const DISTRO_POWEROFF: [&str; 5] = [
        "/sbin/poweroff",
        "/usr/sbin/poweroff",
        "/bin/poweroff",
        "/usr/bin/poweroff",
        "/run/current-system/sw/bin/poweroff",
    ];

    /// Powers off the VM, dispatching on [`ShutdownStrategy`].
    ///
    /// `grace` means different things per strategy: under `DirectReboot` it is the
    /// SIGTERM→SIGKILL dwell time for individual processes; under `SignalInit`
    /// and `DistroInit` it is how long to wait for init's *entire* orderly
    /// teardown (stop all children, sync, power off) before forcing a direct
    /// reboot. Callers should size it to cover the full init shutdown, not
    /// just a SIGTERM window.
    ///
    /// This function does not return on success.
    pub fn poweroff(grace: Duration, guest: Guest) {
        match shutdown_strategy(std::process::id(), guest) {
            ShutdownStrategy::DirectReboot => direct_reboot(grace),
            ShutdownStrategy::SignalInit => {
                tracing::info!("Shutdown: requesting orderly poweroff from busybox init (PID 1)");
                // SAFETY: kill(1, SIGUSR2) asks busybox init to run its poweroff
                // sequence (stop all children, sync, power off). No memory-safety
                // preconditions.
                unsafe { libc::kill(1, libc::SIGUSR2) };
                force_poweroff_after(grace);
            }
            ShutdownStrategy::DistroInit => {
                match run_distro_poweroff() {
                    Ok(command) => {
                        tracing::info!(command, "Shutdown: asked the machine's init to power off");
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "Shutdown: the machine's poweroff command failed; waiting out the grace period"
                    ),
                }
                force_poweroff_after(grace);
            }
        }
    }

    /// Runs the distro's `poweroff`, which returns once it has asked init.
    /// Returns the command that ran.
    fn run_distro_poweroff() -> std::io::Result<&'static str> {
        let command = DISTRO_POWEROFF
            .into_iter()
            .find(|path| Path::new(path).exists())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no poweroff command found")
            })?;
        let status = std::process::Command::new(command).status()?;
        if status.success() {
            Ok(command)
        } else {
            Err(std::io::Error::other(format!(
                "{command} exited with {status}"
            )))
        }
    }

    /// Waits `grace` for init's orderly poweroff, then forces one.
    ///
    /// Init SIGTERMs this process as part of its sequence, so this normally
    /// never wakes from the sleep.
    fn force_poweroff_after(grace: Duration) {
        std::thread::sleep(grace);

        // PID 1 did not power the VM off in time — force it. The agent holds
        // CAP_SYS_BOOT regardless of PID, so reboot() still works.
        tracing::warn!("Shutdown: init did not power off in time; forcing poweroff");

        // SAFETY: sync() flushes all filesystem buffers. No preconditions.
        unsafe { libc::sync() };

        // SAFETY: CAP_SYS_BOOT is held; LINUX_REBOOT_CMD_POWER_OFF triggers PSCI
        // SYSTEM_OFF on ARM64, halting the VM.
        unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };

        tracing::error!("Shutdown: reboot(POWER_OFF) returned unexpectedly, forcing exit");
        // SAFETY: _exit terminates the process immediately.
        unsafe { libc::_exit(1) };
    }

    /// Drives the full shutdown from PID 1: SIGTERM, SIGKILL, sync, reboot.
    fn direct_reboot(grace: Duration) {
        tracing::info!("Shutdown: sending SIGTERM to all processes");

        // SAFETY: kill(-1, SIGTERM) sends SIGTERM to every process except PID 1.
        // The agent is PID 1 so it is not affected.
        unsafe { libc::kill(-1, libc::SIGTERM) };

        wait_for_children(grace);

        tracing::info!("Shutdown: sending SIGKILL to remaining processes");

        // SAFETY: kill(-1, SIGKILL) sends SIGKILL to every process except PID 1.
        unsafe { libc::kill(-1, libc::SIGKILL) };

        // Brief reap pass for SIGKILL'd processes.
        wait_for_children(Duration::from_millis(500));

        tracing::info!("Shutdown: syncing filesystems");

        // SAFETY: sync() flushes all filesystem buffers. No preconditions.
        unsafe { libc::sync() };

        tracing::info!("Shutdown: powering off");

        // SAFETY: Called as PID 1 with CAP_SYS_BOOT. LINUX_REBOOT_CMD_POWER_OFF
        // triggers PSCI SYSTEM_OFF on ARM64, halting the VM.
        unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };

        // Should not reach here — reboot does not return on success.
        // Terminate PID 1 so the kernel doesn't continue running a
        // half-shutdown guest with all processes already killed.
        tracing::error!("Shutdown: reboot(POWER_OFF) returned unexpectedly, forcing exit");
        // SAFETY: _exit terminates the process immediately. All other
        // processes are already dead (SIGKILL'd above).
        unsafe { libc::_exit(1) };
    }

    /// Reaps children via `waitpid(-1, WNOHANG)` until none remain or
    /// `timeout` elapses.
    fn wait_for_children(timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return;
            }
            let mut status: i32 = 0;
            // SAFETY: waitpid(-1, ..., WNOHANG) is safe to call from PID 1.
            // It returns 0 when no children have exited, -1/ECHILD when none remain.
            let pid = unsafe { libc::waitpid(-1, &raw mut status, libc::WNOHANG) };
            if pid <= 0 {
                if pid == -1 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::ECHILD) {
                        tracing::debug!("Shutdown: no more children");
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use platform::poweroff;

#[cfg(not(target_os = "linux"))]
#[allow(dead_code, reason = "stub for non-Linux development builds")]
pub fn poweroff(_grace: Duration, _guest: Guest) {
    tracing::warn!("poweroff not supported on this platform");
}

#[cfg(test)]
mod tests {
    use super::{Guest, ShutdownStrategy, shutdown_strategy};

    #[test]
    fn pid1_drives_shutdown_directly() {
        assert_eq!(
            shutdown_strategy(1, Guest::SystemVm),
            ShutdownStrategy::DirectReboot
        );
        assert_eq!(
            shutdown_strategy(1, Guest::DistroMachine),
            ShutdownStrategy::DirectReboot
        );
    }

    #[test]
    fn non_pid1_delegates_to_busybox_init_in_the_system_vm() {
        // Any non-1 PID means the agent is a supervised child of busybox init.
        assert_eq!(
            shutdown_strategy(2, Guest::SystemVm),
            ShutdownStrategy::SignalInit
        );
        assert_eq!(
            shutdown_strategy(1234, Guest::SystemVm),
            ShutdownStrategy::SignalInit
        );
    }

    /// A distro's init is asked through its own `poweroff`: systemd takes
    /// `SIGUSR2` as "dump your state", not "power off".
    #[test]
    fn non_pid1_asks_a_distro_machines_init_through_its_poweroff() {
        assert_eq!(
            shutdown_strategy(1234, Guest::DistroMachine),
            ShutdownStrategy::DistroInit
        );
    }
}
