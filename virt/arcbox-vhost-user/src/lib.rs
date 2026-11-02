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
//! - [`GuestMemory`]: maps the `SET_MEM_TABLE` regions (POSIX shared memory
//!   from `-object memory-backend-shm,share=on`) into one reservation so a
//!   single `GuestMemWriter` — and so `arcbox_virtio_core::SplitQueue` —
//!   covers guest RAM.
//! - [`Vring`]: per-queue state; once started it owns the `SplitQueue`, the
//!   one owner of the avail cursor `GET_VRING_BASE` reports.
//! - The data plane runs on one worker thread per connection that waits on a
//!   kqueue: vring kick pipes and per-queue triggers from the control thread.
//! - [`Backend`]: answers front-end messages on the calling (or a spawned)
//!   thread and drives a [`VhostUserDevice`].
//!
//! ## macOS notifier behaviour (QEMU 10.2.1, verified in source and on a Mac)
//!
//! QEMU builds without eventfd fall back to non-blocking pipes
//! (`util/event_notifier-posix.c`). It sends the **read** end for
//! `SET_VRING_KICK` and writes 8 bytes per guest kick — under TCG the
//! ioeventfd is emulated (`memory_region_dispatch_write_eventfds`); it sends
//! the **write** end for `SET_VRING_CALL` (`event_notifier_get_wfd`) and
//! drains whatever the back-end writes. `SET_VRING_ERR` gets the read end,
//! which a back-end cannot write, so it is accepted and ignored.

#![cfg(target_os = "macos")]

mod backend;
pub mod connection;
mod device;
mod error;
mod event_loop;
mod memory;
pub mod protocol;
mod vring;

pub use backend::{Backend, BackendHandle};
pub use device::{VhostUserDevice, Vrings};
pub use error::{Error, Result};
pub use memory::GuestMemory;
pub use protocol::ProtocolFeatures;
pub use vring::{CallNotifier, Vring};
