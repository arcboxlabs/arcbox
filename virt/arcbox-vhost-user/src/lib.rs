//! # arcbox-vhost-user
//!
//! A vhost-user back-end for macOS: serves ArcBox's own virtio devices to a
//! QEMU front-end (`vhost-user-fs-pci`, `vhost-user-vsock-pci`) that runs the
//! System VM under TCG on Macs without hardware virtualization. QEMU owns the
//! transport (PCI, MSI-X, guest RAM); this crate owns the data plane, so the
//! guest sees the same virtio-fs tags and vsock ports as on the HV and VZ
//! backends.
//!
//! ## Pieces
//!
//! - [`protocol`]: the wire format; [`connection`]: framed messages with
//!   `SCM_RIGHTS` descriptors (marked close-on-exec — macOS has no
//!   `MSG_CMSG_CLOEXEC`).

#![cfg(target_os = "macos")]

pub mod connection;
mod error;
pub mod protocol;

pub use error::{Error, Result};
pub use protocol::ProtocolFeatures;
