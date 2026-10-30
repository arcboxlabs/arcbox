//! The NIC attachments the VZ driver builds, and the host state behind them.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use arcbox_net::darwin::guest_nat::{self, GuestNat};
use arcbox_net::darwin::inbound_relay::InboundListenerManager;
use arcbox_net::darwin::proxy_detect::ProxyEnvironment;
use arcbox_net::dns::SharedDns;
use arcbox_vm_driver::{MacAddr, NicAttachment, NicSpec, Result};
use arcbox_vz::NetworkDeviceConfiguration;
use tokio_util::sync::CancellationToken;

use crate::{driver_error, unsupported};

/// Socket buffer size (`SO_SNDBUF`/`SO_RCVBUF`) for both ends of a NIC
/// socketpair.
///
/// Both ends are sized: on macOS a `SOCK_DGRAM` unix socket rejects any
/// datagram larger than its `SO_SNDBUF` with `EMSGSIZE`, and the host end
/// sends full guest-bound frames, up to the enhanced MTU that
/// `ARCBOX_DIAG_NET_MTU_4000` enables (see `arcbox-vz/src/device/network.rs`).
const NET_SOCKET_BUF_BYTES: libc::c_int = 8 * 1024 * 1024;

/// What the host side of the guest network needs from the daemon.
#[derive(Clone, Default)]
pub struct HostServices {
    /// DNS ownership shared with the host DNS service.
    pub dns: Option<SharedDns>,
    /// The proxy guest egress is tunnelled through.
    pub proxy: Option<ProxyEnvironment>,
}

/// The host side of a VM's NICs. Dropped after the VM has stopped: the
/// datapath and relay tasks stop, and VZ's borrowed descriptors close.
#[derive(Default)]
pub struct NetResources {
    /// The VZ-facing ends VZ reads and writes; VZ only borrows them.
    fds: Vec<OwnedFd>,
    /// Stops the datapath and relay tasks.
    cancels: Vec<CancellationToken>,
    /// Port forwarding into the `UserNat` NIC, until taken.
    pub inbound: Option<InboundListenerManager>,
    /// The vmnet interfaces behind `HostNat` NICs.
    #[cfg(feature = "vmnet")]
    vmnets: Vec<std::sync::Arc<arcbox_vmnet::Vmnet>>,
}

impl Drop for NetResources {
    fn drop(&mut self) {
        for cancel in &self.cancels {
            cancel.cancel();
        }
        #[cfg(feature = "vmnet")]
        for vmnet in &self.vmnets {
            vmnet.stop();
        }
    }
}

impl NetResources {
    /// Builds the VZ device for `nic`, keeping its host side here.
    pub fn attach(
        &mut self,
        nic: &NicSpec,
        host: &HostServices,
    ) -> Result<NetworkDeviceConfiguration> {
        let mac = nic.mac.to_string();
        match &nic.attachment {
            NicAttachment::UserNat => match self.user_nat(&mac, host) {
                Ok(device) => Ok(device),
                Err(error) => {
                    tracing::warn!(
                        nic = nic.id,
                        "userspace NAT unavailable, falling back to Apple NAT: {error}"
                    );
                    apple_nat(&mac)
                }
            },
            NicAttachment::HostNat => self.host_nat(nic.mac),
            NicAttachment::FileHandle { fd } => {
                // SAFETY: the port lends `fd` for the duration of `boot`;
                // the borrow ends before this function returns.
                let borrowed = unsafe { BorrowedFd::borrow_raw(*fd) };
                let owned = borrowed.try_clone_to_owned()?;
                let device =
                    NetworkDeviceConfiguration::file_handle_with_mac(owned.as_raw_fd(), &mac)
                        .map_err(|e| driver_error(format!("nic `{}`", nic.id), e))?;
                self.fds.push(owned);
                Ok(device)
            }
            other => Err(unsupported(&format!(
                "nic `{}`: {other:?} attachments are not available",
                nic.id
            ))),
        }
    }

    /// The userspace NAT datapath on a socketpair.
    fn user_nat(&mut self, mac: &str, host: &HostServices) -> Result<NetworkDeviceConfiguration> {
        let (vz_fd, host_fd) = socketpair()?;
        // The MTU the VZ device negotiates: the datapath fragments
        // guest-bound UDP to it (ABX-428); guest-bound TCP segments are
        // bounded by the peer's advertised MSS.
        let mtu = arcbox_vz::desired_network_mtu();
        let GuestNat {
            datapath,
            inbound,
            cancel,
        } = GuestNat::new(host_fd, mtu, host.dns.as_ref(), host.proxy.clone());
        let device = NetworkDeviceConfiguration::file_handle_with_mac(vz_fd.as_raw_fd(), mac)
            .map_err(|e| driver_error("userspace NAT device", e))?;
        tokio::spawn(async move {
            if let Err(e) = datapath.run().await {
                tracing::error!("network datapath exited with error: {e}");
            }
        });
        tracing::info!(
            "userspace NAT: gateway={}, guest={}, mtu={mtu}",
            guest_nat::GATEWAY_IP,
            guest_nat::GUEST_IP,
        );
        self.fds.push(vz_fd);
        self.cancels.push(cancel);
        self.inbound = Some(inbound);
        Ok(device)
    }

    /// The host NAT: vmnet shared mode relayed over a socketpair, so the
    /// host bridge interface is known at once; Apple NAT when vmnet fails.
    #[cfg(feature = "vmnet")]
    fn host_nat(&mut self, mac: MacAddr) -> Result<NetworkDeviceConfiguration> {
        match self.vmnet_shared(mac) {
            Ok(device) => Ok(device),
            Err(error) => {
                tracing::warn!("vmnet bridge NIC failed, falling back to Apple NAT: {error}");
                apple_nat(&mac.to_string())
            }
        }
    }

