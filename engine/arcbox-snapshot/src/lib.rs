//! # arcbox-snapshot
//!
//! Snapshot lineage for `ArcBox` computers: the checkpoint catalog, the
//! copy-on-write rootfs manager the checkpoints are cloned through, and
//! the template catalog that promotes a snapshot into a reusable,
//! versioned base image.
//!
//! This is an engine-layer crate — daemon-free and platform-neutral. It
//! owns durable on-disk state and knows nothing about who boots the VM:
//! the guest-side sandbox manager (`arcbox-computer-runtime`) drives it
//! today, and the future registry client belongs here rather than above it.
//!
//! Copy-on-write (`snapshot_cow`) has two backends, chosen through
//! `CowOptions::backend`. `CowBackend::DeviceMapper` (the default)
//! layers a dm-snapshot over loop devices for Firecracker inside a Linux
//! VM; it only *does* anything on Linux, where `dmsetup` and thin pools
//! exist. `CowBackend::Clonefile` gives each sandbox an APFS `clonefile`
//! copy of its template for sandboxes that run as their own VM on the
//! macOS host; off macOS every setup fails. Both compile everywhere so
//! the layer above can too.

pub mod error;
pub mod snapshot;
pub mod snapshot_cow;
pub mod template_catalog;

#[cfg(test)]
mod test_support;

pub use error::{Result, SnapshotError};
pub use snapshot::{SnapshotCatalog, SnapshotInfo, SnapshotType};
