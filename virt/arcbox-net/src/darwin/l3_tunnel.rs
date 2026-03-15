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
            let e = io::Error::last_os_error();
            tracing::warn!(fd = self.fd.as_raw_fd(), error = %e, "TunWriter: writev failed");
            return Err(e);
        }
        let written = (n as usize).saturating_sub(4);
        tracing::trace!(fd = self.fd.as_raw_fd(), written, "TunWriter: wrote to utun");
        Ok(written)
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
        // Create the utun device via the privileged helper (which creates
        // utun as root and passes the fd via DGRAM socketpair).
        // Falls back to direct creation if running as root (development).
        // Use an IP in the guest's subnet (192.168.64.0/24) so that:
        // 1. Host kernel uses this IP as src when routing to container subnets
        // 2. Guest can route replies back via eth0 (default route to 192.168.64.1)
        // 3. Reply arrives at datapath → TunnelConnTrack matches → TunWriter → utun
        //
        // We use .3 to avoid colliding with gateway (.1) and guest (.2).
        let utun_ip = "192.168.64.3";

        let (tun_fd, tun_name) = if super::helper_client::is_available() {
            tracing::info!("L3 tunnel: using privileged helper");
            let session_id = format!("tunnel-{}", std::process::id());
            let (fd, name) = super::helper_client::create_utun(&session_id, utun_ip)?;
            (fd, name)
        } else {
            tracing::info!("L3 tunnel: helper not available, trying direct (needs root)");
            let tun = DarwinTun::new()?;
            let ip: std::net::Ipv4Addr = utun_ip.parse().unwrap();
            tun.configure(ip, ip, std::net::Ipv4Addr::new(255, 255, 255, 252))?;
            let name = tun.name().to_string();
            (extract_tun_fd(tun), name)
        };

        // Set non-blocking for async I/O.
        // SAFETY: fcntl on valid fd.
        let flags = unsafe { libc::fcntl(tun_fd.as_raw_fd(), libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(tun_fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }

        tracing::info!(interface = %tun_name, "L3 tunnel: utun ready");

        // Install routes for container subnets via this utun interface.
        for subnet in &subnets {
            if let Err(e) = super::route_manager::add_route(subnet, &tun_name) {
                tracing::warn!(subnet, error = %e, "failed to install route");
            }
        }

        let fd = Arc::new(tun_fd);

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
    // Use tokio's spawn_blocking for the read loop because AsyncFd with
    // PF_SYSTEM (utun control) sockets may not trigger kqueue readiness
    // events reliably on macOS. A blocking read in a dedicated thread is
    // simpler and proven to work (verified via Python select + os.read).
    let raw_fd = fd.as_raw_fd();
    let cancel_clone = cancel.clone();

    tracing::info!(fd = raw_fd, "L3 tunnel read loop started (blocking mode)");

    let blocking_task = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 65535];
        loop {
            if cancel_clone.is_cancelled() {
                return;
            }

            // Use poll(2) with 1000ms timeout so we can check cancellation.
            let mut pfd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll with valid fd.
            let ret = unsafe { libc::poll(&mut pfd, 1, 1000) };
            if ret < 0 {
                let e = io::Error::last_os_error();
                tracing::error!(error = %e, "poll() failed on utun fd");
                return;
            }
            if ret == 0 {
                continue; // timeout, check cancellation and retry
            }
            tracing::debug!(revents = pfd.revents, fd = raw_fd, "utun fd readable!");

            match read_ip_packet(raw_fd, &mut buf) {
                Ok(0) => {
                    tracing::debug!(fd = raw_fd, "read_ip_packet returned 0 bytes");
                    continue;
                }
                Ok(n) => {
                    tracing::info!(fd = raw_fd, bytes = n, "utun packet received, sending RoutePacket");
                    let packet = buf[..n].to_vec();
                    if cmd_tx.try_send(InboundCommand::RoutePacket(packet)).is_err() {
                        tracing::debug!("tunnel cmd channel full, dropping packet");
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    tracing::debug!(fd = raw_fd, "read_ip_packet WouldBlock");
                    continue;
                }
                Err(e) => {
                    if !cancel_clone.is_cancelled() {
                        tracing::error!(error = %e, "utun read error");
                    }
                    return;
                }
            }
        }
    });

    // Wait for cancellation, then abort the blocking task.
    cancel.cancelled().await;
    // The blocking task checks cancel_clone.is_cancelled() every 100ms.
    // Give it a moment to exit gracefully, then drop it.

    Ok(())
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
