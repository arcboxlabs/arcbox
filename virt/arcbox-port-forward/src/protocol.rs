//! Wire protocol for the vsock port forwarding handshake.
//!
//! The protocol is minimal: a 6-byte connect request followed by a
//! 1-byte status response. After a successful handshake, the vsock
//! stream carries raw TCP payload bidirectionally.
//!
//! ```text
//! Host → Guest:  [target_ip: 4 bytes][target_port: 2 bytes BE]
//! Guest → Host:  [status: 1 byte]
//! ```

use std::net::Ipv4Addr;

/// Length of the connect request header (4 bytes IP + 2 bytes port).
pub const HEADER_LEN: usize = 6;

/// Connect succeeded — bidirectional relay follows.
pub const STATUS_OK: u8 = 0x00;

/// Target actively refused the connection.
pub const STATUS_REFUSED: u8 = 0x01;

/// Target unreachable (timeout, network error, etc.).
pub const STATUS_UNREACHABLE: u8 = 0x02;

/// Internal guest error.
pub const STATUS_ERROR: u8 = 0xFF;

/// Encodes a connect request header.
#[must_use]
pub fn encode_header(ip: Ipv4Addr, port: u16) -> [u8; HEADER_LEN] {
    let octets = ip.octets();
    let port_be = port.to_be_bytes();
    [
        octets[0], octets[1], octets[2], octets[3], port_be[0], port_be[1],
    ]
}

/// Decodes a connect request header.
#[must_use]
pub fn decode_header(buf: &[u8; HEADER_LEN]) -> (Ipv4Addr, u16) {
    let ip = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
    let port = u16::from_be_bytes([buf[4], buf[5]]);
    (ip, port)
}

/// Returns a human-readable description of a status code.
#[must_use]
pub fn status_description(status: u8) -> &'static str {
    match status {
        STATUS_OK => "connected",
        STATUS_REFUSED => "connection refused",
        STATUS_UNREACHABLE => "unreachable",
        STATUS_ERROR => "internal error",
        _ => "unknown error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_header() {
        let ip = Ipv4Addr::new(127, 0, 0, 1);
        let port = 5201;
        let encoded = encode_header(ip, port);
        let (decoded_ip, decoded_port) = decode_header(&encoded);
        assert_eq!(decoded_ip, ip);
        assert_eq!(decoded_port, port);
    }

    #[test]
    fn encode_known_bytes() {
        let header = encode_header(Ipv4Addr::new(10, 0, 2, 1), 80);
        assert_eq!(header, [10, 0, 2, 1, 0, 80]);
    }
}
