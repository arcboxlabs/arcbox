//! Explicit TCP abort and shared flow-close accounting.

use std::net::IpAddr;
use std::sync::atomic::Ordering;

use super::{FastPathConn, HandshakeConn, HandshakeRole, SynFlowKey, TcpBridge};
use crate::ethernet::{TcpFrameParams, build_tcp_rst_frame, build_tcp_rst_only_frame};

impl TcpBridge {
    /// Aborts all current TCP flows and emits guest-bound RST Ethernet frames.
    ///
    /// Stop business ingress and discard queued business packets before calling this method.
    /// Each flow releases its primary socket and cancels its reader before `emit`.
    /// Output is incremental; a second call emits nothing unless new flows exist.
    /// Established resets use SND.NXT. Use [`Self::closed_reply_ip`] for subsequent guest packets.
    pub fn abort(&mut self, mut emit: impl FnMut(Vec<u8>)) {
        for (key, conn) in self.handshake_conns.drain() {
            let frame = conn.abort_frame(&key);
            drop(conn);
            if let Some(frame) = frame {
                emit(frame);
            }
        }
        let guest_mac = self.fast_path_guest_mac.unwrap_or([0xff; 6]);
        for (key, conn) in self.fast_path_conns.drain() {
            let frame =
                build_tcp_rst_only_frame(&conn.reset_params(self.fast_path_gateway_mac, guest_mac));
            conn.report_close(&key, self.observer.as_deref());
            drop(conn);
            emit(frame);
        }
    }

    /// Removes a fast-path flow and reports its byte totals to the observer.
    /// Native owners also close when map replacement or bridge drop removes the primary.
    pub(super) fn close_fast_path(&mut self, key: &SynFlowKey) {
        if let Some(conn) = self.fast_path_conns.remove(key) {
            conn.report_close(key, self.observer.as_deref());
        }
    }
}

impl HandshakeConn {
    fn abort_frame(&self, key: &SynFlowKey) -> Option<Vec<u8>> {
        // RFC 9293 section 3.10.5 deletes SYN-SENT without transmitting a reset.
        if self.role == HandshakeRole::ActiveOpen {
            return None;
        }
        let mut params = TcpFrameParams {
            src_ip: key.dst_ip,
            dst_ip: key.src_ip,
            src_port: key.dst_port,
            dst_port: key.src_port,
            seq: 0,
            ack: self.peer_syn_end,
            window: 0,
            src_mac: self.gw_mac,
            dst_mac: self.guest_mac,
        };
        if self.saved_frame.is_some() {
            params.seq = self.our_isn.wrapping_add(1);
            Some(build_tcp_rst_only_frame(&params))
        } else {
            Some(build_tcp_rst_frame(&params))
        }
    }
}

impl FastPathConn {
    fn report_close(&self, key: &SynFlowKey, observer: Option<&dyn crate::egress::FlowObserver>) {
        if self.up_would_block + self.up_short_writes + self.up_out_of_order + self.retransmits > 0
        {
            tracing::debug!(
                "Fast path close {}:{} → {}:{}: loss recovery (up: would_block={}, short_writes={}, out_of_order={}, ooo_dropped={}; down: retransmits={})",
                key.src_ip,
                key.src_port,
                key.dst_ip,
                key.dst_port,
                self.up_would_block,
                self.up_short_writes,
                self.up_out_of_order,
                self.up_ooo_dropped,
                self.retransmits,
            );
        }
        if let Some(observer) = observer {
            let down = self.down_bytes
                + self
                    .down_shared
                    .as_ref()
                    .map_or(0, |bytes| bytes.load(Ordering::Relaxed));
            observer.on_flow_close(
                crate::egress::FlowKey {
                    src_ip: key.src_ip,
                    src_port: key.src_port,
                    dst_ip: key.dst_ip,
                    dst_port: key.dst_port,
                },
                self.up_bytes,
                down,
            );
        }
    }

    pub(super) fn reset_frame(&self, gateway_mac: [u8; 6], guest_mac: [u8; 6]) -> Vec<u8> {
        build_tcp_rst_frame(&self.reset_params(gateway_mac, guest_mac))
    }

    fn reset_params(&self, gateway_mac: [u8; 6], guest_mac: [u8; 6]) -> TcpFrameParams<IpAddr> {
        TcpFrameParams {
            src_ip: self.remote_ip,
            dst_ip: self.guest_ip,
            src_port: self.remote_port,
            dst_port: self.guest_port,
            seq: self.our_seq.load(Ordering::Relaxed),
            ack: self.last_ack,
            window: 0,
            src_mac: gateway_mac,
            dst_mac: guest_mac,
        }
    }
}
