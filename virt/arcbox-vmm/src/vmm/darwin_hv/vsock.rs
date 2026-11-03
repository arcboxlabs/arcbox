use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use crate::error::{Result, VmmError};

use super::*;

impl HvMachine {
    /// Moves a daemon-facing socketpair fd out of the low descriptor range,
    /// starting at a position that rotates with the connection's host port,
    /// so a retry does not reuse the kqueue registration of the connection
    /// it replaces; see [`crate::vsock_manager::relocate_daemon_end`].
    pub(super) fn duplicate_client_vsock_fd(fd: OwnedFd, host_port: u32) -> Result<OwnedFd> {
        crate::vsock_manager::relocate_daemon_end(fd, host_port)
            .map_err(|e| VmmError::Device(format!("vsock client fd dup failed: {e}")))
    }

    /// Creates the vsock doorbell pipe, installs the ring callback into the
    /// connection manager, and spawns the vsock-io worker thread.
    ///
    /// The worker owns host→guest vsock injection from here on; the vCPU
    /// loop no longer polls vsock. Joined in `stop_darwin_hv` before guest
    /// memory is released.
    pub(super) fn spawn_vsock_rx_worker(
        &mut self,
        device_manager: &Arc<DeviceManager>,
    ) -> Result<()> {
        // Spawn once per VMM lifecycle; `stop_darwin_hv` joins and clears.
        if self.hv_vsock_worker.is_some() {
            return Ok(());
        }

        let mut pipe_fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: `pipe_fds` is a valid 2-element array; pipe writes two
        // fds into it on success.
        let ret = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "vsock doorbell pipe failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: both fds are fresh from pipe above with sole ownership.
        let doorbell_rd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        // SAFETY: same as above for the write end.
        let doorbell_wr = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

        // Both ends non-blocking + cloexec. The write end must never block
        // a producer (a full pipe already guarantees a pending wakeup); the
        // worker drains the read end with a non-blocking loop.
        for fd in [doorbell_rd.as_raw_fd(), doorbell_wr.as_raw_fd()] {
            // SAFETY: `fd` is a live fd owned by the OwnedFds above.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            // SAFETY: same fd; setting O_NONBLOCK is side-effect-only.
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(VmmError::Device(format!(
                    "vsock doorbell O_NONBLOCK failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            // SAFETY: same fd; FD_CLOEXEC is side-effect-only.
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if fd_flags != -1 {
                // SAFETY: same fd as above.
                let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) };
            }
        }

        let doorbell: crate::vsock_manager::VsockDoorbell = Arc::new(move || {
            let byte = [1u8];
            // SAFETY: the write end is owned by this closure and stays open
            // for its lifetime. EAGAIN on a full pipe is fine — a wakeup is
            // already pending.
            let _ = unsafe {
                libc::write(
                    doorbell_wr.as_raw_fd(),
                    byte.as_ptr().cast::<libc::c_void>(),
                    1,
                )
            };
        });
        device_manager
            .vsock_connections()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_doorbell(doorbell);

        let ctx = crate::vsock_rx_worker::VsockRxWorkerContext {
            device_manager: Arc::clone(device_manager),
            doorbell_rd,
            running: self.io_running.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("vsock-io".to_string())
            .spawn(move || crate::vsock_rx_worker::vsock_rx_worker_loop(ctx))
            .map_err(|e| VmmError::Device(format!("spawn vsock-io worker: {e}")))?;
        self.hv_vsock_worker = Some(handle);
        Ok(())
    }

    /// Connects to a vsock port on the guest VM (HV backend).
    ///
    /// Creates a Unix `SOCK_STREAM` socketpair; one end is returned to the
    /// caller for host-side I/O, the other is registered with
    /// `VsockConnectionManager` so the VirtIO vsock device can relay data
    /// between the socketpair and the guest's RX/TX queues.
    ///
    /// Returns immediately after allocating and enqueueing the connection —
    /// `allocate` rings the vsock-io worker's doorbell, which injects the
    /// OP_REQUEST into the guest RX queue right away. The returned fd is
    /// usable immediately; the guest responds with OP_RESPONSE or OP_RST
    /// as soon as it services the interrupt.
    #[allow(clippy::unnecessary_wraps)]
    pub(in crate::vmm) fn connect_vsock_hv(&self, port: u32) -> Result<std::os::unix::io::RawFd> {
        // fds[0] = returned to caller (daemon agent client)
        // fds[1] = internal, owned by VsockConnectionManager
        let (host_fd, internal_fd) = crate::vsock_manager::host_socketpair()
            .map_err(|e| VmmError::Device(format!("vsock socketpair failed: {e}")))?;

        let dm = self
            .hv_device_manager
            .as_ref()
            .ok_or_else(|| VmmError::Device("DeviceManager not initialized".to_string()))?;

        let guest_cid = self.config.guest_cid.unwrap_or(3) as u64;

        let conns = dm.vsock_connections();
        let (conn_id, connect_rx) = {
            let mut mgr = conns
                .lock()
                .map_err(|e| VmmError::Device(format!("vsock manager lock failed: {e}")))?;
            mgr.allocate(port, guest_cid, internal_fd)
        };

        let host_fd =
            Self::duplicate_client_vsock_fd(host_fd, conn_id.host_port).inspect_err(|_| {
                if let Ok(mut mgr) = conns.lock() {
                    mgr.remove(&conn_id);
                }
            })?;

        tracing::info!(
            "HV vsock connect: guest_port={}, host_port={}, host_fd={}",
            port,
            conn_id.host_port,
            host_fd.as_raw_fd(),
        );

        // OP_REQUEST is in backend_rxq and the vsock-io worker's doorbell
        // has been rung; it injects and fires injected_notify. We do NOT
        // block here — the daemon's ping().await handles the timing:
        // - If REQUEST not yet injected: ping timeout (2s) → retry
        // - If injected + RST: read returns EOF → retry
        // - If injected + RESPONSE: read returns data → success
        //
        // The injected_notify channel is kept alive via the VsockConnection's
        // OwnedFd lifetime. When the connection is removed (RST), the sender
        // is dropped, which is fine — we don't read it.
        let _ = connect_rx; // Drop receiver — we don't wait on it.

        Ok(host_fd.into_raw_fd())
    }
}
