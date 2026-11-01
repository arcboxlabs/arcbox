//! `arcbox-hv-driver` — the Hypervisor.framework adapter for the VM driver
//! port, one VMM process per VM.
//!
//! Hypervisor.framework allows one VM per process, and the daemon already
//! holds the System VM, so every VM this driver runs lives in its own
//! `arcbox-hv-vmm` process (ADR 0010, "Process model"). The process runs
//! `arcbox_vmm::hv_driver::HvMachineDriver` on itself and serves the port
//! to this driver over a Unix socket in the VM's runtime dir.
//!
//! ```text
//! daemon                                   arcbox-hv-vmm (one per VM)
//! HvDriver ── spawn ──────────────────────▶ serve::run
//!   HvPrepared / HvHandle                     HvMachineDriver ─▶ Vmm
//!     Control ── request ─┐                     (render, checkpoint,
//!     VmmProcess ◀─ Watch ┴── vmm.sock ──▶       restore, vsock, ...)
//! ```
//!
//! - [`driver`] — [`HvDriver`]: `VmDriver`, `Prepare` (spawn without a
//!   VM) and `Adopt` (find a process again after a daemon restart).
//! - `process` — the process as the driver sees it: exit, kill, events.
//! - `prepared`, `handle` — the port's `PreparedVm` and `VmHandle`, each a
//!   request on the control socket. vsock connections and the console pipe
//!   arrive as descriptors passed over it.
//! - `proto`, `wire`, `control` — the control protocol, its framing with
//!   `SCM_RIGHTS`, and one request with a deadline.
//! - [`serve`] — the process side, run by the `arcbox-hv-vmm` binary.
//!
//! The VM's life is the process's: a guest power-off ends the process with
//! the guest's code, and a kill is `SIGKILL` to it. A process outlives the
//! daemon that spawned it until its handle is dropped without a detach.
//!
//! The crate is empty on other platforms.

#![cfg(target_os = "macos")]

mod control;
pub mod driver;
mod handle;
mod prepared;
mod process;
mod proto;
#[doc(hidden)]
pub mod serve;
mod wire;

use std::os::fd::{AsRawFd, OwnedFd};

use arcbox_vm_driver::{CheckpointImage, CheckpointKind, Error, Result};
pub use driver::HvDriver;

/// The driver's name, as `VmDriver::name` and `VmRecord::driver` report it.
pub const NAME: &str = arcbox_vmm::hv_driver::NAME;

/// Refuses an image this driver did not write.
fn own_format(image: &CheckpointImage) -> Result<()> {
    if image.format.as_str() != arcbox_vmm::hv_driver::FORMAT || image.kind != CheckpointKind::Full
    {
        return Err(Error::ForeignCheckpoint(image.format.clone()));
    }
    Ok(())
}

/// Folds a failure inside the adapter into the port's error, keeping the
/// original as its source.
fn driver_error(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> Error {
    Error::Driver {
        driver: NAME,
        message: message.into(),
        source: Some(Box::new(source)),
    }
}

fn set_nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own, with integer arguments only.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
