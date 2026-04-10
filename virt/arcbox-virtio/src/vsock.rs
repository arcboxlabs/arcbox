//! `VirtIO` socket device (virtio-vsock).
//!
//! Provides socket communication between host and guest without requiring
//! network configuration.
//!
//! On Darwin, the native VZ framework handles vsock. This implementation
//! is primarily used for Linux KVM.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, RwLock};

use crate::error::{Result, VirtioError};
use crate::queue::VirtQueue;
use crate::{VirtioDevice, VirtioDeviceId};

/// Vsock device configuration.
#[derive(Debug, Clone)]
pub struct VsockConfig {
    /// Guest CID (Context Identifier).
    pub guest_cid: u64,
}

impl Default for VsockConfig {
    fn default() -> Self {
        Self {
            guest_cid: 3, // First available guest CID
        }
    }
}

/// `VirtIO` vsock device.
///
/// Enables socket communication between host (CID 2) and guest using
/// virtio transport.
pub struct VirtioVsock {
    config: VsockConfig,
    features: u64,
    acked_features: u64,
    /// Backend for host-side socket handling.
    backend: Option<Arc<Mutex<dyn VsockBackend>>>,
    /// Active connections.
    connections: RwLock<HashMap<(u32, u32), VsockConnection>>,
    /// Queue 0: RX (host -> guest).
    rx_queue: Option<VirtQueue>,
    /// Queue 1: TX (guest -> host).
    tx_queue: Option<VirtQueue>,
    /// Queue 2: Event (control events).
    event_queue: Option<VirtQueue>,
    /// Host-side connection fds keyed by (src_port, dst_port).
    /// Used by the guest-memory process_queue path to forward data
    /// between host sockets and guest vsock queues.
    host_connections: HashMap<u32, std::os::unix::io::RawFd>,
    /// Last processed avail index for TX queue (guest-memory path).
    last_avail_idx_tx: usize,
    /// Last processed avail index for RX queue (guest-memory path).
    last_avail_idx_rx: usize,
}

impl VirtioVsock {
    /// Feature: Stream socket.
    pub const FEATURE_STREAM: u64 = 1 << 0;
    /// Feature: Seqpacket socket.
    pub const FEATURE_SEQPACKET: u64 = 1 << 1;
    /// VirtIO version 1 compliance (required for modern MMIO transport).
    pub const FEATURE_VERSION_1: u64 = 1 << virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;

    /// Well-known CID for host.
    pub const HOST_CID: u64 = 2;
    /// Reserved CID.
    pub const RESERVED_CID: u64 = 1;

    /// Creates a new vsock device.
    #[must_use]
    pub fn new(config: VsockConfig) -> Self {
        Self {
            config,
            features: Self::FEATURE_STREAM
                | Self::FEATURE_VERSION_1
                | crate::queue::VIRTIO_F_EVENT_IDX,
            acked_features: 0,
            backend: None,
            connections: RwLock::new(HashMap::new()),
            rx_queue: None,
            tx_queue: None,
            event_queue: None,
            host_connections: HashMap::new(),
            last_avail_idx_tx: 0,
            last_avail_idx_rx: 0,
        }
    }

