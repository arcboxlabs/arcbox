//! Client for the ArcBox privileged helper.
//!
//! Communicates with `arcbox-helper` via the Unix socket at
//! `/var/run/arcbox/helper.sock` to execute privileged network operations
//! (utun configuration, route management).

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const HELPER_SOCKET: &str = "/var/run/arcbox/helper.sock";

/// Sends a request to the helper and returns the response.
fn send_request(json: &str) -> io::Result<HelperResponse> {
    let mut stream =
        UnixStream::connect(HELPER_SOCKET).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "cannot connect to arcbox-helper at {HELPER_SOCKET}: {e}. \
                     Run 'sudo arcbox-helper' or install with 'arcbox daemon install'."
                ),
            )
        })?;

    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let resp: HelperResponse = serde_json::from_str(line.trim())
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
struct HelperResponse {
    ok: bool,
    error: Option<String>,
}

/// Asks the helper to configure a utun interface (set IP + bring UP).
pub fn configure_utun(name: &str, ip: &str) -> io::Result<()> {
    let req = format!(
        r#"{{"op":"configure_utun","name":"{}","ip":"{}"}}"#,
        name, ip
    );
    send_request(&req)?;
    Ok(())
}

/// Asks the helper to add a route.
pub fn add_route(subnet: &str, iface: &str) -> io::Result<()> {
    let req = format!(
        r#"{{"op":"add_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface
    );
    send_request(&req)?;
    Ok(())
}

/// Asks the helper to remove a route.
pub fn remove_route(subnet: &str, iface: &str) -> io::Result<()> {
    let req = format!(
        r#"{{"op":"remove_route","subnet":"{}","iface":"{}"}}"#,
        subnet, iface
    );
    send_request(&req)?;
    Ok(())
}

/// Returns true if the helper socket exists and is connectable.
pub fn is_available() -> bool {
    UnixStream::connect(HELPER_SOCKET).is_ok()
}
