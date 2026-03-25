//! Best-effort host NFS mount integration for the guest Docker data export.
//!
//! Instead of connecting to the guest's bridge NIC IP (which depends on vmnet
//! or VZNATNetworkDeviceAttachment), we proxy NFS TCP through vsock:
//!
//! ```text
//! mount_nfs localhost:<local_port>:/
//!   → TcpListener on 127.0.0.1
//!   → vsock connect to guest NFS_RELAY_PORT
//!   → guest relay → 127.0.0.1:2049 (nfsd)
//! ```

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use arcbox_constants::ports::NFS_RELAY_PORT;
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime};
use arcbox_protocol::agent::ServiceStatus;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;

const EXPORT_SERVICE: &str = "nfs-export";
const MOUNTD_SERVICE: &str = "rpc.mountd";
const NFSD_SERVICE: &str = "rpc.nfsd";
const READY_STATUS: &str = "ready";
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(20);
const MOUNT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

pub fn spawn(ctx: &DaemonContext) {
    if !ctx.mount_nfs {
        return;
    }

    let runtime = ctx.runtime().clone();
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = reconcile(runtime, &shutdown).await {
            warn!(error = %e, "failed to reconcile host NFS mount");
        }
    });
}

pub fn cleanup(ctx: &DaemonContext) {
    if !ctx.mount_nfs {
        return;
    }

    let Some(mount_path) = resolve_mount_path() else {
        return;
    };

    let Some(info) = current_mount_info(&mount_path) else {
        return;
    };

    if info.fstype != "nfs" {
        return;
    }

    match Command::new("/sbin/umount").arg(&mount_path).status() {
        Ok(status) if status.success() => {
            info!(path = %mount_path.display(), "unmounted ArcBox host NFS mount");
        }
        Ok(status) => {
            warn!(
                path = %mount_path.display(),
                exit_code = status.code().unwrap_or(-1),
                "failed to unmount ArcBox host NFS mount"
            );
        }
        Err(e) => {
            warn!(
                path = %mount_path.display(),
                error = %e,
                "failed to execute umount for ArcBox host NFS mount"
            );
        }
    }
}

async fn reconcile(runtime: Arc<Runtime>, shutdown: &CancellationToken) -> Result<()> {
    let Some(mount_path) = resolve_mount_path() else {
        bail!("could not determine home directory for host NFS mount");
    };

    wait_for_guest_nfs_ready(&runtime, shutdown).await?;

    // Start localhost TCP proxy that forwards to the guest NFS relay via vsock.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    info!(port = local_port, "NFS TCP proxy listening");

    let proxy_runtime = Arc::clone(&runtime);
    let proxy_shutdown = shutdown.clone();
    tokio::spawn(run_proxy(listener, proxy_runtime, proxy_shutdown));

    let expected_source = "127.0.0.1:/".to_string();

    match current_mount_info(&mount_path) {
        Some(info) if info.source == expected_source && info.fstype == "nfs" => {
            debug!(path = %mount_path.display(), source = %expected_source, "host NFS mount already correct");
            return Ok(());
        }
        Some(info) if info.fstype == "nfs" => {
            info!(
                path = %mount_path.display(),
                old_source = %info.source,
                new_source = %expected_source,
                "replacing stale ArcBox host NFS mount"
            );
            unmount(&mount_path)?;
        }
        Some(info) => {
            bail!(
                "mount path {} already occupied by {} ({})",
                mount_path.display(),
                info.source,
                info.fstype
            );
        }
        None => {}
    }

    std::fs::create_dir_all(&mount_path)?;

    let status = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!("vers=4,port={local_port},ro,namedattr"),
            &expected_source,
        ])
        .arg(&mount_path)
        .status()?;

    if !status.success() {
        bail!("mount_nfs exited with {}", status.code().unwrap_or(-1));
    }

    info!(
        path = %mount_path.display(),
        source = %expected_source,
        port = local_port,
        "mounted ArcBox host NFS export via vsock proxy"
    );
    Ok(())
}

/// Accepts TCP connections and relays each to the guest NFS server via vsock.
async fn run_proxy(listener: TcpListener, runtime: Arc<Runtime>, shutdown: CancellationToken) {
    loop {
        let stream = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            result = listener.accept() => match result {
                Ok((stream, _)) => stream,
                Err(e) => {
                    warn!(error = %e, "NFS proxy accept failed");
                    continue;
                }
            }
        };

        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            if let Err(e) = relay_connection(stream, &runtime).await {
                warn!(error = %e, "NFS proxy relay failed");
            }
        });
    }
}

/// Relays a single TCP connection to the guest NFS server via vsock.
async fn relay_connection(mut tcp: tokio::net::TcpStream, runtime: &Runtime) -> Result<()> {
    let name = DEFAULT_MACHINE_NAME.to_string();
    let manager = runtime.machine_manager().clone();

    debug!("NFS proxy: connecting vsock to guest port {NFS_RELAY_PORT}");
    let fd = tokio::task::spawn_blocking(move || manager.connect_vsock_port(&name, NFS_RELAY_PORT))
        .await??;

    info!("NFS proxy: vsock connected, relaying");
    // SAFETY: fd is a valid, newly-opened vsock file descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut vsock = arcbox_docker::proxy::RawFdStream::new(owned)?;

    match copy_bidirectional(&mut tcp, &mut vsock).await {
        Ok((tx, rx)) => debug!(tx, rx, "NFS proxy: relay finished"),
        Err(e) => debug!(error = %e, "NFS proxy: relay error"),
    }
    Ok(())
}