    /// The host NAT: Apple NAT, whose bridge interface the host discovers
    /// from its FDB.
    #[cfg(not(feature = "vmnet"))]
    fn host_nat(&self, mac: MacAddr) -> Result<NetworkDeviceConfiguration> {
        apple_nat(&mac.to_string())
    }

    #[cfg(feature = "vmnet")]
    fn vmnet_shared(&mut self, mac: MacAddr) -> Result<NetworkDeviceConfiguration> {
        use std::sync::Arc;

        use arcbox_vmnet::{Vmnet, VmnetConfig, VmnetRelay};

        let vmnet = Arc::new(
            Vmnet::new(VmnetConfig::shared().with_mac(mac.octets()))
                .map_err(|e| driver_error("vmnet start", e))?,
        );
        let info = vmnet.interface_info().ok_or_else(|| {
            driver_error("vmnet", std::io::Error::other("interface info unavailable"))
        })?;
        let vmnet_mac = arcbox_net::darwin::format_mac(&info.mac);
        tracing::info!(
            mac = vmnet_mac,
            mtu = info.mtu,
            max_packet_size = info.max_packet_size,
            "vmnet bridge NIC created"
        );
        // Stops the interface if anything below fails.
        self.vmnets.push(Arc::clone(&vmnet));

        let (vz_fd, relay_fd) = socketpair()?;
        let cancel = CancellationToken::new();
        let relay = VmnetRelay::new(vmnet, cancel.clone());
        // The VZ NIC takes vmnet's MAC so bridge FDB lookups match.
        let device =
            NetworkDeviceConfiguration::file_handle_with_mac(vz_fd.as_raw_fd(), &vmnet_mac)
                .map_err(|e| driver_error("vmnet bridge device", e))?;
        tokio::spawn(async move {
            if let Err(e) = relay.run(relay_fd).await {
                tracing::error!("vmnet relay exited with error: {e}");
            }
        });
        self.fds.push(vz_fd);
        self.cancels.push(cancel);
        Ok(device)
    }

    /// The NICs' host side, for diagnostics.
    pub fn snapshot(&self) -> serde_json::Value {
        #[cfg(feature = "vmnet")]
        let vmnet: Vec<_> = self
            .vmnets
            .iter()
            .filter_map(|vmnet| vmnet.interface_info())
            .map(|info| {
                serde_json::json!({
                    "mac": arcbox_net::darwin::format_mac(&info.mac),
                    "mtu": info.mtu,
                })
            })
            .collect();
        #[cfg(not(feature = "vmnet"))]
        let vmnet: Vec<serde_json::Value> = Vec::new();
        serde_json::json!({
            "user_nat": self.inbound.is_some(),
            "vmnet": vmnet,
        })
    }
}

fn apple_nat(mac: &str) -> Result<NetworkDeviceConfiguration> {
    NetworkDeviceConfiguration::nat_with_mac(mac).map_err(|e| driver_error("Apple NAT device", e))
}

/// A `SOCK_DGRAM` socketpair for L2 frames, both ends non-blocking,
/// close-on-exec and sized to [`NET_SOCKET_BUF_BYTES`]: (VZ end, host end).
fn socketpair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` has room for the two descriptors socketpair writes.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: socketpair succeeded, so fds[0] is a fresh descriptor we own.
    let vz = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: as above, for fds[1].
    let host = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [&vz, &host] {
        set_nonblock_cloexec(fd)?;
        set_socket_buffers(fd);
    }
    Ok((vz, host))
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
    // ABX-423 bisection knobs: ARCBOX_DIAG_NET_BUF_DEFAULT keeps the
    // defaults; ARCBOX_DIAG_NET_BUF_BYTES overrides the size.
    if std::env::var_os("ARCBOX_DIAG_NET_BUF_DEFAULT").is_some() {
        tracing::warn!("socket buffers left at defaults (ARCBOX_DIAG_NET_BUF_DEFAULT)");
        return;
    }
    let size = std::env::var("ARCBOX_DIAG_NET_BUF_BYTES")
        .ok()
        .and_then(|v| v.parse::<libc::c_int>().ok())
        .inspect(|v| tracing::warn!("socket buffers overridden to {v} (diagnostic)"))
        .unwrap_or(NET_SOCKET_BUF_BYTES);
    for (opt, name) in [
        (libc::SO_SNDBUF, "SO_SNDBUF"),
        (libc::SO_RCVBUF, "SO_RCVBUF"),
    ] {
        // SAFETY: setsockopt on a live descriptor with a `c_int` value.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A full enhanced-MTU frame must fit through the host end: on macOS a
    /// datagram larger than `SO_SNDBUF` fails with `EMSGSIZE`.
    #[test]
    fn a_socketpair_carries_an_enhanced_mtu_frame() {
        let (vz, host) = socketpair().unwrap();
        let frame = vec![0u8; 4000 + 14];
        // SAFETY: plain send/recv on the pair's live descriptors.
        let sent = unsafe { libc::send(host.as_raw_fd(), frame.as_ptr().cast(), frame.len(), 0) };
        assert_eq!(
            sent,
            frame.len().cast_signed(),
            "{}",
            std::io::Error::last_os_error()
        );
        let mut buf = vec![0u8; frame.len()];
        // SAFETY: as above.
        let got = unsafe { libc::recv(vz.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        assert_eq!(got, frame.len().cast_signed());
    }
}
