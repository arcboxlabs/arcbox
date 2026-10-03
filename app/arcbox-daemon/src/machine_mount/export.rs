//! How a machine export is mounted and unmounted: the NFSv3 options, the
//! shape such a mount has in the mount table, and the host addresses the
//! export must admit.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::{debug, warn};

use crate::host_mount::{self, MountInfo};

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

/// The shape of a machine export mount: NFS from the root of a server
/// named by a non-loopback IPv4 address — a machine's bridge address, never
/// the loopback proxy the docker export mounts through. Checked only under
/// the mount root, which is this daemon's, so it cannot mistake a user's
/// mount for its own.
pub(super) fn is_machine_export(info: &MountInfo) -> bool {
    info.fstype == "nfs"
        && info
            .source
            .strip_suffix(":/")
            .and_then(|host| host.parse::<Ipv4Addr>().ok())
            .is_some_and(|host| !host.is_loopback())
}

/// The host's own IPv4 addresses on the network `bridge` belongs to: the
/// addresses the machine will see connections from, and so the only peers
/// its export admits.
pub(super) fn host_addresses_on_link(bridge: Ipv4Addr) -> Vec<IpAddr> {
    let interfaces = match nix::ifaddrs::getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(e) => {
            warn!(error = %e, "getifaddrs failed; the machine export has no peer to admit");
            return Vec::new();
        }
    };
    interfaces
        .filter_map(|interface| {
            let address = interface.address?.as_sockaddr_in()?.ip();
            let netmask = interface.netmask?.as_sockaddr_in()?.ip();
            same_link(address, bridge, netmask).then_some(IpAddr::V4(address))
        })
        .collect()
}

fn same_link(a: Ipv4Addr, b: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    let mask = u32::from(netmask);
    u32::from(a) & mask == u32::from(b) & mask
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

    #[test]
    fn only_an_nfs_root_from_an_address_is_a_machine_export() {
        let ours = MountInfo {
            source: "192.168.64.7:/".to_string(),
            fstype: "nfs".to_string(),
        };
        assert!(is_machine_export(&ours));
        for (source, fstype) in [
            ("ArcBox:/", "nfs"),
            ("127.0.0.1:/", "nfs"),
            ("fileserver:/export", "nfs"),
            ("192.168.64.7:/srv", "nfs"),
            ("//user@server/share", "smbfs"),
        ] {
            let other = MountInfo {
                source: source.to_string(),
                fstype: fstype.to_string(),
            };
            assert!(!is_machine_export(&other), "{source} ({fstype})");
        }
    }

    #[test]
    fn a_link_is_shared_under_the_interface_netmask() {
        let mask: Ipv4Addr = "255.255.255.0".parse().unwrap();
        let host: Ipv4Addr = "192.168.64.1".parse().unwrap();
        assert!(same_link(host, "192.168.64.7".parse().unwrap(), mask));
        assert!(!same_link(host, "192.168.65.7".parse().unwrap(), mask));
        assert!(!same_link(host, "10.0.2.15".parse().unwrap(), mask));
    }

    #[test]
    fn loopback_is_never_on_a_machines_link() {
        // The bridge network is a vmnet subnet; whatever interfaces this
        // host has, 127.0.0.1 is not among the peers a machine would see.
        let peers = host_addresses_on_link("192.168.64.7".parse().unwrap());
        assert!(!peers.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }
}
