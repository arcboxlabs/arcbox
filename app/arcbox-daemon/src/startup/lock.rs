//! `flock`-based exclusive daemon ownership.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use super::cleanup::{is_arcbox_daemon, terminate_stale_daemon};

/// Exclusive file lock held for the daemon's lifetime.
///
/// Uses `flock(LOCK_EX)` on `daemon.lock` so liveness is tracked by the
/// kernel — the lock is released automatically on process exit or crash.
/// The file also stores the current PID for diagnostics and signalling.
pub struct DaemonLock {
    _file: std::fs::File,
}

impl DaemonLock {
    /// Acquire the daemon lock, terminating any stale daemon that holds it.
    ///
    /// 1. Try a non-blocking exclusive lock.
    /// 2. If held → read old PID → SIGTERM → blocking lock (waits for release).
    /// 3. Write current PID into the lock file.
    ///
    /// This is a blocking operation and should be called from
    /// `spawn_blocking`.
    pub fn acquire(run_dir: &Path) -> Result<Self> {
        use std::io::{Seek, Write};

        let lock_path = run_dir.join("daemon.lock");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .context("Failed to open daemon.lock")?;

        // Try non-blocking lock first.
        if try_flock_exclusive(&file) {
            // No stale daemon — we got the lock immediately.
            info!("Daemon lock acquired (no stale daemon)");
        } else {
            // Lock held by another process — read its PID and signal it.
            let old_pid = read_pid_from_file(&mut file);
            if let Some(pid) = old_pid {
                if is_arcbox_daemon(pid) {
                    terminate_stale_daemon(pid);
                } else {
                    warn!(pid, "Lock held by non-arcbox process, waiting for release");
                }
            } else {
                warn!("Lock held but could not read PID, waiting for release");
            }

            // Blocking lock — waits until the holder exits.
            flock_exclusive(&file).context("Failed to acquire daemon lock")?;
            info!("Daemon lock acquired after stale daemon exited");
        }

        // Write our PID into the lock file.
        file.set_len(0).ok();
        file.seek(std::io::SeekFrom::Start(0)).ok();
        write!(file, "{}\n", std::process::id()).ok();

        Ok(Self { _file: file })
    }
}

fn read_pid_from_file(file: &mut std::fs::File) -> Option<i32> {
    use std::io::{Read, Seek};
    file.seek(std::io::SeekFrom::Start(0)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

/// Non-blocking `LOCK_EX`. Returns `true` if the lock was acquired.
fn try_flock_exclusive(file: &std::fs::File) -> bool {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on a valid fd with LOCK_EX|LOCK_NB is safe.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ret == 0
}

/// Blocking `LOCK_EX`.
fn flock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on a valid fd with LOCK_EX is safe.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
