//! Machine export mount options and bounded mount retries.
//! The mount table identifies these exports through `MountInfo::is_machine_export`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::debug;

use crate::host_mount;

/// Budget for mounting one machine. The export answers before the RPC
/// returns, so the retries only cover the NFS client's first connection.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);
const MOUNT_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Mounts the export at `address:port` on `mount_path`, read-write.
pub(super) async fn mount(address: &str, port: u16, mount_path: &Path) -> Result<()> {
    mount_with_retry(
        &render_mount_opts(port),
        &format!("{address}:/"),
        mount_path,
    )
    .await
}

/// Unmounts a machine export, forcing when the plain unmount fails: once
/// the machine's VM is gone, the client's outstanding requests never
/// complete and only `umount -f` abandons them.
pub(super) async fn release(path: &Path) -> Result<()> {
    match host_mount::unmount(path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            debug!(path = %path.display(), error = %e, "umount failed; forcing");
            host_mount::unmount_force(path).await
        }
    }
}

/// Retries `mount_nfs` until it succeeds or [`MOUNT_TIMEOUT`] passes.
async fn mount_with_retry(opts: &str, source: &str, mount_path: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + MOUNT_TIMEOUT;
    loop {
        match host_mount::mount_nfs(opts, source, mount_path).await {
            Ok(()) => return Ok(()),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                bail!("mount_nfs did not succeed within {MOUNT_TIMEOUT:?}: {e}");
            }
            Err(e) => debug!(error = %e, "mount_nfs attempt failed, retrying"),
        }
        tokio::time::sleep(MOUNT_RETRY_INTERVAL).await;
    }
}

/// Read-write NFSv3 mount options for a machine export on `port`.
///
/// - `vers=3,tcp`, `port`/`mountport`: the server speaks NFSv3 over TCP and
///   serves the MOUNT and NFS protocols on one port, so no portmapper is
///   consulted.
/// - `locallocks`: the server has no lock manager; locks are kept on the
///   client, where every tool that takes one still gets its lock.
/// - `nfc`: names leave the Mac in NFC, the form Linux filesystems store.
/// - `rdirplus`: attributes come with directory entries.
/// - `actimeo=10`: a modest attribute cache; a change made inside the
///   machine shows on the Mac within ten seconds.
/// - `deadtimeout=60`: a machine that vanished fails I/O after 60 s instead
///   of hanging Finder forever. The stop path unmounts before that; this
///   covers a crash.
/// - `retrycnt=0`: one connection attempt with the client's quick timeout;
///   the export was answering when the RPC returned, and [`mount_with_retry`]
///   is the retry.
fn render_mount_opts(port: u16) -> String {
    format!(
        "rw,vers=3,tcp,port={port},mountport={port},locallocks,nfc,rdirplus,actimeo=10,deadtimeout=60,retrycnt=0"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_opts_are_read_write_v3_on_the_export_port() {
        let opts = render_mount_opts(51234);
        assert!(opts.starts_with("rw,"));
        assert!(opts.contains("vers=3"));
        assert!(opts.contains("port=51234,mountport=51234"));
        // The server has no lock manager; a `nolocks` mount would fail every
        // tool that takes a lock, `locallocks` keeps them on the client.
        assert!(opts.contains("locallocks"));
        assert!(!opts.contains("nolocks"));
        // Guest uids are mapped, not hidden.
        assert!(!opts.contains("noowners"));
    }
}
