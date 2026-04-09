//! Guest memory RX injection engine for VirtIO-net.
//!
//! Receives raw Ethernet frames via a crossbeam channel and injects
//! them into a guest virtio-net RX queue by writing directly to
//! guest memory (mmap'd region). Runs on a dedicated OS thread,
//! completely independent of the tokio async runtime.

pub mod guest_mem;
pub mod inject;
pub mod irq;
pub mod notify;
pub mod queue;
