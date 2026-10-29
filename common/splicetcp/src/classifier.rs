//! Ethernet and bare IPv4/IPv6 packet classifier for the datapath.
//!
//! Demultiplexes inbound Ethernet frames by protocol. Frames are *fed* to
//! [`FrameClassifier::classify_frame`] by a caller-owned source. On macOS,
//! `FdFrameSource` drains the guest socketpair (VM) or a `utun` (host tunnel).
//! The classifier routes frames by protocol:
//!
//! - **ARP** → [`ArpResponder`] — synthesizes the reply inline
//! - **TCP SYN** → gated for `TcpBridge::handle_outbound_syn`
//! - **TCP (non-SYN)** → queued for `drain_fast_path` / `drain_handshake`
//! - **DHCP** (UDP:67), **DNS** (UDP:53 to gateway), **UDP**, **ICMP** →
//!   stored in the `intercepted` queue for the datapath loop
//!
//! There is no userspace TCP state machine here. Connections are driven by
//! [`TcpBridge`](super::tcp_bridge) and the in-kernel guest stack; this
//! module is purely a demultiplexer.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use arcbox_datapath::FrameBuf;
use arcbox_datapath::pool::PacketPool;
use arcbox_packet::ip::{IpPacketView, TcpPacketView};

use crate::ethernet::{ArpResponder, ETH_HEADER_LEN};
use crate::fragment::FragmentReassembler;

/// Default Ethernet MTU (excludes Ethernet header).
#[cfg(test)]
const DEFAULT_ETHERNET_MTU: usize = 1500;

/// Enhanced MTU (excludes Ethernet header), ~2.7x fewer frames than 1500.
///
/// No longer a negotiated link MTU by default: the VZ device stays at 1500
/// (ABX-423 — see `arcbox-vz/src/device/network.rs`) and the VMMs pass the
/// actual link MTU to the datapath since ABX-428. This constant remains the
/// `ARCBOX_DIAG_NET_MTU_4000` reproduction-mode value and a sizing reference.
pub const ENHANCED_ETHERNET_MTU: usize = 4000;

/// Protocol numbers.
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

/// TCP SYN connection info extracted during frame classification.
///
/// SYN frames are held back (`gated_syns`) until the host connect
/// completes. The full frame is stored so it can be injected into
/// the rx_queue later.
#[derive(Debug, Clone)]
pub struct TcpSynInfo {
    pub dst_port: u16,
    pub src_ip: IpAddr,
    pub src_port: u16,
    pub dst_ip: IpAddr,
    pub syn_seq: u32,
    pub frame: Vec<u8>,
}

/// An intercepted frame from the guest destined for a non-TCP handler.
#[derive(Debug)]
pub struct InterceptedFrame {
    pub frame: FrameBuf,
    pub kind: InterceptedKind,
}

/// Classification of intercepted frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptedKind {
    /// DHCP packet (UDP dst port 67).
    Dhcp,
    /// DNS query (UDP dst port 53 to gateway IP).
    Dns,
    /// Other UDP traffic.
    Udp,
    /// ICMP traffic.
    Icmp,
}

/// Frame classifier.
///
/// Fed Ethernet frames via [`classify_frame`](Self::classify_frame), it
/// responds to ARP inline and routes TCP / UDP / ICMP into the appropriate
/// queues for downstream handlers. The caller owns the ingest source;
/// Unix callers may use `FrameSource` to drain an fd.
pub struct FrameClassifier {
    /// Gateway IP for DNS interception.
    gateway_ip: Ipv4Addr,
    /// Gateway MAC — captured at construction so ARP replies use it.
    gateway_mac: [u8; 6],
    /// TCP frames queued for downstream drains (fast-path intercept,
    /// handshake completion). Frames not matched by either drain are
    /// silently dropped on the next classifier iteration — there is no
    /// userspace TCP stack to consume them.
    rx_queue: VecDeque<FrameBuf>,
    /// Frames intercepted from the guest (DHCP, DNS, UDP, ICMP).
    intercepted: Vec<InterceptedFrame>,
    /// TCP SYN frames held for handoff to `TcpBridge::handle_outbound_syn`.
    gated_syns: Vec<TcpSynInfo>,
    /// ARP replies produced inline by `ArpResponder` during classification,
    /// drained by the datapath loop.
    pending_arp_replies: Vec<Vec<u8>>,
    /// ARP responder (gateway IP only).
    arp: ArpResponder,
    /// Pre-allocated packet pool for zero-alloc frame ownership.
    pool: Arc<PacketPool>,
    /// Buffer-sizing MTU (excludes Ethernet header). At least the device's
    /// configured MTU; see [`ENHANCED_ETHERNET_MTU`].
    #[allow(dead_code)]
    mtu: usize,
    /// Reassembles IPv4 and IPv6 fragments before transport classification.
    reassembler: FragmentReassembler,
}

