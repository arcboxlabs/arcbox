//! Ethernet and borrowed IP entry points for established TCP interception.

use super::TcpBridge;
use crate::ethernet::ETH_HEADER_LEN;

impl TcpBridge {
    /// Intercept an established TCP frame; return None when the classifier must handle the frame.
    pub fn try_fast_path_intercept(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        self.try_fast_path_intercept_batch(std::slice::from_ref(&frame))
            .map(|(_, reply)| reply)
    }

    /// Intercept up to 64 consecutive upload frames, bounded by 256 KiB of payload.
    /// Return the consumed prefix length and one cumulative ACK, RST, or empty reply.
    /// Different ACK state, gaps, control flags, or retained data use the single-frame path.
    pub fn try_fast_path_intercept_batch<T: AsRef<[u8]>>(
        &mut self,
        frames: &[T],
    ) -> Option<(usize, Vec<u8>)> {
        self.intercept_at::<ETH_HEADER_LEN, T>(frames)
    }

    /// Intercept borrowed IP packets with the same bounds and replies as the Ethernet entry point.
    /// The caller must drain earlier classifier input first to preserve handshake and fragment order.
    /// SYNs and fragments return None for classification. Retained upload bytes receive owned copies.
    pub fn try_fast_path_intercept_ip_batch<T: AsRef<[u8]>>(
        &mut self,
        packets: &[T],
    ) -> Option<(usize, Vec<u8>)> {
        let tcp = super::tcp_at(packets.first()?.as_ref(), 0)?;
        if tcp.flags() & 0x02 != 0 {
            return None;
        }
        self.intercept_at::<0, T>(packets)
    }
}