    /// Creates a vsock device with a backend.
    #[must_use]
    pub fn with_backend<B: VsockBackend + 'static>(config: VsockConfig, backend: B) -> Self {
        Self {
            config,
            features: Self::FEATURE_STREAM
                | Self::FEATURE_VERSION_1
                | crate::queue::VIRTIO_F_EVENT_IDX,
            acked_features: 0,
            backend: Some(Arc::new(Mutex::new(backend))),
            connections: RwLock::new(HashMap::new()),
            rx_queue: None,
            tx_queue: None,
            event_queue: None,
            host_connections: HashMap::new(),
            last_avail_idx_tx: 0,
            last_avail_idx_rx: 0,
        }
    }

    /// Sets the backend.
    pub fn set_backend<B: VsockBackend + 'static>(&mut self, backend: B) {
        self.backend = Some(Arc::new(Mutex::new(backend)));
    }

    /// Returns the guest CID.
    #[must_use]
    pub const fn guest_cid(&self) -> u64 {
        self.config.guest_cid
    }

    /// Handles a connection request from guest.
    pub fn handle_connect(&self, src_port: u32, dst_port: u32) -> Result<()> {
        let local = VsockAddr::new(self.config.guest_cid, src_port);
        let remote = VsockAddr::new(Self::HOST_CID, dst_port);

        // Create connection
        let mut conn = VsockConnection::new(local, remote);
        conn.state = ConnectionState::Connecting;

        // Notify backend
        if let Some(ref backend) = self.backend {
            backend.lock().unwrap().on_connect(local)?;
            conn.state = ConnectionState::Connected;
        }

        self.connections
            .write()
            .unwrap()
            .insert((src_port, dst_port), conn);
        tracing::debug!(
            "Vsock connect: {}:{} -> {}:{}",
            self.config.guest_cid,
            src_port,
            Self::HOST_CID,
            dst_port
        );

        Ok(())
    }

    /// Handles data from guest.
    pub fn handle_send(&self, src_port: u32, dst_port: u32, data: &[u8]) -> Result<usize> {
        let local = VsockAddr::new(self.config.guest_cid, src_port);

        if let Some(ref backend) = self.backend {
            backend.lock().unwrap().on_send(local, data)
        } else {
            // Store in connection buffer
            let mut conns = self.connections.write().unwrap();
            if let Some(conn) = conns.get_mut(&(src_port, dst_port)) {
                conn.enqueue_tx(data);
                Ok(data.len())
            } else {
                Err(VirtioError::InvalidOperation("Connection not found".into()))
            }
        }
    }

    /// Handles receive request from guest.
    pub fn handle_recv(&self, src_port: u32, dst_port: u32, buf: &mut [u8]) -> Result<usize> {
        let local = VsockAddr::new(self.config.guest_cid, src_port);

        if let Some(ref backend) = self.backend {
            backend.lock().unwrap().on_recv(local, buf)
        } else {
            // Read from connection buffer
            let mut conns = self.connections.write().unwrap();
            if let Some(conn) = conns.get_mut(&(src_port, dst_port)) {
                let data = conn.dequeue_rx(buf.len());
                buf[..data.len()].copy_from_slice(&data);
                Ok(data.len())
            } else {
                Err(VirtioError::InvalidOperation("Connection not found".into()))
            }
        }
    }

    /// Handles connection close from guest.
    pub fn handle_close(&self, src_port: u32, dst_port: u32) -> Result<()> {
        let local = VsockAddr::new(self.config.guest_cid, src_port);

        if let Some(ref backend) = self.backend {
            backend.lock().unwrap().on_close(local)?;
        }

        self.connections
            .write()
            .unwrap()
            .remove(&(src_port, dst_port));
        tracing::debug!("Vsock close: {}:{}", self.config.guest_cid, src_port);

        Ok(())
    }

    /// Returns the number of active connections.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.connections.read().unwrap().len()
    }

    /// Returns a mutable reference to the TX queue.
    pub fn tx_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.tx_queue.as_mut()
    }

    /// Returns a mutable reference to the RX queue.
    pub fn rx_queue_mut(&mut self) -> Option<&mut VirtQueue> {
        self.rx_queue.as_mut()
    }

    // ========================================================================
    // Packet Processing
    // ========================================================================

    /// Process pending TX queue packets from guest.
    ///
    /// Pops available descriptors from the TX virtqueue, parses vsock headers,
    /// and dispatches each packet based on its operation code. Returns a list
    /// of completed descriptor heads and their written lengths, suitable for
    /// `push_used_batch()`.
    ///
    /// # Errors
    ///
    /// Returns an error if the TX queue is not ready or packet processing fails.
    /// Handles a TX packet from the guest, forwarding data to host fds.
    fn handle_tx_packet_with_fds(
        &self,
        hdr: &VsockHeader,
        payload: &[u8],
        connections: Option<&mut dyn VsockHostConnections>,
    ) {
        // Copy packed fields to locals to avoid unaligned reference UB.
        let src_cid = { hdr.src_cid };
        let dst_cid = { hdr.dst_cid };
        let src_port = { hdr.src_port };
        let dst_port = { hdr.dst_port };
        let buf_alloc = { hdr.buf_alloc };
        let fwd_cnt = { hdr.fwd_cnt };

        match hdr.operation() {
            Some(VsockOp::Request) => {
                tracing::debug!(
                    "Vsock TX: OP_REQUEST src={}:{} dst={}:{}",
                    src_cid,
                    src_port,
                    dst_cid,
                    dst_port,
                );
            }
            Some(VsockOp::Response) => {
                // Guest accepted a host-initiated connection.
                // src_port = guest port, dst_port = host ephemeral port.
                tracing::info!(
                    "Vsock TX: OP_RESPONSE — connection established (guest_port={}, host_port={}, buf_alloc={}, fwd_cnt={})",
                    src_port,
                    dst_port,
                    buf_alloc,
                    fwd_cnt,
                );
                if let Some(conns) = connections {
                    conns.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                    conns.mark_connected(src_port, dst_port);
                }
            }
            Some(VsockOp::Rw) => {
                // Guest sends data. src_port = guest port, dst_port = host port.
                if let Some(conns) = connections {
                    conns.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                    if let Some(fd) = conns.fd_for(src_port, dst_port) {
                        if !payload.is_empty() {
                            // SAFETY: fd is a valid connected socket from the manager.
                            let written = unsafe {
                                libc::write(
                                    fd,
                                    payload.as_ptr().cast::<libc::c_void>(),
                                    payload.len(),
                                )
                            };
                            if written > 0 {
                                tracing::debug!(
                                    "Vsock TX: OP_RW guest_port={} host_port={} -> fd {fd}, {} bytes",
                                    src_port,
                                    dst_port,
                                    written,
                                );
                                // Advance fwd_cnt — may trigger CreditUpdate.
                                conns.advance_fwd_cnt(src_port, dst_port, written as u32);
                            } else if written < 0 {
                                tracing::warn!(
                                    "Vsock: write to fd {fd} failed: {}",
                                    std::io::Error::last_os_error()
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "Vsock TX: OP_RW no host fd for guest_port={} host_port={}",
                            src_port,
                            dst_port,
                        );
                    }
                }
            }
            Some(VsockOp::Shutdown | VsockOp::Rst) => {
                tracing::debug!(
                    "Vsock TX: connection closed guest_port={} host_port={}",
                    src_port,
                    dst_port,
                );
                if let Some(conns) = connections {
                    conns.remove_connection(src_port, dst_port);
                }
            }
            Some(VsockOp::CreditUpdate) => {
                tracing::trace!(
                    "Vsock TX: OP_CREDIT_UPDATE guest_port={} host_port={} buf_alloc={} fwd_cnt={}",
                    src_port,
                    dst_port,
                    buf_alloc,
                    fwd_cnt,
                );
                if let Some(conns) = connections {
                    conns.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                }
            }
            Some(VsockOp::CreditRequest) => {
                tracing::trace!(
                    "Vsock TX: OP_CREDIT_REQUEST guest_port={} host_port={}",
                    src_port,
                    dst_port,
                );
                if let Some(conns) = connections {
                    conns.update_peer_credit(src_port, dst_port, buf_alloc, fwd_cnt);
                    conns.enqueue_credit_update(src_port, dst_port);
                }
            }
            _ => {}
        }
    }

    /// Registers a host-side fd for a guest vsock port.
    /// When the guest sends data to this port, it will be written to the fd.
    /// When the fd has data, it will be injected into the guest RX queue.
    pub fn add_host_connection(&mut self, guest_port: u32, fd: std::os::unix::io::RawFd) {
        tracing::info!("Vsock: host connection for guest port {guest_port} -> fd {fd}");
        self.host_connections.insert(guest_port, fd);
    }

    pub fn process_tx_queue(&mut self, memory: &mut [u8]) -> Result<Vec<(u16, u32)>> {
        // Phase 1: Collect raw descriptor data from the TX queue.
        let mut raw_packets: Vec<(u16, Vec<u8>)> = Vec::new();

        {
            let queue = self
                .tx_queue
                .as_mut()
                .ok_or_else(|| VirtioError::NotReady("TX queue not ready".into()))?;

            while let Some((head_idx, chain)) = queue.pop_avail() {
                let mut data = Vec::new();

                for desc in chain {
                    if !desc.is_write_only() {
                        // Read-only buffers contain the guest-produced packet.
                        let start = desc.addr as usize;
                        let end = start + desc.len as usize;
                        if end <= memory.len() {
                            data.extend_from_slice(&memory[start..end]);
                        }
                    }
                }

                raw_packets.push((head_idx, data));
            }
        }

        // Phase 2: Parse and dispatch each packet.
        let mut completions = Vec::new();
        // Collect RX packets to inject after releasing the connections lock.
        let mut rx_inject: Vec<(VsockHeader, Vec<u8>)> = Vec::new();

        for (head_idx, data) in &raw_packets {
            if data.len() < VsockHeader::SIZE {
                tracing::warn!(
                    "Vsock TX: descriptor {} too short ({} bytes), skipping",
                    head_idx,
                    data.len()
                );
                completions.push((*head_idx, 0u32));
                continue;
            }

            let header = match VsockHeader::from_bytes(&data[..VsockHeader::SIZE]) {
                Some(h) => h,
                None => {
                    tracing::warn!(
                        "Vsock TX: failed to parse header for descriptor {}",
                        head_idx
                    );
                    completions.push((*head_idx, 0u32));
                    continue;
                }
            };

            let payload_len = { header.len } as usize;
            let payload = if payload_len > 0 && data.len() > VsockHeader::SIZE {
                let avail = data.len() - VsockHeader::SIZE;
                &data[VsockHeader::SIZE..VsockHeader::SIZE + payload_len.min(avail)]
            } else {
                &[] as &[u8]
            };

            let src_port = { header.src_port };
            let dst_port = { header.dst_port };

            match header.operation() {
                Some(VsockOp::Request) => {
                    tracing::debug!(
                        "Vsock TX: OP_REQUEST from port {} to port {}",
                        src_port,
                        dst_port
                    );
                    match self.handle_connect(src_port, dst_port) {
                        Ok(()) => {
                            // Build a RESPONSE header to inject into the RX queue.
                            let resp = VsockHeader::new(
                                VsockAddr::new(Self::HOST_CID, dst_port),
                                VsockAddr::new(self.config.guest_cid, src_port),
                                VsockOp::Response,
                            );
                            rx_inject.push((resp, Vec::new()));
                        }
                        Err(e) => {
                            tracing::warn!("Vsock TX: connect failed: {}", e);
                            // Send RST back to the guest.
                            let rst = VsockHeader::new(
                                VsockAddr::new(Self::HOST_CID, dst_port),
                                VsockAddr::new(self.config.guest_cid, src_port),
                                VsockOp::Rst,
                            );
                            rx_inject.push((rst, Vec::new()));
                        }
                    }
                }
                Some(VsockOp::Response) => {
                    // Guest acknowledging a host-initiated connection.
                    tracing::debug!(
                        "Vsock TX: OP_RESPONSE from port {} to port {}",
                        src_port,
                        dst_port
                    );
                    let mut conns = self.connections.write().unwrap();
                    if let Some(conn) = conns.get_mut(&(src_port, dst_port)) {
                        conn.state = ConnectionState::Connected;
                    }
                }
                Some(VsockOp::Rw) => {
                    tracing::trace!(
                        "Vsock TX: OP_RW {} bytes from port {} to port {}",
                        payload.len(),
                        src_port,
                        dst_port
                    );
                    if let Err(e) = self.handle_send(src_port, dst_port, payload) {
                        tracing::warn!("Vsock TX: send failed: {}", e);
                    }
                }
                Some(VsockOp::Shutdown) => {
                    tracing::debug!(
                        "Vsock TX: OP_SHUTDOWN from port {} to port {}",
                        src_port,
                        dst_port
                    );
                    if let Err(e) = self.handle_close(src_port, dst_port) {
                        tracing::warn!("Vsock TX: close failed: {}", e);
                    }
                    // Confirm with RST.
                    let rst = VsockHeader::new(
                        VsockAddr::new(Self::HOST_CID, dst_port),
                        VsockAddr::new(self.config.guest_cid, src_port),
                        VsockOp::Rst,
                    );
                    rx_inject.push((rst, Vec::new()));
                }
                Some(VsockOp::Rst) => {
                    tracing::debug!(
                        "Vsock TX: OP_RST from port {} to port {}",
                        src_port,
                        dst_port
                    );
                    let _ = self.handle_close(src_port, dst_port);
                }
                Some(VsockOp::CreditUpdate) => {
                    let buf_alloc = { header.buf_alloc };
                    let fwd_cnt = { header.fwd_cnt };
                    tracing::trace!(
                        "Vsock TX: OP_CREDIT_UPDATE port {} buf_alloc={} fwd_cnt={}",
                        src_port,
                        buf_alloc,
                        fwd_cnt
                    );
                    let mut conns = self.connections.write().unwrap();
                    if let Some(conn) = conns.get_mut(&(src_port, dst_port)) {
                        conn.update_peer_credit(buf_alloc, fwd_cnt);
                    }
                }
                Some(VsockOp::CreditRequest) => {
                    tracing::trace!(
                        "Vsock TX: OP_CREDIT_REQUEST from port {} to port {}",
                        src_port,
                        dst_port
                    );
                    // Respond with our credit state.
                    let conns = self.connections.read().unwrap();
                    if let Some(conn) = conns.get(&(src_port, dst_port)) {
                        let mut update = VsockHeader::new(
                            VsockAddr::new(Self::HOST_CID, dst_port),
                            VsockAddr::new(self.config.guest_cid, src_port),
                            VsockOp::CreditUpdate,
                        );
                        update.buf_alloc = conn.buf_alloc;
                        update.fwd_cnt = conn.fwd_cnt;
                        rx_inject.push((update, Vec::new()));
                    }
                }
                Some(VsockOp::Invalid) | None => {
                    let raw_op = { header.op };
                    tracing::warn!(
                        "Vsock TX: unknown/invalid op {} from port {}",
                        raw_op,
                        src_port
                    );
                }
            }

            completions.push((*head_idx, data.len() as u32));
        }

        // Phase 3: Inject any pending RX response packets.
        for (hdr, payload) in rx_inject {
            if let Err(e) = self.inject_rx_packet(&hdr, &payload, memory) {
                tracing::warn!("Vsock: failed to inject RX packet: {}", e);
            }
        }

        Ok(completions)
    }

    /// Process a specific virtqueue by index.
    ///
    /// Queue indices follow the VirtIO vsock specification:
    /// - 0: RX (host -> guest) -- processed externally via `inject_rx_packet`
    /// - 1: TX (guest -> host) -- dispatched here
    /// - 2: Event queue       -- not yet implemented
    ///
    /// # Errors
    ///
    /// Returns an error if processing fails.
    pub fn process_queue(&mut self, queue_idx: u16, memory: &mut [u8]) -> Result<Vec<(u16, u32)>> {
        match queue_idx {
            1 => self.process_tx_queue(memory),
            _ => Ok(Vec::new()),
        }
    }

    /// Injects a response packet into the guest RX queue.
    ///
    /// Pops an available descriptor from the RX queue, writes the vsock header
    /// and optional payload into guest memory via the descriptor chain, then
    /// marks it as used. The MMIO/interrupt handler is responsible for
    /// signalling the guest after this call.
    ///
    /// # Errors
    ///
    /// Returns an error if the RX queue is not ready or no descriptors are
    /// available.
    pub fn inject_rx_packet(
        &mut self,
        header: &VsockHeader,
        data: &[u8],
        memory: &mut [u8],
    ) -> Result<()> {
        let queue = self
            .rx_queue
            .as_mut()
            .ok_or_else(|| VirtioError::NotReady("RX queue not ready".into()))?;

        let (head_idx, chain) = queue
            .pop_avail()
            .ok_or_else(|| VirtioError::InvalidQueue("No available RX descriptors".into()))?;

        let header_bytes = header.to_bytes();
        let total_len = header_bytes.len() + data.len();
        let mut frame = Vec::with_capacity(total_len);
        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(data);

        let mut written = 0usize;
        for desc in chain {
            if !desc.is_write_only() {
                continue;
            }
            let start = desc.addr as usize;
            let remaining = frame.len().saturating_sub(written);
            let to_write = remaining.min(desc.len as usize);
            if to_write == 0 {
                continue;
            }
            let end = start + to_write;
            if end > memory.len() {
                return Err(VirtioError::MemoryError(
                    "RX descriptor points outside guest memory".into(),
                ));
            }
            memory[start..end].copy_from_slice(&frame[written..written + to_write]);
            written += to_write;
        }

        queue.push_used(head_idx, written as u32);
        Ok(())
    }
}

impl VirtioDevice for VirtioVsock {
    fn device_id(&self) -> VirtioDeviceId {
        VirtioDeviceId::Vsock
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, features: u64) {
        self.acked_features = self.features & features;
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // Configuration space layout:
        // offset 0: guest_cid (u64)
        let config_data = self.config.guest_cid.to_le_bytes();

        let offset = offset as usize;
        let len = data.len().min(config_data.len().saturating_sub(offset));
        if len > 0 {
            data[..len].copy_from_slice(&config_data[offset..offset + len]);
        }
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // Vsock config is read-only
    }

    fn activate(&mut self) -> Result<()> {
        // Create virtqueues: RX (0), TX (1), Event (2).
        self.rx_queue = Some(VirtQueue::new(256)?);
        self.tx_queue = Some(VirtQueue::new(256)?);
        self.event_queue = Some(VirtQueue::new(64)?);

        // If no backend is set, use loopback for testing.
        if self.backend.is_none() {
            tracing::info!("Vsock: using loopback backend (no backend configured)");
            self.backend = Some(Arc::new(Mutex::new(LoopbackBackend::new())));
        }
        tracing::info!(
            "Vsock device activated, guest CID: {}",
            self.config.guest_cid
        );
        Ok(())
    }

    fn reset(&mut self) {
        self.acked_features = 0;
        self.connections.write().unwrap().clear();
        self.backend = None;
        self.rx_queue = None;
        self.tx_queue = None;
        self.event_queue = None;
        self.last_avail_idx_tx = 0;
        self.last_avail_idx_rx = 0;
    }

    fn process_queue(
        &mut self,
        queue_idx: u16,
        memory: &mut [u8],
        queue_config: &crate::QueueConfig,
    ) -> Result<Vec<(u16, u32)>> {
        // Queue 0 = RX (host→guest), Queue 1 = TX (guest→host), Queue 2 = Event.
        // We handle TX here: extract vsock packets, forward data to host fds.
        // We also try to inject pending RX data from host fds.
        if queue_idx != 1 || !queue_config.ready || queue_config.size == 0 {
            return Ok(Vec::new());
        }

        // Translate GPAs to slice offsets by subtracting gpa_base (checked to
        // guard against a malicious guest providing a GPA below the RAM base).
        let gpa_base = queue_config.gpa_base as usize;
        let desc_addr = (queue_config.desc_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid desc GPA {:#x} below ram base {:#x}",
                    queue_config.desc_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("desc GPA below ram base".into())
            })?;
        let avail_addr = (queue_config.avail_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid avail GPA {:#x} below ram base {:#x}",
                    queue_config.avail_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("avail GPA below ram base".into())
            })?;
        let used_addr = (queue_config.used_addr as usize)
            .checked_sub(gpa_base)
            .ok_or_else(|| {
                tracing::warn!(
                    "invalid used GPA {:#x} below ram base {:#x}",
                    queue_config.used_addr,
                    gpa_base
                );
                VirtioError::InvalidQueue("used GPA below ram base".into())
            })?;
        let q_size = queue_config.size as usize;

        if avail_addr + 4 > memory.len() {
            return Ok(Vec::new());
        }
        let avail_idx =
            u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]) as usize;

        let mut current_avail = self.last_avail_idx_tx;
        let mut completions = Vec::new();

        while current_avail != avail_idx {
            let ring_off = avail_addr + 4 + 2 * (current_avail % q_size);
            if ring_off + 2 > memory.len() {
                break;
            }
            let head_idx = u16::from_le_bytes([memory[ring_off], memory[ring_off + 1]]) as usize;

            // Walk descriptor chain to extract vsock packet.
            let mut packet_data = Vec::new();
            let mut idx = head_idx;
            for _ in 0..q_size {
                let d_off = desc_addr + idx * 16;
                if d_off + 16 > memory.len() {
                    break;
                }
                let addr = match (u64::from_le_bytes(memory[d_off..d_off + 8].try_into().unwrap())
                    as usize)
                    .checked_sub(gpa_base)
                {
                    Some(a) => a,
                    None => continue,
                };
                let len =
                    u32::from_le_bytes(memory[d_off + 8..d_off + 12].try_into().unwrap()) as usize;
                let flags = u16::from_le_bytes(memory[d_off + 12..d_off + 14].try_into().unwrap());
                let next = u16::from_le_bytes(memory[d_off + 14..d_off + 16].try_into().unwrap());

                // TX descriptors are read-only (guest→host data).
                if flags & crate::queue::flags::WRITE == 0 && addr + len <= memory.len() {
                    packet_data.extend_from_slice(&memory[addr..addr + len]);
                }

                if flags & crate::queue::flags::NEXT == 0 {
                    break;
                }
                idx = next as usize;
            }

            // Parse vsock header (44 bytes) and forward via host fds.
            if packet_data.len() >= VsockHeader::SIZE {
                if let Some(hdr) = VsockHeader::from_bytes(&packet_data[..VsockHeader::SIZE]) {
                    let op_val = { hdr.op };
                    let src_cid = { hdr.src_cid };
                    let dst_cid = { hdr.dst_cid };
                    let src_port = { hdr.src_port };
                    let dst_port = { hdr.dst_port };
                    tracing::trace!(
                        "Vsock TX: op={} src={}:{} dst={}:{} len={} (packet_data={} bytes)",
                        op_val,
                        src_cid,
                        src_port,
                        dst_cid,
                        dst_port,
                        { hdr.len },
                        packet_data.len(),
                    );

                    let payload = &packet_data[VsockHeader::SIZE..];
                    if let Some(ref conns_arc) = queue_config.vsock_connections {
                        if let Ok(mut conns) = conns_arc.lock() {
                            self.handle_tx_packet_with_fds(&hdr, payload, Some(&mut *conns));
                        }
                    } else {
                        self.handle_tx_packet_with_fds(&hdr, payload, None);
                    }
                }
            } else {
                tracing::warn!(
                    "Vsock TX: packet too short ({} bytes < {} header), skipping",
                    packet_data.len(),
                    VsockHeader::SIZE,
                );
            }

            // Update used ring.
            let used_idx_off = used_addr + 2;
            let used_idx = u16::from_le_bytes([memory[used_idx_off], memory[used_idx_off + 1]]);
            let used_entry = used_addr + 4 + ((used_idx as usize) % q_size) * 8;
            if used_entry + 8 <= memory.len() {
                memory[used_entry..used_entry + 4]
                    .copy_from_slice(&(head_idx as u32).to_le_bytes());
                memory[used_entry + 4..used_entry + 8]
                    .copy_from_slice(&(packet_data.len() as u32).to_le_bytes());
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                let new_used = used_idx.wrapping_add(1);
                memory[used_idx_off..used_idx_off + 2].copy_from_slice(&new_used.to_le_bytes());
            }

            // Update avail_event.
            let avail_event_off = used_addr + 4 + 8 * q_size;
            if avail_event_off + 2 <= memory.len() {
                let ae = ((current_avail + 1) as u16).to_le_bytes();
                memory[avail_event_off] = ae[0];
                memory[avail_event_off + 1] = ae[1];
            }

            completions.push((head_idx as u16, packet_data.len() as u32));
            current_avail += 1;
        }

        self.last_avail_idx_tx = current_avail;
        Ok(completions)
    }
}

