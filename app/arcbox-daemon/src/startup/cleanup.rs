//! Stale daemon termination and resource release.

use std::time::Duration;

use tracing::{info, warn};

/// Send SIGTERM to a verified stale daemon and wait for it to exit.  Falls
/// back to SIGKILL after 30 s as a last resort.
pub(super) fn terminate_stale_daemon(old_pid: i32) {
    warn!(old_pid, "Stale daemon still running, sending SIGTERM");
    // SAFETY: sending a signal to a verified arcbox-daemon process.
    let ret = unsafe { libc::kill(old_pid, libc::SIGTERM) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        warn!(old_pid, %err, "Failed to send SIGTERM to stale daemon");
        return;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline && is_process_alive(old_pid) {
        std::thread::sleep(Duration::from_millis(500));
    }

    if is_process_alive(old_pid) {
        warn!(
            old_pid,
            "Stale daemon did not exit after 30s, sending SIGKILL"
        );
        // SAFETY: last resort — the old daemon is unresponsive.
        let ret = unsafe { libc::kill(old_pid, libc::SIGKILL) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            warn!(old_pid, %err, "Failed to send SIGKILL to stale daemon");
        }
        std::thread::sleep(Duration::from_secs(1));
    } else {
        info!(old_pid, "Stale daemon exited gracefully");
    }
}

fn is_process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) is a standard POSIX existence check.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we lack permission to signal it.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub(super) fn is_arcbox_daemon(pid: i32) -> bool {
    match libproc::proc_pid::pidpath(pid) {
        Ok(path) => path.contains("arcbox-daemon") || path.contains("arcboxlabs.desktop.daemon"),
        Err(_) => false,
    }
}

/// Wait for processes holding `docker.img` open to exit on their own.
#[cfg(target_os = "macos")]
pub(super) fn wait_for_docker_img_holders(docker_img: &std::path::Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let pids = match libproc::processes::pids_by_path(docker_img, false, false) {
            Ok(pids) => pids
                .into_iter()
                .filter(|&p| p != std::process::id())
                .collect::<Vec<_>>(),
            Err(e) => {
                warn!(%e, "Failed to query processes holding docker.img");
                break;
            }
        };
        if pids.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            warn!(
                ?pids,
                "Processes still holding docker.img after 10s — \
                 not killing them to avoid data loss; VM startup may fail"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn wait_for_docker_img_holders(_docker_img: &std::path::Path) {}
