//! In-kernel NFSv4 server management.
//!
//! Starts the kernel NFS server via procfs, exporting the Btrfs data drive
//! at `/run/arcbox/data` as the v4 pseudo-root (fsid=0). Degrades gracefully
//! if the kernel was built without `CONFIG_NFSD`.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use arcbox_constants::ports::NFS_PORT;
use arcbox_protocol::agent::{NfsEnsureRequest, NfsEnsureResponse, RuntimeEnsureResponse};

use crate::agent::ensure_runtime::{self, RuntimeGuard, STATUS_FAILED, STATUS_STARTED};
use crate::rpc::RpcResponse;

/// Returns the global NFS `RuntimeGuard` singleton.
fn nfs_guard() -> &'static RuntimeGuard {
    static GUARD: OnceLock<RuntimeGuard> = OnceLock::new();
    GUARD.get_or_init(RuntimeGuard::new)
}

/// Maps `RuntimeEnsureResponse` (used by the generic state machine) into
/// the NFS-specific protobuf response.
fn map_response(r: &RuntimeEnsureResponse) -> NfsEnsureResponse {
    NfsEnsureResponse {
        ready: r.ready,
        message: r.message.clone(),
        status: r.status.clone(),
    }
}

/// Idempotent, concurrency-safe EnsureNfs handler.
pub async fn handle_ensure_nfs(req: NfsEnsureRequest) -> RpcResponse {
    let resp = ensure_runtime::ensure_runtime(
        nfs_guard(),
        req.start_if_needed,
        do_nfs_start(),
        do_nfs_probe(),
    )
    .await;

    RpcResponse::NfsEnsure(map_response(&resp))
}

/// The Btrfs data drive mount point used as the NFS export root.
const NFS_EXPORT_PATH: &str = "/run/arcbox/data";

/// Performs the actual NFS server start sequence (called only by the driver).
async fn do_nfs_start() -> RuntimeEnsureResponse {
    // Ensure the data drive is mounted (idempotent).
    if let Err(e) = super::agent::ensure_data_mount() {
        return failed(&format!("data mount failed: {e}"));
    }

    // Try loading the nfsd module (may already be built-in).
    let _ = std::process::Command::new("/sbin/modprobe")
        .arg("nfsd")
        .status();

    if !Path::new("/proc/fs/nfsd").exists() {
        return failed("kernel NFSD not available (missing /proc/fs/nfsd)");
    }

    // Mount the nfsd pseudo-filesystem if not already mounted.
    if !crate::mount::is_mounted("/proc/fs/nfsd") {
        let status = std::process::Command::new("/bin/busybox")
            .args(["mount", "-t", "nfsd", "nfsd", "/proc/fs/nfsd"])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return failed(&format!(
                    "mount -t nfsd failed (exit={})",
                    s.code().unwrap_or(-1)
                ));
            }
            Err(e) => return failed(&format!("mount exec failed: {e}")),
        }
    }

    // Configure NFSv4-only: disable v2/v3, enable v4.0 only.
    if let Err(e) = std::fs::write("/proc/fs/nfsd/versions", "-2 -3 +4 -4.1 -4.2") {
        return failed(&format!("failed to set NFS versions: {e}"));
    }

    // Write /etc/exports for the data drive (fsid=0 makes it the v4 pseudo-root).
    if let Err(e) = std::fs::create_dir_all("/etc") {
        return failed(&format!("mkdir /etc failed: {e}"));
    }
    let export_line = format!(
        "{} *(rw,no_root_squash,fsid=0,no_subtree_check,sec=sys)\n",
        NFS_EXPORT_PATH
    );
    if let Err(e) = std::fs::write("/etc/exports", &export_line) {
        return failed(&format!("failed to write /etc/exports: {e}"));
    }

    // Apply exports. Try exportfs first (available if NFS userspace tools are
    // in the rootfs), otherwise fall back to starting threads directly — the
    // kernel will read /etc/exports on thread start for simple configurations.
    if Path::new("/usr/sbin/exportfs").exists() {
        let _ = std::process::Command::new("/usr/sbin/exportfs")
            .args(["-ra"])
            .status();
    }

    // Set up NFS transport: listen on TCP port 2049.
    if let Err(e) = std::fs::write("/proc/fs/nfsd/portlist", "+tcp 2049") {
        return failed(&format!("failed to configure NFS transport: {e}"));
    }

    // Start NFS server threads.
    if let Err(e) = std::fs::write("/proc/fs/nfsd/threads", "8") {
        return failed(&format!("failed to start NFS threads: {e}"));
    }

    // Poll TCP 127.0.0.1:2049 up to 5 seconds.
    for _ in 0..50 {
        if probe_nfs_tcp().await {
            return RuntimeEnsureResponse {
                ready: true,
                endpoint: format!("tcp:{NFS_PORT}"),
                message: "nfsd started".to_string(),
                status: STATUS_STARTED.to_string(),
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    failed("nfsd started but TCP port 2049 not reachable within 5s")
}

/// Probes NFS status without attempting to start.
async fn do_nfs_probe() -> RuntimeEnsureResponse {
    if probe_nfs_tcp().await {
        RuntimeEnsureResponse {
            ready: true,
            endpoint: format!("tcp:{NFS_PORT}"),
            message: "nfsd running".to_string(),
            status: ensure_runtime::STATUS_REUSED.to_string(),
        }
    } else {
        failed("nfsd not running (TCP port 2049 not reachable)")
    }
}

/// TCP connect probe to 127.0.0.1:2049.
async fn probe_nfs_tcp() -> bool {
    tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect(("127.0.0.1", NFS_PORT)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

fn failed(message: &str) -> RuntimeEnsureResponse {
    tracing::warn!("NFS: {}", message);
    RuntimeEnsureResponse {
        ready: false,
        endpoint: String::new(),
        message: message.to_string(),
        status: STATUS_FAILED.to_string(),
    }
}
