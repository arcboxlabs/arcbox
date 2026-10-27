use super::segmentation::{FrameCtx, emit_data_frames};
use super::{SynFlowKey, TcpBridge};

impl TcpBridge {
    pub(super) fn intercept_open_at<const IP_START: usize, T: AsRef<[u8]>>(
        &mut self,
        frames: &[T],
    ) -> Option<(usize, Vec<u8>)> {
        let frame = frames.first()?.as_ref();
        let mut consumed = 1;
        let tcp = super::packet::tcp_at(frame, IP_START)?;
        if IP_START == 0 && tcp.flags() & 0x02 != 0 {
            return None;
        }
        let ip_start = IP_START;
        let ip_end = ip_start + tcp.ip().packet().len();
        let src_ip = tcp.ip().source();
        let dst_ip = tcp.ip().destination();
        let src_port = tcp.source_port();
        let dst_port = tcp.destination_port();
        let flags = tcp.flags();
        let key = SynFlowKey {
            src_ip,
            src_port,
            dst_ip,
            dst_port,
        };

        let conn = self.fast_path_conns.get_mut(&key)?;
        // Any guest frame for the flow — data, ACK, dup-ACK, even a bare
        // window probe — is its sign of life for the dead-flow reaper.
        conn.last_guest_activity = std::time::Instant::now();

        let guest_seq = tcp.sequence();

        // FIN or RST → handle teardown ourselves.
        if flags & 0x04 != 0 {
            // RST: close host stream immediately, no response needed.
            tracing::debug!("Fast path: RST from guest {src_ip}:{src_port}→{dst_ip}:{dst_port}");
            self.close_fast_path(&key);
            return Some((consumed, Vec::new())); // Intercepted, no reply frame.
        }

        if conn.stream.is_cancelled() {
            let rst = conn.reset_frame(
                self.fast_path_gateway_mac,
                self.fast_path_guest_mac.unwrap_or([0xFF; 6]),
            );
            self.close_fast_path(&key);
            return Some((consumed, rst));
        }

        // IP/TCP views exclude padding and include extension/header lengths.
        // FIN processing follows payload writes because a FIN can carry data.
        let payload_len = tcp.payload().len();
        let payload_end = ip_end;
        let payload_start = payload_end - payload_len;

        // Download flow control: every guest frame carrying ACK updates our
        // view of how much of OUR stream it received and how much receive
        // window it advertises. Monotonic on the ack (a reordered frame
        // must not regress it); the window rides along with the newest ack,
        // and an equal ack still applies it (pure window updates). A pure
        // ACK repeating the same ack AND window while our data is in flight
        // is a duplicate ACK — three of them mean the guest is missing a
        // frame and triggers fast retransmit in `poll_fast_path`.
        if flags & 0x10 != 0 {
            let guest_ack = tcp.acknowledgment();
            let current = conn.guest_acked.load(std::sync::atomic::Ordering::Relaxed);
            let advance = guest_ack.wrapping_sub(current);
            // Reject an ACK beyond what we actually sent (RFC 793: an ACK
            // must not cover data past SND.NXT). Accepting one would let
            // poll_fast_path drain retransmit_buf past real in-flight bytes
            // and move retransmit_seq beyond our_seq, so those bytes could
            // never be retransmitted — the flow would truncate or wedge.
            let sent = conn.our_seq.load(std::sync::atomic::Ordering::Relaxed);
            let beyond_sent =
                guest_ack.wrapping_sub(sent) != 0 && guest_ack.wrapping_sub(sent) < 0x8000_0000;
            if advance < 0x8000_0000 && !beyond_sent {
                let scaled_win = u32::from(tcp.window()) << conn.guest_wscale;
                // FIN|SYN|RST all clear — `trailing_zeros` (clippy's
                // suggestion) would obscure that this is a flag test.
                let plain_ack = flags & (0x01 | 0x02 | 0x04) == 0;
                if advance == 0
                    && payload_len == 0
                    && plain_ack
                    && scaled_win == conn.guest_window.load(std::sync::atomic::Ordering::Relaxed)
                {
                    let in_flight = sent.wrapping_sub(guest_ack);
                    if in_flight > 0 && in_flight < 0x8000_0000 {
                        conn.dup_acks = conn.dup_acks.saturating_add(1);
                        if conn.dup_acks >= 3 {
                            conn.fast_retransmit = true;
                        }
                    }
                } else if advance > 0 {
                    conn.dup_acks = 0;
                }
                conn.guest_acked
                    .store(guest_ack, std::sync::atomic::Ordering::Relaxed);
                conn.guest_window
                    .store(scaled_win, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Write payload to host stream (if any). The invariant that keeps
        // uploads lossless: `last_ack` advances ONLY over bytes actually
        // written, in order, to the host socket. Everything else
        // (WouldBlock, the unwritten tail of a short write) stays un-ACKed
        // and is retained for a later poll. Future segments stay parked
        // until the missing prefix arrives; only evicted bytes require
        // guest retransmission. ACKing past unwritten bytes would silently
        // lose data.
        let mut in_order_advanced = false;
        if payload_len > 0 {
            let seq_end = guest_seq.wrapping_add(payload_len as u32);
            // Bytes this segment extends past our contiguous cursor.
            let extends = seq_end.wrapping_sub(conn.last_ack);
            if extends == 0 || extends >= 0x8000_0000 {
                // Entirely at or behind the cursor — a retransmit of data
                // we already ACKed. Skip the write, fall through to re-ACK.
                tracing::trace!(
                    "Fast path TX: {src_ip}:{src_port}→{dst_ip}:{dst_port} retransmit (guest_seq={guest_seq}, last_ack={}, payload={payload_len})",
                    conn.last_ack
                );
            } else {
                // Leading bytes already ACKed (partial retransmit overlap).
                let overlap = conn.last_ack.wrapping_sub(guest_seq);
                if overlap >= 0x8000_0000 {
                    // guest_seq is ahead of the cursor: a hole precedes this
                    // segment. Writing it now would corrupt the byte stream
                    // and ACKing it would bury the hole forever. Park it
                    // (bounded) for delivery once the hole fills; leave
                    // last_ack alone — the ACK below is a dup-ACK, which is
                    // exactly what makes the guest fast-retransmit the gap.
                    conn.up_out_of_order += 1;
                    conn.buffer_upload_segment(guest_seq, &frame[payload_start..payload_end]);
                } else {
                    let fresh = &frame[payload_start + overlap as usize..payload_end];
                    let mut batch = super::upload_batch::UploadBatch::new(fresh);
                    if overlap == 0 && conn.ooo_segs.is_empty() {
                        batch.extend(&tcp, &frames[1..], seq_end, ip_start);
                    }
                    consumed = batch.count;
                    match batch.send(conn) {
                        Ok(advanced) => in_order_advanced = advanced,
                        Err(e) => {
                            tracing::warn!("Fast path TX write error, RST to guest: {e}");
                            // Mirror the RX-error path: the host stream is dead,
                            // so tear the guest side down with RST instead of
                            // leaving it ESTABLISHED forever (ABX-431).
                            let rst = crate::ethernet::build_tcp_rst_frame(
                                &crate::ethernet::TcpFrameParams {
                                    src_ip: conn.remote_ip,
                                    dst_ip: conn.guest_ip,
                                    src_port: conn.remote_port,
                                    dst_port: conn.guest_port,
                                    seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                                    ack: conn.last_ack,
                                    window: 0,
                                    src_mac: self.fast_path_gateway_mac,
                                    dst_mac: self.fast_path_guest_mac.unwrap_or([0xFF; 6]),
                                },
                            );
                            self.close_fast_path(&key);
                            return Some((consumed, rst));
                        }
                    }
                }
            }
        }

        // A cursor advance may have landed right behind parked out-of-order
        // segments — flush them now so the ACK below leaps over everything
        // contiguous instead of forcing the guest to retransmit data that
        // already arrived.
        if in_order_advanced
            && !conn.ooo_segs.is_empty()
            && let Err(e) = conn.drain_parked_segments()
        {
            tracing::warn!("Fast path TX drain error, RST to guest: {e}");
            let rst = crate::ethernet::build_tcp_rst_frame(&crate::ethernet::TcpFrameParams {
                src_ip: conn.remote_ip,
                dst_ip: conn.guest_ip,
                src_port: conn.remote_port,
                dst_port: conn.guest_port,
                seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                ack: conn.last_ack,
                window: 0,
                src_mac: self.fast_path_gateway_mac,
                dst_mac: self.fast_path_guest_mac.unwrap_or([0xFF; 6]),
            });
            self.close_fast_path(&key);
            return Some((consumed, rst));
        }

        // FIN handling — only once every byte before it has been written and
        // ACKed. A FIN whose sequence position is beyond `last_ack` (data
        // still missing, or its own payload only partially written) must not
        // tear anything down: closing here would cut off the retransmissions
        // that repair the gap, truncating the upload. The guest re-sends the
        // gap and the FIN; until then it gets the dup-ACK below.
        if flags & 0x01 != 0 {
            let fin_seq = guest_seq.wrapping_add(payload_len as u32);
            if fin_seq == conn.last_ack {
                // FIN consumes 1 sequence number (in addition to any data
                // bytes already accounted for above).
                conn.set_last_ack(conn.last_ack.wrapping_add(1));
                let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
                // Non-inline flow: shut only the upstream's write side (the guest
                // will send no more) and just ACK the FIN here. If the host is
                // still open, poll keeps relaying host→guest until the host EOFs
                // and then emits OUR FIN. If the host already closed, our FIN
                // and any unacknowledged download bytes stay in the recovery
                // state, so poll retransmits them with the original sequence
                // numbers. Reaping happens in poll once both sides are closed.
                // A full close here would discard that recovery state, send a
                // second FIN at a new sequence number, and, with unread bytes
                // buffered, RST the real upstream.
                // Inline flows (whose reader owns host→guest) fall through to
                // the full teardown.
                if !conn.inline_owned {
                    tracing::debug!(
                        "Fast path: guest half-close {src_ip}:{src_port}→{dst_ip}:{dst_port}"
                    );
                    let _ = conn.stream.shutdown(std::net::Shutdown::Write);
                    conn.guest_fin_at = Some(std::time::Instant::now());
                    let ack =
                        crate::ethernet::build_tcp_ack_frame(&crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                            ack: conn.last_ack,
                            window: 65535,
                            src_mac: self.fast_path_gateway_mac,
                            dst_mac: guest_mac,
                        });
                    return Some((consumed, ack));
                }
                tracing::debug!(
                    "Fast path: FIN from guest {src_ip}:{src_port}→{dst_ip}:{dst_port}"
                );
                let fin_ack =
                    crate::ethernet::build_tcp_fin_frame(&crate::ethernet::TcpFrameParams {
                        src_ip: conn.remote_ip,
                        dst_ip: conn.guest_ip,
                        src_port: conn.remote_port,
                        dst_port: conn.guest_port,
                        seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                        ack: conn.last_ack,
                        window: 65535,
                        src_mac: self.fast_path_gateway_mac,
                        dst_mac: guest_mac,
                    });
                self.close_fast_path(&key);
                return Some((consumed, fin_ack));
            }
            tracing::debug!(
                "Fast path: FIN from guest {src_ip}:{src_port}→{dst_ip}:{dst_port} deferred (fin_seq={fin_seq}, last_ack={})",
                conn.last_ack
            );
        }

        // Only ACK frames that carried payload, a deferred FIN, or a SYN.
        // Replying to a pure ACK would create a dup-ACK loop that triggers
        // fast retransmits on the peer and tanks host→VM throughput. A
        // retransmitted SYN-ACK (the guest never saw our completing ACK, common
        // on the inbound ActiveOpen path) is the one no-payload-no-FIN frame we
        // must still ACK, or the guest retransmits until it times out — any
        // data or FIN it carried was already handled by the paths above, so it
        // is never dropped.
        if payload_len == 0 && flags & (0x01 | 0x02) == 0 {
            return Some((consumed, Vec::new()));
        }

        let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
        let ack = crate::ethernet::build_tcp_ack_frame(&crate::ethernet::TcpFrameParams {
            src_ip: conn.remote_ip,
            dst_ip: conn.guest_ip,
            src_port: conn.remote_port,
            dst_port: conn.guest_port,
            seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
            ack: conn.last_ack,
            window: 65535,
            src_mac: self.fast_path_gateway_mac,
            dst_mac: guest_mac,
        });

        Some((consumed, ack))
    }

    /// Polls fast-path host streams for readable data and generates frames
    /// to inject into the guest.
    ///
    /// Returns frames to be written to the guest FD via `enqueue_or_write`.
    pub fn poll_fast_path(&mut self) -> Vec<Vec<u8>> {
        let controlled = self.prepare_controlled_poll();
        let mut frames = Vec::new();
        let mut to_remove = Vec::new();
        let guest_mac = self.fast_path_guest_mac.unwrap_or([0xFF; 6]);
        let gw_mac = self.fast_path_gateway_mac;
        let ctx = FrameCtx {
            large_frames: self.large_frames_enabled,
            guest_mss: self.fast_path_guest_mss,
            gw_mac,
            guest_mac,
        };
        let now = std::time::Instant::now();
        let dead_flow_timeout = self.dead_flow_timeout;
        let read_buf = &mut self.fast_path_read_buf;
        let read_cap = if ctx.large_frames {
            32 * 1024
        } else {
            read_buf.len()
        };

        for (key, conn) in &mut self.fast_path_conns {
            if conn.stream.is_cancelled() {
                frames.push(conn.reset_frame(gw_mac, guest_mac));
                to_remove.push(*key);
                continue;
            }
            // Failed inline readers mark the shared stream dead before poll reaps it.
            if conn
                .dead
                .as_ref()
                .is_some_and(|d| d.load(std::sync::atomic::Ordering::Relaxed))
            {
                to_remove.push(*key);
                continue;
            }
            let previous_ack = conn.last_ack;
            if !conn.ooo_segs.is_empty()
                && let Err(e) = conn.drain_parked_segments()
            {
                tracing::warn!("Fast path TX drain error, RST to guest: {e}");
                frames.push(crate::ethernet::build_tcp_rst_frame(
                    &crate::ethernet::TcpFrameParams {
                        src_ip: conn.remote_ip,
                        dst_ip: conn.guest_ip,
                        src_port: conn.remote_port,
                        dst_port: conn.guest_port,
                        seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                        ack: conn.last_ack,
                        window: 0,
                        src_mac: gw_mac,
                        dst_mac: guest_mac,
                    },
                ));
                to_remove.push(*key);
                continue;
            }
            if conn.last_ack != previous_ack {
                frames.push(crate::ethernet::build_tcp_ack_frame(
                    &crate::ethernet::TcpFrameParams {
                        src_ip: conn.remote_ip,
                        dst_ip: conn.guest_ip,
                        src_port: conn.remote_port,
                        dst_port: conn.guest_port,
                        seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                        ack: conn.last_ack,
                        window: 65535,
                        src_mac: gw_mac,
                        dst_mac: guest_mac,
                    },
                ));
            }
            if conn.inline_owned {
                // Zero-window persist for the inline path. The inject thread /
                // direct_rx reader stops reading at a zero send budget and can't
                // probe on its own, so a lost guest window-update ACK would
                // deadlock the download forever. Emit a keepalive-style probe —
                // one byte at `our_seq - 1`, an old duplicate — which a
                // zero-window guest must answer with a window-bearing ACK
                // (RFC 793 §3.9). It consumes no sequence space and touches no
                // shared state, so a lost probe is harmlessly re-sent next
                // interval with no stream gap. Gated on nothing-in-flight: the
                // guest has ACKed up to `our_seq`, so `our_seq - 1` is always
                // an old byte, and any in-flight data would elicit its own
                // window-bearing ACKs.
                let sent = conn.our_seq.load(std::sync::atomic::Ordering::Relaxed);
                let acked = conn.guest_acked.load(std::sync::atomic::Ordering::Relaxed);
                let window = conn
                    .guest_window
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .min(super::HONORED_WINDOW_CAP);
                let in_flight = sent.wrapping_sub(acked);
                let nothing_in_flight = in_flight == 0 || in_flight >= 0x8000_0000;
                // Dead-flow give-up: the inline path has no RTO, so a guest
                // that vanished with data in flight freezes the send budget
                // and would hold the entry, host fd, and owner task forever
                // (the persist probe below never fires while bytes are
                // outstanding). Soliciting = unACKed data outstanding or a
                // closed window under persist; either demands a guest reply,
                // so prolonged unanswered soliciting means the endpoint is
                // gone. Both clocks must be stale — the soliciting clock
                // starts fresh when a long-idle flow's owner resumes sending,
                // so its guest gets the full deadline to answer.
                let soliciting = !nothing_in_flight || conn.window_stalled_at.is_some();
                if !soliciting {
                    conn.soliciting_since = None;
                } else if conn.soliciting_since.is_none() {
                    conn.soliciting_since = Some(now);
                }
                if conn
                    .soliciting_since
                    .is_some_and(|since| now.duration_since(since) >= dead_flow_timeout)
                    && now.duration_since(conn.last_guest_activity) >= dead_flow_timeout
                {
                    tracing::warn!(
                        "Fast path: guest silent {:?} on soliciting inline flow {}:{} → {}:{}, RST + reap",
                        dead_flow_timeout,
                        key.src_ip,
                        key.src_port,
                        key.dst_ip,
                        key.dst_port,
                    );
                    frames.push(crate::ethernet::build_tcp_rst_frame(
                        &crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: sent,
                            ack: conn.last_ack,
                            window: 0,
                            src_mac: gw_mac,
                            dst_mac: guest_mac,
                        },
                    ));
                    to_remove.push(*key);
                    continue;
                }
                // Sender-side loss recovery for inline flows, from the shared
                // ring the owner tees every sent payload into (the inline
                // counterpart of `retransmit_buf`): drain the ACKed prefix,
                // then re-emit everything past the guest's ack point on
                // triple-dup-ACK (counted by the intercept, same as
                // non-inline) or RTO. The path beyond guest eth0 (bridge →
                // veth → container netns backlog) drops under burst, so
                // window gating alone cannot prevent a wedge — without this
                // a single lost frame stalled the flow forever.
                // Retransmissions travel the ordinary polled frame path
                // (guest_tx), not the zero-copy inject path — correct and
                // rare, so the cost is irrelevant.
                if let Some(retx) = conn.retx.as_ref().map(std::sync::Arc::clone) {
                    let mut ring = retx.lock().unwrap();
                    let (ref mut base, ref mut ring_buf, fin_seq) = *ring;
                    let drained = acked.wrapping_sub(*base);
                    if drained > 0 && drained < 0x8000_0000 {
                        // The guest ACKed more of our stream — release the
                        // buffered prefix and reset the loss-recovery clocks.
                        let n = (drained as usize).min(ring_buf.len());
                        ring_buf.drain(..n);
                        *base = acked;
                        conn.last_progress = now;
                        conn.rto = super::INITIAL_RTO;
                        conn.dup_acks = 0;
                    }
                    if !nothing_in_flight {
                        let timed_out = now.duration_since(conn.last_progress) >= conn.rto;
                        if conn.fast_retransmit || timed_out {
                            let offset = acked.wrapping_sub(*base) as usize;
                            if offset < ring_buf.len() {
                                // Clamp to in-flight bytes: the owner tees
                                // after advancing SND.NXT, but never emit
                                // past `sent` even if a racing tee briefly
                                // runs ahead — the guest would ACK bytes
                                // the intercept then rejects as beyond
                                // `our_seq`.
                                let data: Vec<u8> = ring_buf
                                    .iter()
                                    .skip(offset)
                                    .take(in_flight as usize)
                                    .copied()
                                    .collect();
                                emit_data_frames(&ctx, conn, acked, &data, &mut frames);
                            }
                            if let Some(fin_seq) = fin_seq
                                && sent.wrapping_sub(fin_seq) < 0x8000_0000
                                && fin_seq.wrapping_sub(acked) < 0x8000_0000
                            {
                                frames.push(crate::ethernet::build_tcp_fin_frame(
                                    &crate::ethernet::TcpFrameParams {
                                        src_ip: conn.remote_ip,
                                        dst_ip: conn.guest_ip,
                                        src_port: conn.remote_port,
                                        dst_port: conn.guest_port,
                                        seq: fin_seq,
                                        ack: conn.last_ack,
                                        window: 65535,
                                        src_mac: gw_mac,
                                        dst_mac: guest_mac,
                                    },
                                ));
                            }
                            tracing::debug!(
                                "Fast path inline retransmit {}:{} → {}:{} ({} bytes from seq={acked}, cause={}, rto={:?})",
                                conn.remote_ip,
                                conn.remote_port,
                                conn.guest_ip,
                                conn.guest_port,
                                in_flight,
                                if conn.fast_retransmit {
                                    "dup-acks"
                                } else {
                                    "rto"
                                },
                                conn.rto,
                            );
                            conn.fast_retransmit = false;
                            conn.dup_acks = 0;
                            conn.retransmits += 1;
                            conn.last_progress = now;
                            conn.rto = (conn.rto * 2).min(super::MAX_RTO);
                        }
                    } else {
                        // Nothing in flight — keep the RTO clock parked so
                        // the next send starts a fresh timeout window.
                        conn.last_progress = now;
                        conn.fast_retransmit = false;
                    }
                }
                if super::send_budget(sent, acked, window) == 0 && nothing_in_flight {
                    match conn.window_stalled_at {
                        None => conn.window_stalled_at = Some(now),
                        Some(at)
                            if now.duration_since(at) >= super::ZERO_WINDOW_PERSIST_INTERVAL =>
                        {
                            conn.window_stalled_at = Some(now);
                            frames.push(crate::ethernet::build_tcp_data_frame(
                                &crate::ethernet::TcpFrameParams {
                                    src_ip: conn.remote_ip,
                                    dst_ip: conn.guest_ip,
                                    src_port: conn.remote_port,
                                    dst_port: conn.guest_port,
                                    seq: sent.wrapping_sub(1),
                                    ack: conn.last_ack,
                                    window: 65535,
                                    src_mac: gw_mac,
                                    dst_mac: guest_mac,
                                },
                                &[0u8],
                            ));
                        }
                        Some(_) => {}
                    }
                } else {
                    conn.window_stalled_at = None;
                }
                continue;
            }
            // A half-closed non-inline flow is fully done once the host has
            // also EOF'd (our FIN was sent) and the guest ACKed that FIN.
            if let Some(fin_at) = conn.guest_fin_at {
                if conn.host_eof {
                    if let Some(fin_seq) = conn.fin_seq {
                        let acked_past_fin = conn
                            .guest_acked
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .wrapping_sub(fin_seq);
                        if (1..0x8000_0000).contains(&acked_past_fin) {
                            to_remove.push(*key);
                            continue;
                        }
                    }
                }
                // FIN_WAIT2 leak guard: the guest finished sending but the
                // upstream keeps its write side open (or the guest silently
                // dropped its FIN_WAIT2 state). The clock is refreshed on every
                // host→guest byte below, so this only fires on a truly idle
                // half-open — reap it instead of holding the entry + fd forever.
                if now.duration_since(fin_at) >= super::HALF_CLOSE_TIMEOUT {
                    tracing::debug!(
                        "Fast path: half-close idle timeout, reaping {}:{} → {}:{}",
                        key.src_ip,
                        key.src_port,
                        key.dst_ip,
                        key.dst_port,
                    );
                    to_remove.push(*key);
                    continue;
                }
            }
            // ---- Sender-side loss recovery (runs for host_eof flows too:
            // in-flight data and the FIN itself still need repair) ----
            let acked = conn.guest_acked.load(std::sync::atomic::Ordering::Relaxed);
            let drained = acked.wrapping_sub(conn.retransmit_seq);
            if drained > 0 && drained < 0x8000_0000 {
                // The guest ACKed more of our stream — release the buffered
                // prefix and reset the loss-recovery clocks.
                let n = (drained as usize).min(conn.retransmit_buf.len());
                conn.retransmit_buf.drain(..n);
                conn.retransmit_seq = acked;
                conn.last_progress = now;
                conn.rto = super::INITIAL_RTO;
                conn.dup_acks = 0;
            }
            let sent = conn.our_seq.load(std::sync::atomic::Ordering::Relaxed);
            let in_flight = sent.wrapping_sub(acked);
            // Dead-flow give-up: while soliciting (unACKed data or FIN being
            // RTO-retransmitted, or a closed window under persist probes) a
            // live guest always answers — with at least a dup-ACK or a
            // window-bearing ACK. Prolonged unanswered soliciting means the
            // guest endpoint is gone; without this the RTO path retransmits
            // forever and the entry leaks its fd and retransmission buffer.
            // Both clocks must be stale: a long-idle flow whose upstream just
            // woke up starts the soliciting clock here and now, so its guest
            // gets the full deadline to answer despite an ancient last frame.
            let soliciting =
                (in_flight > 0 && in_flight < 0x8000_0000) || conn.window_stalled_at.is_some();
            if !soliciting {
                conn.soliciting_since = None;
            } else if conn.soliciting_since.is_none() {
                conn.soliciting_since = Some(now);
            }
            if conn
                .soliciting_since
                .is_some_and(|since| now.duration_since(since) >= dead_flow_timeout)
                && now.duration_since(conn.last_guest_activity) >= dead_flow_timeout
            {
                tracing::warn!(
                    "Fast path: guest silent {:?} on soliciting flow {}:{} → {}:{}, RST + reap",
                    dead_flow_timeout,
                    key.src_ip,
                    key.src_port,
                    key.dst_ip,
                    key.dst_port,
                );
                frames.push(crate::ethernet::build_tcp_rst_frame(
                    &crate::ethernet::TcpFrameParams {
                        src_ip: conn.remote_ip,
                        dst_ip: conn.guest_ip,
                        src_port: conn.remote_port,
                        dst_port: conn.guest_port,
                        seq: sent,
                        ack: conn.last_ack,
                        window: 0,
                        src_mac: gw_mac,
                        dst_mac: guest_mac,
                    },
                ));
                to_remove.push(*key);
                continue;
            }
            if in_flight > 0 && in_flight < 0x8000_0000 {
                let timed_out = now.duration_since(conn.last_progress) >= conn.rto;
                if conn.fast_retransmit || timed_out {
                    // Resend everything from the guest's ack point: buffered
                    // data first, then the FIN if it is still unACKed. The
                    // path beyond guest eth0 (bridge → veth → container
                    // backlog) drops under burst; without this a single
                    // dropped frame wedges the flow forever.
                    let offset = acked.wrapping_sub(conn.retransmit_seq) as usize;
                    if offset < conn.retransmit_buf.len() {
                        let data: Vec<u8> =
                            conn.retransmit_buf.iter().skip(offset).copied().collect();
                        emit_data_frames(&ctx, conn, acked, &data, &mut frames);
                    }
                    if let Some(fin_seq) = conn.fin_seq
                        && sent.wrapping_sub(fin_seq) < 0x8000_0000
                        && fin_seq.wrapping_sub(acked) < 0x8000_0000
                    {
                        frames.push(crate::ethernet::build_tcp_fin_frame(
                            &crate::ethernet::TcpFrameParams {
                                src_ip: conn.remote_ip,
                                dst_ip: conn.guest_ip,
                                src_port: conn.remote_port,
                                dst_port: conn.guest_port,
                                seq: fin_seq,
                                ack: conn.last_ack,
                                window: 65535,
                                src_mac: gw_mac,
                                dst_mac: guest_mac,
                            },
                        ));
                    }
                    tracing::debug!(
                        "Fast path retransmit {}:{} → {}:{} ({} bytes from seq={acked}, cause={}, rto={:?})",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port,
                        in_flight,
                        if conn.fast_retransmit {
                            "dup-acks"
                        } else {
                            "rto"
                        },
                        conn.rto,
                    );
                    conn.fast_retransmit = false;
                    conn.dup_acks = 0;
                    conn.retransmits += 1;
                    conn.last_progress = now;
                    conn.rto = (conn.rto * 2).min(super::MAX_RTO);
                }
            } else {
                // Nothing in flight — keep the RTO clock parked so the next
                // send starts a fresh timeout window.
                conn.last_progress = now;
                conn.fast_retransmit = false;
            }

            if conn.host_eof {
                continue;
            }

            // Never read (hence send) beyond the guest's advertised receive
            // window (capped: the cap bounds both the retransmission buffer
            // and the burst a flow can throw at the guest's bridge/veth
            // backlog). Unread bytes stay in the host socket buffer —
            // kernel-level backpressure toward the upstream.
            let window = conn
                .guest_window
                .load(std::sync::atomic::Ordering::Relaxed)
                .min(super::HONORED_WINDOW_CAP);
            let budget = super::send_budget(sent, acked, window) as usize;
            let cap = if budget == 0 {
                if !conn.window_stalled {
                    conn.window_stalled = true;
                    conn.window_stalled_at = Some(now);
                    tracing::debug!(
                        "Fast path window stall {}:{} → {}:{} (sent={sent}, acked={acked}, window={window})",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port,
                    );
                }
                // Zero-window persist: a window closed with nothing in flight is
                // reopened only by a guest window-update ACK; if that ACK is
                // lost the flow deadlocks. Probe with a single byte past the
                // window on a timer to force the guest to re-advertise it. Once
                // the probe byte is in flight, the RTO retransmit path above
                // keeps probing on its own, so this only kicks the first byte.
                let in_flight = sent.wrapping_sub(acked);
                let nothing_in_flight = in_flight == 0 || in_flight >= 0x8000_0000;
                let probe_due = conn.window_stalled_at.is_some_and(|at| {
                    now.duration_since(at) >= super::ZERO_WINDOW_PERSIST_INTERVAL
                });
                if nothing_in_flight && probe_due {
                    conn.window_stalled_at = Some(now);
                    1 // one byte beyond the closed window
                } else {
                    continue;
                }
            } else {
                if conn.window_stalled {
                    conn.window_stalled = false;
                    conn.window_stalled_at = None;
                    tracing::debug!(
                        "Fast path window reopened {}:{} → {}:{} (sent={sent}, acked={acked}, window={window})",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port,
                    );
                }
                budget.min(read_cap)
            };

            use std::io::Read;
            match conn.stream.read(&mut read_buf[..cap]) {
                Ok(0) => {
                    // Host EOF — send FIN to guest.
                    conn.host_eof = true;
                    let seq_now = conn.our_seq.load(std::sync::atomic::Ordering::Relaxed);
                    let fin =
                        crate::ethernet::build_tcp_fin_frame(&crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: seq_now,
                            ack: conn.last_ack,
                            window: 65535,
                            src_mac: gw_mac,
                            dst_mac: guest_mac,
                        });
                    // FIN consumes 1 SEQ; remember its position so a lost
                    // FIN is retransmitted like any other in-flight byte.
                    conn.fin_seq = Some(seq_now);
                    conn.our_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    frames.push(fin);
                    // Don't remove yet — keep the entry so the guest's FIN-ACK
                    // response is handled by try_fast_path_intercept (which will
                    // see the FIN flag and clean up).
                }
                Ok(n) => {
                    conn.down_bytes += n as u64;
                    // Host→guest progress refreshes the half-close idle clock so
                    // a slow but live streamed response is never reaped.
                    if conn.guest_fin_at.is_some() {
                        conn.guest_fin_at = Some(now);
                    }
                    let seq_now = conn.our_seq.load(std::sync::atomic::Ordering::Relaxed);
                    conn.retransmit_buf.extend(&read_buf[..n]);
                    emit_data_frames(&ctx, conn, seq_now, &read_buf[..n], &mut frames);
                    conn.our_seq
                        .fetch_add(n as u32, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No data available — expected for non-blocking.
                }
                Err(e) => {
                    tracing::warn!(
                        "Fast path RX error {}:{} → {}:{}, RST to guest: {e}",
                        conn.remote_ip,
                        conn.remote_port,
                        conn.guest_ip,
                        conn.guest_port
                    );
                    // Upstream died mid-stream — propagate as RST. Data may
                    // be lost, so a FIN would let the guest mistake a
                    // truncated stream for a complete one, and no frame at
                    // all leaves the guest ESTABLISHED forever (ABX-431).
                    let rst =
                        crate::ethernet::build_tcp_rst_frame(&crate::ethernet::TcpFrameParams {
                            src_ip: conn.remote_ip,
                            dst_ip: conn.guest_ip,
                            src_port: conn.remote_port,
                            dst_port: conn.guest_port,
                            seq: conn.our_seq.load(std::sync::atomic::Ordering::Relaxed),
                            ack: conn.last_ack,
                            window: 0,
                            src_mac: gw_mac,
                            dst_mac: guest_mac,
                        });
                    frames.push(rst);
                    to_remove.push(*key);
                }
            }
        }

        self.finish_controlled_poll(controlled, &mut frames);
        for key in to_remove {
            self.close_fast_path(&key);
        }

        frames
    }
}
