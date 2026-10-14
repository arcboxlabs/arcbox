//! Transfer established host streams into the polled or inline data path.

use super::{FastPathConn, GSO_SEGMENT_MSS, SynFlowKey, TcpBridge};

/// TCP state established by the completed guest-facing handshake.
#[derive(Debug, Clone, Copy)]
pub struct FastPathInit {
    /// Next sequence number sent toward the guest.
    pub our_seq: u32,
    /// Next guest sequence number accepted by the host socket.
    pub last_ack: u32,
    /// Maximum segment size advertised by the guest.
    pub peer_mss: u16,
    /// Negotiated shift in 0..=14 for subsequent guest receive-window updates.
    pub peer_wscale: Option<u8>,
    /// Initial receive window in bytes, already scaled for a non-SYN ACK.
    pub guest_window: u32,
}

impl TcpBridge {
    /// Promotes a connection to the fast path.
    ///
    /// Called when a shim handshake reaches ESTABLISHED and has a
    /// pre-connected host stream. The connection is moved into
    /// `fast_path_conns` and subsequent data bypasses handshake bookkeeping.
    /// The initial window must be valid before an inline reader receives the stream.
    pub fn promote_to_fast_path(
        &mut self,
        key: SynFlowKey,
        stream: std::net::TcpStream,
        init: FastPathInit,
    ) {
        let FastPathInit {
            our_seq,
            last_ack,
            peer_mss,
            peer_wscale,
            guest_window,
        } = init;
        // Set non-blocking for polling in the event loop.
        stream.set_nonblocking(true).ok();
        stream.set_nodelay(true).ok();
        // Keepalive on the upstream leg: a silently dead upstream (route
        // flap, crashed proxy) otherwise never errors, leaving the guest leg
        // ESTABLISHED forever. Applies to the underlying socket, so inline
        // owners reading a cloned fd inherit it. See UPSTREAM_KEEPALIVE_*.
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(super::UPSTREAM_KEEPALIVE_IDLE)
            .with_interval(super::UPSTREAM_KEEPALIVE_INTERVAL)
            .with_retries(super::UPSTREAM_KEEPALIVE_RETRIES);
        if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
            tracing::warn!("Fast path: keepalive setup failed for {key:?}: {e}");
        }

        let last_ack_atomic = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(last_ack));
        let our_seq_atomic = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(our_seq));
        // Publish the handshake's window before the sink can start reading the host socket.
        let guest_acked = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(our_seq));
        let guest_window = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(guest_window));
        // Shared host→guest byte counter — allocated ONLY when a flow observer is
        // installed, so an un-observed consumer (e.g. the VMM datapath) pays no
        // per-connection allocation or atomic. The inline inject thread increments
        // it; the bridge reads it at teardown.
        let down_shared = self
            .observer
            .is_some()
            .then(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));

        // Shim-synthesized handshakes know exact SEQ/ACK at promotion
        // time, so hand the cloned stream to the inject thread
        // immediately for zero-copy host→guest reads.
        let mut inline_owned = false;

        // Only hand a flow to the inline/GSO inject path when its peer can accept
        // the fixed 1460-byte GSO segments that path emits. A smaller-MSS peer
        // (e.g. a sub-1500 overlay bridge behind eth0) stays on the clamped
        // polling path below, where each segment matches its advertised MSS.
        let inline_eligible = peer_mss >= GSO_SEGMENT_MSS;
        let mut dead: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
        let mut retx: Option<crate::direct_rx::RetxRing> = None;
        if let Some(sink) = self.conn_sink.as_ref().filter(|_| inline_eligible) {
            match stream.try_clone() {
                Ok(cloned) => {
                    let gw_mac = self.fast_path_gateway_mac;
                    let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
                    let dead_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    // Retransmission ring: base seq starts at our current
                    // send cursor; the owner tees sent bytes, the bridge
                    // drains and re-emits (see RetxRing).
                    let retx_ring: crate::direct_rx::RetxRing = std::sync::Arc::new(
                        std::sync::Mutex::new((our_seq, std::collections::VecDeque::new(), None)),
                    );
                    let promoted = crate::direct_rx::PromotedConn {
                        stream: cloned,
                        remote_ip: key.dst_ip,
                        guest_ip: key.src_ip,
                        remote_port: key.dst_port,
                        guest_port: key.src_port,
                        peer_mss,
                        our_seq: std::sync::Arc::clone(&our_seq_atomic),
                        last_ack: std::sync::Arc::clone(&last_ack_atomic),
                        guest_acked: std::sync::Arc::clone(&guest_acked),
                        guest_window: std::sync::Arc::clone(&guest_window),
                        down_bytes: down_shared.clone(),
                        gw_mac,
                        guest_mac,
                        dead: std::sync::Arc::clone(&dead_flag),
                        retx: std::sync::Arc::clone(&retx_ring),
                    };
                    if sink.send_conn(promoted) {
                        inline_owned = true;
                        dead = Some(dead_flag);
                        retx = Some(retx_ring);
                        tracing::info!(
                            "Fast path: promoted INLINE {}:{} → {}:{} (seq={our_seq}, ack={last_ack})",
                            key.src_ip,
                            key.src_port,
                            key.dst_ip,
                            key.dst_port,
                        );
                    } else {
                        tracing::warn!("Fast path: inline sink full, falling back to channel path");
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Fast path: try_clone failed ({e}), falling back to channel path"
                    );
                }
            }
        } else {
            tracing::info!(
                "Fast path: promoted {}:{} → {}:{} (seq={our_seq}, ack={last_ack})",
                key.src_ip,
                key.src_port,
                key.dst_ip,
                key.dst_port,
            );
        }

        if !inline_owned && self.fast_path_read_buf.is_empty() {
            self.fast_path_read_buf.resize(128 * 1024, 0);
        }
        self.fast_path_conns.insert(
            key,
            FastPathConn {
                stream,
                our_seq: our_seq_atomic,
                last_ack,
                last_ack_shared: Some(std::sync::Arc::clone(&last_ack_atomic)),
                remote_ip: key.dst_ip,
                guest_ip: key.src_ip,
                remote_port: key.dst_port,
                guest_port: key.src_port,
                peer_mss,
                guest_wscale: peer_wscale.unwrap_or(0),
                guest_acked,
                guest_window,
                up_would_block: 0,
                up_short_writes: 0,
                up_out_of_order: 0,
                ooo_segs: std::collections::VecDeque::new(),
                ooo_bytes: 0,
                up_ooo_dropped: 0,
                window_stalled: false,
                window_stalled_at: None,
                retransmit_buf: std::collections::VecDeque::new(),
                retransmit_seq: our_seq,
                last_progress: std::time::Instant::now(),
                rto: super::INITIAL_RTO,
                dup_acks: 0,
                fast_retransmit: false,
                fin_seq: None,
                retransmits: 0,
                host_eof: false,
                guest_fin_at: None,
                inline_owned,
                up_bytes: 0,
                down_bytes: 0,
                down_shared: if inline_owned { down_shared } else { None },
                dead,
                last_guest_activity: std::time::Instant::now(),
                soliciting_since: None,
                retx,
            },
        );
    }
}
