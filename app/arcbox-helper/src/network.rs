//! Privileged network operations: utun creation + fd passing, route management.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[allow(unused_imports)]
use std::os::unix::net::UnixDatagram;

use sendfd::SendWithFd;

use crate::validate;

/// Creates a utun device, configures it, and passes the fd to the client
/// via a DGRAM socketpair (avoids STREAM data+fd desync).
///
/// Protocol:
/// 1. Helper creates utun + configures IP
/// 2. Helper creates socketpair(DGRAM) → [helper_end, client_end]
/// 3. Helper sends client_end fd via sendfd on the STREAM connection
/// 4. Helper sends utun fd via sendfd on helper_end (DGRAM — atomic)
/// 5. Helper closes helper_end and its copy of client_end
/// 6. Returns (utun_name) so caller can send JSON response on STREAM
pub fn create_utun(stream_fd: RawFd, ip: &str) -> io::Result<String> {
    if !validate::is_valid_ipv4(ip) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid IP"));
    }
    let ip_addr: Ipv4Addr = ip.parse().unwrap();

    // Create the utun device (requires root).
    let tun = arcbox_net::darwin::DarwinTun::new()?;
    let tun_name = tun.name().to_string();

    // Configure IP and bring UP.
    // local = requested IP, peer = local+1.
    // Point-to-point semantics require distinct local/peer for proper
    // kernel input path handling.
    let local_u32 = u32::from(ip_addr);
    let peer_addr = Ipv4Addr::from(local_u32.wrapping_add(1));
    tun.configure(ip_addr, peer_addr, Ipv4Addr::new(255, 255, 255, 252))?;

    tracing::info!(interface = %tun_name, %ip, "utun created and configured");

    // Create a DGRAM socketpair for atomic fd transfer.
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: socketpair with valid parameters.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fds are valid from socketpair.
    let helper_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let client_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // Step 1: Send client_end fd to the client via STREAM (with 1 marker byte).
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(libc::dup(stream_fd)) };
    stream.send_with_fd(&[0u8], &[client_end.as_raw_fd()])?;
    drop(client_end); // Client owns it now.
    // Prevent stream Drop from closing the original stream_fd — we dup'd it.
    // Actually, we created a new OwnedFd via dup, so dropping stream only
    // closes the dup, not the original. This is correct.

    // Step 2: Send utun fd via DGRAM helper_end (atomic delivery).
    let helper_dgram =
        unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(libc::dup(helper_end.as_raw_fd())) };
    helper_dgram.send_with_fd(&[0u8], &[tun.as_raw_fd()])?;
    drop(helper_end);

    // Keep the utun alive — the fd was duplicated by sendmsg, but we still
    // need the original to stay open until the client receives it. Actually,
    // sendmsg with SCM_RIGHTS duplicates the fd in the kernel, so even if
    // we close our copy, the client's copy is independent. We can let tun
    // drop safely — the client holds its own reference.

    Ok(tun_name)
}

/// Adds a route for a subnet via an interface.
///
/// Uses `/sbin/route` directly. `net-route` requires a tokio runtime
/// which is too heavy for this synchronous helper process.
pub fn add_route(subnet: &str, iface: &str) -> io::Result<()> {
    if !validate::is_valid_cidr(subnet) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid subnet: {subnet}"),
        ));
    }
    if !validate::is_valid_utun_name(iface) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid interface: {iface}"),
        ));
    }

    let output = std::process::Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, "-interface", iface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, iface, "route added");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("File exists") {
            tracing::debug!(subnet, iface, "route already exists");
            Ok(())
        } else {
            tracing::warn!(subnet, iface, stderr = %stderr, "route add failed");
            Err(io::Error::new(io::ErrorKind::Other, stderr.to_string()))
        }
    }
}

/// Adds a route for a subnet via a gateway IP.
///
/// Uses `/sbin/route -n add -net <subnet> <gateway>`.
pub fn add_route_gateway(subnet: &str, gateway: &str) -> io::Result<()> {
    if !validate::is_valid_cidr(subnet) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid subnet: {subnet}"),
        ));
    }
    if !validate::is_valid_ipv4(gateway) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid gateway: {gateway}"),
        ));
    }

    let output = std::process::Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, gateway])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, gateway, "gateway route added");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("File exists") {
            tracing::debug!(subnet, gateway, "route already exists");
            Ok(())
        } else {
            tracing::warn!(subnet, gateway, stderr = %stderr, "route add failed");
            Err(io::Error::new(io::ErrorKind::Other, stderr.to_string()))
        }
    }
}

/// Removes a route for a subnet via an interface.
pub fn remove_route(subnet: &str, iface: &str) -> io::Result<()> {
    if !validate::is_valid_cidr(subnet) || !validate::is_valid_utun_name(iface) {
        return Ok(());
    }

    let output = std::process::Command::new("/sbin/route")
        .args(["-n", "delete", "-net", subnet, "-interface", iface])
        .output()?;

    if output.status.success() {
        tracing::info!(subnet, iface, "route removed");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(subnet, iface, stderr = %stderr, "route delete (may not exist)");
    }
    Ok(())
}
