use std::net::{IpAddr, Ipv4Addr};

use super::{
    ETH_HEADER_LEN,
    checksum::{ipv4_header_checksum, tcp_checksum},
};

/// Parameters for constructing TCP frames on the fast path.
///
/// Both addresses must belong to the same IP family. IPv4 remains the default type.
#[derive(Debug, Clone, Copy)]
pub struct TcpFrameParams<A = Ipv4Addr> {
    pub src_ip: A,
    pub dst_ip: A,
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub window: u16,
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
}

fn build_frame<A: Copy + Into<IpAddr>>(
    p: &TcpFrameParams<A>,
    flags: u8,
    options: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let src = p.src_ip.into();
    let dst = p.dst_ip.into();
    let tcp_header_len = 20 + options.len();
    let tcp_len = tcp_header_len + payload.len();
    let ip_len = match (src, dst) {
        (IpAddr::V4(_), IpAddr::V4(_)) => 20,
        (IpAddr::V6(_), IpAddr::V6(_)) => 40,
        _ => panic!("TCP frame addresses must use the same IP family"),
    };
    let length = if ip_len == 20 {
        ip_len + tcp_len
    } else {
        tcp_len
    };
    let length = u16::try_from(length).expect("TCP frame exceeds the IP length field");
    let tcp = ETH_HEADER_LEN + ip_len;
    let mut frame = Vec::with_capacity(tcp + tcp_len);
    // Reserved fields must be zero; payload bytes are copied without an initial clear.
    frame.resize(tcp + 20, 0);
    frame[..6].copy_from_slice(&p.dst_mac);
    frame[6..12].copy_from_slice(&p.src_mac);
    let ip = ETH_HEADER_LEN;
    match (src, dst) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => {
            frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
            frame[ip] = 0x45;
            frame[ip + 2..ip + 4].copy_from_slice(&length.to_be_bytes());
            frame[ip + 6..ip + 8].copy_from_slice(&0x4000u16.to_be_bytes());
            frame[ip + 8] = 64;
            frame[ip + 9] = 6;
            frame[ip + 12..ip + 16].copy_from_slice(&src.octets());
            frame[ip + 16..ip + 20].copy_from_slice(&dst.octets());
            let checksum = ipv4_header_checksum(&frame[ip..ip + 20]);
            frame[ip + 10..ip + 12].copy_from_slice(&checksum.to_be_bytes());
        }
        (IpAddr::V6(src), IpAddr::V6(dst)) => {
            frame[12..14].copy_from_slice(&0x86ddu16.to_be_bytes());
            frame[ip] = 0x60;
            frame[ip + 4..ip + 6].copy_from_slice(&length.to_be_bytes());
            frame[ip + 6] = 6;
            frame[ip + 7] = 64;
            frame[ip + 8..ip + 24].copy_from_slice(&src.octets());
            frame[ip + 24..ip + 40].copy_from_slice(&dst.octets());
        }
        _ => unreachable!(),
    }
    frame[tcp..tcp + 2].copy_from_slice(&p.src_port.to_be_bytes());
    frame[tcp + 2..tcp + 4].copy_from_slice(&p.dst_port.to_be_bytes());
    frame[tcp + 4..tcp + 8].copy_from_slice(&p.seq.to_be_bytes());
    frame[tcp + 8..tcp + 12].copy_from_slice(&p.ack.to_be_bytes());
    frame[tcp + 12] = ((tcp_header_len / 4) as u8) << 4;
    frame[tcp + 13] = flags;
    frame[tcp + 14..tcp + 16].copy_from_slice(&p.window.to_be_bytes());
    frame.extend_from_slice(options);
    frame.extend_from_slice(payload);
    let checksum = match (src, dst) {
        (IpAddr::V4(src), IpAddr::V4(dst)) => tcp_checksum(src, dst, &frame[tcp..]),
        (IpAddr::V6(src), IpAddr::V6(dst)) => {
            let mut sum = crate::checksum::checksum_add(&src.octets())
                + crate::checksum::checksum_add(&dst.octets())
                + tcp_len as u32
                + 6
                + crate::checksum::checksum_add(&frame[tcp..]);
            while sum > 0xffff {
                sum = (sum & 0xffff) + (sum >> 16);
            }
            !sum as u16
        }
        _ => unreachable!(),
    };
    frame[tcp + 16..tcp + 18].copy_from_slice(&checksum.to_be_bytes());
    frame
}

