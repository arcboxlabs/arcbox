//! Proxy tunnel implementations: HTTP CONNECT and SOCKS5.
//!
//! Used by [`TcpBridge`] when the host has a system proxy configured, to
//! connect using the domain name (from [`DnsResolutionLog`]) rather than the
//! raw IP address. This is critical for fake-ip proxy environments where the
//! destination IP is a virtual address that only the proxy can resolve.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Establishes a TCP tunnel via an HTTP CONNECT proxy (RFC 7231 §4.3.6).
///
/// The returned `TcpStream` is an end-to-end tunnel — all subsequent reads
/// and writes go directly to the target host through the proxy.
pub async fn connect_via_http_proxy(
    proxy: SocketAddr,
    host: &str,
    port: u16,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy).await?;

    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;

    // Read the proxy's response. HTTP CONNECT responses are typically short.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "proxy closed connection before responding",
        ));
    }

    let response = String::from_utf8_lossy(&buf[..n]);
    // Accept any 2xx status as success (200, 204, etc).
    let status_ok = response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2");

    if status_ok {
        tracing::debug!(
            proxy = %proxy,
            target = %format!("{host}:{port}"),
            "HTTP CONNECT tunnel established"
        );
        Ok(stream)
    } else {
        let status_line = response.lines().next().unwrap_or("<empty>");
        Err(io::Error::other(format!(
            "HTTP CONNECT proxy rejected: {status_line}"
        )))
    }
}

/// Establishes a TCP tunnel via a SOCKS5 proxy (RFC 1928, no-auth subset).
///
/// Uses ATYP=0x03 (domain name) so the proxy resolves the hostname, avoiding
/// fake-ip issues entirely.
pub async fn connect_via_socks5(proxy: SocketAddr, host: &str, port: u16) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy).await?;

    // Phase 1: Greeting — version=5, 1 method (no auth).
    stream.write_all(&[0x05, 0x01, 0x00]).await?;

    let mut greeting_resp = [0u8; 2];
    stream.read_exact(&mut greeting_resp).await?;

    if greeting_resp[0] != 0x05 {
        return Err(io::Error::other(format!(
            "SOCKS5: unsupported version {}",
            greeting_resp[0]
        )));
    }
    if greeting_resp[1] == 0xFF {
        return Err(io::Error::other(
            "SOCKS5: no acceptable authentication method",
        ));
    }

    // Phase 2: Connect request — ATYP=0x03 (domain name).
    let host_bytes = host.as_bytes();
    if host_bytes.len() > 255 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "SOCKS5: domain name too long",
        ));
    }

    let mut req = Vec::with_capacity(7 + host_bytes.len());
    req.push(0x05); // version
    req.push(0x01); // cmd: CONNECT
    req.push(0x00); // reserved
    req.push(0x03); // atyp: domain name
    req.push(host_bytes.len() as u8);
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;

    // Phase 3: Read response (at least 10 bytes for IPv4 bind addr).
    let mut resp = [0u8; 10];
    stream.read_exact(&mut resp).await?;

    if resp[0] != 0x05 {
        return Err(io::Error::other(format!(
            "SOCKS5: unexpected version in response: {}",
            resp[0]
        )));
    }

    if resp[1] != 0x00 {
        let reason = match resp[1] {
            0x01 => "general SOCKS server failure",
            0x02 => "connection not allowed by ruleset",
            0x03 => "network unreachable",
            0x04 => "host unreachable",
            0x05 => "connection refused",
            0x06 => "TTL expired",
            0x07 => "command not supported",
            0x08 => "address type not supported",
            _ => "unknown error",
        };
        return Err(io::Error::other(format!(
            "SOCKS5: connect failed: {reason} (code {})",
            resp[1]
        )));
    }

    // Consume remaining bind address bytes based on ATYP.
    match resp[3] {
        0x01 => {} // IPv4: already read 4 bytes + 2 port = within resp
        0x04 => {
            // IPv6: need 12 more bytes (16 total - 4 already read).
            let mut extra = [0u8; 12];
            stream.read_exact(&mut extra).await?;
        }
        0x03 => {
            // Domain: read length + domain + port. The 4th byte we read
            // as IPv4 octets is actually the domain length.
            let dlen = resp[4] as usize;
            if dlen > 0 {
                let remaining = dlen.saturating_sub(4) + 2; // domain bytes - already read + port
                if remaining > 0 {
                    let mut extra = vec![0u8; remaining];
                    stream.read_exact(&mut extra).await?;
                }
            }
        }
        _ => {}
    }

    tracing::debug!(
        proxy = %proxy,
        target = %format!("{host}:{port}"),
        "SOCKS5 tunnel established"
    );
    Ok(stream)
}

#[cfg(test)]
mod tests {
    // Integration tests require a running proxy server. These are tested
    // manually against Surge/Clash in development.
    //
    // TODO: Add mock proxy server tests.
}