impl FrameClassifier {
    /// Default packet-pool capacity: 4096 buffers. Each buffer is
    /// `MAX_PACKET_SIZE` (64 KiB), so resident pool memory is ~256 MiB. This is
    /// acceptable for the VM datapath because:
    /// - One pool per VM (not per connection)
    /// - Idle pages are compressed by macOS memory compressor
    /// - Smaller pools increase heap fallback frequency, defeating the purpose
    /// - At ~300K frames/sec (10 Gbps / 4000 MTU), 4096 gives ~13 ms burst headroom
    ///
    /// Memory-constrained hosts (e.g. an iOS Network Extension) should construct
    /// with [`with_pool_capacity`](Self::with_pool_capacity) and a smaller count.
    ///
    /// Note: this is deliberately distinct from `arcbox_datapath::DEFAULT_POOL_CAPACITY`
    /// (8192) — it preserves the classifier's own historical pool size and is
    /// unrelated to the datapath ring/pool default. Pass *this* constant to
    /// [`with_pool_capacity`](Self::with_pool_capacity), not the `arcbox_datapath` one.
    pub const DEFAULT_POOL_CAPACITY: usize = 4096;

    /// Creates a new classifier with the
    /// [default pool capacity](Self::DEFAULT_POOL_CAPACITY).
    ///
    /// `mtu` sizes internal buffers and must be at least the device's
    /// configured MTU; `ENHANCED_ETHERNET_MTU` (4000) is safe headroom for a
    /// default 1500 link.
    ///
    /// The gateway MAC (needed to synthesize ARP replies) is set later via
    /// [`set_gateway_mac`](Self::set_gateway_mac), once it is known.
    pub fn new(gateway_ip: Ipv4Addr, mtu: usize) -> Self {
        Self::with_pool_capacity(gateway_ip, mtu, Self::DEFAULT_POOL_CAPACITY)
    }

    /// Creates a classifier with an explicit packet-pool capacity (buffer count).
    ///
    /// Each pooled buffer is `MAX_PACKET_SIZE` (64 KiB), so resident pool memory
    /// is `pool_capacity * 64 KiB`. The default
    /// ([`DEFAULT_POOL_CAPACITY`](Self::DEFAULT_POOL_CAPACITY), 4096 ⇒ ~256 MiB)
    /// suits the VM datapath but is far too large for a memory-constrained host
    /// such as an iOS Network Extension (capped near 50 MiB) — pass a small count
    /// there (e.g. 256 ⇒ ~16 MiB).
    ///
    /// The pool only bounds burst headroom: once it is exhausted the classifier
    /// falls back to heap allocation, so a small pool trades throughput under
    /// bursts for a bounded footprint, never correctness. `pool_capacity` is
    /// clamped to a minimum of 1 by the pool.
    pub fn with_pool_capacity(gateway_ip: Ipv4Addr, mtu: usize, pool_capacity: usize) -> Self {
        // `PacketPool::new` is infallible in practice (it clamps capacity to ≥1);
        // `expect` documents the `Result` contract.
        let pool = Arc::new(PacketPool::new(pool_capacity).expect("packet pool allocation"));
        Self {
            gateway_ip,
            gateway_mac: [0; 6],
            rx_queue: VecDeque::new(),
            intercepted: Vec::new(),
            gated_syns: Vec::new(),
            pending_arp_replies: Vec::new(),
            arp: ArpResponder::new(gateway_ip, [0; 6]),
            pool,
            mtu,
            reassembler: FragmentReassembler::new(),
        }
    }

