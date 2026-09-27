//! Disk-space reclamation: trims the guest's data filesystems so the host's
//! sparse image releases the blocks they freed.
//!
//! Both guests mount their Btrfs data disk with `discard=async`, which
//! returns *fully* freed extents to the host within a couple of minutes on
//! its own. What it never returns is the free space inside a block group that
//! is still partly used, and it drops the queue on unmount — so a full
//! `FITRIM` is the path that gets everything back. The host drives it: on
//! demand through `abctl disk compact`, and on its own schedule (see
//! `arcbox-daemon`'s `disk_reclaim`). Neither guest runs a trim loop of its
//! own, and neither ships an `fstrim` binary — the ioctl is issued directly.
//!
//! The System VM trims the Btrfs data volume (one trim covers every
//! subvolume) and the ext4 metadata volume. A distro machine has no mount
//! point that reaches its Btrfs data disk — the shim mounts it under a
//! tmpfs that is gone after `pivot_root`, leaving only the overlay root, and
//! overlayfs refuses `FITRIM` — so the agent mounts the device again in a
//! private mount namespace for the duration of the trim.

use std::os::fd::AsRawFd as _;
use std::path::Path;

use arcbox_connect::v1::DiskTrimResponse;
use arcbox_constants::cmdline::MACHINE_DATA_KEY;

use super::btrfs::BTRFS_TEMP_MOUNT;
use super::cmdline::cmdline_value;
use super::metadata_volume::METADATA_MOUNT;
use crate::agent::Guest;
use crate::rpc::{ErrorResponse, RpcResponse};

/// Where a distro machine's data disk is mounted for the trim. The mount
/// exists only inside the trim's private mount namespace; the machine sees
/// just the empty directory on its `/run`.
const MACHINE_TRIM_MOUNT: &str = "/run/arcbox/trim";

/// `struct fstrim_range` from `<linux/fs.h>`, the `FITRIM` argument: the
/// kernel reads the range and writes back the bytes it discarded in `len`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct FstrimRange {
    start: u64,
    len: u64,
    minlen: u64,
}

// FITRIM = _IOWR('X', 121, struct fstrim_range)
nix::ioctl_readwrite!(fitrim, b'X', 121, FstrimRange);

/// Handles a `DiskTrim` RPC: trims every data filesystem of the guest kind
/// the agent serves. A filesystem that refuses the trim fails the request
/// (a generic error response, so the daemon's `disk_trim()` surfaces an
/// `Err`), with the per-filesystem outcome in the message.
///
/// The trim runs on a thread of its own rather than tokio's blocking pool:
/// the machine path moves its thread into a private mount namespace, and a
/// pool thread outlives the closure and goes on to run other blocking work,
/// which would then see the wrong mounts.
pub(super) async fn handle_disk_trim(guest: Guest) -> RpcResponse {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("disk-trim".into())
        .spawn(move || {
            // The receiver only goes away if the RPC was cancelled.
            let _ = tx.send(trim_guest(guest));
        });
    let outcome = match spawned {
        Ok(_) => rx
            .await
            .map_err(|_| "disk trim thread exited without a result".to_owned()),
        Err(e) => Err(format!("spawn disk trim thread: {e}")),
    };
    match outcome.and_then(|trimmed| trimmed) {
        Ok(trimmed) => RpcResponse::DiskTrim(DiskTrimResponse {
            result: trimmed.summary(),
            bytes_trimmed: trimmed.bytes,
            ..Default::default()
        }),
        Err(reason) => RpcResponse::Error(ErrorResponse::new(
            500,
            format!("disk trim failed: {reason}"),
        )),
    }
}

/// Bytes each trimmed filesystem discarded, in trim order.
struct Trimmed {
    per_fs: Vec<(String, u64)>,
    bytes: u64,
}

