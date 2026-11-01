//! One VM's userspace NAT datapath and the socket its NIC is attached to.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use arcbox_net::darwin::datapath_loop::DatapathControl;
use arcbox_net::darwin::guest_nat::GuestNat;
use arcbox_net::darwin::inbound_relay::InboundListenerManager;
use arcbox_net::darwin::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::Result;
use tokio_util::sync::CancellationToken;

/// Socket buffer size (`SO_SNDBUF`/`SO_RCVBUF`) for both ends of a NIC
/// socketpair: on macOS a `SOCK_DGRAM` unix socket refuses a datagram
/// larger than its send buffer, and the receive queue of one end is what
/// the other end's writes fill.
const NET_SOCKET_BUF_BYTES: libc::c_int = 8 * 1024 * 1024;

/// The MTU a guest NIC negotiates unless the composer says otherwise: what
/// both VZ and the HV virtio-net device default to.
pub const DEFAULT_MTU: usize = 1500;

/// What every datapath this network builds shares with the host.
#[derive(Clone)]
pub struct DatapathServices {
    /// DNS ownership shared with the host DNS service; without it each
    /// datapath's forwarder keeps its own registry.
    pub dns: Option<SharedDns>,
    /// The proxy guest egress is tunnelled through; without it guest flows
    /// connect directly from the host.
    pub proxy: Option<ProxyEnvironment>,
    /// The MTU of the guest NIC: guest-bound UDP is fragmented to it.
    pub mtu: usize,
}

impl Default for DatapathServices {
    fn default() -> Self {
        Self {
            dns: None,
            proxy: None,
            mtu: DEFAULT_MTU,
        }
    }
}

/// A running datapath behind one VM's NIC. Dropping it stops the datapath;
/// the VM's end of the socketpair closes with it, and a VMM that duplicated
/// that end keeps a dead link.
pub struct VmDatapath {
    /// The end the VMM reads and writes. Lent to the driver as a
    /// `NicAttachment::FileHandle`.
    vm_fd: OwnedFd,
    cancel: CancellationToken,
    control: DatapathControl,
    /// Port forwarding into this VM, until taken.
    inbound: Option<InboundListenerManager>,
}

impl VmDatapath {
    /// Builds the datapath on a fresh socketpair and starts it on the
    /// current tokio runtime.
    pub fn start(services: &DatapathServices) -> Result<Self> {
        let (vm_fd, host_fd) = socketpair()?;
        let GuestNat {
            datapath,
            inbound,
            cancel,
        } = GuestNat::new(
            host_fd,
            services.mtu,
            services.dns.as_ref(),
            services.proxy.clone(),
        );
        let control = datapath.control();
        tokio::spawn(async move {
            if let Err(error) = datapath.run().await {
                tracing::error!("host sandbox datapath exited with error: {error}");
            }
        });
        Ok(Self {
            vm_fd,
            cancel,
            control,
            inbound: Some(inbound),
        })
    }

    /// The VMM's end, as the number a `FileHandle` attachment carries.
    pub fn vm_fd(&self) -> RawFd {
        self.vm_fd.as_raw_fd()
    }

    pub fn control(&self) -> DatapathControl {
        self.control.clone()
    }

    pub fn take_inbound(&mut self) -> Option<InboundListenerManager> {
        self.inbound.take()
    }
}

impl Drop for VmDatapath {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A `SOCK_DGRAM` socketpair for L2 frames, both ends non-blocking,
/// close-on-exec and sized to [`NET_SOCKET_BUF_BYTES`]: (VM end, host end).
fn socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` has room for the two descriptors socketpair writes.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: socketpair succeeded, so fds[0] is a fresh descriptor we own.
    let vm = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above, for fds[1].
    let host = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [&vm, &host] {
        set_nonblock_cloexec(fd)?;
        set_socket_buffers(fd);
    }
    Ok((vm, host))
}

fn set_nonblock_cloexec(fd: &OwnedFd) -> Result<()> {
    let fd = fd.as_raw_fd();
    for (get, set, flag) in [
        (libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC),
        (libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK),
    ] {
        // SAFETY: fcntl get on a descriptor the caller owns.
        let flags = unsafe { libc::fcntl(fd, get) };
        // SAFETY: fcntl set on the same descriptor, adding one flag.
        if flags == -1 || unsafe { libc::fcntl(fd, set, flags | flag) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Enlarges both socket buffers; a failure leaves the defaults and is only
/// logged.
fn set_socket_buffers(fd: &OwnedFd) {
    let size = NET_SOCKET_BUF_BYTES;
    for (opt, name) in [
        (libc::SO_SNDBUF, "SO_SNDBUF"),
        (libc::SO_RCVBUF, "SO_RCVBUF"),
    ] {
        // SAFETY: setsockopt on a live descriptor with a `c_int` value that
        // outlives the call.
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&raw const size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            tracing::warn!(
                "setsockopt {name} failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