// ============================================================================
// Host-Side Connection Management Trait
// ============================================================================

/// Abstracts host-side vsock connection tracking for the HV backend.
///
/// The VZ backend handles connections natively via Virtualization.framework.
/// For the HV backend, a concrete `VsockConnectionManager` (in arcbox-vmm)
/// implements this trait and is shared with VirtioVsock via QueueConfig.
pub trait VsockHostConnections: Send + Sync {
    /// Returns the host fd for a connection identified by (guest_port, host_port).
    fn fd_for(&self, guest_port: u32, host_port: u32) -> Option<std::os::unix::io::RawFd>;

    /// Marks a connection as established (called when OP_RESPONSE is received).
    fn mark_connected(&mut self, guest_port: u32, host_port: u32);

    /// Removes a connection and closes the associated fd (called on OP_RST).
    fn remove_connection(&mut self, guest_port: u32, host_port: u32);

    /// Updates peer credit state from an incoming guest packet.
    /// Called for every TX packet to keep credit info in sync.
    fn update_peer_credit(
        &mut self,
        _guest_port: u32,
        _host_port: u32,
        _buf_alloc: u32,
        _fwd_cnt: u32,
    ) {
    }

    /// Advances fwd_cnt after writing guest data to the host stream.
    /// Returns `true` if the host has pending RX data as a result (CreditUpdate).
    fn advance_fwd_cnt(&mut self, _guest_port: u32, _host_port: u32, _bytes: u32) -> bool {
        false
    }

    /// Enqueues a CreditUpdate to be sent on the next RX fill.
    fn enqueue_credit_update(&mut self, _guest_port: u32, _host_port: u32) {}
}

/// Vsock address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VsockAddr {
    /// Context Identifier.
    pub cid: u64,
    /// Port number.
    pub port: u32,
}

impl VsockAddr {
    /// Creates a new vsock address.
    #[must_use]
    pub const fn new(cid: u64, port: u32) -> Self {
        Self { cid, port }
    }

    /// Returns the host address for a given port.
    #[must_use]
    pub const fn host(port: u32) -> Self {
        Self::new(VirtioVsock::HOST_CID, port)
    }
}

// ============================================================================
// Vsock Packet Types
// ============================================================================

/// Vsock operation types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum VsockOp {
    /// Invalid operation.
    Invalid = 0,
    /// Request connection.
    Request = 1,
    /// Connection response.
    Response = 2,
    /// Reset connection.
    Rst = 3,
    /// Shutdown connection.
    Shutdown = 4,
    /// Data transfer.
    Rw = 5,
    /// Credit update.
    CreditUpdate = 6,
    /// Credit request.
    CreditRequest = 7,
}

