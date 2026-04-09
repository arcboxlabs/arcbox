//! Vsock-based TCP port forwarding for ArcBox VMs.
//!
//! Replaces the virtio-net proxy path (smoltcp → Eth/IP/TCP frame
//! construction → virtio-net descriptor injection) with a direct vsock
//! pipe that carries raw payload bytes with zero header overhead.
//!
//! # Architecture
//!
//! ```text
//! Host TCP client → TcpListener.accept()
//!     → VsockConnector.connect(PORT_FORWARD_VSOCK_PORT)
//!         → [6-byte target header]
//!         → guest agent connects to target
//!         → bidirectional relay (vsock ↔ TCP)
//! ```
//!
//! The host-side [`VsockPortForwarder`] manages TCP listeners and, for
//! each accepted connection, opens a vsock pipe to the guest port
//! forwarding service. A trait [`VsockConnector`] abstracts the
//! platform-specific vsock connection mechanism (HV socketpair, VZ
//! framework, AF_VSOCK).

mod forwarder;
pub mod protocol;

pub use forwarder::{VsockConnector, VsockPortForwarder};
