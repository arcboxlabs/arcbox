use super::{
    HANDSHAKE_MAX_RETRANSMITS, HANDSHAKE_TOTAL_TTL, HandshakeConn, HandshakeRole, MAX_PENDING_SYNS,
    SHIM_MSS, SHIM_WSCALE, SynFlowKey, TcpBridge, build_rst_from_syn, next_isn, should_retransmit,
};
use crate::ethernet::ETH_HEADER_LEN;
use std::time::Instant as StdInstant;
use tokio::sync::oneshot;

impl TcpBridge {
    /// Parses a TCP SYN frame and registers an in-shim `PassiveOpen`
    /// handshake: captures the guest ISN and options, generates our ISN,
    /// and spawns an async host connect. The SYN-ACK is emitted later by
    /// `poll_handshakes` once the connect resolves.
    ///
    /// Returns an RST frame when capacity rejects the SYN.
    /// Returns `None` for malformed input or a tracked handshake.
    ///
    /// `gateway_mac` and `guest_mac` are the MAC addresses used on the
    /// guest's Ethernet link. The guest MAC is learned from the source MAC
    /// of inbound frames.
    pub fn handle_outbound_syn(
        &mut self,
        syn_frame: &[u8],
        gateway_mac: [u8; 6],
        guest_mac: [u8; 6],
    ) -> Option<Vec<u8>> {
        let tcp = super::packet::tcp_at(syn_frame, ETH_HEADER_LEN)?;
        // Require SYN set, ACK clear.
        if tcp.flags() & 0x12 != 0x02 {
            return None;
        }
        let src_ip = tcp.ip().source();
        let dst_ip = tcp.ip().destination();
        let src_port = tcp.source_port();
        let dst_port = tcp.destination_port();
        let guest_isn = tcp.sequence();
        let key = SynFlowKey {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
        };

        // Retransmit of an existing handshake — same ISN means the guest
        // is re-sending the SYN because our SYN-ACK was lost. Drop
        // silently; poll_handshakes will handle the re-send.
        if let Some(existing) = self.handshake_conns.get(&key) {
            if existing.role == HandshakeRole::PassiveOpen && existing.peer_isn == guest_isn {
                tracing::debug!("Handshake shim: SYN retransmit dropped for {key:?}");
                return None;
            }
            // Different ISN = new connection attempt, evict stale entry.
            tracing::debug!("Handshake shim: ISN changed for {key:?}, replacing");
            self.handshake_conns.remove(&key);
        }

        // Capacity guard — send RST instead of silent drop.
        if self.handshake_conns.len() >= MAX_PENDING_SYNS {
            tracing::warn!("Handshake shim: capacity reached, RST for {key:?}");
            return build_rst_from_syn(syn_frame, gateway_mac);
        }

        // Parse peer options (MSS / WScale).
        let opts = crate::ethernet::parse_tcp_syn_options(tcp.segment());
        let peer_wscale = opts.wscale;
        let peer_mss = opts
            .mss
            .unwrap_or_else(|| if src_ip.is_ipv6() { 1220 } else { 536 });

        // Decide + dial egress via the injected resolver. The default
        // reproduces the historical inline behavior: route via the configured
        // upstream proxy (connecting by hostname, so Fake-IP destinations
        // resolve on the proxy's side) or a direct connect, with the
        // gateway→loopback translation and a connect timeout. The hostname is
        // recovered from the destination IP via the DNS resolution log.
        let domain = self.dns_log.as_ref().and_then(|log| log.lookup(dst_ip));
        let result_rx = self.egress.resolve(crate::egress::FlowMeta {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
            domain,
        });
        // Wake the datapath loop the moment the connect resolves: on an
        // idle loop nothing else fires until the next guest frame or the
        // 1 s timer tick, and that tick otherwise dominates every fresh
        // connection's latency (sequential fetches ran at ~2 s/request).
        let result_rx = if let Some(waker) = &self.handshake_waker {
            let waker = std::sync::Arc::clone(waker);
            let (mut tx, rx) = oneshot::channel();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    () = tx.closed() => {}
                    result = result_rx => {
                        if tx.send(result.unwrap_or(None)).is_ok() {
                            waker.notify_one();
                        }
                    }
                }
            });
            rx
        } else {
            result_rx
        };

        let our_isn = next_isn();
        self.handshake_conns.insert(
            key,
            HandshakeConn {
                flow_key: key,
                role: HandshakeRole::PassiveOpen,
                our_isn,
                peer_isn: guest_isn,
                peer_syn_end: guest_isn
                    .wrapping_add(tcp.payload().len() as u32)
                    .wrapping_add(1)
                    .wrapping_add(u32::from(tcp.flags() & 0x01 != 0)),
                host_stream: None,
                connect_rx: Some(result_rx),
                peer_wscale,
                peer_mss,
                gw_mac: gateway_mac,
                guest_mac,
                retransmit_count: 0,
                last_sent: None,
                saved_frame: None,
                created: StdInstant::now(),
            },
        );

        tracing::debug!(
            "Handshake shim: passive-open registered {key:?} our_isn={our_isn:08x} guest_isn={guest_isn:08x}"
        );
        None
    }

    /// Registers an inbound port-forward `ActiveOpen` handshake. We will
    /// emit a SYN toward the guest on the next `poll_handshakes`, then
    /// wait for the guest's SYN-ACK.
    ///
    /// Called from the datapath when `InboundListenerManager` accepts a
    /// new host connection.
    pub fn initiate_active_handshake(
        &mut self,
        flow_key: SynFlowKey,
        host_stream: impl Into<crate::NativeTcp<std::net::TcpStream>>,
        gateway_mac: [u8; 6],
        guest_mac: [u8; 6],
    ) {
        let host_stream = host_stream.into();
        host_stream.set_nonblocking(true).ok();
        host_stream.set_nodelay(true).ok();

        let our_isn = next_isn();
        // Evict any stale entry for the same four-tuple.
        self.handshake_conns.remove(&flow_key);

        self.handshake_conns.insert(
            flow_key,
            HandshakeConn {
                flow_key,
                role: HandshakeRole::ActiveOpen,
                our_isn,
                peer_isn: 0, // filled in when SYN-ACK arrives
                peer_syn_end: 0,
                host_stream: Some(host_stream),
                connect_rx: None,
                peer_wscale: Some(SHIM_WSCALE),
                peer_mss: if flow_key.src_ip.is_ipv6() {
                    1220
                } else {
                    SHIM_MSS
                },
                gw_mac: gateway_mac,
                guest_mac,
                retransmit_count: 0,
                last_sent: None,
                saved_frame: None,
                created: StdInstant::now(),
            },
        );

        tracing::debug!(
            "Handshake shim: active-open registered {flow_key:?} our_isn={our_isn:08x}"
        );
    }

    /// Drives all in-progress handshakes: polls host connects, emits
    /// initial frames (SYN-ACK for passive, SYN for active), retransmits,
    /// and aborts after the TTL / retransmit limit.
    ///
    /// Returns frames to inject to the guest.
    pub fn poll_handshakes(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut to_abort: Vec<SynFlowKey> = Vec::new();
        let now = StdInstant::now();

        for (key, conn) in &mut self.handshake_conns {
            if conn
                .host_stream
                .as_ref()
                .is_some_and(crate::NativeTcp::is_cancelled)
            {
                if conn.role == HandshakeRole::PassiveOpen || conn.saved_frame.is_some() {
                    out.push(handshake_abort_rst(key, conn));
                }
                to_abort.push(*key);
                continue;
            }
            // Global TTL — abort stuck handshakes, telling the guest: without
            // the RST it retries SYNs against a flow we already gave up on
            // and its socket only dies by its own (much longer) timeout.
            if now.duration_since(conn.created) > HANDSHAKE_TOTAL_TTL {
                tracing::warn!("Handshake shim: TTL exceeded for {key:?}, aborting with RST");
                out.push(handshake_abort_rst(key, conn));
                to_abort.push(*key);
                continue;
            }

            match conn.role {
                HandshakeRole::PassiveOpen => {
                    // If host connect hasn't resolved, try to pick up the result.
                    if conn.host_stream.is_none() {
                        let Some(rx) = conn.connect_rx.as_mut() else {
                            // No stream, no pending connect — aborted.
                            out.push(handshake_abort_rst(key, conn));
                            to_abort.push(*key);
                            continue;
                        };
                        match rx.try_recv() {
                            Ok(Some(crate::egress::EgressConn::Tcp(tokio_stream))) => {
                                if tokio_stream.is_cancelled() {
                                    out.push(handshake_abort_rst(key, conn));
                                    to_abort.push(*key);
                                    continue;
                                }
                                match tokio_stream.into_std() {
                                    Ok(std_stream) => {
                                        std_stream.set_nonblocking(true).ok();
                                        std_stream.set_nodelay(true).ok();
                                        conn.host_stream = Some(std_stream);
                                        conn.connect_rx = None;
                                    }
                                    Err(e) => {
                                        tracing::debug!(
                                            "Handshake shim: into_std failed for {key:?}: {e}"
                                        );
                                        out.push(handshake_abort_rst(key, conn));
                                        to_abort.push(*key);
                                        continue;
                                    }
                                }
                            }
                            Ok(None) => {
                                // Host connect refused / timed out. Emit
                                // RST|ACK toward the guest so the originating
                                // socket sees an immediate ECONNREFUSED
                                // instead of waiting for SYN retransmits.
                                tracing::debug!("Handshake shim: host connect failed for {key:?}");
                                out.push(handshake_abort_rst(key, conn));
                                to_abort.push(*key);
                                continue;
                            }
                            Err(oneshot::error::TryRecvError::Empty) => {
                                continue; // still connecting
                            }
                            Err(oneshot::error::TryRecvError::Closed) => {
                                out.push(handshake_abort_rst(key, conn));
                                to_abort.push(*key);
                                continue;
                            }
                        }
                    }

                    // Host stream is ready. Emit SYN-ACK (first time) or
                    // retransmit based on timer.
                    if conn.saved_frame.is_none() {
                        let frame = crate::ethernet::build_tcp_syn_ack_frame(
                            &crate::ethernet::SynAckParams {
                                src_ip: key.dst_ip,
                                dst_ip: key.src_ip,
                                src_port: key.dst_port,
                                dst_port: key.src_port,
                                seq: conn.our_isn,
                                ack: conn.peer_isn.wrapping_add(1),
                                src_mac: conn.gw_mac,
                                dst_mac: conn.guest_mac,
                                mss: super::segmentation::family_mss(
                                    key.src_ip,
                                    usize::from(self.advertised_mss),
                                ) as u16,
                                wscale: conn.peer_wscale.map(|_| SHIM_WSCALE),
                                // The fast path only supports cumulative ACKs.
                                sack_permitted: false,
                            },
                        );
                        conn.saved_frame = Some(frame.clone());
                        conn.last_sent = Some(now);
                        out.push(frame);
                    } else if should_retransmit(conn, now) {
                        if conn.retransmit_count >= HANDSHAKE_MAX_RETRANSMITS {
                            to_abort.push(*key);
                            continue;
                        }
                        if let Some(ref frame) = conn.saved_frame {
                            out.push(frame.clone());
                            conn.retransmit_count += 1;
                            conn.last_sent = Some(now);
                        }
                    }
                }
                HandshakeRole::ActiveOpen => {
                    // Stop driving the guest-side handshake only if the host
                    // client that opened this inbound connection reset it — a
                    // hard socket error on the non-blocking peek. A clean EOF
                    // (peek Ok(0)) is NOT abandonment: the client half-closed
                    // its write side but can still receive our response, so we
                    // let the handshake complete and the established fast path
                    // propagate the EOF as a FIN, exactly as it does after
                    // promotion. Treating EOF as dead here broke protocols that
                    // write-half-close to signal end-of-input.
                    let host_reset = conn.host_stream.as_ref().is_some_and(|s| {
                        let mut probe = [0u8; 1];
                        match s.peek(&mut probe) {
                            Ok(_) => false, // data or a clean EOF — peer is not gone
                            Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
                        }
                    });
                    if host_reset {
                        // If we already sent our SYN the guest may sit in
                        // SYN-RECEIVED; clear it with an RST at seq = our_isn+1
                        // (its RCV.NXT), which a RFC 5961 guest accepts —
                        // `handshake_abort_rst`'s seq=0 is for the passive-open
                        // (SYN-SENT) guest and would be dropped as out-of-window
                        // here. With no SYN sent the guest has no state to clear.
                        if conn.saved_frame.is_some() {
                            out.push(crate::ethernet::build_tcp_rst_frame(
                                &crate::ethernet::TcpFrameParams {
                                    src_ip: key.dst_ip,
                                    dst_ip: key.src_ip,
                                    src_port: key.dst_port,
                                    dst_port: key.src_port,
                                    seq: conn.our_isn.wrapping_add(1),
                                    ack: 0,
                                    window: 0,
                                    src_mac: conn.gw_mac,
                                    dst_mac: conn.guest_mac,
                                },
                            ));
                        }
                        to_abort.push(*key);
                        continue;
                    }
                    if conn.saved_frame.is_none() {
                        let frame =
                            crate::ethernet::build_tcp_syn_frame(&crate::ethernet::SynParams {
                                // We're sending from gateway → guest.
                                src_ip: key.dst_ip,
                                dst_ip: key.src_ip,
                                src_port: key.dst_port,
                                dst_port: key.src_port,
                                seq: conn.our_isn,
                                src_mac: conn.gw_mac,
                                dst_mac: conn.guest_mac,
                                mss: super::segmentation::family_mss(
                                    key.src_ip,
                                    usize::from(self.advertised_mss),
                                ) as u16,
                                wscale: Some(SHIM_WSCALE),
                                sack_permitted: false,
                            });
                        conn.saved_frame = Some(frame.clone());
                        conn.last_sent = Some(now);
                        out.push(frame);
                    } else if should_retransmit(conn, now) {
                        if conn.retransmit_count >= HANDSHAKE_MAX_RETRANSMITS {
                            to_abort.push(*key);
                            continue;
                        }
                        if let Some(ref frame) = conn.saved_frame {
                            out.push(frame.clone());
                            conn.retransmit_count += 1;
                            conn.last_sent = Some(now);
                        }
                    }
                }
            }
        }

        for key in to_abort {
            self.handshake_conns.remove(&key);
        }

        out
    }

    /// Called when a TCP frame arrives that matches a pending handshake
    /// (keyed on `SynFlowKey`). For PassiveOpen: consumes the guest's ACK
    /// and promotes to `FastPathConn`, then processes the same frame's
    /// payload and receive window through the fast path. For ActiveOpen: consumes the
    /// guest's SYN-ACK, emits our final ACK, and promotes.
    ///
    /// Returns `Some(Vec<frame>)` if the frame was consumed by the shim
    /// (possibly emitting reply frames). Returns `None` if no matching
    /// handshake exists or the frame doesn't match the expected phase.
    pub fn try_complete_handshake(&mut self, frame: &[u8]) -> Option<Vec<Vec<u8>>> {
        let tcp = super::packet::tcp_at(frame, ETH_HEADER_LEN)?;
        let flags = tcp.flags();
        let seq = tcp.sequence();
        let ack = tcp.acknowledgment();
        let key = SynFlowKey {
            src_ip: tcp.ip().source(),
            src_port: tcp.source_port(),
            dst_ip: tcp.ip().destination(),
            dst_port: tcp.destination_port(),
        };
        let conn = self.handshake_conns.get(&key)?;
        if conn
            .host_stream
            .as_ref()
            .is_some_and(crate::NativeTcp::is_cancelled)
        {
            let replies = if flags & 0x04 == 0 {
                vec![handshake_abort_rst(&key, conn)]
            } else {
                Vec::new()
            };
            self.handshake_conns.remove(&key);
            return Some(replies);
        }

        match conn.role {
            HandshakeRole::PassiveOpen => {
                // Expect ACK set, SYN clear, and RST/FIN both clear. Without
                // the RST/FIN check, RST|ACK (0x14) and FIN|ACK (0x11) would
                // pass the old `flags & 0x12 != 0x10` test and incorrectly
                // promote an already-aborted guest flow into the fast path.
                const SYN: u8 = 0x02;
                const ACK: u8 = 0x10;
                const RST: u8 = 0x04;
                const FIN: u8 = 0x01;
                if flags & (SYN | ACK) != ACK || flags & (RST | FIN) != 0 {
                    if flags & RST != 0 {
                        tracing::debug!("Handshake shim: RST during PassiveOpen for {key:?}");
                        self.handshake_conns.remove(&key);
                    }
                    return None;
                }
                // Guest is ACKing our SYN-ACK: ack should be our_isn + 1.
                if ack != conn.our_isn.wrapping_add(1) {
                    tracing::debug!(
                        "Handshake shim: passive ACK mismatch for {key:?} got ack={ack:08x} want={:08x}",
                        conn.our_isn.wrapping_add(1)
                    );
                    return None;
                }
                let conn = self.handshake_conns.remove(&key)?;
                let our_seq = conn.our_isn.wrapping_add(1);
                let last_ack = conn.peer_isn.wrapping_add(1);
                let peer_mss = conn.peer_mss;
                let peer_wscale = conn.peer_wscale;
                let Some(stream) = conn.host_stream else {
                    return Some(Vec::new());
                };
                self.promote_to_fast_path(key, stream, our_seq, last_ack, peer_mss, peer_wscale);
                // The final ACK can carry data; preserve the fast path's accepted-byte ACK discipline.
                Some(match self.try_fast_path_intercept(frame) {
                    Some(reply) if !reply.is_empty() => vec![reply],
                    _ => Vec::new(),
                })
            }
            HandshakeRole::ActiveOpen => {
                // Expect SYN + ACK.
                if flags & 0x12 != 0x12 {
                    return None;
                }
                if ack != conn.our_isn.wrapping_add(1) {
                    tracing::debug!(
                        "Handshake shim: active SYN-ACK ack mismatch for {key:?} got ack={ack:08x} want={:08x}",
                        conn.our_isn.wrapping_add(1)
                    );
                    return None;
                }
                // Capture guest ISN from their SYN-ACK.
                let guest_isn = seq;
                // Mirror their options back so we can promote with sane state.
                let peer_opts = crate::ethernet::parse_tcp_syn_options(tcp.segment());

                let mut conn = self.handshake_conns.remove(&key)?;
                conn.peer_isn = guest_isn;
                if peer_opts.wscale.is_some() {
                    conn.peer_wscale = peer_opts.wscale;
                }
                // Record the guest endpoint's real MSS so host→guest segments
                // are sized to what it can forward (a bridged container behind
                // eth0 advertises its own, smaller, veth MSS here).
                if let Some(mss) = peer_opts.mss {
                    conn.peer_mss = mss;
                }

                // Build ACK completing the handshake.
                let our_seq = conn.our_isn.wrapping_add(1);
                let last_ack = guest_isn.wrapping_add(1);
                let ack_frame =
                    crate::ethernet::build_tcp_ack_frame(&crate::ethernet::TcpFrameParams {
                        // Direction: gateway → guest.
                        src_ip: key.dst_ip,
                        dst_ip: key.src_ip,
                        src_port: key.dst_port,
                        dst_port: key.src_port,
                        seq: our_seq,
                        ack: last_ack,
                        window: 65535,
                        src_mac: conn.gw_mac,
                        dst_mac: conn.guest_mac,
                    });

                let peer_mss = conn.peer_mss;
                let peer_wscale = conn.peer_wscale;
                let Some(stream) = conn.host_stream else {
                    return Some(vec![ack_frame]);
                };
                self.promote_to_fast_path(key, stream, our_seq, last_ack, peer_mss, peer_wscale);
                Some(vec![ack_frame])
            }
        }
    }
}

/// Reset the guest with the sequence space for its handshake role.
fn handshake_abort_rst(key: &SynFlowKey, conn: &HandshakeConn) -> Vec<u8> {
    let params = crate::ethernet::TcpFrameParams {
        src_ip: key.dst_ip,
        dst_ip: key.src_ip,
        src_port: key.dst_port,
        dst_port: key.src_port,
        seq: if conn.saved_frame.is_some() {
            conn.our_isn.wrapping_add(1)
        } else {
            0
        },
        ack: conn.peer_isn.wrapping_add(1),
        window: 0,
        src_mac: conn.gw_mac,
        dst_mac: conn.guest_mac,
    };
    if conn.role == HandshakeRole::ActiveOpen {
        crate::ethernet::build_tcp_rst_only_frame(&params)
    } else {
        crate::ethernet::build_tcp_rst_frame(&params)
    }
}