impl VsockOp {
    /// Converts from u16.
    #[must_use]
    pub const fn from_u16(val: u16) -> Option<Self> {
        match val {
            0 => Some(Self::Invalid),
            1 => Some(Self::Request),
            2 => Some(Self::Response),
            3 => Some(Self::Rst),
            4 => Some(Self::Shutdown),
            5 => Some(Self::Rw),
            6 => Some(Self::CreditUpdate),
            7 => Some(Self::CreditRequest),
            _ => None,
        }
    }
}

/// Vsock packet header.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct VsockHeader {
    /// Source CID.
    pub src_cid: u64,
    /// Destination CID.
    pub dst_cid: u64,
    /// Source port.
    pub src_port: u32,
    /// Destination port.
    pub dst_port: u32,
    /// Payload length.
    pub len: u32,
    /// Socket type (stream = 1).
    pub socket_type: u16,
    /// Operation.
    pub op: u16,
    /// Flags.
    pub flags: u32,
    /// Buffer allocation.
    pub buf_alloc: u32,
    /// Forward count.
    pub fwd_cnt: u32,
}

impl VsockHeader {
    /// Header size in bytes.
    ///
    /// The VirtIO vsock spec defines the header as exactly 44 bytes (packed).
    /// We cannot use `mem::size_of::<Self>()` because Rust adds trailing padding
    /// to satisfy the struct's 8-byte alignment (from u64 fields), yielding 48.
    /// The guest kernel sends and expects exactly 44 bytes per header.
    pub const SIZE: usize = 44;

    /// Creates a new header.
    #[must_use]
    pub const fn new(src: VsockAddr, dst: VsockAddr, op: VsockOp) -> Self {
        Self {
            src_cid: src.cid,
            dst_cid: dst.cid,
            src_port: src.port,
            dst_port: dst.port,
            len: 0,
            socket_type: 1, // SOCK_STREAM
            op: op as u16,
            flags: 0,
            buf_alloc: 64 * 1024,
            fwd_cnt: 0,
        }
    }

    /// Returns the operation type.
    #[must_use]
    pub const fn operation(&self) -> Option<VsockOp> {
        VsockOp::from_u16(self.op)
    }

    /// Parses a vsock header from a byte slice.
    ///
    /// Returns `None` if the slice is too short.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            src_cid: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            dst_cid: u64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]),
            src_port: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
            dst_port: u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            len: u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            socket_type: u16::from_le_bytes([bytes[28], bytes[29]]),
            op: u16::from_le_bytes([bytes[30], bytes[31]]),
            flags: u32::from_le_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]),
            buf_alloc: u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]),
            fwd_cnt: u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
        })
    }

    /// Serializes the header to a byte array.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        // Access packed fields by copying to locals.
        let src_cid = self.src_cid;
        let dst_cid = self.dst_cid;
        let src_port = self.src_port;
        let dst_port = self.dst_port;
        let len = self.len;
        let socket_type = self.socket_type;
        let op = self.op;
        let flags = self.flags;
        let buf_alloc = self.buf_alloc;
        let fwd_cnt = self.fwd_cnt;

        buf[0..8].copy_from_slice(&src_cid.to_le_bytes());
        buf[8..16].copy_from_slice(&dst_cid.to_le_bytes());
        buf[16..20].copy_from_slice(&src_port.to_le_bytes());
        buf[20..24].copy_from_slice(&dst_port.to_le_bytes());
        buf[24..28].copy_from_slice(&len.to_le_bytes());
        buf[28..30].copy_from_slice(&socket_type.to_le_bytes());
        buf[30..32].copy_from_slice(&op.to_le_bytes());
        buf[32..36].copy_from_slice(&flags.to_le_bytes());
        buf[36..40].copy_from_slice(&buf_alloc.to_le_bytes());
        buf[40..44].copy_from_slice(&fwd_cnt.to_le_bytes());
        buf
    }
}

// ============================================================================
// Connection State
// ============================================================================

/// Connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial state.
    Idle,
    /// Connection requested.
    Connecting,
    /// Connected.
    Connected,
    /// Shutting down.
    Closing,
    /// Closed.
    Closed,
}

/// A vsock connection.
#[derive(Debug)]
#[allow(dead_code)]
pub struct VsockConnection {
    /// Local address.
    pub local: VsockAddr,
    /// Remote address.
    pub remote: VsockAddr,
    /// Connection state.
    pub state: ConnectionState,
    /// Receive buffer.
    rx_buf: Vec<u8>,
    /// Transmit buffer.
    tx_buf: Vec<u8>,
    /// Buffer allocation (credit).
    buf_alloc: u32,
    /// Forward count.
    fwd_cnt: u32,
    /// Peer buffer allocation.
    peer_buf_alloc: u32,
    /// Peer forward count.
    peer_fwd_cnt: u32,
}

impl VsockConnection {
    /// Creates a new connection.
    #[must_use]
    pub fn new(local: VsockAddr, remote: VsockAddr) -> Self {
        Self {
            local,
            remote,
            state: ConnectionState::Idle,
            rx_buf: Vec::with_capacity(64 * 1024),
            tx_buf: Vec::with_capacity(64 * 1024),
            buf_alloc: 64 * 1024,
            fwd_cnt: 0,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
        }
    }

    /// Returns bytes available to send.
    #[must_use]
    pub fn tx_available(&self) -> usize {
        self.tx_buf.len()
    }

    /// Returns bytes available to receive.
    #[must_use]
    pub fn rx_available(&self) -> usize {
        self.rx_buf.len()
    }

    /// Enqueues data for transmission.
    pub fn enqueue_tx(&mut self, data: &[u8]) {
        self.tx_buf.extend_from_slice(data);
    }

    /// Dequeues transmitted data.
    pub fn dequeue_tx(&mut self, max_len: usize) -> Vec<u8> {
        let len = max_len.min(self.tx_buf.len());
        self.tx_buf.drain(..len).collect()
    }

    /// Enqueues received data.
    pub fn enqueue_rx(&mut self, data: &[u8]) {
        self.rx_buf.extend_from_slice(data);
        self.fwd_cnt = self.fwd_cnt.wrapping_add(data.len() as u32);
    }

    /// Dequeues received data.
    pub fn dequeue_rx(&mut self, max_len: usize) -> Vec<u8> {
        let len = max_len.min(self.rx_buf.len());
        self.rx_buf.drain(..len).collect()
    }

    /// Updates peer credit info.
    pub const fn update_peer_credit(&mut self, buf_alloc: u32, fwd_cnt: u32) {
        self.peer_buf_alloc = buf_alloc;
        self.peer_fwd_cnt = fwd_cnt;
    }
}

// ============================================================================
// Vsock Backend
// ============================================================================

/// Vsock backend trait for handling host-side socket operations.
pub trait VsockBackend: Send + Sync {
    /// Called when guest requests a connection.
    fn on_connect(&mut self, addr: VsockAddr) -> Result<()>;

    /// Called when guest sends data.
    fn on_send(&mut self, addr: VsockAddr, data: &[u8]) -> Result<usize>;

    /// Called when guest requests data.
    fn on_recv(&mut self, addr: VsockAddr, buf: &mut [u8]) -> Result<usize>;

    /// Called when guest closes connection.
    fn on_close(&mut self, addr: VsockAddr) -> Result<()>;

    /// Checks if there's pending data for a connection.
    fn has_pending_data(&self, addr: VsockAddr) -> bool;
}

/// Loopback vsock backend for testing.
#[derive(Debug, Default)]
pub struct LoopbackBackend {
    /// Pending data per connection.
    pending: HashMap<VsockAddr, Vec<u8>>,
}

impl LoopbackBackend {
    /// Creates a new loopback backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl VsockBackend for LoopbackBackend {
    fn on_connect(&mut self, addr: VsockAddr) -> Result<()> {
        self.pending.insert(addr, Vec::new());
        tracing::debug!("Loopback: connection from {:?}", addr);
        Ok(())
    }

    fn on_send(&mut self, addr: VsockAddr, data: &[u8]) -> Result<usize> {
        // Echo back the data
        if let Some(buf) = self.pending.get_mut(&addr) {
            buf.extend_from_slice(data);
        }
        Ok(data.len())
    }

    fn on_recv(&mut self, addr: VsockAddr, buf: &mut [u8]) -> Result<usize> {
        if let Some(pending) = self.pending.get_mut(&addr) {
            let len = buf.len().min(pending.len());
            buf[..len].copy_from_slice(&pending[..len]);
            pending.drain(..len);
            Ok(len)
        } else {
            Ok(0)
        }
    }

    fn on_close(&mut self, addr: VsockAddr) -> Result<()> {
        self.pending.remove(&addr);
        tracing::debug!("Loopback: connection closed {:?}", addr);
        Ok(())
    }

    fn has_pending_data(&self, addr: VsockAddr) -> bool {
        self.pending.get(&addr).map_or(false, |b| !b.is_empty())
    }
}

/// TCP-based vsock backend.
///
/// Maps vsock ports to TCP ports for host-side handling.
pub struct TcpBackend {
    /// Guest CID.
    guest_cid: u64,
    /// Base TCP port (vsock port N maps to TCP port base + N).
    base_port: u16,
    /// Active connections.
    connections: RwLock<HashMap<VsockAddr, TcpStream>>,
    /// Listeners for incoming connections.
    listeners: RwLock<HashMap<u32, TcpListener>>,
}

impl TcpBackend {
    /// Creates a new TCP backend.
    #[must_use]
    pub fn new(guest_cid: u64, base_port: u16) -> Self {
        Self {
            guest_cid,
            base_port,
            connections: RwLock::new(HashMap::new()),
            listeners: RwLock::new(HashMap::new()),
        }
    }

    /// Listens on a vsock port.
    pub fn listen(&self, port: u32) -> Result<()> {
        let tcp_port = self.base_port + port as u16;
        let listener = TcpListener::bind(format!("127.0.0.1:{tcp_port}"))
            .map_err(|e| VirtioError::Io(format!("Failed to bind: {e}")))?;

        listener
            .set_nonblocking(true)
            .map_err(|e| VirtioError::Io(format!("Failed to set nonblocking: {e}")))?;

        self.listeners.write().unwrap().insert(port, listener);
        tracing::info!("Vsock listening on port {} (TCP {})", port, tcp_port);
        Ok(())
    }