    /// Sets the gateway MAC used by the inline ARP responder.
    pub fn set_gateway_mac(&mut self, mac: [u8; 6]) {
        self.gateway_mac = mac;
        self.arp = ArpResponder::new(self.gateway_ip, mac);
    }

    /// Drains pending ARP replies produced inline by the classifier.
    pub fn take_arp_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_arp_replies)
    }

    /// Clears any TCP frames left in the rx queue after the fast-path /
    /// handshake drains have run. These are frames that didn't match any
    /// active flow — they're silently dropped since there is no userspace
    /// TCP stack to process them. (A legitimate guest kernel will retransmit
    /// if needed.)
    pub fn clear_unmatched_rx(&mut self) {
        self.rx_queue.clear();
    }

    /// Takes all intercepted frames, leaving the internal buffer empty.
    pub fn take_intercepted(&mut self) -> Vec<InterceptedFrame> {
        std::mem::take(&mut self.intercepted)
    }

    /// Takes all gated TCP SYN frames, leaving the buffer empty.
    pub fn take_gated_syns(&mut self) -> Vec<TcpSynInfo> {
        std::mem::take(&mut self.gated_syns)
    }

    /// Filters fast-path TCP data frames from `rx_queue`.
    ///
    /// For each frame in `rx_queue`, calls `try_intercept` with a reference to
    /// the frame data. If the callback returns `Some(ack_frame)`, the frame is
    /// removed from the queue and the ACK is collected for injection to the guest.
    ///
    /// Frames that are not intercepted (callback returns `None`) remain in the
    /// queue for the handshake drain or eventual drop by `clear_unmatched_rx`.
    pub fn drain_fast_path(
        &mut self,
        mut try_intercept: impl FnMut(&[u8]) -> Option<Vec<u8>>,
    ) -> Vec<Vec<u8>> {
        let mut ack_frames = Vec::new();

        // Filter in-place: remove intercepted frames via forward iteration.
        // VecDeque::remove(i) is O(n) but fast-path connections are few and
        // most frames pass through unintercepted, so removals are rare.
        let mut i = 0;
        while i < self.rx_queue.len() {
            if let Some(ack) = try_intercept(&self.rx_queue[i]) {
                if !ack.is_empty() {
                    ack_frames.push(ack);
                }
                self.rx_queue.remove(i);
                // Don't increment i — next element shifted into current slot.
            } else {
                i += 1;
            }
        }

        ack_frames
    }

    /// Drain queued TCP frames in arrival order, including handshake promotion before later data.
    /// Consecutive established upload frames share one bounded socket write and cumulative ACK.
    pub fn drain_tcp(&mut self, bridge: &mut crate::tcp_bridge::TcpBridge) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();
        while !self.rx_queue.is_empty() {
            if let Some((count, reply)) =
                bridge.try_fast_path_intercept_batch(self.rx_queue.make_contiguous())
            {
                self.rx_queue.drain(..count);
                if !reply.is_empty() {
                    replies.push(reply);
                }
            } else if let Some(frame) = self.rx_queue.pop_front()
                && let Some(handshake) = bridge.try_complete_handshake(&frame)
            {
                replies.extend(handshake);
            }
        }
        replies
    }

    /// Filters handshake-completion frames from `rx_queue`.
    ///
    /// Parallel to [`drain_fast_path`] but for frames that complete an
    /// in-progress shim handshake (guest ACK → PassiveOpen completion, or
    /// guest SYN-ACK → ActiveOpen completion). The callback returns
    /// `Some(reply_frames)` if consumed (the final ACK for ActiveOpen, or
    /// replies from processing PassiveOpen payload); `None` leaves the frame queued.
    pub fn drain_handshake(
        &mut self,
        mut try_complete: impl FnMut(&[u8]) -> Option<Vec<Vec<u8>>>,
    ) -> Vec<Vec<u8>> {
        let mut reply_frames = Vec::new();

        let mut i = 0;
        while i < self.rx_queue.len() {
            if let Some(replies) = try_complete(&self.rx_queue[i]) {
                reply_frames.extend(replies);
                self.rx_queue.remove(i);
            } else {
                i += 1;
            }
        }

        reply_frames
    }

    /// Classifies a frame and routes it to the appropriate queue.
    ///
    /// The frame is a raw Ethernet frame borrowed from the caller's ingest source. Only
    /// the branches that retain a frame (TCP rx queue, intercepted UDP/ICMP,
    /// gated SYNs) copy it into a pooled [`FrameBuf`]; ARP and dropped frames
    /// never touch the pool. Also learns the guest MAC from the source address.
    pub fn classify_frame(&mut self, frame: &[u8], guest_mac: &mut Option<[u8; 6]>) {
        if frame.len() < ETH_HEADER_LEN {
            return;
        }

        // Learn guest MAC from source address.
        let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
        learn_guest_mac(src_mac, guest_mac);

        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

        match ethertype {
            // ARP → inline responder (replies only for the gateway IP)
            0x0806 => {
                if let Some(reply) = self.arp.handle_arp(frame) {
                    self.pending_arp_replies.push(reply);
                }
            }
            // IPv4 and IPv6 → classify by protocol
            0x0800 | 0x86dd => {
                if let Some(ip) = IpPacketView::parse(&frame[ETH_HEADER_LEN..])
                    && (ethertype == 0x0800) == ip.source().is_ipv4()
                {
                    self.classify_ip(&frame[..ETH_HEADER_LEN], ip);
                }
            }
            // Everything else → drop
            _ => {
                tracing::debug!(
                    "Dropping frame with EtherType {:#06x}, len={}",
                    ethertype,
                    frame.len()
                );
            }
        }
    }

    /// Classifies bare IPv4/IPv6 input using the synthetic host/gateway Ethernet header.
    /// Retained packets are copied directly into their owned frame; no framing scratch buffer is needed.
    /// Drops input exceeding the ordinary packet length limit of its address family.
    pub fn classify_ip_packet(&mut self, packet: &[u8], guest_mac: &mut Option<[u8; 6]>) {
        let Some(ip) = IpPacketView::parse(packet) else {
            return;
        };
        let ipv6 = ip.source().is_ipv6();
        let maximum = usize::from(u16::MAX) + if ipv6 { 40 } else { 0 };
        if packet.len() > maximum {
            return;
        }
        learn_guest_mac(crate::shim::HOST_MAC, guest_mac);
        let mut header = crate::shim::synthetic_l2_header();
        if ipv6 {
            header[12..14].copy_from_slice(&[0x86, 0xdd]);
        }
        self.classify_ip(&header, ip);
    }

    /// Classifies a structurally valid IP packet by its transport protocol.
    fn classify_ip(&mut self, header: &[u8], ip: IpPacketView<'_>) {
        let packet = ip.packet();
        if ip.fragment().is_some() {
            if let Some(reassembled) = self
                .reassembler
                .push(&[header, packet].concat(), std::time::Instant::now())
                && let Some(ip) = IpPacketView::parse(&reassembled[ETH_HEADER_LEN..])
            {
                self.classify_ip(&reassembled[..ETH_HEADER_LEN], ip);
            }
            return;
        }
        match ip.protocol() {
            PROTO_TCP => {
                let Some(tcp) = TcpPacketView::parse(ip) else {
                    return;
                };
                if tcp.flags() & 0x12 == 0x02 {
                    self.gated_syns.push(TcpSynInfo {
                        src_ip: ip.source(),
                        dst_ip: ip.destination(),
                        src_port: tcp.source_port(),
                        dst_port: tcp.destination_port(),
                        syn_seq: tcp.sequence(),
                        frame: [header, packet].concat(),
                    });
                } else {
                    self.rx_queue
                        .push_back(FrameBuf::from_pool_with_prefix(&self.pool, header, packet));
                }
            }
            PROTO_UDP => {
                let Some(udp) = ip.transport().filter(|bytes| bytes.len() >= 8) else {
                    return;
                };
                let length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
                if length < 8 || length > udp.len() {
                    return;
                }
                let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
                let kind = if dst_port == 67 && ip.destination().is_ipv4() {
                    InterceptedKind::Dhcp
                } else if dst_port == 53 && ip.destination() == IpAddr::V4(self.gateway_ip) {
                    InterceptedKind::Dns
                } else {
                    InterceptedKind::Udp
                };
                let frame = FrameBuf::from_pool_with_prefix(&self.pool, header, packet);
                self.intercepted.push(InterceptedFrame { frame, kind });
            }
            PROTO_ICMP if ip.source().is_ipv4() => {
                let frame = FrameBuf::from_pool_with_prefix(&self.pool, header, packet);
                self.intercepted.push(InterceptedFrame {
                    frame,
                    kind: InterceptedKind::Icmp,
                });
            }
            _ => {}
        }
    }
}

