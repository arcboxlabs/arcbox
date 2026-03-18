//! Best-effort self-setup tasks that the daemon runs during startup.
//!
//! Each function checks if the setup is already in place and, if not, attempts
//! to install it via `arcbox-helper`. Failures are logged as warnings — they
//! never block daemon startup. The canonical setup path is `arcbox install`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Ensures the DNS resolver file is installed for the given domain/port.
///
/// Checks `/etc/resolver/<domain>` first; if absent, invokes
/// `arcbox-helper dns install`.
pub async fn ensure_dns_resolver(port: u16, domain: &str) {
    let domain = domain.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let resolver_path = format!("/etc/resolver/{domain}");
        if Path::new(&resolver_path).exists() {
            tracing::debug!(domain, "DNS resolver already installed");
            return;
        }

        let helper = helper_path();
        match Command::new(&helper)
            .args([
                "dns",
                "install",
                "--domain",
                &domain,
                "--port",
                &port.to_string(),
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!(domain, port, "DNS resolver installed via helper");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                tracing::warn!(
                    domain,
                    stderr = %stderr.trim(),
                    stdout = %stdout.trim(),
                    "DNS resolver install failed (run 'sudo arcbox install' to fix)"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "arcbox-helper not found, skipping DNS resolver setup"
                );
            }
        }
    })
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "DNS resolver setup task panicked");
    }
}

/// Ensures the Docker socket symlink exists at `/var/run/docker.sock`.
///
/// Checks if the symlink already points to `docker_sock_path`; if not,
/// invokes `arcbox-helper socket link`.
pub async fn ensure_docker_socket(docker_sock_path: &Path) {
    let target = docker_sock_path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        // Check if /var/run/docker.sock already points to our socket.
        let system_sock = Path::new("/var/run/docker.sock");
        if let Ok(existing) = std::fs::read_link(system_sock) {
            if existing == target {
                tracing::debug!("Docker socket symlink already correct");
                return;
            }
        }

        let helper = helper_path();
        let target_str = target.to_string_lossy();
        match Command::new(&helper)
            .args(["socket", "link", "--target", &target_str])
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!(target = %target_str, "Docker socket symlink created via helper");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                tracing::warn!(
                    stderr = %stderr.trim(),
                    stdout = %stdout.trim(),
                    "Docker socket link failed (run 'sudo arcbox install' to fix)"
                );
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "arcbox-helper not found, skipping Docker socket setup"
                );
            }
        }
    })
    .await;

    if let Err(e) = result {
        tracing::warn!(error = %e, "Docker socket setup task panicked");
    }
}

/// Resolves the helper binary path (mirrors route_reconciler::helper_path).
fn helper_path() -> PathBuf {
    if let Ok(path) = std::env::var("ARCBOX_HELPER_PATH") {
        return PathBuf::from(path);
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let helper = dir.join("arcbox-helper");
            if helper.exists() {
                return helper;
            }
        }
    }

    if let Some(home) = dirs::home_dir() {
        let helper = home.join(".arcbox/bin/arcbox-helper");
        if helper.exists() {
            return helper;
        }
    }

    for path in [
        "/usr/local/libexec/arcbox-helper",
        "/usr/local/bin/arcbox-helper",
    ] {
        if Path::new(path).exists() {
            return PathBuf::from(path);
        }
    }

    PathBuf::from("arcbox-helper")
}