    /// Accepts a pending connection.
    pub fn accept(&self, port: u32) -> Result<Option<VsockAddr>> {
        let listeners = self.listeners.read().unwrap();
        if let Some(listener) = listeners.get(&port) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    stream
                        .set_nonblocking(true)
                        .map_err(|e| VirtioError::Io(format!("Failed to set nonblocking: {e}")))?;

                    let local = VsockAddr::new(VirtioVsock::HOST_CID, port);
                    let remote = VsockAddr::new(self.guest_cid, port);

                    self.connections.write().unwrap().insert(remote, stream);
                    Ok(Some(local))
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(VirtioError::Io(format!("Accept failed: {e}"))),
            }
        } else {
            Ok(None)
        }
    }
}

impl VsockBackend for TcpBackend {
    fn on_connect(&mut self, addr: VsockAddr) -> Result<()> {
        let tcp_port = self.base_port + addr.port as u16;
        let stream = TcpStream::connect(format!("127.0.0.1:{tcp_port}"))
            .map_err(|e| VirtioError::Io(format!("Connect failed: {e}")))?;

        stream
            .set_nonblocking(true)
            .map_err(|e| VirtioError::Io(format!("Failed to set nonblocking: {e}")))?;

        self.connections.write().unwrap().insert(addr, stream);
        Ok(())
    }

    fn on_send(&mut self, addr: VsockAddr, data: &[u8]) -> Result<usize> {
        let mut connections = self.connections.write().unwrap();
        if let Some(stream) = connections.get_mut(&addr) {
            stream
                .write(data)
                .map_err(|e| VirtioError::Io(format!("Send failed: {e}")))
        } else {
            Err(VirtioError::InvalidOperation("Connection not found".into()))
        }
    }

    fn on_recv(&mut self, addr: VsockAddr, buf: &mut [u8]) -> Result<usize> {
        let mut connections = self.connections.write().unwrap();
        if let Some(stream) = connections.get_mut(&addr) {
            match stream.read(buf) {
                Ok(n) => Ok(n),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(VirtioError::Io(format!("Recv failed: {e}"))),
            }
        } else {
            Err(VirtioError::InvalidOperation("Connection not found".into()))
        }
    }

    fn on_close(&mut self, addr: VsockAddr) -> Result<()> {
        self.connections.write().unwrap().remove(&addr);
        Ok(())
    }

    fn has_pending_data(&self, addr: VsockAddr) -> bool {
        // TCP streams don't have a simple way to check pending data
        // Would need peek() or poll()
        self.connections.read().unwrap().contains_key(&addr)
    }
}

impl std::fmt::Debug for TcpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpBackend")
            .field("guest_cid", &self.guest_cid)
            .field("base_port", &self.base_port)
            .finish()
    }
}

// ============================================================================
// Host Vsock Backend (Hypervisor.framework)
// ============================================================================

/// Active forwarding channel between a guest vsock port and a host-side Unix
/// socket.
#[cfg(unix)]
#[derive(Debug)]
struct VsockChannel {
    /// The connected Unix socket on the host side.
    stream: UnixStream,
}

/// Host-side vsock backend for the HV (Hypervisor.framework) backend.
///
/// Accepts connections from the guest and forwards them to host-side Unix
/// domain sockets. Each guest port is mapped to a Unix socket path on the host;
/// when the guest opens a connection the backend connects to the corresponding
/// socket and relays data bidirectionally.
///
/// This is the primary backend used for arcbox-agent RPC communication on
/// macOS.
#[cfg(unix)]
pub struct HostVsockBackend {
    /// Port to Unix socket path mappings.
    port_map: HashMap<u32, String>,
    /// Active forwarding channels keyed by (src_port, dst_port).
    channels: HashMap<(u32, u32), VsockChannel>,
}

#[cfg(unix)]
impl std::fmt::Debug for HostVsockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostVsockBackend")
            .field("port_map", &self.port_map)
            .field("channels_count", &self.channels.len())
            .finish()
    }
}

#[cfg(unix)]
impl HostVsockBackend {
    /// Creates a new host vsock backend with no port mappings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            port_map: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    /// Registers a mapping from guest vsock port to a host-side Unix socket
    /// path.
    ///
    /// When the guest opens a connection to `port`, the backend will connect
    /// to `socket_path` on the host and relay traffic.
    pub fn add_port_mapping(&mut self, port: u32, socket_path: String) {
        self.port_map.insert(port, socket_path);
    }

    /// Creates a backend with pre-configured port mappings.
    #[must_use]
    pub fn with_port_map(port_map: HashMap<u32, String>) -> Self {
        Self {
            port_map,
            channels: HashMap::new(),
        }
    }
}

#[cfg(unix)]
impl Default for HostVsockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl VsockBackend for HostVsockBackend {
    fn on_connect(&mut self, addr: VsockAddr) -> Result<()> {
        let socket_path = self.port_map.get(&addr.port).ok_or_else(|| {
            VirtioError::InvalidOperation(format!("No port mapping for vsock port {}", addr.port))
        })?;

        let stream = UnixStream::connect(socket_path).map_err(|e| {
            VirtioError::Io(format!(
                "Failed to connect to Unix socket {}: {}",
                socket_path, e
            ))
        })?;

        stream
            .set_nonblocking(true)
            .map_err(|e| VirtioError::Io(format!("Failed to set nonblocking: {}", e)))?;

        tracing::debug!(
            "HostVsockBackend: connected port {} to {}",
            addr.port,
            socket_path
        );

        // Use (port, port) as channel key -- the guest side always uses
        // the same port for the connection.
        self.channels
            .insert((addr.port, addr.port), VsockChannel { stream });
        Ok(())
    }

    fn on_send(&mut self, addr: VsockAddr, data: &[u8]) -> Result<usize> {
        let channel = self
            .channels
            .get_mut(&(addr.port, addr.port))
            .ok_or_else(|| {
                VirtioError::InvalidOperation(format!("No channel for vsock port {}", addr.port))
            })?;

        channel
            .stream
            .write(data)
            .map_err(|e| VirtioError::Io(format!("Failed to write to Unix socket: {}", e)))
    }

    fn on_recv(&mut self, addr: VsockAddr, buf: &mut [u8]) -> Result<usize> {
        let channel = self
            .channels
            .get_mut(&(addr.port, addr.port))
            .ok_or_else(|| {
                VirtioError::InvalidOperation(format!("No channel for vsock port {}", addr.port))
            })?;

        match channel.stream.read(buf) {
            Ok(n) => Ok(n),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(VirtioError::Io(format!(
                "Failed to read from Unix socket: {}",
                e
            ))),
        }
    }

    fn on_close(&mut self, addr: VsockAddr) -> Result<()> {
        self.channels.remove(&(addr.port, addr.port));
        tracing::debug!("HostVsockBackend: closed port {}", addr.port);
        Ok(())
    }

    fn has_pending_data(&self, addr: VsockAddr) -> bool {
        // Non-blocking peek to check if data is available on the Unix socket.
        self.channels.contains_key(&(addr.port, addr.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // VsockConfig Tests
    // ==========================================================================

    #[test]
    fn test_vsock_config_default() {
        let config = VsockConfig::default();
        assert_eq!(config.guest_cid, 3);
    }

    #[test]
    fn test_vsock_config_custom() {
        let config = VsockConfig { guest_cid: 100 };
        assert_eq!(config.guest_cid, 100);
    }

    #[test]
    fn test_vsock_config_clone() {
        let config = VsockConfig { guest_cid: 42 };
        let cloned = config.clone();
        assert_eq!(cloned.guest_cid, 42);
    }

    // ==========================================================================
    // VirtioVsock Tests
    // ==========================================================================

    #[test]
    fn test_vsock_new() {
        let vsock = VirtioVsock::new(VsockConfig::default());
        assert_eq!(vsock.guest_cid(), 3);
    }

    #[test]
    fn test_vsock_device_id() {
        let vsock = VirtioVsock::new(VsockConfig::default());
        assert_eq!(vsock.device_id(), VirtioDeviceId::Vsock);
    }

    #[test]
    fn test_vsock_features() {
        let vsock = VirtioVsock::new(VsockConfig::default());
        let features = vsock.features();
        assert!(features & VirtioVsock::FEATURE_STREAM != 0);
    }

    #[test]
    fn test_vsock_ack_features() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());

        vsock.ack_features(VirtioVsock::FEATURE_STREAM);
        assert_eq!(vsock.acked_features, VirtioVsock::FEATURE_STREAM);
    }

    #[test]
    fn test_vsock_ack_unsupported_feature() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());

