//! Bounded vectored writes for consecutive upload frames and retained upload recovery.

use super::FastPathConn;
use std::io::{self, IoSlice, Write};

const MAX_SEGMENTS: usize = 64;
const MAX_BYTES: usize = 256 * 1024;

pub(super) struct UploadBatch<'a> {
    payloads: [IoSlice<'a>; MAX_SEGMENTS],
    pub count: usize,
    bytes: usize,
}

impl<'a> UploadBatch<'a> {
    pub fn new(payload: &'a [u8]) -> Self {
        let mut payloads = [IoSlice::new(&[]); MAX_SEGMENTS];
        payloads[0] = IoSlice::new(payload);
        Self {
            payloads,
            count: 1,
            bytes: payload.len(),
        }
    }

    /// Coalesce only identical download ACK state; the intercept applies that state once.
    pub fn extend<T: AsRef<[u8]>>(
        &mut self,
        first: &[u8],
        following: &'a [T],
        mut seq: u32,
        ip_start: usize,
    ) {
        let Some(first_tcp) = plain_upload(first, ip_start) else {
            return;
        };
        for frame in following.iter().take(MAX_SEGMENTS - 1) {
            let Some(tcp) = plain_upload(frame.as_ref(), ip_start) else {
                break;
            };
            let payload = tcp.payload();
            if tcp.ip().source() != first_tcp.ip().source()
                || tcp.ip().destination() != first_tcp.ip().destination()
                || tcp.source_port() != first_tcp.source_port()
                || tcp.destination_port() != first_tcp.destination_port()
                || tcp.acknowledgment() != first_tcp.acknowledgment()
                || tcp.window() != first_tcp.window()
                || tcp.sequence() != seq
                || self.bytes + payload.len() > MAX_BYTES
            {
                break;
            }
            self.payloads[self.count] = IoSlice::new(payload);
            self.count += 1;
            self.bytes += payload.len();
            seq = seq.wrapping_add(payload.len() as u32);
        }
    }

    /// Advance the ACK only over accepted bytes; retain every rejected suffix for polling.
    pub fn send(&self, conn: &mut FastPathConn) -> io::Result<bool> {
        let accepted = match self.write_to(&mut conn.stream) {
            Ok(n) => {
                conn.up_bytes += n as u64;
                conn.set_last_ack(conn.last_ack.wrapping_add(n as u32));
                if n < self.bytes {
                    conn.up_short_writes += 1;
                }
                Some(n)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                conn.up_would_block += 1;
                None
            }
            Err(e) => return Err(e),
        };
        let mut skip = accepted.unwrap_or(0);
        let mut seq = conn.last_ack;
        for payload in &self.payloads[..self.count] {
            let offset = skip.min(payload.len());
            skip -= offset;
            let tail = &payload[offset..];
            if !tail.is_empty() {
                conn.buffer_upload_segment(seq, tail);
                seq = seq.wrapping_add(tail.len() as u32);
            }
        }
        Ok(accepted.is_some())
    }

    fn write_to(&self, stream: &mut crate::NativeTcp<std::net::TcpStream>) -> io::Result<usize> {
        if self.count == 1 {
            stream.write(&self.payloads[0])
        } else {
            stream.write_vectored(&self.payloads[..self.count])
        }
    }
}

impl FastPathConn {
    /// Drain contiguous retained bytes in bounded batches, clipping retransmission overlap.
    /// Stop at a hole, short write, or WouldBlock; propagate hard errors for RST teardown.
    pub(super) fn drain_parked_segments(&mut self) -> io::Result<()> {
        loop {
            self.discard_acked_upload_segments();
            let mut batch = UploadBatch {
                payloads: [IoSlice::new(&[]); MAX_SEGMENTS],
                count: 0,
                bytes: 0,
            };
            let mut cursor = self.last_ack;
            for (seq, data) in &self.ooo_segs {
                let overlap = cursor.wrapping_sub(*seq);
                if overlap >= 0x8000_0000 {
                    break;
                }
                if overlap as usize >= data.len() {
                    continue;
                }
                let start = overlap as usize;
                let len = (data.len() - start).min(MAX_BYTES - batch.bytes);
                batch.payloads[batch.count] = IoSlice::new(&data[start..start + len]);
                batch.count += 1;
                batch.bytes += len;
                cursor = cursor.wrapping_add(len as u32);
                if batch.count == MAX_SEGMENTS || batch.bytes == MAX_BYTES {
                    break;
                }
            }
            if batch.count == 0 {
                break;
            }
            let offered = batch.bytes;
            let take = match batch.write_to(&mut self.stream) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.up_would_block += 1;
                    break;
                }
                Err(e) => return Err(e),
            };
            self.up_bytes += take as u64;
            self.set_last_ack(self.last_ack.wrapping_add(take as u32));
            if take < offered {
                self.up_short_writes += 1;
                break;
            }
        }
        self.discard_acked_upload_segments();
        Ok(())
    }

    fn discard_acked_upload_segments(&mut self) {
        while self.ooo_segs.front().is_some_and(|(seq, data)| {
            seq.wrapping_add(data.len() as u32)
                .wrapping_sub(self.last_ack)
                .cast_signed()
                <= 0
        }) {
            let (_, data) = self.ooo_segs.pop_front().unwrap();
            self.ooo_bytes -= data.len();
        }
    }
}

/// Batch eligibility is stricter than single-frame interception; other frames stay on the existing path.
fn plain_upload(frame: &[u8], ip_start: usize) -> Option<arcbox_packet::ip::TcpPacketView<'_>> {
    let tcp = super::tcp_at(frame, ip_start)?;
    if !matches!(tcp.flags(), 0x10 | 0x18) || tcp.payload().is_empty() {
        return None;
    }
    Some(tcp)
}
