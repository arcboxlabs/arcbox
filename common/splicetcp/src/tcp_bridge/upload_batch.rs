//! Bounded vectored writes for consecutive upload frames with identical ACK state.

use super::FastPathConn;
use crate::ethernet::ETH_HEADER_LEN;
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
    pub fn extend<T: AsRef<[u8]>>(&mut self, first: &[u8], following: &'a [T], mut seq: u32) {
        let Some((first_ip, first_tcp, _)) = plain_upload(first) else {
            return;
        };
        for frame in following.iter().take(MAX_SEGMENTS - 1) {
            let Some((ip, tcp, payload)) = plain_upload(frame.as_ref()) else {
                break;
            };
            if ip[12..20] != first_ip[12..20]
                || tcp[..4] != first_tcp[..4]
                || tcp[8..12] != first_tcp[8..12]
                || tcp[14..16] != first_tcp[14..16]
                || u32::from_be_bytes(tcp[4..8].try_into().unwrap()) != seq
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
        let result = if self.count == 1 {
            conn.stream.write(&self.payloads[0])
        } else {
            conn.stream.write_vectored(&self.payloads[..self.count])
        };
        let accepted = match result {
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
}

/// Batch eligibility is stricter than single-frame interception; other frames stay on the existing path.
fn plain_upload(frame: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    if frame.get(12..14)? != [0x08, 0x00] {
        return None;
    }
    let ip = frame.get(ETH_HEADER_LEN..)?;
    let header = ip.get(..20)?;
    let ihl = usize::from(header[0] & 0x0f) * 4;
    if header[0] >> 4 != 4
        || ihl < 20
        || header[9] != 6
        || u16::from_be_bytes([header[6], header[7]]) & 0x3fff != 0
    {
        return None;
    }
    let ip = ip.get(..usize::from(u16::from_be_bytes([header[2], header[3]])))?;
    let tcp = ip.get(ihl..)?;
    let header = tcp.get(..20)?;
    let offset = usize::from(header[12] >> 4) * 4;
    if offset < 20 || !matches!(header[13], 0x10 | 0x18) {
        return None;
    }
    let payload = tcp.get(offset..)?;
    if payload.is_empty() {
        return None;
    }
    Some((ip, tcp, payload))
}