/// Learns the guest MAC from a frame's source address.
fn learn_guest_mac(mac: [u8; 6], guest_mac: &mut Option<[u8; 6]>) {
    // Skip broadcast/multicast
    if mac[0] & 0x01 != 0 || mac == [0; 6] {
        return;
    }
    if guest_mac.is_none() {
        tracing::info!(
            "Learned guest MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
    }
    *guest_mac = Some(mac);
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only `fd_source_drain_reads_and_classifies` below needs raw-fd
    // plumbing and `FdFrameSource`, which is macOS-only; gate that import
    // and its helpers with it rather than the whole test module, since
    // every other test here exercises platform-neutral classifier logic.
    #[cfg(target_os = "macos")]
    use std::os::fd::{FromRawFd, RawFd};

    #[cfg(target_os = "macos")]
    use crate::frame_source::{FdFrameSource, FrameSource};

    #[cfg(target_os = "macos")]
    fn socketpair() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        use std::os::fd::OwnedFd;
        let mut fds: [i32; 2] = [0; 2];
        // SAFETY: valid pointer to 2-element array.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0, "socketpair() failed");
        // SAFETY: fds are valid file descriptors from socketpair.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[cfg(target_os = "macos")]
    fn set_nonblocking(fd: RawFd) {
        // SAFETY: fcntl on a valid fd.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    /// Writes raw bytes to an FD.
    #[cfg(target_os = "macos")]
    fn fd_write_raw(fd: RawFd, data: &[u8]) {
        // SAFETY: writing from valid buffer to valid fd.
        unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    }

    /// Builds a minimal ARP frame for testing.
    fn make_arp_frame() -> Vec<u8> {
        // Request: "Who has 192.168.64.1? Tell 192.168.64.2"
        let mut frame = vec![0u8; 42];
        let guest_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        // Ethernet: dst=broadcast, src=guest, type=ARP
        frame[0..6].copy_from_slice(&[0xFF; 6]);
        frame[6..12].copy_from_slice(&guest_mac);
        frame[12..14].copy_from_slice(&[0x08, 0x06]);
        // ARP payload
        frame[14..16].copy_from_slice(&1u16.to_be_bytes()); // hw type = Ethernet
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // proto = IPv4
        frame[18] = 6; // hlen
        frame[19] = 4; // plen
        frame[20..22].copy_from_slice(&1u16.to_be_bytes()); // opcode = request
        frame[22..28].copy_from_slice(&guest_mac); // sender hw
        frame[28..32].copy_from_slice(&[192, 168, 64, 2]); // sender ip
        frame[32..38].copy_from_slice(&[0; 6]); // target hw (unknown)
        frame[38..42].copy_from_slice(&[192, 168, 64, 1]); // target ip = gateway
        frame
    }

    /// Builds a minimal TCP frame for testing.
    fn make_tcp_frame() -> Vec<u8> {
        let mut frame = vec![0u8; ETH_HEADER_LEN + 40];
        let guest_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        // Ethernet
        frame[0..6].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        frame[6..12].copy_from_slice(&guest_mac);
        frame[12..14].copy_from_slice(&[0x08, 0x00]); // IPv4
        // IPv4 header
        let ip = ETH_HEADER_LEN;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&40u16.to_be_bytes());
        frame[ip + 9] = PROTO_TCP;
        frame[ip + 12..ip + 16].copy_from_slice(&[192, 168, 64, 2]);
        frame[ip + 16..ip + 20].copy_from_slice(&[1, 1, 1, 1]);
        frame[ip + 20 + 12] = 0x50;
        frame
    }

    /// Builds a minimal UDP frame for testing.
    fn make_udp_frame(dst_ip: [u8; 4], dst_port: u16) -> Vec<u8> {
        let mut frame = vec![0u8; ETH_HEADER_LEN + 28];
        let guest_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        frame[0..6].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        frame[6..12].copy_from_slice(&guest_mac);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        let ip = ETH_HEADER_LEN;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&28u16.to_be_bytes());
        frame[ip + 9] = PROTO_UDP;
        frame[ip + 12..ip + 16].copy_from_slice(&[192, 168, 64, 2]);
        frame[ip + 16..ip + 20].copy_from_slice(&dst_ip);
        // UDP header
        let l4 = ip + 20;
        frame[l4..l4 + 2].copy_from_slice(&1234u16.to_be_bytes());
        frame[l4 + 2..l4 + 4].copy_from_slice(&dst_port.to_be_bytes());
        frame[l4 + 4..l4 + 6].copy_from_slice(&8u16.to_be_bytes());
        frame
    }

    /// Builds a minimal ICMP frame for testing.
    fn make_icmp_frame() -> Vec<u8> {
        let mut frame = vec![0u8; ETH_HEADER_LEN + 28];
        let guest_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        frame[0..6].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        frame[6..12].copy_from_slice(&guest_mac);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        let ip = ETH_HEADER_LEN;
        frame[ip] = 0x45;
        frame[ip + 2..ip + 4].copy_from_slice(&28u16.to_be_bytes());
        frame[ip + 9] = PROTO_ICMP;
        frame[ip + 12..ip + 16].copy_from_slice(&[192, 168, 64, 2]);
        frame[ip + 16..ip + 20].copy_from_slice(&[8, 8, 8, 8]);
        frame
    }

    #[test]
    fn classify_arp_generates_reply() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        device.set_gateway_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0x01]);
        let mut guest_mac = None;

        device.classify_frame(&make_arp_frame(), &mut guest_mac);

        assert!(device.rx_queue.is_empty(), "ARP must not queue");
        assert_eq!(
            device.pending_arp_replies.len(),
            1,
            "ArpResponder should have produced a reply"
        );
    }

    /// Builds a minimal IPv6 TCP SYN frame (EtherType 0x86DD).
    fn make_ipv6_tcp_syn_frame() -> Vec<u8> {
        // ETH(14) + IPv6(40) + TCP(20).
        let mut frame = vec![0u8; ETH_HEADER_LEN + 40 + 20];
        let guest_mac = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        frame[0..6].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        frame[6..12].copy_from_slice(&guest_mac);
        frame[12..14].copy_from_slice(&0x86DDu16.to_be_bytes()); // IPv6
        let ip = ETH_HEADER_LEN;
        frame[ip] = 0x60; // version = 6
        frame[ip + 4..ip + 6].copy_from_slice(&20u16.to_be_bytes()); // payload length
        frame[ip + 6] = PROTO_TCP; // next header
        frame[ip + 7] = 64; // hop limit
        // src ::1, dst 2606:4700:4700::1111 (a real public v6 resolver shape).
        frame[ip + 8..ip + 24].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        frame[ip + 24..ip + 40].copy_from_slice(&[
            0x26, 0x06, 0x47, 0x00, 0x47, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x11,
        ]);
        // TCP SYN.
        let l4 = ip + 40;
        frame[l4 + 2..l4 + 4].copy_from_slice(&443u16.to_be_bytes()); // dst port
        frame[l4 + 12] = 0x50; // data offset
        frame[l4 + 13] = 0x02; // SYN
        frame
    }

    #[test]
    fn classify_ipv6_syn_requires_matching_ethernet_family() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        device.set_gateway_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0x01]);
        let mut guest_mac = None;

        let frame = make_ipv6_tcp_syn_frame();
        for (mut invalid, ethertype) in [
            (frame.clone(), [0x08, 0x00]),
            (make_tcp_frame(), [0x86, 0xdd]),
        ] {
            invalid[12..14].copy_from_slice(&ethertype);
            device.classify_frame(&invalid, &mut guest_mac);
        }
        assert!(device.rx_queue.is_empty());
        assert!(device.intercepted.is_empty());
        assert!(device.gated_syns.is_empty());
        device.classify_frame(&frame, &mut guest_mac);
        assert_eq!(device.take_gated_syns()[0].frame, frame);
        assert!(device.pending_arp_replies.is_empty());
    }

    /// Reject every truncated IPv6/TCP prefix before retaining bytes.
    #[test]
    fn classify_truncated_ipv6_does_not_panic() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        let frame = make_ipv6_tcp_syn_frame();
        for length in 0..frame.len() {
            device.classify_frame(&frame[..length], &mut guest_mac);
            if length >= ETH_HEADER_LEN {
                device.classify_ip_packet(&frame[ETH_HEADER_LEN..length], &mut guest_mac);
            }
        }
        assert!(device.rx_queue.is_empty());
        assert!(device.intercepted.is_empty());
        assert!(device.gated_syns.is_empty());
    }

    #[test]
    fn classify_tcp_goes_to_rx_queue() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        device.classify_frame(&make_tcp_frame(), &mut guest_mac);

        assert_eq!(device.rx_queue.len(), 1);
        assert!(device.intercepted.is_empty());
    }

    #[test]
    fn small_pool_capacity_classifies_and_falls_back_to_heap() {
        // A memory-constrained classifier (e.g. an iOS Network Extension) built
        // with a tiny pool must still classify, and must not drop frames once the
        // pool is exhausted — the pool only bounds burst headroom, not
        // correctness.
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let cap = 4;
        let mut device = FrameClassifier::with_pool_capacity(gateway_ip, DEFAULT_ETHERNET_MTU, cap);
        let mut guest_mac = None;

        // Queue more frames than the pool holds (none are drained, so each holds
        // a buffer); every frame must still land in the rx queue.
        let n = cap + 2;
        for _ in 0..n {
            device.classify_frame(&make_tcp_frame(), &mut guest_mac);
        }
        assert_eq!(
            device.rx_queue.len(),
            n,
            "small pool must not drop frames past capacity (heap fallback)",
        );
    }

    #[test]
    fn classify_dhcp_intercepted() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        device.classify_frame(&make_udp_frame([255, 255, 255, 255], 67), &mut guest_mac);

        assert!(device.rx_queue.is_empty());
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Dhcp);
    }

    #[test]
    fn classify_dns_to_gateway_intercepted() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        device.classify_frame(&make_udp_frame([192, 168, 64, 1], 53), &mut guest_mac);

        assert!(device.rx_queue.is_empty());
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Dns);
    }

    #[test]
    fn classify_dns_to_external_is_udp() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        // DNS to 8.8.8.8 is regular UDP, not intercepted as DNS.
        device.classify_frame(&make_udp_frame([8, 8, 8, 8], 53), &mut guest_mac);

        assert!(device.rx_queue.is_empty());
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Udp);
    }

    #[test]
    fn classify_icmp_intercepted() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        device.classify_frame(&make_icmp_frame(), &mut guest_mac);

        assert!(device.rx_queue.is_empty());
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Icmp);
    }

    #[test]
    fn classify_regular_udp_intercepted() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        device.classify_frame(&make_udp_frame([1, 1, 1, 1], 443), &mut guest_mac);

        assert!(device.rx_queue.is_empty());
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Udp);
    }

    #[test]
    fn fragmented_udp_reassembles_into_one_intercept() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        // A 3000-byte UDP datagram fragmented at MTU 1500, as the guest
        // kernel would emit it (3 fragments).
        let payload = vec![0xA5u8; 3000];
        let frags = crate::ethernet::build_udp_ip_ethernet(
            Ipv4Addr::new(192, 168, 64, 2),
            Ipv4Addr::new(1, 1, 1, 1),
            40000,
            9999,
            &payload,
            [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            1500,
        );
        assert_eq!(frags.len(), 3);

        for f in &frags {
            device.classify_frame(f, &mut guest_mac);
        }

        // Exactly one intercepted frame, carrying the whole datagram.
        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Udp);
        let frame = &device.intercepted[0].frame;
        assert_eq!(frame.len(), ETH_HEADER_LEN + 20 + 8 + payload.len());
        assert_eq!(&frame[ETH_HEADER_LEN + 28..], &payload[..]);
    }

    #[test]
    fn fragmented_dns_to_gateway_classifies_as_dns() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        // Oversized DNS query to the gateway: the kind must be decided on
        // the reassembled datagram, not on any single fragment.
        let payload = vec![0u8; 2000];
        let frags = crate::ethernet::build_udp_ip_ethernet(
            Ipv4Addr::new(192, 168, 64, 2),
            gateway_ip,
            40000,
            53,
            &payload,
            [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            1500,
        );
        for f in &frags {
            device.classify_frame(f, &mut guest_mac);
        }

        assert_eq!(device.intercepted.len(), 1);
        assert_eq!(device.intercepted[0].kind, InterceptedKind::Dns);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn fd_source_drain_reads_and_classifies() {
        use std::os::fd::AsRawFd;
        let (host_fd, guest_fd) = socketpair();
        set_nonblocking(host_fd.as_raw_fd());

        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        device.set_gateway_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0x01]);
        let mut source = FdFrameSource::new(host_fd.as_raw_fd());
        let mut guest_mac = None;

        // Write an ARP request and an ICMP frame via the guest side.
        fd_write_raw(guest_fd.as_raw_fd(), &make_arp_frame());
        fd_write_raw(guest_fd.as_raw_fd(), &make_icmp_frame());

        // This is the exact ingest composition the datapath loop performs.
        source.drain(|frame| device.classify_frame(frame, &mut guest_mac));

        assert!(device.rx_queue.is_empty(), "ARP must not queue");
        assert_eq!(
            device.pending_arp_replies.len(),
            1,
            "ARP request should produce a reply"
        );
        assert_eq!(device.intercepted.len(), 1, "ICMP should be intercepted");
        assert!(guest_mac.is_some(), "Guest MAC should be learned");
    }

    #[test]
    fn learn_guest_mac_ignores_broadcast() {
        let mut mac = None;
        learn_guest_mac([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], &mut mac);
        assert!(mac.is_none());
    }

    #[test]
    fn learn_guest_mac_records_unicast() {
        let mut mac = None;
        learn_guest_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE], &mut mac);
        assert_eq!(mac, Some([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE]));
    }

    #[test]
    fn classify_tcp_syn_is_gated() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        // Build a TCP SYN frame (flags = 0x02).
        let mut frame = make_tcp_frame();
        let ip = ETH_HEADER_LEN;
        let l4 = ip + 20;
        frame[l4 + 13] = 0x02; // SYN flag
        // Set src port = 12345, dst port = 443
        frame[l4..l4 + 2].copy_from_slice(&12345u16.to_be_bytes());
        frame[l4 + 2..l4 + 4].copy_from_slice(&443u16.to_be_bytes());
        // Set seq = 1000
        frame[l4 + 4..l4 + 8].copy_from_slice(&1000u32.to_be_bytes());

        device.classify_frame(&frame, &mut guest_mac);

        assert!(device.rx_queue.is_empty(), "SYN should NOT go to rx_queue");
        assert_eq!(device.gated_syns.len(), 1);
        assert_eq!(device.gated_syns[0].dst_port, 443);
        assert_eq!(device.gated_syns[0].src_port, 12345);
        assert_eq!(device.gated_syns[0].syn_seq, 1000);
        assert_eq!(device.gated_syns[0].dst_ip, Ipv4Addr::new(1, 1, 1, 1));
        assert_eq!(device.gated_syns[0].src_ip, Ipv4Addr::new(192, 168, 64, 2));
    }

    #[test]
    fn classify_tcp_non_syn_goes_to_rx_queue() {
        let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
        let mut device = FrameClassifier::new(gateway_ip, DEFAULT_ETHERNET_MTU);
        let mut guest_mac = None;

        // Build a TCP ACK frame (flags = 0x10).
        let mut frame = make_tcp_frame();
        let ip = ETH_HEADER_LEN;
        let l4 = ip + 20;
        frame[l4 + 13] = 0x10; // ACK flag

        device.classify_frame(&frame, &mut guest_mac);

        assert_eq!(
            device.rx_queue.len(),
            1,
            "Non-SYN TCP should go to rx_queue"
        );
        assert!(device.gated_syns.is_empty());
    }
}

#[cfg(test)]
mod l3_tests;

#[cfg(test)]
mod ipv6_tests;
