//! Frame sink trait for host-to-guest frame injection.
//!
//! The datapath loop constructs Ethernet frames and sends them through
//! a [`FrameSink`]. The implementation (typically a crossbeam channel)
//! delivers them to the RX injection thread which writes to guest memory.

/// Sends raw Ethernet frames from the producer (datapath loop) to the
/// consumer (RX injection thread).
pub trait FrameSink: Send + Sync {
    /// Sends a frame. Returns `true` if accepted, `false` if full (frame
    /// dropped). The frame is a raw Ethernet frame WITHOUT the 12-byte
    /// virtio-net header (the injection thread adds that).
    fn send(&self, frame: Vec<u8>) -> bool;
}

/// [`FrameSink`] backed by a crossbeam bounded channel.
pub struct ChannelFrameSink {
    tx: crossbeam_channel::Sender<Vec<u8>>,
}

impl ChannelFrameSink {
    /// Creates a new channel-backed frame sink from the sending half.
    pub fn new(tx: crossbeam_channel::Sender<Vec<u8>>) -> Self {
        Self { tx }
    }
}

impl FrameSink for ChannelFrameSink {
    fn send(&self, frame: Vec<u8>) -> bool {
        self.tx.try_send(frame).is_ok()
    }
}
