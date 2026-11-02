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
//!   kqueue: vring kick pipes, per-queue triggers from the control thread,
//!   and anything the device registers through a [`Registrar`].
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
//!
//! ## Serving a device
//!
//! ```text
//! socketpair(AF_UNIX, SOCK_STREAM) → (ours, theirs)
//! qemu ... -object memory-backend-shm,id=mem,size=<RAM>,share=on \
//!          -machine virt,memory-backend=mem \
//!          -chardev socket,id=fs0,fd=<theirs> \
//!          -device vhost-user-fs-pci,chardev=fs0,tag=arcbox
//! Backend::new("vhost-fs-arcbox", device).spawn(ours)
//! ```
//!
//! QEMU connects at start-up and blocks in `vhost_dev_init` until the
//! back-end answers, so spawn the back-end before (or right after) QEMU.
//! Guest RAM must come from a shareable backend; ordinary RAM has no
//! descriptor and vhost-user refuses to start. One connection serves one
//! device; when QEMU exits the back-end resets the device and returns it.
//!
//! ## Writing a device
//!
//! Implement [`VhostUserDevice`]. Offer `VIRTIO_F_VERSION_1` plus the device
//! bits; `VIRTIO_RING_F_EVENT_IDX` is supported. In
//! [`VhostUserDevice::process_queue`], take the ring from [`Vrings::get_mut`]
//! and either call [`Vring::process_avail`] (pop, handle, publish, re-arm
//! EVENT_IDX, interrupt once) or drive [`Vring::queue_mut`] directly.
//!
//! **virtio-fs** is [`fs`]: one [`fs::FsShare`] per tag (`arcbox`, `users`,
//! `private` for the System VM), each spawned on its own socketpair.
//!
//! **vsock** (one back-end, ports dialled from the host): two queues, RX and
//! TX — QEMU keeps the event queue (`vhost-vsock-common.c`: `vhost_vqs[2]`),
//! so transport-reset events are not the back-end's. Offer
//! [`ProtocolFeatures::CONFIG`] and return the guest CID as the 8-byte
//! little-endian config space; `vhost-user-vsock-pci` reads it at realize
//! time and refuses a back-end without it. Keep the HV pieces: `VsockConnectionManager` for
//! connection state and `connect_vsock(port)` socketpairs. Replace the
//! `vsock-io` thread and its doorbell pipe with this worker: create a
//! [`Waker`] in `activate` for the doorbell, register each connection's
//! socketpair end with [`Registrar::watch_readable`] (edge-triggered when RX
//! may stall on missing guest buffers), and run RX injection from
//! `handle_event`. The injection code takes a `QueueConfig` and a
//! `DeviceCtx`: build them from [`Vring::queue_config`], [`Vring::memory`]
//! and [`Vring::call_notifier`], and hand the avail cursor back with
//! [`Vring::set_last_avail_idx`] so `GET_VRING_BASE` stays right.

#![cfg(target_os = "macos")]

mod backend;
pub mod connection;
mod device;
mod error;
mod event_loop;
pub mod fs;
mod memory;
pub mod protocol;
mod vring;

pub use backend::{Backend, BackendHandle};
pub use device::{VhostUserDevice, Vrings};
pub use error::{Error, Result};
pub use event_loop::{MAX_DEVICE_TOKEN, Registrar, Trigger, Waker};
pub use memory::GuestMemory;
pub use protocol::ProtocolFeatures;
pub use vring::{CallNotifier, Vring};