/// Builds a fully checksummed TCP ACK frame without payload.
///
/// # Panics
///
/// Panics if the addresses have different IP families.
#[must_use]
pub fn build_tcp_ack_frame<A: Copy + Into<IpAddr>>(p: &TcpFrameParams<A>) -> Vec<u8> {
    build_frame(p, 0x10, &[], &[])
}

/// Builds a fully checksummed TCP ACK/PSH frame carrying payload.
///
/// # Panics
///
/// Panics if the addresses have different IP families or the payload exceeds
/// 65495 bytes for IPv4 or 65515 bytes for IPv6.
#[must_use]
pub fn build_tcp_data_frame<A: Copy + Into<IpAddr>>(
    p: &TcpFrameParams<A>,
    payload: &[u8],
) -> Vec<u8> {
    build_frame(p, 0x18, &[], payload)
}

/// Builds a TCP FIN/ACK frame with a receive window of 65535.
///
/// # Panics
///
/// Panics if the addresses have different IP families.
#[must_use]
pub fn build_tcp_fin_frame<A: Copy + Into<IpAddr>>(p: &TcpFrameParams<A>) -> Vec<u8> {
    build_frame(
        &TcpFrameParams {
            window: 65535,
            ..*p
        },
        0x11,
        &[],
        &[],
    )
}

/// Builds a TCP RST/ACK frame with a zero receive window.
///
/// # Panics
///
/// Panics if the addresses have different IP families.
#[must_use]
pub fn build_tcp_rst_frame<A: Copy + Into<IpAddr>>(p: &TcpFrameParams<A>) -> Vec<u8> {
    build_frame(&TcpFrameParams { window: 0, ..*p }, 0x14, &[], &[])
}

/// Builds a TCP data frame with only a pseudo-header checksum (for GSO).
///
/// Identical to [`build_tcp_data_frame`] except the TCP checksum field
/// contains only the pseudo-header checksum. The guest kernel completes
/// the checksum per-segment during GSO segmentation.
#[must_use]
pub fn build_tcp_data_frame_partial_csum(p: &TcpFrameParams, payload: &[u8]) -> Vec<u8> {
    let TcpFrameParams {
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        seq,
        ack,
        window,
        src_mac,
        dst_mac,
    } = *p;

    let tcp_hdr_len = 20;
    let tcp_total_len = tcp_hdr_len + payload.len();
    let ip_total_len = 20 + tcp_total_len;
    // Same IPv4 total_length overflow guard as `build_tcp_data_frame`.
    // Callers are GSO-oriented and bounded by the RX descriptor budget,
    // but asserting here keeps us honest if that ever slips.
    assert!(
        u16::try_from(ip_total_len).is_ok(),
        "build_tcp_data_frame_partial_csum: ip_total_len={ip_total_len} overflows u16"
    );
    let frame_len = ETH_HEADER_LEN + ip_total_len;
    let mut frame = Vec::with_capacity(frame_len);
    // Preserve zeroed reserved fields without clearing payload bytes before copying.
    frame.resize(ETH_HEADER_LEN + 20 + tcp_hdr_len, 0);

    // -- Ethernet header --
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes());

    // -- IPv4 header --
    let ip = ETH_HEADER_LEN;
    frame[ip] = 0x45;
    frame[ip + 2..ip + 4].copy_from_slice(&(ip_total_len as u16).to_be_bytes());
    frame[ip + 6..ip + 8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
    frame[ip + 8] = 64; // TTL
    frame[ip + 9] = 6; // TCP
    frame[ip + 12..ip + 16].copy_from_slice(&src_ip.octets());
    frame[ip + 16..ip + 20].copy_from_slice(&dst_ip.octets());
    let ip_cksum = ipv4_header_checksum(&frame[ip..ip + 20]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_cksum.to_be_bytes());

    // -- TCP header --
    let tcp = ip + 20;
    frame[tcp..tcp + 2].copy_from_slice(&src_port.to_be_bytes());
    frame[tcp + 2..tcp + 4].copy_from_slice(&dst_port.to_be_bytes());
    frame[tcp + 4..tcp + 8].copy_from_slice(&seq.to_be_bytes());
    frame[tcp + 8..tcp + 12].copy_from_slice(&ack.to_be_bytes());
    frame[tcp + 12] = 0x50; // Data offset: 5
    frame[tcp + 13] = 0x18; // Flags: ACK | PSH
    frame[tcp + 14..tcp + 16].copy_from_slice(&window.to_be_bytes());

    // -- Payload --
    frame.extend_from_slice(payload);

    // Pseudo-header-only TCP checksum. The guest kernel completes it
    // per-segment during GSO segmentation (VIRTIO_NET_HDR_F_NEEDS_CSUM).
    let pseudo_cksum = tcp_pseudo_header_checksum(src_ip, dst_ip, tcp_total_len);
    frame[tcp + 16..tcp + 18].copy_from_slice(&pseudo_cksum.to_be_bytes());

    frame
}

