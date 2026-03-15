//! ArcBox privileged helper.
//!
//! A minimal root process that executes network operations requiring elevated
//! privileges on behalf of the unprivileged arcbox-daemon. Communicates via
//! a Unix domain socket at `/var/run/arcbox/helper.sock`.
//!
//! # Supported operations
//!
//! - `configure_utun`: Set IPv4 address on a utun interface and bring it UP
//! - `add_route`: Install a host route for a subnet via a utun interface
//! - `remove_route`: Remove a host route
//!
//! # Installation
//!
//! ```bash
//! sudo cp arcbox-helper /usr/local/bin/
//! sudo arcbox daemon install  # creates LaunchDaemon plist
//! ```
//!
//! # Protocol
//!
//! Newline-delimited JSON over the Unix socket. Each connection sends one
//! request and receives one response, then closes.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

const SOCKET_PATH: &str = "/var/run/arcbox/helper.sock";
const SOCKET_DIR: &str = "/var/run/arcbox";

fn main() {
    tracing_subscriber::fmt::init();

    // Ensure socket directory exists.
    if let Err(e) = std::fs::create_dir_all(SOCKET_DIR) {
        eprintln!("Failed to create {SOCKET_DIR}: {e}");
        std::process::exit(1);
    }

    // Remove stale socket.
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {SOCKET_PATH}: {e}");
            std::process::exit(1);
        }
    };

    // Allow non-root users to connect.
    set_socket_permissions(SOCKET_PATH);

    tracing::info!("arcbox-helper listening on {SOCKET_PATH}");

    // Install signal handler for clean shutdown.
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let running = running.clone();
        if let Err(e) = ctrlc(running) {
            tracing::warn!(error = %e, "failed to install signal handler");
        }
    }

    for stream in listener.incoming() {
        if !running.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        match stream {
            Ok(stream) => handle_connection(stream),
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }

    let _ = std::fs::remove_file(SOCKET_PATH);
    tracing::info!("arcbox-helper stopped");
}

fn ctrlc(
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: signal() with valid signal number.
    unsafe {
        libc::signal(libc::SIGTERM, {
            extern "C" fn handler(_: libc::c_int) {
                // Best-effort: can't access `running` here, but the accept()
                // call will return an error after the socket is closed.
                let _ = std::fs::remove_file(SOCKET_PATH);
                std::process::exit(0);
            }
            handler as libc::sighandler_t
        });
    }
    // Also handle Ctrl-C for interactive use.
    unsafe {
        libc::signal(libc::SIGINT, {
            extern "C" fn handler(_: libc::c_int) {
                let _ = std::fs::remove_file(SOCKET_PATH);
                std::process::exit(0);
            }
            handler as libc::sighandler_t
        });
    }
    let _ = running;
    Ok(())
}

// ── Protocol ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    /// utun interface name (e.g. "utun13").
    #[serde(default)]
    name: String,
    /// IPv4 address for configure_utun.
    #[serde(default)]
    ip: String,
    /// Subnet CIDR for add_route/remove_route (e.g. "172.16.0.0/12").
    #[serde(default)]
    subnet: String,
    /// Interface for routing (e.g. "utun13").
    #[serde(default)]
    iface: String,
}

#[derive(Debug, Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn success() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
        }
    }
}

fn handle_connection(stream: std::os::unix::net::UnixStream) {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    if reader.read_line(&mut line).is_err() {
        return;
    }

    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => dispatch(req),
        Err(e) => Response::err(format!("invalid request: {e}")),
    };

    let mut writer = &stream;
    let _ = serde_json::to_writer(&mut writer, &resp);
    let _ = writer.write_all(b"\n");
}

fn dispatch(req: Request) -> Response {
    match req.op.as_str() {
        "configure_utun" => op_configure_utun(&req.name, &req.ip),
        "add_route" => op_add_route(&req.subnet, &req.iface),
        "remove_route" => op_remove_route(&req.subnet, &req.iface),
        other => Response::err(format!("unknown op: {other}")),
    }
}

// ── Operations ──────────────────────────────────────────────────────────────

fn op_configure_utun(name: &str, ip: &str) -> Response {
    // Validate interface name to prevent command injection.
    if !name.starts_with("utun") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
        return Response::err(format!("invalid interface name: {name}"));
    }
    if ip.parse::<std::net::Ipv4Addr>().is_err() {
        return Response::err(format!("invalid IP: {ip}"));
    }

    // ifconfig utunN inet <ip> <ip> up
    match Command::new("/sbin/ifconfig")
        .args([name, "inet", ip, ip, "up"])
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(name, ip, "configured utun");
            Response::success()
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(name, ip, stderr = %stderr, "ifconfig failed");
            Response::err(stderr.to_string())
        }
        Err(e) => Response::err(format!("exec ifconfig: {e}")),
    }
}

fn op_add_route(subnet: &str, iface: &str) -> Response {
    if !validate_cidr(subnet) {
        return Response::err(format!("invalid subnet: {subnet}"));
    }
    if !validate_iface(iface) {
        return Response::err(format!("invalid interface: {iface}"));
    }

    match Command::new("/sbin/route")
        .args(["-n", "add", "-net", subnet, "-interface", iface])
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(subnet, iface, "route added");
            Response::success()
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("File exists") {
                tracing::debug!(subnet, iface, "route already exists");
                Response::success()
            } else {
                tracing::warn!(subnet, iface, stderr = %stderr, "route add failed");
                Response::err(stderr.to_string())
            }
        }
        Err(e) => Response::err(format!("exec route: {e}")),
    }
}

fn op_remove_route(subnet: &str, iface: &str) -> Response {
    if !validate_cidr(subnet) {
        return Response::err(format!("invalid subnet: {subnet}"));
    }
    if !validate_iface(iface) {
        return Response::err(format!("invalid interface: {iface}"));
    }

    match Command::new("/sbin/route")
        .args(["-n", "delete", "-net", subnet, "-interface", iface])
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::info!(subnet, iface, "route removed");
            Response::success()
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::debug!(subnet, iface, stderr = %stderr, "route delete (may not exist)");
            Response::success() // Best-effort removal.
        }
        Err(e) => Response::err(format!("exec route: {e}")),
    }
}

// ── Validation ──────────────────────────────────────────────────────────────

fn validate_cidr(s: &str) -> bool {
    let Some((addr, prefix)) = s.split_once('/') else {
        return false;
    };
    addr.parse::<std::net::Ipv4Addr>().is_ok()
        && prefix.parse::<u8>().is_ok_and(|p| p <= 32)
}

fn validate_iface(s: &str) -> bool {
    s.starts_with("utun") && s[4..].chars().all(|c| c.is_ascii_digit())
}

fn set_socket_permissions(path: &str) {
    // Allow group/other to connect (0o777 on the socket).
    // SAFETY: valid path and mode.
    let c_path = std::ffi::CString::new(path).unwrap();
    unsafe {
        libc::chmod(c_path.as_ptr(), 0o777);
    }
}