        // SEQPACKET is not supported by default
        vsock.ack_features(VirtioVsock::FEATURE_SEQPACKET);
        assert_eq!(vsock.acked_features, 0);
    }

    #[test]
    fn test_vsock_read_config() {
        let config = VsockConfig {
            guest_cid: 0x12345678,
        };
        let vsock = VirtioVsock::new(config);

        let mut data = [0u8; 8];
        vsock.read_config(0, &mut data);

        let cid = u64::from_le_bytes(data);
        assert_eq!(cid, 0x12345678);
    }

    #[test]
    fn test_vsock_read_config_partial() {
        let config = VsockConfig {
            guest_cid: 0xDEADBEEF,
        };
        let vsock = VirtioVsock::new(config);

        // Read only first 4 bytes
        let mut data = [0u8; 4];
        vsock.read_config(0, &mut data);

        let low_bytes = u32::from_le_bytes(data);
        assert_eq!(low_bytes, 0xDEADBEEF);
    }

    #[test]
    fn test_vsock_read_config_offset() {
        let config = VsockConfig {
            guest_cid: 0xAABBCCDD_11223344,
        };
        let vsock = VirtioVsock::new(config);

        // Read from offset 4
        let mut data = [0u8; 4];
        vsock.read_config(4, &mut data);

        let high_bytes = u32::from_le_bytes(data);
        assert_eq!(high_bytes, 0xAABBCCDD);
    }

    #[test]
    fn test_vsock_read_config_beyond() {
        let vsock = VirtioVsock::new(VsockConfig::default());

        let mut data = [0xFFu8; 4];
        vsock.read_config(100, &mut data);

        // Should not crash, data might be unchanged
    }

    #[test]
    fn test_vsock_write_config_noop() {
        let mut vsock = VirtioVsock::new(VsockConfig { guest_cid: 42 });

        // Write should be no-op
        vsock.write_config(0, &[0xFF; 8]);

        // CID should be unchanged
        assert_eq!(vsock.guest_cid(), 42);
    }

    #[test]
    fn test_vsock_activate() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        assert!(vsock.activate().is_ok());
    }

    #[test]
    fn test_vsock_reset() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.ack_features(VirtioVsock::FEATURE_STREAM);
        assert_ne!(vsock.acked_features, 0);

        vsock.reset();
        assert_eq!(vsock.acked_features, 0);
    }

    // ==========================================================================
    // VsockAddr Tests
    // ==========================================================================

    #[test]
    fn test_vsock_addr_new() {
        let addr = VsockAddr::new(3, 1234);
        assert_eq!(addr.cid, 3);
        assert_eq!(addr.port, 1234);
    }

    #[test]
    fn test_vsock_addr_host() {
        let addr = VsockAddr::host(8080);
        assert_eq!(addr.cid, VirtioVsock::HOST_CID);
        assert_eq!(addr.cid, 2);
        assert_eq!(addr.port, 8080);
    }

    #[test]
    fn test_vsock_addr_clone_copy() {
        let addr = VsockAddr::new(10, 5000);
        let cloned = addr.clone();
        let copied = addr; // Copy

        assert_eq!(cloned.cid, 10);
        assert_eq!(copied.port, 5000);
    }

    #[test]
    fn test_vsock_addr_eq() {
        let addr1 = VsockAddr::new(3, 1234);
        let addr2 = VsockAddr::new(3, 1234);
        let addr3 = VsockAddr::new(3, 5678);

        assert_eq!(addr1, addr2);
        assert_ne!(addr1, addr3);
    }

    #[test]
    fn test_vsock_addr_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(VsockAddr::new(3, 1234));
        set.insert(VsockAddr::new(3, 1234)); // Duplicate
        set.insert(VsockAddr::new(4, 1234));

        assert_eq!(set.len(), 2);
    }

    // ==========================================================================
    // Constants Tests
    // ==========================================================================

    #[test]
    fn test_vsock_constants() {
        assert_eq!(VirtioVsock::HOST_CID, 2);
        assert_eq!(VirtioVsock::RESERVED_CID, 1);
        assert_eq!(VirtioVsock::FEATURE_STREAM, 1 << 0);
        assert_eq!(VirtioVsock::FEATURE_SEQPACKET, 1 << 1);
    }

    // ==========================================================================
    // Backend Tests
    // ==========================================================================

    #[test]
    fn test_loopback_backend() {
        let mut backend = LoopbackBackend::new();
        let addr = VsockAddr::new(3, 1234);

        // Connect
        backend.on_connect(addr).unwrap();

        // Send data (echo)
        let data = b"hello world";
        let sent = backend.on_send(addr, data).unwrap();
        assert_eq!(sent, data.len());

        // Check pending
        assert!(backend.has_pending_data(addr));

        // Receive
        let mut buf = [0u8; 64];
        let received = backend.on_recv(addr, &mut buf).unwrap();
        assert_eq!(received, data.len());
        assert_eq!(&buf[..received], data);

        // Close
        backend.on_close(addr).unwrap();
        assert!(!backend.has_pending_data(addr));
    }

    #[test]
    fn test_vsock_with_loopback_backend() {
        let vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        assert_eq!(vsock.guest_cid(), 3);
        assert_eq!(vsock.connection_count(), 0);
    }

    #[test]
    fn test_vsock_connect_send_recv() {
        let vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());

        // Connect
        vsock.handle_connect(1000, 80).unwrap();
        assert_eq!(vsock.connection_count(), 1);

        // Send
        let data = b"GET / HTTP/1.1";
        let sent = vsock.handle_send(1000, 80, data).unwrap();
        assert_eq!(sent, data.len());

        // Receive (loopback echoes back)
        let mut buf = [0u8; 64];
        let received = vsock.handle_recv(1000, 80, &mut buf).unwrap();
        assert_eq!(received, data.len());
        assert_eq!(&buf[..received], data);

        // Close
        vsock.handle_close(1000, 80).unwrap();
        assert_eq!(vsock.connection_count(), 0);
    }

    #[test]
    fn test_vsock_connection_state() {
        let conn = VsockConnection::new(VsockAddr::new(3, 1000), VsockAddr::new(2, 80));
        assert_eq!(conn.state, ConnectionState::Idle);
        assert_eq!(conn.tx_available(), 0);
        assert_eq!(conn.rx_available(), 0);
    }

    #[test]
    fn test_vsock_connection_buffers() {
        let mut conn = VsockConnection::new(VsockAddr::new(3, 1000), VsockAddr::new(2, 80));

        // TX buffer
        conn.enqueue_tx(b"hello");
        assert_eq!(conn.tx_available(), 5);
        let data = conn.dequeue_tx(3);
        assert_eq!(&data, b"hel");
        assert_eq!(conn.tx_available(), 2);

        // RX buffer
        conn.enqueue_rx(b"world");
        assert_eq!(conn.rx_available(), 5);
        let data = conn.dequeue_rx(10);
        assert_eq!(&data, b"world");
        assert_eq!(conn.rx_available(), 0);
    }

    #[test]
    fn test_vsock_header() {
        let src = VsockAddr::new(3, 1000);
        let dst = VsockAddr::new(2, 80);
        let header = VsockHeader::new(src, dst, VsockOp::Request);

        // Copy fields to avoid unaligned reference issues with packed struct
        let src_cid = header.src_cid;
        let dst_cid = header.dst_cid;
        let src_port = header.src_port;
        let dst_port = header.dst_port;

        assert_eq!(src_cid, 3);
        assert_eq!(dst_cid, 2);
        assert_eq!(src_port, 1000);
        assert_eq!(dst_port, 80);
        assert_eq!(header.operation(), Some(VsockOp::Request));
    }

    #[test]
    fn test_vsock_op_from_u16() {
        assert_eq!(VsockOp::from_u16(0), Some(VsockOp::Invalid));
        assert_eq!(VsockOp::from_u16(1), Some(VsockOp::Request));
        assert_eq!(VsockOp::from_u16(5), Some(VsockOp::Rw));
        assert_eq!(VsockOp::from_u16(100), None);
    }

    #[test]
    fn test_tcp_backend_creation() {
        let backend = TcpBackend::new(3, 10000);
        assert!(!backend.has_pending_data(VsockAddr::new(3, 1234)));
    }

    // ==========================================================================
    // VsockHeader Serialization Tests
    // ==========================================================================

    #[test]
    fn test_vsock_header_size() {
        assert_eq!(VsockHeader::SIZE, 44);
    }

    #[test]
    fn test_vsock_header_roundtrip() {
        let src = VsockAddr::new(3, 1000);
        let dst = VsockAddr::new(2, 80);
        let original = VsockHeader::new(src, dst, VsockOp::Request);

        let bytes = original.to_bytes();
        assert_eq!(bytes.len(), VsockHeader::SIZE);

        let parsed = VsockHeader::from_bytes(&bytes).unwrap();
        // Copy fields to locals to avoid unaligned references with packed struct.
        let p_src_cid = parsed.src_cid;
        let p_dst_cid = parsed.dst_cid;
        let p_src_port = parsed.src_port;
        let p_dst_port = parsed.dst_port;
        let p_socket_type = parsed.socket_type;
        assert_eq!(p_src_cid, 3);
        assert_eq!(p_dst_cid, 2);
        assert_eq!(p_src_port, 1000);
        assert_eq!(p_dst_port, 80);
        assert_eq!(parsed.operation(), Some(VsockOp::Request));
        assert_eq!(p_socket_type, 1); // SOCK_STREAM
    }

    #[test]
    fn test_vsock_header_from_bytes_too_short() {
        let short = [0u8; 20];
        assert!(VsockHeader::from_bytes(&short).is_none());
    }

    #[test]
    fn test_vsock_header_to_bytes_all_fields() {
        let mut header = VsockHeader::new(
            VsockAddr::new(0xAABB, 0x1234),
            VsockAddr::new(0xCCDD, 0x5678),
            VsockOp::Rw,
        );
        header.len = 256;
        header.flags = 0x42;
        header.buf_alloc = 32768;
        header.fwd_cnt = 100;

        let bytes = header.to_bytes();
        let parsed = VsockHeader::from_bytes(&bytes).unwrap();

        // Copy fields to locals to avoid unaligned references with packed struct.
        let p_src_cid = parsed.src_cid;
        let p_dst_cid = parsed.dst_cid;
        let p_src_port = parsed.src_port;
        let p_dst_port = parsed.dst_port;
        let p_len = parsed.len;
        let p_op = parsed.op;
        let p_flags = parsed.flags;
        let p_buf_alloc = parsed.buf_alloc;
        let p_fwd_cnt = parsed.fwd_cnt;
        assert_eq!(p_src_cid, 0xAABB);
        assert_eq!(p_dst_cid, 0xCCDD);
        assert_eq!(p_src_port, 0x1234);
        assert_eq!(p_dst_port, 0x5678);
        assert_eq!(p_len, 256);
        assert_eq!(p_op, VsockOp::Rw as u16);
        assert_eq!(p_flags, 0x42);
        assert_eq!(p_buf_alloc, 32768);
        assert_eq!(p_fwd_cnt, 100);
    }

    // ==========================================================================
    // Queue Activation Tests
    // ==========================================================================

    #[test]
    fn test_vsock_activate_creates_queues() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        assert!(vsock.rx_queue.is_none());
        assert!(vsock.tx_queue.is_none());
        assert!(vsock.event_queue.is_none());

        vsock.activate().unwrap();

        assert!(vsock.rx_queue.is_some());
        assert!(vsock.tx_queue.is_some());
        assert!(vsock.event_queue.is_some());
    }

    #[test]
    fn test_vsock_reset_clears_queues() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();
        assert!(vsock.rx_queue.is_some());

        vsock.reset();
        assert!(vsock.rx_queue.is_none());
        assert!(vsock.tx_queue.is_none());
        assert!(vsock.event_queue.is_none());
    }

    // ==========================================================================
    // process_tx_queue Tests
    // ==========================================================================

    /// Helper: Build a simulated guest memory region with a vsock packet
    /// placed at a given address, and configure the TX queue with matching
    /// descriptors.
    fn setup_tx_packet(
        vsock: &mut VirtioVsock,
        guest_addr: usize,
        header: &VsockHeader,
        payload: &[u8],
        memory: &mut Vec<u8>,
    ) {
        let header_bytes = header.to_bytes();
        let total = header_bytes.len() + payload.len();

        // Ensure memory is large enough.
        if memory.len() < guest_addr + total {
            memory.resize(guest_addr + total, 0);
        }

        memory[guest_addr..guest_addr + header_bytes.len()].copy_from_slice(&header_bytes);
        if !payload.is_empty() {
            memory[guest_addr + header_bytes.len()..guest_addr + total].copy_from_slice(payload);
        }

        // Set up descriptor in the TX queue.
        let queue = vsock.tx_queue.as_mut().unwrap();
        let desc = crate::queue::Descriptor {
            addr: guest_addr as u64,
            len: total as u32,
            flags: 0, // Read-only for device
            next: 0,
        };
        queue.set_descriptor(0, desc).unwrap();
        queue.add_avail(0).unwrap();
    }

    #[test]
    fn test_process_tx_queue_not_ready() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        // Do not activate -- queues are None.
        let mut memory = vec![0u8; 1024];
        let result = vsock.process_tx_queue(&mut memory);
        assert!(result.is_err());
    }

    #[test]
    fn test_process_tx_queue_empty() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let mut memory = vec![0u8; 4096];
        let completions = vsock.process_tx_queue(&mut memory).unwrap();
        assert!(completions.is_empty());
    }

    #[test]
    fn test_process_tx_queue_connect_request() {
        let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        vsock.activate().unwrap();

        let mut memory = vec![0u8; 4096];

        // Guest sends OP_REQUEST from port 1000 to host port 80.
        let header = VsockHeader::new(
            VsockAddr::new(3, 1000),
            VsockAddr::new(VirtioVsock::HOST_CID, 80),
            VsockOp::Request,
        );
        setup_tx_packet(&mut vsock, 0x100, &header, &[], &mut memory);

        // Also prepare RX queue with a write-only descriptor for the response.
        {
            let rx_queue = vsock.rx_queue.as_mut().unwrap();
            let rx_desc = crate::queue::Descriptor {
                addr: 0x800,
                len: 256,
                flags: crate::queue::flags::WRITE,
                next: 0,
            };
            rx_queue.set_descriptor(0, rx_desc).unwrap();
            rx_queue.add_avail(0).unwrap();
        }

        let completions = vsock.process_tx_queue(&mut memory).unwrap();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].0, 0); // descriptor head index

        // Verify connection was created.
        assert_eq!(vsock.connection_count(), 1);

        // Verify a RESPONSE was injected into the RX queue.
        let resp_header = VsockHeader::from_bytes(&memory[0x800..0x800 + VsockHeader::SIZE]);
        assert!(resp_header.is_some());
        let resp = resp_header.unwrap();
        assert_eq!(resp.operation(), Some(VsockOp::Response));
        let resp_src_cid = resp.src_cid;
        let resp_dst_cid = resp.dst_cid;
        assert_eq!(resp_src_cid, VirtioVsock::HOST_CID);
        assert_eq!(resp_dst_cid, 3);
    }

    #[test]
    fn test_process_tx_queue_data_rw() {
        let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        vsock.activate().unwrap();

        // First establish a connection.
        vsock.handle_connect(1000, 80).unwrap();

        let mut memory = vec![0u8; 4096];

        // Guest sends data via OP_RW.
        let payload = b"hello world";
        let mut header = VsockHeader::new(
            VsockAddr::new(3, 1000),
            VsockAddr::new(VirtioVsock::HOST_CID, 80),
            VsockOp::Rw,
        );
        header.len = payload.len() as u32;
        setup_tx_packet(&mut vsock, 0x100, &header, payload, &mut memory);

        let completions = vsock.process_tx_queue(&mut memory).unwrap();
        assert_eq!(completions.len(), 1);

        // Verify data was forwarded to the backend (loopback echoes it).
        let backend = vsock.backend.as_ref().unwrap();
        let mut backend = backend.lock().unwrap();
        let addr = VsockAddr::new(3, 1000);
        assert!(backend.has_pending_data(addr));

        let mut buf = [0u8; 64];
        let n = backend.on_recv(addr, &mut buf).unwrap();
        assert_eq!(&buf[..n], payload);
    }

    #[test]
    fn test_process_tx_queue_shutdown() {
        let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        vsock.activate().unwrap();

        // Establish connection first.
        vsock.handle_connect(2000, 443).unwrap();
        assert_eq!(vsock.connection_count(), 1);

        let mut memory = vec![0u8; 4096];

        // Guest sends OP_SHUTDOWN.
        let header = VsockHeader::new(
            VsockAddr::new(3, 2000),
            VsockAddr::new(VirtioVsock::HOST_CID, 443),
            VsockOp::Shutdown,
        );
        setup_tx_packet(&mut vsock, 0x100, &header, &[], &mut memory);

        // Provide an RX descriptor for the RST response.
        {
            let rx_queue = vsock.rx_queue.as_mut().unwrap();
            let rx_desc = crate::queue::Descriptor {
                addr: 0x800,
                len: 256,
                flags: crate::queue::flags::WRITE,
                next: 0,
            };
            rx_queue.set_descriptor(0, rx_desc).unwrap();
            rx_queue.add_avail(0).unwrap();
        }

        let completions = vsock.process_tx_queue(&mut memory).unwrap();
        assert_eq!(completions.len(), 1);

        // Connection should be closed.
        assert_eq!(vsock.connection_count(), 0);

        // RST should have been injected into the RX queue.
        let rst_header = VsockHeader::from_bytes(&memory[0x800..0x800 + VsockHeader::SIZE]);
        assert!(rst_header.is_some());
        assert_eq!(rst_header.unwrap().operation(), Some(VsockOp::Rst));
    }

    #[test]
    fn test_process_tx_queue_credit_update() {
        let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        vsock.activate().unwrap();

        vsock.handle_connect(3000, 22).unwrap();

        let mut memory = vec![0u8; 4096];

        let mut header = VsockHeader::new(
            VsockAddr::new(3, 3000),
            VsockAddr::new(VirtioVsock::HOST_CID, 22),
            VsockOp::CreditUpdate,
        );
        header.buf_alloc = 131_072;
        header.fwd_cnt = 500;
        setup_tx_packet(&mut vsock, 0x100, &header, &[], &mut memory);

        let completions = vsock.process_tx_queue(&mut memory).unwrap();
        assert_eq!(completions.len(), 1);

        // Verify credit was updated on the connection.
        let conns = vsock.connections.read().unwrap();
        let conn = conns.get(&(3000, 22)).unwrap();
        assert_eq!(conn.peer_buf_alloc, 131_072);
        assert_eq!(conn.peer_fwd_cnt, 500);
    }

    // ==========================================================================
    // process_queue Dispatch Tests
    // ==========================================================================

    #[test]
    fn test_process_queue_dispatches_tx() {
        let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
        vsock.activate().unwrap();

        let mut memory = vec![0u8; 4096];

        // Empty TX queue should return empty completions.
        let completions = vsock.process_queue(1, &mut memory).unwrap();
        assert!(completions.is_empty());
    }

    #[test]
    fn test_process_queue_unknown_index() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let mut memory = vec![0u8; 1024];
        // Unhandled queue indices return empty.
        let completions = vsock.process_queue(0, &mut memory).unwrap();
        assert!(completions.is_empty());
        let completions = vsock.process_queue(2, &mut memory).unwrap();
        assert!(completions.is_empty());
        let completions = vsock.process_queue(99, &mut memory).unwrap();
        assert!(completions.is_empty());
    }

    // ==========================================================================
    // inject_rx_packet Tests
    // ==========================================================================

    #[test]
    fn test_inject_rx_packet_not_ready() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        // Don't activate -- no RX queue.
        let header = VsockHeader::new(
            VsockAddr::host(80),
            VsockAddr::new(3, 1000),
            VsockOp::Response,
        );
        let mut memory = vec![0u8; 1024];
        let result = vsock.inject_rx_packet(&header, &[], &mut memory);
        assert!(result.is_err());
    }

    #[test]
    fn test_inject_rx_packet_no_descriptors() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let header = VsockHeader::new(
            VsockAddr::host(80),
            VsockAddr::new(3, 1000),
            VsockOp::Response,
        );
        let mut memory = vec![0u8; 1024];
        // No descriptors added to the RX avail ring.
        let result = vsock.inject_rx_packet(&header, &[], &mut memory);
        assert!(result.is_err());
    }

    #[test]
    fn test_inject_rx_packet_with_data() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let mut memory = vec![0u8; 4096];

        // Set up an RX write-only descriptor.
        {
            let rx_queue = vsock.rx_queue.as_mut().unwrap();
            let desc = crate::queue::Descriptor {
                addr: 0x200,
                len: 512,
                flags: crate::queue::flags::WRITE,
                next: 0,
            };
            rx_queue.set_descriptor(0, desc).unwrap();
            rx_queue.add_avail(0).unwrap();
        }

        let payload = b"response data";
        let mut header =
            VsockHeader::new(VsockAddr::host(80), VsockAddr::new(3, 1000), VsockOp::Rw);
        header.len = payload.len() as u32;

        vsock
            .inject_rx_packet(&header, payload, &mut memory)
            .unwrap();

        // Verify header was written.
        let written_hdr =
            VsockHeader::from_bytes(&memory[0x200..0x200 + VsockHeader::SIZE]).unwrap();
        assert_eq!(written_hdr.operation(), Some(VsockOp::Rw));
        let wh_src_cid = written_hdr.src_cid;
        assert_eq!(wh_src_cid, VirtioVsock::HOST_CID);

        // Verify payload was written after the header.
        let payload_start = 0x200 + VsockHeader::SIZE;
        assert_eq!(
            &memory[payload_start..payload_start + payload.len()],
            payload
        );
    }

    // ==========================================================================
    // HostVsockBackend Tests (Unix only)
    // ==========================================================================

    #[cfg(unix)]
    #[test]
    fn test_host_vsock_backend_new() {
        let backend = HostVsockBackend::new();
        assert!(backend.port_map.is_empty());
        assert!(backend.channels.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_host_vsock_backend_add_port_mapping() {
        let mut backend = HostVsockBackend::new();
        backend.add_port_mapping(1234, "/tmp/test.sock".to_string());
        assert_eq!(backend.port_map.get(&1234).unwrap(), "/tmp/test.sock");
    }

    #[cfg(unix)]
    #[test]
    fn test_host_vsock_backend_connect_no_mapping() {
        let mut backend = HostVsockBackend::new();
        let addr = VsockAddr::new(3, 9999);
        let result = backend.on_connect(addr);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_host_vsock_backend_with_unix_socket() {
        use std::os::unix::net::UnixListener;

        let tmpdir = tempfile::tempdir().unwrap();
        let sock_path = tmpdir.path().join("test.sock");
        let sock_path_str = sock_path.to_str().unwrap().to_string();

        // Start a listener.
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let mut backend = HostVsockBackend::new();
        backend.add_port_mapping(5000, sock_path_str);

        // Connect should succeed.
        let addr = VsockAddr::new(3, 5000);
        backend.on_connect(addr).unwrap();

        // Send data.
        let data = b"ping";
        let sent = backend.on_send(addr, data).unwrap();
        assert_eq!(sent, data.len());

        // Close.
        backend.on_close(addr).unwrap();
        assert!(!backend.has_pending_data(addr));
    }

    // ==========================================================================
    // Guest-Memory-Based process_queue (HV backend path) Tests
    // ==========================================================================

    /// Builds a simulated split virtqueue layout in a flat memory buffer.
    /// Returns (desc_addr, avail_addr, used_addr, q_size).
    ///
    /// Layout:
    ///   desc_table: `base` .. `base + q_size * 16`
    ///   avail_ring: next 16-byte-aligned offset, size = 4 + 2*q_size + 2
    ///   used_ring:  next 16-byte-aligned offset, size = 4 + 8*q_size + 2
    fn setup_virtqueue_layout(
        memory: &mut Vec<u8>,
        base: usize,
        q_size: usize,
    ) -> (usize, usize, usize) {
        let desc_addr = base;
        let avail_addr = desc_addr + q_size * 16;
        // Align to 16 bytes.
        let avail_addr = (avail_addr + 15) & !15;
        let avail_size = 4 + 2 * q_size + 2;
        let used_addr = avail_addr + avail_size;
        let used_addr = (used_addr + 15) & !15;
        let used_size = 4 + 8 * q_size + 2;
        let total = used_addr + used_size;
        if memory.len() < total {
            memory.resize(total, 0);
        }
        (desc_addr, avail_addr, used_addr)
    }

    /// Helper: write a descriptor to the descriptor table in guest memory.
    fn write_descriptor(
        memory: &mut [u8],
        desc_addr: usize,
        idx: usize,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let off = desc_addr + idx * 16;
        memory[off..off + 8].copy_from_slice(&addr.to_le_bytes());
        memory[off + 8..off + 12].copy_from_slice(&len.to_le_bytes());
        memory[off + 12..off + 14].copy_from_slice(&flags.to_le_bytes());
        memory[off + 14..off + 16].copy_from_slice(&next.to_le_bytes());
    }

    /// Helper: add a descriptor head to the avail ring and increment avail_idx.
    fn avail_ring_push(memory: &mut [u8], avail_addr: usize, q_size: usize, head_idx: u16) {
        let avail_idx =
            u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]) as usize;
        let ring_off = avail_addr + 4 + 2 * (avail_idx % q_size);
        memory[ring_off..ring_off + 2].copy_from_slice(&head_idx.to_le_bytes());
        let new_idx = (avail_idx + 1) as u16;
        memory[avail_addr + 2..avail_addr + 4].copy_from_slice(&new_idx.to_le_bytes());
    }

    /// Verifies that the guest-memory-based `process_queue` correctly parses
    /// a 44-byte OP_RESPONSE packet from the TX virtqueue. This exercises the
    /// exact code path used by the HV backend when the guest responds to
    /// a host-initiated vsock connection.
    #[test]
    fn test_process_queue_guest_memory_op_response() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let q_size = 16usize;
        let mut memory = vec![0u8; 0x10000];

        // Set up TX queue layout at offset 0x4000.
        let (desc_addr, avail_addr, used_addr) =
            setup_virtqueue_layout(&mut memory, 0x4000, q_size);

        // Place an OP_RESPONSE packet at GPA 0x8000.
        let pkt_addr = 0x8000usize;
        let hdr = VsockHeader::new(
            VsockAddr::new(3, 1024), // src: guest CID 3, port 1024
            VsockAddr::host(50000),  // dst: host CID 2, port 50000
            VsockOp::Response,
        );
        let hdr_bytes = hdr.to_bytes();
        assert_eq!(
            hdr_bytes.len(),
            44,
            "VsockHeader must serialize to 44 bytes"
        );
        memory[pkt_addr..pkt_addr + 44].copy_from_slice(&hdr_bytes[..44]);

        // Set up descriptor: read-only, 44 bytes, pointing to pkt_addr.
        write_descriptor(&mut memory, desc_addr, 0, pkt_addr as u64, 44, 0, 0);
        // Push to avail ring.
        avail_ring_push(&mut memory, avail_addr, q_size, 0);

        // Build QueueConfig.
        // Use a mock VsockHostConnections to capture the mark_connected call.
        use std::sync::{Arc, Mutex};
        struct MockConns {
            connected: Vec<(u32, u32)>,
            credit_updates: Vec<(u32, u32, u32, u32)>,
        }
        impl VsockHostConnections for MockConns {
            fn fd_for(&self, _gp: u32, _hp: u32) -> Option<std::os::unix::io::RawFd> {
                None
            }
            fn mark_connected(&mut self, gp: u32, hp: u32) {
                self.connected.push((gp, hp));
            }
            fn remove_connection(&mut self, _gp: u32, _hp: u32) {}
            fn update_peer_credit(&mut self, gp: u32, hp: u32, ba: u32, fc: u32) {
                self.credit_updates.push((gp, hp, ba, fc));
            }
        }

        let mock = Arc::new(Mutex::new(MockConns {
            connected: Vec::new(),
            credit_updates: Vec::new(),
        }));

        let qcfg = crate::QueueConfig {
            desc_addr: desc_addr as u64,
            avail_addr: avail_addr as u64,
            used_addr: used_addr as u64,
            size: q_size as u16,
            ready: true,
            gpa_base: 0,
            vsock_connections: Some(mock.clone()),
        };

        let completions =
            <VirtioVsock as VirtioDevice>::process_queue(&mut vsock, 1, &mut memory, &qcfg)
                .unwrap();

        // Should have processed 1 packet.
        assert_eq!(
            completions.len(),
            1,
            "Expected 1 completion for OP_RESPONSE"
        );
        assert_eq!(completions[0].0, 0, "head_idx should be 0");
        assert_eq!(completions[0].1, 44, "written bytes should be 44");

        // Verify mark_connected was called.
        let mock_guard = mock.lock().unwrap();
        assert_eq!(
            mock_guard.connected.len(),
            1,
            "mark_connected should be called once for OP_RESPONSE"
        );
        assert_eq!(mock_guard.connected[0], (1024, 50000));

        // Verify credit was updated.
        assert_eq!(mock_guard.credit_updates.len(), 1);
        assert_eq!(
            mock_guard.credit_updates[0],
            (1024, 50000, 64 * 1024, 0),
            "peer credit should be synced from OP_RESPONSE header"
        );
    }

    /// Verifies that a 44-byte OP_RST from guest is correctly parsed via
    /// the guest-memory process_queue path.
    #[test]
    fn test_process_queue_guest_memory_op_rst() {
        let mut vsock = VirtioVsock::new(VsockConfig::default());
        vsock.activate().unwrap();

        let q_size = 16usize;
        let mut memory = vec![0u8; 0x10000];

        let (desc_addr, avail_addr, used_addr) =
            setup_virtqueue_layout(&mut memory, 0x4000, q_size);

        // OP_RST packet at GPA 0x8000.
        let pkt_addr = 0x8000usize;
        let hdr = VsockHeader::new(
            VsockAddr::new(3, 1024),
            VsockAddr::host(50000),
            VsockOp::Rst,
        );
        memory[pkt_addr..pkt_addr + 44].copy_from_slice(&hdr.to_bytes()[..44]);

        write_descriptor(&mut memory, desc_addr, 0, pkt_addr as u64, 44, 0, 0);
        avail_ring_push(&mut memory, avail_addr, q_size, 0);

        use std::sync::{Arc, Mutex};
        struct MockConns {
            removed: Vec<(u32, u32)>,
        }
        impl VsockHostConnections for MockConns {
            fn fd_for(&self, _: u32, _: u32) -> Option<std::os::unix::io::RawFd> {
                None
            }
            fn mark_connected(&mut self, _: u32, _: u32) {}
            fn remove_connection(&mut self, gp: u32, hp: u32) {
                self.removed.push((gp, hp));
            }
        }
        let mock = Arc::new(Mutex::new(MockConns {
            removed: Vec::new(),
        }));

        let qcfg = crate::QueueConfig {
            desc_addr: desc_addr as u64,
            avail_addr: avail_addr as u64,
            used_addr: used_addr as u64,
            size: q_size as u16,
            ready: true,
            gpa_base: 0,
            vsock_connections: Some(mock.clone()),
        };

        let completions =
            <VirtioVsock as VirtioDevice>::process_queue(&mut vsock, 1, &mut memory, &qcfg)
                .unwrap();
        assert_eq!(completions.len(), 1);

        let mock_guard = mock.lock().unwrap();
        assert_eq!(mock_guard.removed.len(), 1);
        assert_eq!(mock_guard.removed[0], (1024, 50000));
    }

    /// Verifies VsockHeader serialization produces exactly 44 bytes
    /// and is correctly round-trippable.
    #[test]
    fn test_vsock_header_size_is_44() {
        assert_eq!(VsockHeader::SIZE, 44);
        let hdr = VsockHeader::new(
            VsockAddr::host(50000),
            VsockAddr::new(3, 1024),
            VsockOp::Request,
        );
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), 44);

        // Verify round-trip.
        let parsed = VsockHeader::from_bytes(&bytes).unwrap();
        assert_eq!({ parsed.src_cid }, 2);
        assert_eq!({ parsed.dst_cid }, 3);
        assert_eq!({ parsed.src_port }, 50000);
        assert_eq!({ parsed.dst_port }, 1024);
        assert_eq!({ parsed.op }, VsockOp::Request as u16);
        assert_eq!({ parsed.socket_type }, 1);
        assert_eq!({ parsed.buf_alloc }, 64 * 1024);
    }
}
