//! Build bounded TCP frames for first transmissions and retransmissions.

use super::super::{FastPathConn, GSO_SEGMENT_MSS, TCP_MIN_MSS};
use crate::ethernet::ETH_HEADER_LEN;

/// Frame-construction context shared by the first-transmission and
/// retransmission paths of `poll_fast_path`.
pub(super) struct FrameCtx {
    pub(super) large_frames: bool,
    pub(super) guest_mss: usize,
    pub(super) gw_mac: [u8; 6],
    pub(super) guest_mac: [u8; 6],
}

/// Builds guest-bound TCP data frames for `data` starting at `seq_start`:
/// one GSO-style frame when enabled and the peer accepts 1460-byte
/// segments, MSS-clamped segmentation otherwise (each emitted IP packet
/// must fit every link on the path — e.g. a 1500-MTU docker bridge behind
/// a 4000-MTU eth0). Does NOT advance `conn.our_seq` — retransmissions
/// re-emit already-consumed sequence space.
pub(super) fn emit_data_frames(
    ctx: &FrameCtx,
    conn: &FastPathConn,
    seq_start: u32,
    data: &[u8],
    frames: &mut Vec<Vec<u8>>,
) {
    if ctx.large_frames && conn.peer_mss >= GSO_SEGMENT_MSS {
        let large = ETH_HEADER_LEN + 40 + data.len() > 1500;
        let params = crate::ethernet::TcpFrameParams {
            src_ip: conn.remote_ip,
            dst_ip: conn.guest_ip,
            src_port: conn.remote_port,
            dst_port: conn.guest_port,
            seq: seq_start,
            ack: conn.last_ack,
            window: 65535,
            src_mac: ctx.gw_mac,
            dst_mac: ctx.guest_mac,
        };
        frames.push(if large {
            crate::ethernet::build_tcp_data_frame_partial_csum(&params, data)
        } else {
            crate::ethernet::build_tcp_data_frame(&params, data)
        });
        return;
    }
    let seg = ctx
        .guest_mss
        .min(usize::from(conn.peer_mss.max(TCP_MIN_MSS)));
    let mut offset = 0;
    while offset < data.len() {
        let chunk_end = (offset + seg).min(data.len());
        frames.push(crate::ethernet::build_tcp_data_frame(
            &crate::ethernet::TcpFrameParams {
                src_ip: conn.remote_ip,
                dst_ip: conn.guest_ip,
                src_port: conn.remote_port,
                dst_port: conn.guest_port,
                seq: seq_start.wrapping_add(offset as u32),
                ack: conn.last_ack,
                window: 65535,
                src_mac: ctx.gw_mac,
                dst_mac: ctx.guest_mac,
            },
            &data[offset..chunk_end],
        ));
        offset = chunk_end;
    }
}