/// Computes the TCP pseudo-header checksum only (for GSO offload).
///
/// The guest kernel adds the TCP header + payload contribution per segment.
#[must_use]
pub fn tcp_pseudo_header_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, tcp_len: usize) -> u16 {
    let mut sum: u32 = 0;
    let src = src_ip.octets();
    let dst = dst_ip.octets();
    sum += u32::from(u16::from_be_bytes([src[0], src[1]]));
    sum += u32::from(u16::from_be_bytes([src[2], src[3]]));
    sum += u32::from(u16::from_be_bytes([dst[0], dst[1]]));
    sum += u32::from(u16::from_be_bytes([dst[2], dst[3]]));
    sum += 6u32; // TCP protocol
    sum += tcp_len as u32;
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !sum as u16
}

/// Peer TCP options observed on an incoming SYN or SYN-ACK frame.
///
/// Captured so the handshake synthesizer can mirror the peer's negotiated
/// options back in its SYN-ACK response (or record them for later use).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TcpSynOptions {
    /// Maximum Segment Size advertised by the peer. `None` if the option
    /// was absent (RFC 9293 §3.7.1 says default 536, but we treat absence
    /// as "use our default").
    pub mss: Option<u16>,
    /// Window scale shift count (0–14). `None` if the peer did not advertise it.
    pub wscale: Option<u8>,
    /// Whether the peer included SACK-Permitted.
    pub sack_permitted: bool,
    /// Whether the peer included Timestamps. We do not echo these back —
    /// both endpoints must send TSopt in their SYN for it to be used
    /// (RFC 7323 §3.1), so not echoing disables timestamps cleanly.
    pub timestamps: bool,
}

/// Parses the TCP options field of a SYN (or SYN-ACK) segment.
///
/// `tcp_segment` is the TCP header + options + (optionally) payload. The
/// `data_offset` field (upper nibble of byte 12, in 32-bit words) determines
/// where the options end. Malformed or unknown options cause early
/// termination without returning an error — the returned struct reflects
/// only what was parsed successfully.
#[must_use]
pub fn parse_tcp_syn_options(tcp_segment: &[u8]) -> TcpSynOptions {
    let mut opts = TcpSynOptions::default();
    if tcp_segment.len() < 20 {
        return opts;
    }
    let data_offset = usize::from(tcp_segment[12] >> 4) * 4;
    if data_offset < 20 || data_offset > tcp_segment.len() {
        return opts;
    }
    let options = &tcp_segment[20..data_offset];
    let mut i = 0;
    while i < options.len() {
        let kind = options[i];
        match kind {
            0 => break, // End of option list
            1 => {
                i += 1;
            } // NOP
            2 => {
                // MSS
                if i + 4 > options.len() || options[i + 1] != 4 {
                    break;
                }
                opts.mss = Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                i += 4;
            }
            3 => {
                // Window scale
                if i + 3 > options.len() || options[i + 1] != 3 {
                    break;
                }
                opts.wscale = Some(options[i + 2]);
                i += 3;
            }
            4 => {
                // SACK-Permitted
                if i + 2 > options.len() || options[i + 1] != 2 {
                    break;
                }
                opts.sack_permitted = true;
                i += 2;
            }
            8 => {
                // Timestamps
                if i + 10 > options.len() || options[i + 1] != 10 {
                    break;
                }
                opts.timestamps = true;
                i += 10;
            }
            _ => {
                // Skip using the Length field if present; bail if malformed.
                if i + 1 >= options.len() {
                    break;
                }
                let len = usize::from(options[i + 1]);
                if len < 2 || i + len > options.len() {
                    break;
                }
                i += len;
            }
        }
    }
    opts
}

