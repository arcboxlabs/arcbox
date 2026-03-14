//! L3 tunnel service for host → container direct routing via utun.
//!
//! Creates a macOS utun device, installs host-side routes for container
//! subnets, and runs a bidirectional packet relay:
//!
//! - **Inbound** (host → guest): reads IP packets from utun, sends
//!   `InboundCommand::RoutePacket` to the datapath for L2 injection.
//! - **Return** (guest → host): handled by the datapath via `TunWriter`
//!   when `TunnelConnTrack` matches a reply.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::inbound_relay::InboundCommand;
use super::tun::DarwinTun;

/// Cloneable write handle for the utun device.
///
/// The datapath uses this to send return packets (guest → host) back
/// through the utun. The read side is owned by `L3TunnelService`'s
/// async read loop.
#[derive(Clone)]
pub struct TunWriter {
    fd: Arc<OwnedFd>,
}

/// Wraps an `Arc<OwnedFd>` for `AsyncFd` registration.
struct AsyncOwnedFd(Arc<OwnedFd>);

impl AsRawFd for AsyncOwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl TunWriter {
    /// Writes a raw IP packet to the utun device.
    ///
    /// Prepends the 4-byte AF header automatically.
    pub fn write_packet(&self, ip_packet: &[u8]) -> io::Result<usize> {
        if ip_packet.is_empty() {
            return Ok(0);
        }

        let af: u32 = match ip_packet[0] >> 4 {
            4 => libc::AF_INET as u32,
            6 => libc::AF_INET6 as u32,
            v => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported IP version: {v}"),
                ));
            }
        };

        let af_bytes = af.to_ne_bytes();
        let iov = [
            libc::iovec {
                iov_base: af_bytes.as_ptr() as *mut _,
                iov_len: 4,
            },
            libc::iovec {
                iov_base: ip_packet.as_ptr() as *mut _,
                iov_len: ip_packet.len(),
            },
        ];

        // SAFETY: writev with valid fd and properly initialized iovecs.
        let n = unsafe { libc::writev(self.fd.as_raw_fd(), iov.as_ptr(), 2) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((n as usize).saturating_sub(4))
    }
}

/// L3 tunnel service managing a utun device for host ↔ container routing.
pub struct L3TunnelService {
    tun_name: String,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl L3TunnelService {
    /// Creates a utun device, installs routes, and starts the read loop.
    ///
    /// Returns `(Self, TunWriter)` — pass the `TunWriter` to the datapath
    /// so it can write return packets.
    ///
    /// `subnets` are CIDR strings like `"172.17.0.0/16"` for which host
    /// routes pointing to the utun will be installed.
    pub async fn start(
        subnets: Vec<String>,
        inbound_cmd_tx: mpsc::Sender<InboundCommand>,
    ) -> io::Result<(Self, TunWriter)> {
        // Create the utun device (no root needed on macOS 10.10+).
        let tun = DarwinTun::new()?;
        let tun_name = tun.name().to_string();

        // Configure the utun with a point-to-point address and bring UP.
        // macOS requires an IPv4 address on the interface for `-interface`
        // routes to work. We use 240.0.0.1 (Class E reserved, never routed
        // on the internet, not claimed by any VPN/proxy tool).
        tun.configure(
            std::net::Ipv4Addr::new(240, 0, 0, 1),
            std::net::Ipv4Addr::new(240, 0, 0, 1),
            std::net::Ipv4Addr::new(255, 255, 255, 252),
        )?;
        tun.set_nonblocking(true)?;

        // Install routes for container subnets via this utun interface.
        for subnet in &subnets {
            if let Err(e) = super::route_manager::add_route(subnet, &tun_name) {
                tracing::warn!(subnet, error = %e, "failed to install route");
            }
        }

        // Extract the fd for shared use between read loop and TunWriter.
        // SAFETY: We're taking the OwnedFd out of DarwinTun. The DarwinTun
        // struct is consumed (we only keep tun_name). The fd lives in the Arc.
        let fd = Arc::new(extract_tun_fd(tun));

        let writer = TunWriter { fd: Arc::clone(&fd) };

        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_tun_name = tun_name.clone();
        let task_subnets = subnets;

        let task = tokio::spawn(async move {
            if let Err(e) =
                read_loop(Arc::clone(&fd), inbound_cmd_tx, task_cancel).await
            {
                tracing::error!(error = %e, "L3 tunnel read loop failed");
            }
            // Cleanup: remove routes.
            for subnet in &task_subnets {
                let _ = super::route_manager::remove_route(subnet, &task_tun_name);
            }
            tracing::info!(interface = %task_tun_name, "L3 tunnel stopped");
        });

        tracing::info!(interface = %tun_name, "L3 tunnel service started");

        Ok((
            Self {
                tun_name,
                cancel,
                task,
            },
            writer,
        ))
    }

    /// Returns the utun interface name (e.g. "utun5").
    #[allow(dead_code)]
    pub fn tun_name(&self) -> &str {
        &self.tun_name
    }

    /// Stops the tunnel service and removes routes.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Async read loop: reads IP packets from the utun and sends them to the
/// datapath as `RoutePacket` commands.
async fn read_loop(
    fd: Arc<OwnedFd>,
    cmd_tx: mpsc::Sender<InboundCommand>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let async_fd = AsyncFd::new(AsyncOwnedFd(fd))?;
    let mut buf = vec![0u8; 65535];

    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            readable = async_fd.readable() => {
                let mut guard = readable?;
                // Drain all available packets.
                loop {
                    match guard.try_io(|inner| read_ip_packet(inner.get_ref().as_raw_fd(), &mut buf)) {
                        Ok(Ok(0)) => break,
                        Ok(Ok(n)) => {
                            let packet = buf[..n].to_vec();
                            if cmd_tx.try_send(InboundCommand::RoutePacket(packet)).is_err() {
                                tracing::debug!("tunnel cmd channel full, dropping packet");
                            }
                        }
                        Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Ok(Err(e)) => return Err(e),
                        Err(_would_block) => break,
                    }
                }
                guard.clear_ready();
            }
        }
    }
}

/// Reads one IP packet from the utun fd, stripping the 4-byte AF header.
fn read_ip_packet(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let mut af_header = [0u8; 4];
    let iov = [
        libc::iovec {
            iov_base: af_header.as_mut_ptr().cast(),
            iov_len: 4,
        },
        libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        },
    ];
    // SAFETY: readv with valid fd and properly initialized iovecs.
    let n = unsafe { libc::readv(fd, iov.as_ptr(), 2) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let total = n as usize;
    if total <= 4 {
        return Ok(0);
    }
    Ok(total - 4)
}

/// Extracts the `OwnedFd` from a `DarwinTun` without closing it.
///
/// # Safety
/// After this call, `tun` is consumed and must not be used. The returned
/// `OwnedFd` assumes ownership of the file descriptor.
fn extract_tun_fd(tun: DarwinTun) -> OwnedFd {
    use std::os::fd::FromRawFd;
    let fd = tun.as_raw_fd();
    // Prevent DarwinTun's Drop from closing the fd.
    std::mem::forget(tun);
    // SAFETY: fd is valid and we just prevented the original owner from closing it.
    unsafe { OwnedFd::from_raw_fd(fd) }
}
