//! The primary NIC's socketpair and the host network stack behind it,
//! shared by the VZ and QEMU backends.
//!
//! The guest end of a `SOCK_DGRAM` socketpair carries raw Ethernet frames:
//! VZ attaches it with `VZFileHandleNetworkDeviceAttachment`, QEMU with
//! `-netdev dgram,local.type=fd`. The host end feeds the guest NAT datapath
//! (DHCP, DNS, socket proxy, TCP bridge).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::error::{Result, VmmError};

/// Socket buffer size (`SO_SNDBUF`/`SO_RCVBUF`) for both ends of a network
/// socketpair. 8 MiB accommodates burst traffic.
///
/// Both ends must be sized, not just the VZ side: on macOS a `SOCK_DGRAM` unix
/// socket rejects any datagram larger than its `SO_SNDBUF` with `EMSGSIZE`. The
/// host datapath end is the *sender* of guest-inbound frames; the default
/// (~2 KiB) buffer leaves almost no headroom over a full 1514-byte frame, and
/// none at all when `ARCBOX_DIAG_NET_MTU_4000` raises the link to the enhanced
/// MTU (see `arcbox-vz/src/device/network.rs`).
const NET_SOCKET_BUF_BYTES: libc::c_int = 8 * 1024 * 1024;

/// NIC1 wired to the guest NAT datapath. The VMM hands `guest_fd` to the
/// hypervisor, keeps it open while the VM runs, and fires `cancel` on stop.
pub(super) struct GuestNic {
    /// The hypervisor's end of the frame socketpair.
    pub(super) guest_fd: OwnedFd,
    /// Stops the datapath task.
    pub(super) cancel: tokio_util::sync::CancellationToken,
    /// Port-forwarding listeners into the guest.
    pub(super) inbound: arcbox_net::darwin::inbound_relay::InboundListenerManager,
}

impl GuestNic {
    /// Creates the socketpair and spawns the datapath on the current tokio
    /// runtime; `mtu` is the one the guest NIC negotiates.
    pub(super) fn new(
        mtu: usize,
        shared_dns: Option<&arcbox_net::dns::SharedDns>,
        proxy_env: Option<arcbox_fakeip::proxy_detect::ProxyEnvironment>,
    ) -> Result<Self> {
        use arcbox_net::darwin::guest_nat::{self, GuestNat};

        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: socketpair with valid parameters.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "socketpair failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let guest_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let host_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Prevent fd inheritance to child processes and enable non-blocking I/O.
        set_nonblock_cloexec(guest_fd.as_raw_fd())?;
        set_nonblock_cloexec(host_fd.as_raw_fd())?;
        // Size both ends of the pair: the guest side buffers bursts, and the
        // host datapath side must be able to *send* a full enhanced-MTU frame
        // to the guest (see NET_SOCKET_BUF_BYTES).
        set_socket_buffers(guest_fd.as_raw_fd());
        set_socket_buffers(host_fd.as_raw_fd());

        let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
            VmmError::Device(format!(
                "tokio runtime not available for network datapath: {e}"
            ))
        })?;
        let GuestNat {
            datapath,
            inbound,
            cancel,
        } = GuestNat::new(host_fd, mtu, shared_dns, proxy_env);
        tracing::info!(
            "Custom network stack: socket proxy (gateway={}, guest={})",
            guest_nat::GATEWAY_IP,
            guest_nat::GUEST_IP,
        );
        runtime.spawn(async move {
            if let Err(e) = datapath.run().await {
                tracing::error!("Network datapath exited with error: {}", e);
            }
        });
        tracing::info!("Network datapath task spawned");

        Ok(Self {
            guest_fd,
            cancel,
            inbound,
        })
    }
}

