//! Filesystem mounting in guest.
//!
//! This module is only functional on Linux as it runs inside the guest VM.

use anyhow::Result;
use arcbox_constants::virtiofs::{
    MOUNT_ARCBOX, MOUNT_PRIVATE, MOUNT_USERS, TAG_ARCBOX, TAG_PRIVATE, TAG_USERS,
};

/// Mount a filesystem.
#[cfg(target_os = "linux")]
pub fn mount_fs(source: &str, target: &str, fstype: &str, options: &[String]) -> Result<()> {
    use nix::mount::{MsFlags, mount};
    use std::path::Path;

    tracing::info!("Mounting {} on {} (type: {})", source, target, fstype);

    // Create mount point if it doesn't exist
    std::fs::create_dir_all(target)?;

    let data: Option<String> = if options.is_empty() {
        None
    } else {
        Some(options.join(","))
    };

    mount(
        Some(source),
        Path::new(target),
        Some(fstype),
        MsFlags::empty(),
        data.as_deref(),
    )?;

    Ok(())
}

/// Mount a filesystem (stub for non-Linux platforms).
#[cfg(not(target_os = "linux"))]
pub fn mount_fs(source: &str, target: &str, fstype: &str, _options: &[String]) -> Result<()> {
    tracing::warn!(
        "mount_fs is only supported on Linux (called with source={}, target={}, fstype={})",
        source,
        target,
        fstype
    );
    anyhow::bail!("mount_fs is only supported on Linux")
}

/// Unmount a filesystem.
#[cfg(target_os = "linux")]
pub fn unmount_fs(target: &str) -> Result<()> {
    tracing::info!("Unmounting {}", target);
    nix::mount::umount(target)?;
    Ok(())
}

/// Unmount a filesystem (stub for non-Linux platforms).
#[cfg(not(target_os = "linux"))]
pub fn unmount_fs(target: &str) -> Result<()> {
    tracing::warn!("unmount_fs is only supported on Linux (target={})", target);
    anyhow::bail!("unmount_fs is only supported on Linux")
}

/// Mount virtiofs share with default options (cache=auto).
pub fn mount_virtiofs(tag: &str, mountpoint: &str) -> Result<()> {
    mount_fs(tag, mountpoint, "virtiofs", &[])
}

/// Mount virtiofs share with `cache=always`.
///
/// Tells the guest kernel to aggressively cache file data and metadata,
/// skipping revalidation round-trips to the host. Only safe for shares
/// whose contents do not change on the host while the VM is running
/// (e.g. the `/arcbox` runtime directory).
pub fn mount_virtiofs_cached(tag: &str, mountpoint: &str) -> Result<()> {
    mount_fs(tag, mountpoint, "virtiofs", &["cache=always".to_string()])
}

/// Checks if a path is already mounted.
#[cfg(target_os = "linux")]
pub fn is_mounted(path: &str) -> bool {
    use std::fs;
    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        content.lines().any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.get(1).is_some_and(|&p| p == path)
        })
    } else {
        false
    }
}

#[cfg(not(target_os = "linux"))]
pub fn is_mounted(_path: &str) -> bool {
    false
}

/// Mount all standard VirtioFS shares if not already mounted.
///
/// This mounts:
/// - "arcbox" tag -> /arcbox (data directory)
/// - "users" tag -> /Users (macOS /Users, bind-mounted to original path)
pub fn mount_standard_shares() {
    // The /arcbox share may already be mounted by the trampoline (without
    // cache options). Remount with cache=always so the guest kernel
    // aggressively caches runtime binaries (dockerd, containerd, etc.)
    // that don't change while the VM is running.
    if is_mounted(MOUNT_ARCBOX) {
        remount_with_cache(MOUNT_ARCBOX);
    } else if let Err(e) = mount_virtiofs_cached(TAG_ARCBOX, MOUNT_ARCBOX) {
        tracing::warn!("Failed to mount arcbox share: {}", e);
    } else {
        tracing::info!("Mounted arcbox share at {} (cache=always)", MOUNT_ARCBOX);
    }

    // Mount /private share for macOS symlink targets (/tmp → /private/tmp, etc.).
    // The Docker API proxy rewrites bind-mount paths so they resolve here
    // instead of hitting the guest's isolated tmpfs.
    if !is_mounted(MOUNT_PRIVATE) {
        if let Err(e) = mount_virtiofs(TAG_PRIVATE, MOUNT_PRIVATE) {
            tracing::debug!("Private share not available: {}", e);
        } else {
            tracing::info!("Mounted private share at {}", MOUNT_PRIVATE);
        }
    }

    // Mount /Users share for transparent macOS path support.
    // This allows `docker run -v /Users/foo/project:/app` to work directly
    // because /Users exists in the guest at the same path as on the host.
    // Uses default cache=auto since host files can change at any time.
    if !is_mounted(MOUNT_USERS) {
        if let Err(e) = mount_virtiofs(TAG_USERS, MOUNT_USERS) {
            tracing::debug!("Users share not available: {}", e);
        } else {
            tracing::info!("Mounted users share at {}", MOUNT_USERS);
        }
    }
}

/// Remount an existing VirtioFS mount with `cache=always`.
#[cfg(target_os = "linux")]
fn remount_with_cache(mountpoint: &str) {
    use nix::mount::{MsFlags, mount};
    use std::path::Path;

    match mount(
        None::<&str>,
        Path::new(mountpoint),
        None::<&str>,
        MsFlags::MS_REMOUNT,
        Some("cache=always"),
    ) {
        Ok(()) => {
            tracing::info!(mountpoint, "remounted with cache=always");
        }
        Err(e) => {
            tracing::debug!(mountpoint, error = %e, "remount with cache=always failed (non-fatal)");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn remount_with_cache(_mountpoint: &str) {}
