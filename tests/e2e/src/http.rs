//! A minimal host-side HTTP client for reaching guest workloads.
//!
//! Plain HTTP/1.0 over `std::net`, so a scenario depends on neither `curl`
//! on the runner nor an HTTP crate.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Per-attempt read/write budget for one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One `GET /` against `addr`, returning the raw response.
///
/// # Errors
///
/// Returns an error if the connection, the request, or the read fails.
pub fn get(addr: &str) -> Result<String> {
    let mut stream = TcpStream::connect(addr).context("connect")?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .context("write request")?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("read response")?;
    Ok(response)
}

/// Retries [`get`] until a response arrives or `grace` elapses — a server
/// that is starting, or a forward still being set up, refuses for a while.
///
/// # Errors
///
/// Returns the last attempt's error once `grace` is spent.
pub fn get_with_retry(addr: &str, grace: Duration) -> Result<String> {
    let started = Instant::now();
    let mut last: Option<anyhow::Error> = None;
    while started.elapsed() < grace {
        match get(addr) {
            Ok(body) => return Ok(body),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no attempt was made")))
        .with_context(|| format!("no successful response within {grace:?}"))
}
