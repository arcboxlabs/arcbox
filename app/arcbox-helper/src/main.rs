//! ArcBox privileged network helper.
//!
//! Root process listening on `/var/run/arcbox/helper.sock`. Executes
//! privileged network operations (utun creation, route management) on
//! behalf of the unprivileged arcbox-daemon.
//!
//! Protocol: newline-delimited JSON over STREAM socket. FD passing uses
//! a separate DGRAM socketpair to guarantee atomic delivery.
//!
//! Install: `sudo abctl daemon install`

mod network;
mod validate;

use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::Path;

use serde::{Deserialize, Serialize};

const SOCKET_PATH: &str = "/var/run/arcbox/helper.sock";
const SOCKET_DIR: &str = "/var/run/arcbox";
const CONFIG_PATH: &str = "/etc/arcbox/helper.json";

const PROTOCOL_VERSION: u32 = 1;

// ── Config ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HelperConfig {
    authorized_uid: u32,
    #[serde(default)]
    authorized_gid: u32,
}

fn load_config() -> Option<HelperConfig> {
    let data = std::fs::read_to_string(CONFIG_PATH).ok()?;
    serde_json::from_str(&data).ok()
}

// ── Protocol types ──────────────────────────────────────────────────────

/// Parses a line as either a hello or an op.
fn parse_request(line: &str) -> Result<ParsedRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("json parse: {e}"))?;

    if v.get("hello").is_some() {
        let hello: HelloRequest =
            serde_json::from_value(v["hello"].clone()).map_err(|e| format!("bad hello: {e}"))?;
        Ok(ParsedRequest::Hello(hello))
    } else if let Some(op) = v.get("op").and_then(|v| v.as_str()) {
        Ok(ParsedRequest::Op {
            op: op.to_string(),
            ip: v
                .get("ip")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            subnet: v
                .get("subnet")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            iface: v
                .get("iface")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            gateway: v
                .get("gateway")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    } else {
        Err("expected 'hello' or 'op' field".to_string())
    }
}

enum ParsedRequest {
    Hello(HelloRequest),
    Op {
        op: String,
        ip: String,
        subnet: String,
        iface: String,
        gateway: String,
    },
}

#[derive(Debug, Deserialize)]
struct HelloRequest {
    version: u32,
    session_id: String,
    #[serde(default = "default_mtu")]
    mtu: u16,
    #[serde(default)]
    features: Vec<String>,
}

fn default_mtu() -> u16 {
    1500
}

#[derive(Debug, Serialize)]
struct HelloResponse {
    hello: HelloResponseInner,
}

#[derive(Debug, Serialize)]
struct HelloResponseInner {
    version: u32,
    backend: String,
    session_id: String,
    features: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OpResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl OpResponse {
    fn success() -> Self {
        Self {
            ok: true,
            name: None,
            error: None,
        }
    }
    fn with_name(name: String) -> Self {
        Self {
            ok: true,
            name: Some(name),
            error: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            name: None,
            error: Some(msg.into()),
        }
    }
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt::init();

    let config = load_config();
    if let Some(ref c) = config {
        tracing::info!(
            uid = c.authorized_uid,
            gid = c.authorized_gid,
            "loaded config"
        );
    } else {
        tracing::warn!("no config at {CONFIG_PATH}, accepting all connections");
    }

    // Ensure socket directory.
    let _ = std::fs::create_dir_all(SOCKET_DIR);

    // Remove stale socket.
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "failed to bind {SOCKET_PATH}");
            std::process::exit(1);
        }
    };

    // Set socket permissions based on config.
    set_socket_permissions(&config);

    // Install signal handlers.
    install_signal_handlers();

    tracing::info!("arcbox-helper listening on {SOCKET_PATH}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = authorize_connection(&stream, &config) {
                    tracing::warn!(error = %e, "connection rejected");
                    continue;
                }
                handle_connection(stream);
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
}

// ── Connection handling ─────────────────────────────────────────────────