async fn wait_for_guest_nfs_ready(runtime: &Runtime, shutdown: &CancellationToken) -> Result<()> {
    let deadline = tokio::time::Instant::now() + MOUNT_READY_TIMEOUT;
    let mut last_error = "guest NFS export not ready".to_string();

    loop {
        if shutdown.is_cancelled() {
            bail!("daemon shutdown started before NFS mount reconciliation completed");
        }

        if tokio::time::Instant::now() >= deadline {
            bail!("{last_error}");
        }

        match runtime.get_agent(DEFAULT_MACHINE_NAME) {
            Ok(mut agent) => {
                match agent.ensure_runtime(true).await {
                    Ok(_) => {}
                    Err(e) => {
                        last_error = format!("EnsureRuntime failed: {e}");
                        tokio::time::sleep(MOUNT_RETRY_INTERVAL).await;
                        continue;
                    }
                }

                match agent.get_runtime_status().await {
                    Ok(status) if nfs_services_ready(&status.services) => return Ok(()),
                    Ok(status) => {
                        last_error = format!("guest NFS services not ready: {}", status.detail);
                    }
                    Err(e) => {
                        last_error = format!("GetRuntimeStatus failed: {e}");
                    }
                }
            }
            Err(e) => {
                last_error = format!("failed to connect to guest agent: {e}");
            }
        }

        tokio::time::sleep(MOUNT_RETRY_INTERVAL).await;
    }
}

fn nfs_services_ready(services: &[ServiceStatus]) -> bool {
    [EXPORT_SERVICE, MOUNTD_SERVICE, NFSD_SERVICE]
        .into_iter()
        .all(|name| {
            services
                .iter()
                .any(|service| service.name == name && service.status == READY_STATUS)
        })
}

fn resolve_mount_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| resolve_mount_path_from_home(&home))
}

fn resolve_mount_path_from_home(home: &Path) -> PathBuf {
    home.join("ArcBox")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountInfo {
    source: String,
    fstype: String,
}

fn current_mount_info(path: &Path) -> Option<MountInfo> {
    let output = Command::new("/sbin/mount").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let target = path.to_string_lossy();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((mountpoint, info)) = parse_mount_line(line) else {
            continue;
        };
        if mountpoint == target {
            return Some(info);
        }
    }

    None
}

fn parse_mount_line(line: &str) -> Option<(&str, MountInfo)> {
    let (source, rest) = line.split_once(" on ")?;
    let (mountpoint, suffix) = rest.split_once(" (")?;
    let fstype = suffix
        .split([',', ')'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    Some((
        mountpoint,
        MountInfo {
            source: source.to_string(),
            fstype,
        },
    ))
}

fn unmount(path: &Path) -> Result<()> {
    let status = Command::new("/sbin/umount").arg(path).status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("umount exited with {}", status.code().unwrap_or(-1))
    }
}

#[cfg(test)]
mod tests {
    use super::{MountInfo, nfs_services_ready, parse_mount_line, resolve_mount_path_from_home};
    use arcbox_protocol::agent::ServiceStatus;
    use std::path::{Path, PathBuf};

    #[test]
    fn resolve_mount_path_uses_arcbox_home_dir() {
        let path = resolve_mount_path_from_home(Path::new("/Users/tester"));
        assert_eq!(path, PathBuf::from("/Users/tester/ArcBox"));
    }

    #[test]
    fn parse_mount_line_parses_nfs_entry() {
        let line = "10.0.0.2:/ on /Users/tester/ArcBox (nfs, nodev, nosuid, mounted by tester)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, "/Users/tester/ArcBox");
        assert_eq!(
            info,
            MountInfo {
                source: "10.0.0.2:/".to_string(),
                fstype: "nfs".to_string()
            }
        );
    }

    #[test]
    fn parse_mount_line_parses_localhost_nfs() {
        let line = "localhost:/ on /Users/tester/ArcBox (nfs, nodev, nosuid, mounted by tester)";
        let (mountpoint, info) = parse_mount_line(line).expect("line should parse");
        assert_eq!(mountpoint, "/Users/tester/ArcBox");
        assert_eq!(
            info,
            MountInfo {
                source: "localhost:/".to_string(),
                fstype: "nfs".to_string()
            }
        );
    }

    #[test]
    fn nfs_services_ready_requires_all_three_services() {
        let services = vec![
            ServiceStatus {
                name: "nfs-export".to_string(),
                status: "ready".to_string(),
                detail: String::new(),
            },
            ServiceStatus {
                name: "rpc.mountd".to_string(),
                status: "ready".to_string(),
                detail: String::new(),
            },
            ServiceStatus {
                name: "rpc.nfsd".to_string(),
                status: "ready".to_string(),
                detail: String::new(),
            },
        ];

        assert!(nfs_services_ready(&services));
    }
}
