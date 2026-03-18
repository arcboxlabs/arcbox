//! Best-effort self-setup tasks that the daemon runs during startup.
//!
//! Each function checks if the setup is already in place and, if not, attempts
//! to configure it via `arcbox-helper` (tarpc). Failures are logged as
//! warnings — they never block daemon startup. The canonical setup path
//! is `arcbox install`.

use std::path::Path;

use arcbox_helper::client::Client;

/// Ensures the DNS resolver file is installed for the given domain/port.
///
/// Checks `/etc/resolver/<domain>` first; if absent, asks the helper to
/// install it.
pub async fn ensure_dns_resolver(port: u16, domain: &str) {
    let resolver_path = format!("/etc/resolver/{domain}");
    if Path::new(&resolver_path).exists() {
        tracing::debug!(domain, "DNS resolver already installed");
        return;
    }

    let client = match Client::connect().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "arcbox-helper not reachable, skipping DNS resolver setup");
            return;
        }
    };

    match client.dns_install(domain, port).await {
        Ok(()) => {
            tracing::info!(domain, port, "DNS resolver installed via helper");
        }
        Err(e) => {
            tracing::warn!(
                domain,
                error = %e,
                "DNS resolver install failed (run 'sudo arcbox install' to fix)"
            );
        }
    }
}

/// Ensures the Docker socket symlink exists at `/var/run/docker.sock`.
///
/// Checks if the symlink already points to `docker_sock_path`; if not,
/// asks the helper to create it.
pub async fn ensure_docker_socket(docker_sock_path: &Path) {
    let system_sock = Path::new("/var/run/docker.sock");
    if let Ok(existing) = std::fs::read_link(system_sock) {
        if existing == docker_sock_path {
            tracing::debug!("Docker socket symlink already correct");
            return;
        }
    }

    let client = match Client::connect().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "arcbox-helper not reachable, skipping Docker socket setup");
            return;
        }
    };

    let target_str = docker_sock_path.to_string_lossy();
    match client.socket_link(&target_str).await {
        Ok(()) => {
            tracing::info!(target = %target_str, "Docker socket symlink created via helper");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Docker socket link failed (run 'sudo arcbox install' to fix)"
            );
        }
    }
}
