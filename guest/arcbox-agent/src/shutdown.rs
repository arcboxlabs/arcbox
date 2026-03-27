//! Orderly PID 1 shutdown sequence.
//!
//! When the host requests a graceful shutdown over vsock, the agent calls
//! [`poweroff`] which terminates all processes, flushes filesystems, and
//! powers off the VM via `reboot(LINUX_REBOOT_CMD_POWER_OFF)` (PSCI
//! SYSTEM_OFF on ARM64).

use std::time::Duration;

#[cfg(target_os = "linux")]
mod platform {
    use std::time::{Duration, Instant};

    /// Performs orderly PID 1 shutdown and powers off the VM.
    ///
    /// Sends SIGTERM to all processes, waits up to `grace` for them to exit,
    /// sends SIGKILL to survivors, flushes filesystems, and calls
    /// `reboot(LINUX_REBOOT_CMD_POWER_OFF)`.
    ///
    /// This function does not return on success.
    pub fn poweroff(grace: Duration) {
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
                if Instant::now() >= deadline {
                    return;
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
pub fn poweroff(_grace: Duration) {
    tracing::warn!("poweroff not supported on this platform");
}