/// Sets `O_NONBLOCK` and `FD_CLOEXEC` on a raw file descriptor.
///
/// Socketpair fds are blocking and inheritable by default. Setting
/// `CLOEXEC` prevents leaking them to child processes, and `NONBLOCK`
/// is required for async I/O via tokio.
pub(super) fn set_nonblock_cloexec(fd: libc::c_int) -> Result<()> {
    // SAFETY: fcntl F_GETFD/F_SETFD/F_GETFL/F_SETFL are standard POSIX
    // operations on a valid fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return Err(VmmError::Device(format!(
                "fcntl F_GETFD failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
            return Err(VmmError::Device(format!(
                "fcntl F_SETFD FD_CLOEXEC failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 {
            return Err(VmmError::Device(format!(
                "fcntl F_GETFL failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return Err(VmmError::Device(format!(
                "fcntl F_SETFL O_NONBLOCK failed: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

/// Enlarges `SO_SNDBUF` and `SO_RCVBUF` to `NET_SOCKET_BUF_BYTES` on `fd`.
///
/// Failures are logged and tolerated — the socket still works with default
/// buffers, only with less headroom (see `NET_SOCKET_BUF_BYTES` for why the
/// host end in particular must be sized once the enhanced MTU is active).
pub(super) fn set_socket_buffers(fd: libc::c_int) {
    // ABX-423 bisection knobs: ARCBOX_DIAG_NET_BUF_DEFAULT skips the
    // enlargement entirely; ARCBOX_DIAG_NET_BUF_BYTES overrides the size
    // (buffer sizing is the first dimension found to change the freeze
    // incidence).
    if std::env::var_os("ARCBOX_DIAG_NET_BUF_DEFAULT").is_some() {
        tracing::warn!("socket buffers left at defaults (ARCBOX_DIAG_NET_BUF_DEFAULT)");
        return;
    }
    let size = std::env::var("ARCBOX_DIAG_NET_BUF_BYTES")
        .ok()
        .and_then(|v| v.parse::<libc::c_int>().ok())
        .inspect(|v| tracing::warn!("socket buffers overridden to {v} (diagnostic)"))
        .unwrap_or(NET_SOCKET_BUF_BYTES);
    // SAFETY: setsockopt on a valid fd with a correctly-sized `c_int` value.
    unsafe {
        for (opt, name) in [
            (libc::SO_SNDBUF, "SO_SNDBUF"),
            (libc::SO_RCVBUF, "SO_RCVBUF"),
        ] {
            if libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                tracing::warn!(
                    "setsockopt {name} failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    /// A `SOCK_DGRAM` unix socket caps datagram size at `SO_SNDBUF`; the host
    /// end of the network socketpair must be enlarged or a full enhanced-MTU
    /// frame (4000 + 14-byte Ethernet header) to the guest fails with
    /// `EMSGSIZE` and is silently dropped. Guards the host-side sizing that
    /// the `ARCBOX_DIAG_NET_MTU_4000` reproduction mode relies on.
    #[test]
    fn enlarged_socket_buffer_accepts_enhanced_mtu_frame() {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: socketpair with a valid out pointer.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair failed");
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let send = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let _recv = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let send_fd = send.as_raw_fd();

        // Enhanced MTU (4000) plus a 14-byte Ethernet header.
        let frame = vec![0u8; arcbox_net::darwin::classifier::ENHANCED_ETHERNET_MTU + 14];

        // Precondition: a small send buffer rejects the frame with EMSGSIZE.
        let small: libc::c_int = 1024;
        // SAFETY: setsockopt on a valid fd with a `c_int` option value.
        unsafe {
            libc::setsockopt(
                send_fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const small).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        // SAFETY: send on a valid fd from a valid buffer.
        let n = unsafe { libc::send(send_fd, frame.as_ptr().cast(), frame.len(), 0) };
        assert!(
            n < 0,
            "small SO_SNDBUF should reject a {}-byte datagram",
            frame.len()
        );

        // Enlarging the buffers lets the same frame through.
        set_socket_buffers(send_fd);
        // SAFETY: send on a valid fd from a valid buffer.
        let n = unsafe { libc::send(send_fd, frame.as_ptr().cast(), frame.len(), 0) };
        assert_eq!(
            n,
            isize::try_from(frame.len()).unwrap(),
            "enlarged SO_SNDBUF must accept a full enhanced-MTU frame"
        );
    }
}