fn handle_connection(stream: std::os::unix::net::UnixStream) {
    let peer_fd = stream.as_raw_fd();
    let mut reader = BufReader::new(&stream);
    let mut hello_done = false;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !hello_done {
            match parse_request(trimmed) {
                Ok(ParsedRequest::Hello(hello)) => {
                    if hello.version > PROTOCOL_VERSION {
                        let _ = write_json(
                            &stream,
                            &serde_json::json!({
                                "error": "version_mismatch",
                                "server_version": PROTOCOL_VERSION,
                            }),
                        );
                        break;
                    }
                    let resp = handle_hello(&hello);
                    let _ = write_json(&stream, &resp);
                    hello_done = true;
                }
                _ => {
                    let _ = write_json(&stream, &serde_json::json!({"error": "expected hello"}));
                    break;
                }
            }
            continue;
        }

        let resp = match parse_request(trimmed) {
            Ok(ParsedRequest::Op {
                op,
                ip,
                subnet,
                iface,
                gateway,
            }) => dispatch_op(&op, &ip, &subnet, &iface, &gateway, peer_fd),
            Ok(ParsedRequest::Hello(_)) => OpResponse::err("duplicate hello"),
            Err(e) => OpResponse::err(e),
        };

        let _ = write_json(&stream, &resp);

        // For create_utun, fd was already sent inside dispatch_op.
        // Close after response for all ops.
        break;
    }
}

fn handle_hello(hello: &HelloRequest) -> HelloResponse {
    tracing::debug!(
        version = hello.version,
        session_id = %hello.session_id,
        mtu = hello.mtu,
        "hello received"
    );
    HelloResponse {
        hello: HelloResponseInner {
            version: PROTOCOL_VERSION,
            backend: "helper".to_string(),
            session_id: hello.session_id.clone(),
            features: vec![],
        },
    }
}

fn dispatch_op(
    op: &str,
    ip: &str,
    subnet: &str,
    iface: &str,
    gateway: &str,
    stream_fd: i32,
) -> OpResponse {
    match op {
        "create_utun" => match network::create_utun(stream_fd, ip) {
            Ok(name) => OpResponse::with_name(name),
            Err(e) => OpResponse::err(e.to_string()),
        },
        "add_route" => match network::add_route(subnet, iface) {
            Ok(()) => OpResponse::success(),
            Err(e) => OpResponse::err(e.to_string()),
        },
        "add_route_gw" => match network::add_route_gateway(subnet, gateway) {
            Ok(()) => OpResponse::success(),
            Err(e) => OpResponse::err(e.to_string()),
        },
        "remove_route" => match network::remove_route(subnet, iface) {
            Ok(()) => OpResponse::success(),
            Err(e) => OpResponse::err(e.to_string()),
        },
        other => OpResponse::err(format!("unknown op: {other}")),
    }
}

fn write_json(stream: &std::os::unix::net::UnixStream, value: &impl Serialize) -> io::Result<()> {
    let mut writer = stream;
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

// ── Authorization ───────────────────────────────────────────────────────

fn authorize_connection(
    stream: &std::os::unix::net::UnixStream,
    config: &Option<HelperConfig>,
) -> io::Result<()> {
    let Some(config) = config else {
        return Ok(()); // No config = accept all (dev mode).
    };

    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: getpeereid with valid socket fd and output pointers.
    let ret = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    if uid == 0 || uid == config.authorized_uid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "uid {uid} not authorized (expected {})",
                config.authorized_uid
            ),
        ))
    }
}

fn set_socket_permissions(config: &Option<HelperConfig>) {
    let c_path = std::ffi::CString::new(SOCKET_PATH).unwrap();
    if let Some(config) = config {
        // SAFETY: chown/chmod with valid path.
        unsafe {
            libc::chown(c_path.as_ptr(), 0, config.authorized_gid);
            libc::chmod(c_path.as_ptr(), 0o660);
        }
    } else {
        // Dev mode: world-accessible.
        unsafe {
            libc::chmod(c_path.as_ptr(), 0o777);
        }
    }
}

fn install_signal_handlers() {
    // SAFETY: signal handlers with valid signal numbers.
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, sigterm_handler as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

extern "C" fn sigterm_handler(_: libc::c_int) {
    let _ = std::fs::remove_file(SOCKET_PATH);
    std::process::exit(0);
}