impl Trimmed {
    fn summary(&self) -> String {
        self.per_fs
            .iter()
            .map(|(name, bytes)| format!("{name}: {bytes} bytes trimmed"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn trim_guest(guest: Guest) -> Result<Trimmed, String> {
    match guest {
        Guest::SystemVm => trim_mounts(&[BTRFS_TEMP_MOUNT, METADATA_MOUNT]),
        Guest::DistroMachine => trim_machine_data_disk(),
    }
}

/// Trims every mount point in `mounts`; an unmounted one is skipped, since
/// the metadata volume is optional (older daemons attach no third disk).
fn trim_mounts(mounts: &[&str]) -> Result<Trimmed, String> {
    let mut per_fs = Vec::new();
    for mount in mounts {
        if !crate::mount::is_mounted(mount) {
            tracing::debug!(mount, "not mounted; nothing to trim");
            continue;
        }
        let bytes = trim_mount(Path::new(mount)).map_err(|e| format!("{mount}: {e}"))?;
        tracing::info!(mount, bytes, "trimmed");
        per_fs.push(((*mount).to_owned(), bytes));
    }
    let bytes = per_fs.iter().map(|(_, b)| *b).sum();
    Ok(Trimmed { per_fs, bytes })
}

/// Trims a distro machine's Btrfs data disk. The device comes from the shim's
/// cmdline contract; it is mounted afresh in a private mount namespace (the
/// kernel lets one filesystem be mounted at several places), trimmed, and the
/// mount disappears with the namespace when this thread returns.
fn trim_machine_data_disk() -> Result<Trimmed, String> {
    let device = cmdline_value(MACHINE_DATA_KEY)
        .ok_or_else(|| format!("{MACHINE_DATA_KEY} missing from the kernel cmdline"))?;
    enter_private_mount_namespace()?;
    std::fs::create_dir_all(MACHINE_TRIM_MOUNT)
        .map_err(|e| format!("create {MACHINE_TRIM_MOUNT}: {e}"))?;
    nix::mount::mount(
        Some(device.as_str()),
        MACHINE_TRIM_MOUNT,
        Some("btrfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .map_err(|e| format!("mount {device} on {MACHINE_TRIM_MOUNT}: {e}"))?;
    let bytes = trim_mount(Path::new(MACHINE_TRIM_MOUNT)).map_err(|e| format!("{device}: {e}"))?;
    tracing::info!(device, bytes, "trimmed the machine data disk");
    Ok(Trimmed {
        per_fs: vec![(device, bytes)],
        bytes,
    })
}

/// Moves the calling thread into its own mount namespace, with propagation
/// off so the trim mount is invisible to the machine and torn down when the
/// namespace's last user (this thread) exits. `unshare(CLONE_NEWNS)` is
/// per-thread (and implies `CLONE_FS`), which is why the trim owns its
/// thread — see [`handle_disk_trim`].
fn enter_private_mount_namespace() -> Result<(), String> {
    use nix::mount::MsFlags;
    use nix::sched::{CloneFlags, unshare};

    unshare(CloneFlags::CLONE_NEWNS).map_err(|e| format!("unshare mount namespace: {e}"))?;
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| format!("make mounts private: {e}"))
}

/// Issues `FITRIM` over the whole filesystem at `mount` and returns the
/// bytes the filesystem discarded.
fn trim_mount(mount: &Path) -> std::io::Result<u64> {
    let dir = std::fs::File::open(mount)?;
    let mut range = FstrimRange {
        start: 0,
        len: u64::MAX,
        minlen: 0,
    };
    // SAFETY: `dir` is an open directory on the filesystem to trim and
    // `range` is a live `fstrim_range` the ioctl reads and writes back.
    unsafe { fitrim(dir.as_raw_fd(), &raw mut range) }?;
    Ok(range.len)
}

#[cfg(test)]
mod tests {
    use super::Trimmed;

    #[test]
    fn summary_lists_each_filesystem() {
        let trimmed = Trimmed {
            per_fs: vec![
                ("/run/arcbox/data".into(), 4096),
                ("/run/arcbox/metadata".into(), 0),
            ],
            bytes: 4096,
        };
        assert_eq!(
            trimmed.summary(),
            "/run/arcbox/data: 4096 bytes trimmed; /run/arcbox/metadata: 0 bytes trimmed"
        );
    }
}
