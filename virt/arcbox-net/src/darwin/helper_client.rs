//! Client for the ArcBox privileged network helper.
//!
//! Communicates with `arcbox-helper` via the Unix socket at
//! `/var/run/arcbox/helper.sock`. Protocol: hello handshake, then one
//! JSON op per connection. FD passing uses a DGRAM socketpair for
//! atomic delivery.

use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use sendfd::RecvWithFd;

const HELPER_SOCKET: &str = "/var/run/arcbox/helper.sock";

/// Opens a connection to the helper and performs the hello handshake.
/// Returns the stream ready for an op request.
fn connect_and_hello(session_id: &str) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(HELPER_SOCKET).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot connect to arcbox-helper at {HELPER_SOCKET}: {e}. \
                 Run 'sudo arcbox-helper' or install with 'sudo abctl daemon install'."
            ),
        )
    })?;

    // Send hello.
    let hello = format!(
        r#"{{"hello":{{"version":1,"session_id":"{}","mtu":1500,"features":[]}}}}"#,
        session_id,
    );
    stream.write_all(hello.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    // Read hello response.
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    // Check for error.
    if line.contains("\"error\"") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("helper hello failed: {}", line.trim()),
        ));
    }

    Ok(stream)
}

/// Sends a JSON op and reads the JSON response line.
fn send_op(stream: &mut UnixStream, json: &str) -> io::Result<OpResponse> {
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: OpResponse = serde_json::from_str(line.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if resp.ok {
        Ok(resp)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            resp.error.unwrap_or_else(|| "unknown helper error".into()),
        ))
    }
}

#[derive(serde::Deserialize)]
struct OpResponse {
    ok: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Creates a utun via the helper. Returns (utun_fd, interface_name).
///
/// Protocol:
/// 1. Hello handshake on STREAM
/// 2. Send create_utun op
/// 3. Receive DGRAM client_end fd via SCM_RIGHTS on STREAM
/// 4. Receive utun fd via SCM_RIGHTS on DGRAM client_end (atomic)
/// 5. Read JSON response on STREAM
pub fn create_utun(session_id: &str, ip: &str) -> io::Result<(OwnedFd, String)> {
    let mut stream = connect_and_hello(session_id)?;

    let req = format!(r#"{{"op":"create_utun","ip":"{}"}}"#, ip);
    stream.write_all(req.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    // Step 1: Receive DGRAM client_end fd via STREAM.
    let mut marker = [0u8; 1];
    let mut dgram_fds = [-1i32; 1];
    let (_n, fd_count) = stream.recv_with_fd(&mut marker, &mut dgram_fds)?;
    if fd_count == 0 || dgram_fds[0] < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper did not send DGRAM fd",
        ));
    }
    // SAFETY: fd received from helper via SCM_RIGHTS is valid.
    let dgram_end = unsafe { OwnedFd::from_raw_fd(dgram_fds[0]) };

    // Step 2: Receive utun fd via DGRAM (atomic delivery).
    let dgram_stream =
        unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(libc::dup(dgram_end.as_raw_fd())) };
    let mut marker2 = [0u8; 1];
    let mut utun_fds = [-1i32; 1];

    // sendfd works on UnixDatagram too (it implements RecvWithFd).
    let recv_result: io::Result<(usize, usize)> = {
        use std::os::fd::AsRawFd;
        // Manual recvmsg since sendfd::RecvWithFd may not be impl'd for UnixDatagram.
        let mut iov = libc::iovec {
            iov_base: marker2.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut cmsg_buf = [0u8; 64];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast();
        msg.msg_controllen = cmsg_buf.len() as _;

        // SAFETY: recvmsg with valid fd and properly initialized msghdr.
        let n = unsafe { libc::recvmsg(dgram_stream.as_raw_fd(), &mut msg, 0) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            // Parse cmsg for SCM_RIGHTS.
            let mut fd_count = 0;
            // SAFETY: iterate over cmsghdr chain.
            unsafe {
                let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
                while !cmsg.is_null() {
                    if (*cmsg).cmsg_level == libc::SOL_SOCKET
                        && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                    {
                        let fd_ptr = libc::CMSG_DATA(cmsg).cast::<i32>();
                        utun_fds[0] = *fd_ptr;
                        fd_count = 1;
                    }
                    cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
                }
            }
            Ok((n as usize, fd_count))
        }
    };

    let (_n, fd_count) = recv_result?;
    drop(dgram_stream);
    drop(dgram_end);

    if fd_count == 0 || utun_fds[0] < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper did not send utun fd via DGRAM",
        ));
    }

    // SAFETY: utun fd received from helper is valid.
    let utun_fd = unsafe { OwnedFd::from_raw_fd(utun_fds[0]) };

    // Step 3: Read JSON response.
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: OpResponse = serde_json::from_str(line.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let name = resp
        .name
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing utun name"))?;

    // Verify the fd is really a utun by trying getsockopt.
    let mut verify_buf = [0u8; 64];
    let mut verify_len: libc::socklen_t = 64;
    let verify_ret = unsafe {
        libc::getsockopt(
            utun_fd.as_raw_fd(),
            libc::SYSPROTO_CONTROL,
            2, // UTUN_OPT_IFNAME
            verify_buf.as_mut_ptr().cast(),
            &mut verify_len,
        )
    };
    if verify_ret == 0 {
        let verified_name = std::str::from_utf8(&verify_buf[..verify_len as usize])
            .unwrap_or("???")
            .trim_end_matches('\0');
        tracing::info!(
            fd = utun_fd.as_raw_fd(),
            verified_name,
            json_name = %name,
            "received and verified utun fd from helper"
        );
    } else {
        tracing::error!(
            fd = utun_fd.as_raw_fd(),
            errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1),
            "received fd from helper but getsockopt UTUN_OPT_IFNAME FAILED — fd may not be a utun"
        );
    }

    Ok((utun_fd, name))
}

/// Asks the helper to add a route.
pub fn add_route(subnet: &str, iface: &str) -> io::Result<()> {
    let mut stream = connect_and_hello("route")?;
    let req = format!(
        r#"{{"op":"add_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface,
    );
    send_op(&mut stream, &req)?;
    Ok(())
}

/// Asks the helper to remove a route.
pub fn remove_route(subnet: &str, iface: &str) -> io::Result<()> {
    let mut stream = connect_and_hello("route")?;
    let req = format!(
        r#"{{"op":"remove_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface,
    );
    send_op(&mut stream, &req)?;
    Ok(())
}

/// Probes helper availability: connect + hello handshake.
pub fn is_available() -> bool {
    connect_and_hello("probe").is_ok()
}
