//! Zombie reaping for the PID 1 agent.
//!
//! When the agent runs as PID 1 (legacy standalone boot), orphaned
//! grandchildren such as containerd shims are reparented to it and must be
//! reaped on SIGCHLD or they accumulate as zombies.

#[cfg(target_os = "linux")]
mod platform {
    /// Reap every zombie via `waitpid(-1, WNOHANG)`.
    fn reap_children() {
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
        use nix::unistd::Pid;

        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    tracing::info!(pid = pid.as_raw(), status, "reaped child process");
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) => {
                    tracing::warn!(pid = pid.as_raw(), signal = %sig, "child killed by signal");
                }
                // No more zombies.
                Ok(WaitStatus::StillAlive) => break,
                // ECHILD = no children at all; any other error is unexpected.
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "unexpected waitpid error");
                    break;
                }
                _ => {}
            }
        }
    }

    /// Spawn a background task that reaps zombies on SIGCHLD.
    ///
    /// Must be called from a tokio runtime. Runs until the process exits.
    pub fn spawn_reaper() {
        tokio::spawn(async move {
            let mut sigchld = match tokio::signal::unix::signal(
                tokio::signal::unix::SignalKind::child(),
            ) {
                Ok(s) => s,
                Err(e) => {
                    // PID 1 must not panic. Degrade gracefully: zombies may
                    // accumulate but the agent keeps running.
                    tracing::error!(error = %e, "failed to register SIGCHLD handler, zombie reaping disabled");
                    return;
                }
            };

            loop {
                if sigchld.recv().await.is_none() {
                    // Signal stream closed — should not happen for PID 1,
                    // but degrade gracefully rather than spin.
                    tracing::error!("SIGCHLD stream closed unexpectedly, zombie reaping disabled");
                    return;
                }
                reap_children();
            }
        });
    }
}

#[cfg(target_os = "linux")]
pub use platform::spawn_reaper;

/// Stub for non-Linux development builds.
#[cfg(not(target_os = "linux"))]
pub fn spawn_reaper() {}
