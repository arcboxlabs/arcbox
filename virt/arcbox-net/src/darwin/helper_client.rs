//! Client for the ArcBox privileged network helper.
//!
//! Communicates with `arcbox-helper` via Unix socket at
//! `/var/run/arcbox/helper.sock` for privileged route management.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const HELPER_SOCKET: &str = "/var/run/arcbox/helper.sock";

/// Opens a connection and performs hello handshake.
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

    let hello = format!(
        r#"{{"hello":{{"version":1,"session_id":"{}","mtu":1500,"features":[]}}}}"#,
        session_id,
    );
    stream.write_all(hello.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    if line.contains("\"error\"") {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("helper hello failed: {}", line.trim()),
        ));
    }

    Ok(stream)
}

/// Sends a JSON op and reads response.
fn send_op(stream: &mut UnixStream, json: &str) -> io::Result<()> {
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            resp["error"].as_str().unwrap_or("unknown error"),
        ))
    }
}

/// Adds a gateway route via the helper.
pub fn add_route_gateway(subnet: &str, gateway: &str) -> io::Result<()> {
    let mut stream = connect_and_hello("route")?;
    let req = format!(
        r#"{{"op":"add_route_gw","subnet":"{}","gateway":"{}"}}"#,
        subnet, gateway,
    );
    send_op(&mut stream, &req)
}

/// Adds an interface route via the helper.
pub fn add_route(subnet: &str, iface: &str) -> io::Result<()> {
    let mut stream = connect_and_hello("route")?;
    let req = format!(
        r#"{{"op":"add_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface,
    );
    send_op(&mut stream, &req)
}

/// Removes a route via the helper.
pub fn remove_route(subnet: &str, iface: &str) -> io::Result<()> {
    let mut stream = connect_and_hello("route")?;
    let req = format!(
        r#"{{"op":"remove_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface,
    );
    send_op(&mut stream, &req)
}

/// Probes helper availability: connect + hello.
pub fn is_available() -> bool {
    connect_and_hello("probe").is_ok()
}