/// Parameters for constructing a TCP SYN-ACK frame.
///
/// Used by the handshake synthesizer to respond to a guest SYN without
/// going through a userspace TCP state machine. The caller supplies `mss`;
/// `wscale` and `sack_permitted` follow the peer advertisement.
/// Both addresses must have the same IP family; IPv4 is the default type.
#[derive(Debug, Clone, Copy)]
pub struct SynAckParams<A = Ipv4Addr> {
    pub src_ip: A,
    pub dst_ip: A,
    pub src_port: u16,
    pub dst_port: u16,
    /// Our chosen ISN (appears as SEQ in the SYN-ACK).
    pub seq: u32,
    /// Peer ISN + 1 (appears as ACK in the SYN-ACK).
    pub ack: u32,
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    /// MSS to advertise (typically 1460 for Ethernet with DF bit set).
    pub mss: u16,
    /// Window scale to advertise. `None` means don't include the option
    /// (peer did not send WScale in its SYN, so we must not negotiate it).
    pub wscale: Option<u8>,
    /// Whether to include SACK-Permitted.
    pub sack_permitted: bool,
}

fn syn_options(mss: u16, wscale: Option<u8>, sack_permitted: bool) -> Vec<u8> {
    let mut options = Vec::with_capacity(12);
    options.extend_from_slice(&[2, 4]);
    options.extend_from_slice(&mss.to_be_bytes());
    if sack_permitted {
        options.extend_from_slice(&[4, 2]);
    }
    if let Some(shift) = wscale {
        options.extend_from_slice(&[3, 3, shift]);
    }
    while !options.len().is_multiple_of(4) {
        options.push(1);
    }
    options
}

/// Builds a TCP SYN-ACK frame that accepts a guest SYN.
///
/// Options included: MSS (always), WScale (if `Some`), SACK-Permitted (if true).
/// Timestamps are omitted — not echoing disables TSopt for the connection,
/// saving 12 bytes/segment of overhead.
///
/// # Panics
///
/// Panics if the addresses have different IP families.
#[must_use]
pub fn build_tcp_syn_ack_frame<A: Copy + Into<IpAddr>>(p: &SynAckParams<A>) -> Vec<u8> {
    let params = TcpFrameParams {
        src_ip: p.src_ip,
        dst_ip: p.dst_ip,
        src_port: p.src_port,
        dst_port: p.dst_port,
        seq: p.seq,
        ack: p.ack,
        window: 65535,
        src_mac: p.src_mac,
        dst_mac: p.dst_mac,
    };
    build_frame(
        &params,
        0x12,
        &syn_options(p.mss, p.wscale, p.sack_permitted),
        &[],
    )
}

/// Parameters for constructing a TCP SYN frame (active open).
///
/// Used when arcbox initiates a TCP connection toward the guest for an
/// inbound port-forward. MSS is always included. WScale and SACK-Permitted
/// are included only when requested by the caller. Both addresses must have
/// the same IP family; IPv4 is the default type.
#[derive(Debug, Clone, Copy)]
pub struct SynParams<A = Ipv4Addr> {
    pub src_ip: A,
    pub dst_ip: A,
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub mss: u16,
    pub wscale: Option<u8>,
    /// Whether to include SACK-Permitted. Enable only when the caller supports SACK.
    pub sack_permitted: bool,
}

/// Builds a TCP SYN frame for active open toward the guest.
///
/// # Panics
///
/// Panics if the addresses have different IP families.
#[must_use]
pub fn build_tcp_syn_frame<A: Copy + Into<IpAddr>>(p: &SynParams<A>) -> Vec<u8> {
    let params = TcpFrameParams {
        src_ip: p.src_ip,
        dst_ip: p.dst_ip,
        src_port: p.src_port,
        dst_port: p.dst_port,
        seq: p.seq,
        ack: 0,
        window: 65535,
        src_mac: p.src_mac,
        dst_mac: p.dst_mac,
    };
    build_frame(
        &params,
        0x02,
        &syn_options(p.mss, p.wscale, p.sack_permitted),
        &[],
    )
}
