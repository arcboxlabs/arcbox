//! Explicit transport abandonment with bounded guest-facing TCP state.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use super::{FastPathConn, StdInstant, SynFlowKey, TcpBridge};
use crate::ethernet::{ETH_HEADER_LEN, TcpFrameParams, build_tcp_ack_frame};

pub(super) const ABANDON_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const MAX_ABANDONED_CONNS: usize = 4096;

#[derive(Clone)]
pub(super) struct AbandonedConn {
    our_seq: u32,
    last_ack: u32,
    guest_fin: bool,
    pub(super) last_activity: StdInstant,
}

impl From<&FastPathConn> for AbandonedConn {
    fn from(conn: &FastPathConn) -> Self {
        Self {
            our_seq: conn.our_seq.load(Ordering::Relaxed),
            last_ack: conn.last_ack,
            guest_fin: conn.guest_fin_at.is_some(),
            last_activity: StdInstant::now(),
        }
    }
}

pub(super) struct ControlledPoll {
    key: SynFlowKey,
    signal: Arc<AtomicBool>,
    before: AbandonedConn,
}

struct Segment {
    key: SynFlowKey,
    seq: u32,
    flags: u8,
    payload_len: u32,
}

impl Segment {
    fn parse(frame: &[u8], ip_start: usize) -> Option<Self> {
        let ip = frame.get(ip_start..)?;
        if ip.len() < 40
            || ip[0] >> 4 != 4
            || ip[9] != 6
            || u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0
        {
            return None;
        }
        let ihl = usize::from(ip[0] & 0x0f) * 4;
        let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
        let tcp = ip.get(ihl..total)?;
        if ihl < 20 || tcp.len() < 20 {
            return None;
        }
        let offset = usize::from(tcp[12] >> 4) * 4;
        if offset < 20 || offset > tcp.len() {
            return None;
        }
        Some(Self {
            key: SynFlowKey {
                src_ip: [ip[12], ip[13], ip[14], ip[15]].into(),
                dst_ip: [ip[16], ip[17], ip[18], ip[19]].into(),
                src_port: u16::from_be_bytes([tcp[0], tcp[1]]),
                dst_port: u16::from_be_bytes([tcp[2], tcp[3]]),
            },
            seq: u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]),
            flags: tcp[13],
            payload_len: (tcp.len() - offset) as u32,
        })
    }
}

impl TcpBridge {
    pub(super) fn intercept_at<const IP_START: usize, T: AsRef<[u8]>>(
        &mut self,
        frames: &[T],
    ) -> Option<(usize, Vec<u8>)> {
        if self.flow_controls.is_empty() && self.abandoned_conns.is_empty() {
            return self.intercept_open_at::<IP_START, T>(frames);
        }
        let segment = Segment::parse(frames.first()?.as_ref(), IP_START)?;
        let key = segment.key;
        let control = self.flow_controls.get(&key).cloned();
        let before = control
            .as_ref()
            .and_then(|_| self.fast_path_conns.get(&key).map(AbandonedConn::from));
        if let Some(signal) = &control
            && signal.load(Ordering::Acquire)
            && let Some(before) = &before
        {
            self.abandon_flow(key, before.clone());
        }
        if self.abandoned_conns.contains_key(&key) {
            return Some((1, self.intercept_abandoned(segment)));
        }
        let reply = self.intercept_open_at::<IP_START, T>(frames);
        // The signal must survive an I/O error removing the original connection.
        if control.is_some_and(|signal| signal.load(Ordering::Acquire))
            && let Some(before) = before
        {
            self.abandon_flow(key, before);
            return Some((1, self.intercept_abandoned(segment)));
        }
        reply
    }

    fn intercept_abandoned(&mut self, segment: Segment) -> Vec<u8> {
        if segment.flags & 0x04 != 0 {
            self.abandoned_conns.remove(&segment.key);
            return Vec::new();
        }
        let conn = self
            .abandoned_conns
            .get_mut(&segment.key)
            .expect("discard state exists");
        conn.last_activity = StdInstant::now();
        let end = segment.seq.wrapping_add(segment.payload_len);
        if !conn.guest_fin && segment.seq.wrapping_sub(conn.last_ack).cast_signed() <= 0 {
            if end.wrapping_sub(conn.last_ack).cast_signed() > 0 {
                conn.last_ack = end;
            }
            if segment.flags & 0x01 != 0 && end == conn.last_ack {
                conn.last_ack = conn.last_ack.wrapping_add(1);
                conn.guest_fin = true;
            }
        }
        if segment.payload_len == 0
            && segment.flags & (0x01 | 0x02) == 0
            && segment.seq != conn.last_ack.wrapping_sub(1)
        {
            return Vec::new();
        }
        build_tcp_ack_frame(&TcpFrameParams {
            src_ip: segment.key.dst_ip,
            dst_ip: segment.key.src_ip,
            src_port: segment.key.dst_port,
            dst_port: segment.key.src_port,
            seq: conn.our_seq,
            ack: conn.last_ack,
            window: u16::MAX,
            src_mac: self.fast_path_gateway_mac,
            dst_mac: self.fast_path_guest_mac.unwrap_or([0xff; 6]),
        })
    }

    fn abandon_flow(&mut self, key: SynFlowKey, mut state: AbandonedConn) {
        if let Some(conn) = self.fast_path_conns.get(&key) {
            state.last_ack = conn.last_ack;
            state.guest_fin = conn.guest_fin_at.is_some();
        }
        self.close_fast_path(&key);
        if self.abandoned_conns.len() >= MAX_ABANDONED_CONNS
            && let Some(oldest) = self
                .abandoned_conns
                .iter()
                .min_by_key(|(_, conn)| conn.last_activity)
                .map(|(key, _)| *key)
        {
            self.abandoned_conns.remove(&oldest);
        }
        self.abandoned_conns.insert(key, state);
    }

    pub(super) fn prepare_controlled_poll(&mut self) -> Vec<ControlledPoll> {
        let now = StdInstant::now();
        self.abandoned_conns
            .retain(|_, conn| now.duration_since(conn.last_activity) < ABANDON_IDLE_TIMEOUT);
        let mut controlled: Vec<_> = self
            .flow_controls
            .iter()
            .filter_map(|(key, signal)| {
                self.fast_path_conns.get(key).map(|conn| ControlledPoll {
                    key: *key,
                    signal: Arc::clone(signal),
                    before: AbandonedConn::from(conn),
                })
            })
            .collect();
        controlled.retain(|conn| {
            if conn.signal.load(Ordering::Acquire) {
                self.abandon_flow(conn.key, conn.before.clone());
                false
            } else {
                true
            }
        });
        controlled
    }

    pub(super) fn finish_controlled_poll(
        &mut self,
        controlled: Vec<ControlledPoll>,
        frames: &mut Vec<Vec<u8>>,
    ) {
        let mut abandoned = std::collections::HashSet::new();
        for conn in controlled {
            if conn.signal.load(Ordering::Acquire) {
                abandoned.insert(conn.key);
                if !self.abandoned_conns.contains_key(&conn.key) {
                    self.abandon_flow(conn.key, conn.before);
                }
            }
        }
        if !abandoned.is_empty() {
            frames.retain(|frame| {
                let segment = Segment::parse(frame, ETH_HEADER_LEN).expect("bridge emitted TCP");
                let key = SynFlowKey {
                    src_ip: segment.key.dst_ip,
                    dst_ip: segment.key.src_ip,
                    src_port: segment.key.dst_port,
                    dst_port: segment.key.src_port,
                };
                !abandoned.contains(&key)
            });
        }
    }
}
