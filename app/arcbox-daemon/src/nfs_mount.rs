//! Best-effort host NFS mount integration for the guest Docker data export.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Result, bail};
use arcbox_core::{DEFAULT_MACHINE_NAME, Runtime};
use arcbox_protocol::agent::ServiceStatus;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::context::DaemonContext;

const EXPORT_SERVICE: &str = "nfs-export";
const MOUNTD_SERVICE: &str = "rpc.mountd";
const NFSD_SERVICE: &str = "rpc.nfsd";
const READY_STATUS: &str = "ready";
const NFS_PORT: u16 = 2049;
const MOUNT_READY_TIMEOUT: Duration = Duration::from_secs(20);
const MOUNT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

pub fn spawn(ctx: &DaemonContext) {
    if !ctx.mount_nfs {
        return;
    }

    let runtime = ctx.runtime().clone();
    let shutdown = ctx.shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = reconcile(&runtime, &shutdown).await {
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

async fn reconcile(runtime: &Runtime, shutdown: &CancellationToken) -> Result<()> {
    let Some(mount_path) = resolve_mount_path() else {
        bail!("could not determine home directory for host NFS mount");
    };

    let guest_ip = wait_for_guest_nfs_ready(runtime, shutdown).await?;
    let expected_source = format!("{guest_ip}:/");

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
            &format!("vers=4,port={NFS_PORT},ro,namedattr"),
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
        "mounted ArcBox host NFS export"
    );
    Ok(())
}

async fn wait_for_guest_nfs_ready(
    runtime: &Runtime,
    shutdown: &CancellationToken,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + MOUNT_READY_TIMEOUT;
    let mut last_error = "guest NFS export not ready".to_string();

    loop {
        if shutdown.is_cancelled() {
            bail!("daemon shutdown started before NFS mount reconciliation completed");
        }

        if tokio::time::Instant::now() >= deadline {
            bail!("{last_error}");
        }

        let guest_ip = runtime
            .machine_manager()
            .get(DEFAULT_MACHINE_NAME)
            .and_then(|machine| machine.ip_address)
            .filter(|ip| !ip.is_empty());

        match guest_ip {
            Some(guest_ip) => match runtime.get_agent(DEFAULT_MACHINE_NAME) {
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
                        Ok(status) if nfs_services_ready(&status.services) => return Ok(guest_ip),
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
            },
            None => {
                last_error = "default machine has no routable IP yet".to_string();
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
