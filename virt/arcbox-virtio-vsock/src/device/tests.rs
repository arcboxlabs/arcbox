use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use arcbox_virtio_core::{QueueConfig, VirtioDevice, VirtioDeviceId};

use crate::addr::{VsockAddr, VsockHostConnections};
use crate::backend::LoopbackBackend;
use crate::manager;
use crate::protocol::{VsockHeader, VsockOp};

use super::*;

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
}

#[test]
fn test_vsock_write_config_noop() {
    let mut vsock = VirtioVsock::new(VsockConfig { guest_cid: 42 });

    vsock.write_config(0, &[0xFF; 8]);

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

#[test]
fn test_vsock_constants() {
    assert_eq!(VirtioVsock::HOST_CID, 2);
    assert_eq!(VirtioVsock::RESERVED_CID, 1);
    assert_eq!(VirtioVsock::FEATURE_STREAM, 1 << 0);
    assert_eq!(VirtioVsock::FEATURE_SEQPACKET, 1 << 1);
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

    vsock.handle_connect(1000, 80).unwrap();
    assert_eq!(vsock.connection_count(), 1);

    let data = b"GET / HTTP/1.1";
    let sent = vsock.handle_send(1000, 80, data).unwrap();
    assert_eq!(sent, data.len());

    let mut buf = [0u8; 64];
    let received = vsock.handle_recv(1000, 80, &mut buf).unwrap();
    assert_eq!(received, data.len());
    assert_eq!(&buf[..received], data);

    vsock.handle_close(1000, 80).unwrap();
    assert_eq!(vsock.connection_count(), 0);
}

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

    if memory.len() < guest_addr + total {
        memory.resize(guest_addr + total, 0);
    }

    memory[guest_addr..guest_addr + header_bytes.len()].copy_from_slice(&header_bytes);
    if !payload.is_empty() {
        memory[guest_addr + header_bytes.len()..guest_addr + total].copy_from_slice(payload);
    }

    let queue = vsock.tx_queue.as_mut().unwrap();
    let desc = arcbox_virtio_core::queue::Descriptor {
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
    assert_eq!(completions, []);
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
        let rx_desc = arcbox_virtio_core::queue::Descriptor {
            addr: 0x800,
            len: 256,
            flags: arcbox_virtio_core::queue::flags::WRITE,
            next: 0,
        };
        rx_queue.set_descriptor(0, rx_desc).unwrap();
        rx_queue.add_avail(0).unwrap();
    }

    let completions = vsock.process_tx_queue(&mut memory).unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].0, 0); // descriptor head index

    assert_eq!(vsock.connection_count(), 1);

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

    vsock.handle_connect(1000, 80).unwrap();

    let mut memory = vec![0u8; 4096];

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

    vsock.handle_connect(2000, 443).unwrap();
    assert_eq!(vsock.connection_count(), 1);

    let mut memory = vec![0u8; 4096];

    let header = VsockHeader::new(
        VsockAddr::new(3, 2000),
        VsockAddr::new(VirtioVsock::HOST_CID, 443),
        VsockOp::Shutdown,
    );
    setup_tx_packet(&mut vsock, 0x100, &header, &[], &mut memory);

    // Provide an RX descriptor for the RST response.
    {
        let rx_queue = vsock.rx_queue.as_mut().unwrap();
        let rx_desc = arcbox_virtio_core::queue::Descriptor {
            addr: 0x800,
            len: 256,
            flags: arcbox_virtio_core::queue::flags::WRITE,
            next: 0,
        };
        rx_queue.set_descriptor(0, rx_desc).unwrap();
        rx_queue.add_avail(0).unwrap();
    }

    let completions = vsock.process_tx_queue(&mut memory).unwrap();
    assert_eq!(completions.len(), 1);

    assert_eq!(vsock.connection_count(), 0);

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

    let conns = vsock.connections.read().unwrap();
    let conn = conns.get(&(3000, 22)).unwrap();
    assert_eq!(conn.peer_buf_alloc, 131_072);
    assert_eq!(conn.peer_fwd_cnt, 500);
}

#[test]
fn test_process_queue_dispatches_tx() {
    let mut vsock = VirtioVsock::with_backend(VsockConfig::default(), LoopbackBackend::new());
    vsock.activate().unwrap();

    let mut memory = vec![0u8; 4096];

    let completions = vsock.process_queue(1, &mut memory).unwrap();
    assert_eq!(completions, []);
}

#[test]
fn test_process_queue_unknown_index() {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
    vsock.activate().unwrap();

    let mut memory = vec![0u8; 1024];
    let completions = vsock.process_queue(0, &mut memory).unwrap();
    assert_eq!(completions, []);
    let completions = vsock.process_queue(2, &mut memory).unwrap();
    assert_eq!(completions, []);
    let completions = vsock.process_queue(99, &mut memory).unwrap();
    assert_eq!(completions, []);
}

#[test]
fn test_inject_rx_packet_not_ready() {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
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
    let result = vsock.inject_rx_packet(&header, &[], &mut memory);
    assert!(result.is_err());
}

#[test]
fn test_inject_rx_packet_with_data() {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
    vsock.activate().unwrap();

    let mut memory = vec![0u8; 4096];

    {
        let rx_queue = vsock.rx_queue.as_mut().unwrap();
        let desc = arcbox_virtio_core::queue::Descriptor {
            addr: 0x200,
            len: 512,
            flags: arcbox_virtio_core::queue::flags::WRITE,
            next: 0,
        };
        rx_queue.set_descriptor(0, desc).unwrap();
        rx_queue.add_avail(0).unwrap();
    }

    let payload = b"response data";
    let mut header = VsockHeader::new(VsockAddr::host(80), VsockAddr::new(3, 1000), VsockOp::Rw);
    header.len = payload.len() as u32;

    vsock
        .inject_rx_packet(&header, payload, &mut memory)
        .unwrap();

    let written_hdr = VsockHeader::from_bytes(&memory[0x200..0x200 + VsockHeader::SIZE]).unwrap();
    assert_eq!(written_hdr.operation(), Some(VsockOp::Rw));
    let wh_src_cid = written_hdr.src_cid;
    assert_eq!(wh_src_cid, VirtioVsock::HOST_CID);

    let payload_start = 0x200 + VsockHeader::SIZE;
    assert_eq!(
        &memory[payload_start..payload_start + payload.len()],
        payload
    );
}

/// Builds a simulated split virtqueue layout in a flat memory buffer.
/// Returns (`desc_addr`, `avail_addr`, `used_addr`).
fn setup_virtqueue_layout(
    memory: &mut Vec<u8>,
    base: usize,
    q_size: usize,
) -> (usize, usize, usize) {
    let desc_addr = base;
    let avail_addr = desc_addr + q_size * 16;
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

fn avail_ring_push(memory: &mut [u8], avail_addr: usize, q_size: usize, head_idx: u16) {
    let avail_idx = u16::from_le_bytes([memory[avail_addr + 2], memory[avail_addr + 3]]) as usize;
    let ring_off = avail_addr + 4 + 2 * (avail_idx % q_size);
    memory[ring_off..ring_off + 2].copy_from_slice(&head_idx.to_le_bytes());
    let new_idx = (avail_idx + 1) as u16;
    memory[avail_addr + 2..avail_addr + 4].copy_from_slice(&new_idx.to_le_bytes());
}

/// Verifies that the guest-memory-based `process_queue` correctly parses
/// a 44-byte OP_RESPONSE packet from the TX virtqueue.
#[test]
fn test_process_queue_guest_memory_op_response() {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
    vsock.activate().unwrap();

    let q_size = 16usize;
    let mut memory = vec![0u8; 0x10000];

    let (desc_addr, avail_addr, used_addr) = setup_virtqueue_layout(&mut memory, 0x4000, q_size);

    let pkt_addr = 0x8000usize;
    let hdr = VsockHeader::new(
        VsockAddr::new(3, 1024),
        VsockAddr::host(50000),
        VsockOp::Response,
    );
    let hdr_bytes = hdr.to_bytes();
    assert_eq!(
        hdr_bytes.len(),
        44,
        "VsockHeader must serialize to 44 bytes"
    );
    memory[pkt_addr..pkt_addr + 44].copy_from_slice(&hdr_bytes[..44]);

    write_descriptor(&mut memory, desc_addr, 0, pkt_addr as u64, 44, 0, 0);
    avail_ring_push(&mut memory, avail_addr, q_size, 0);

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

    let qcfg = QueueConfig {
        desc_addr: desc_addr as u64,
        avail_addr: avail_addr as u64,
        used_addr: used_addr as u64,
        size: q_size as u16,
        ready: true,
        gpa_base: 0,
    };
    vsock.bind_connections(mock.clone());

    let completions =
        <VirtioVsock as VirtioDevice>::process_queue(&mut vsock, 1, &mut memory, &qcfg).unwrap();

    assert_eq!(
        completions.len(),
        1,
        "Expected 1 completion for OP_RESPONSE"
    );
    assert_eq!(completions[0].0, 0, "head_idx should be 0");
    assert_eq!(completions[0].1, 44, "written bytes should be 44");

    let mock_guard = mock.lock().unwrap();
    assert_eq!(
        mock_guard.connected.len(),
        1,
        "mark_connected should be called once for OP_RESPONSE"
    );
    assert_eq!(mock_guard.connected[0], (1024, 50000));

    assert_eq!(mock_guard.credit_updates.len(), 1);
    assert_eq!(
        mock_guard.credit_updates[0],
        (1024, 50000, 64 * 1024, 0),
        "peer credit should be synced from OP_RESPONSE header"
    );
}

/// Verifies that a 44-byte OP_RST from guest is correctly parsed via
/// the guest-memory `process_queue` path.
#[test]
fn test_process_queue_guest_memory_op_rst() {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
    vsock.activate().unwrap();

    let q_size = 16usize;
    let mut memory = vec![0u8; 0x10000];

    let (desc_addr, avail_addr, used_addr) = setup_virtqueue_layout(&mut memory, 0x4000, q_size);

    let pkt_addr = 0x8000usize;
    let hdr = VsockHeader::new(
        VsockAddr::new(3, 1024),
        VsockAddr::host(50000),
        VsockOp::Rst,
    );
    memory[pkt_addr..pkt_addr + 44].copy_from_slice(&hdr.to_bytes()[..44]);

    write_descriptor(&mut memory, desc_addr, 0, pkt_addr as u64, 44, 0, 0);
    avail_ring_push(&mut memory, avail_addr, q_size, 0);

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

    let qcfg = QueueConfig {
        desc_addr: desc_addr as u64,
        avail_addr: avail_addr as u64,
        used_addr: used_addr as u64,
        size: q_size as u16,
        ready: true,
        gpa_base: 0,
    };
    vsock.bind_connections(mock.clone());

    let completions =
        <VirtioVsock as VirtioDevice>::process_queue(&mut vsock, 1, &mut memory, &qcfg).unwrap();
    assert_eq!(completions.len(), 1);

    let mock_guard = mock.lock().unwrap();
    assert_eq!(mock_guard.removed.len(), 1);
    assert_eq!(mock_guard.removed[0], (1024, 50000));
}

/// A host→guest RW is sized to the buffer it lands in. The Linux driver
/// posts `SKB_WITH_OVERHEAD(4 KiB)` buffers (3776 bytes on a 4 KiB-page
/// arm64 kernel) and drops a packet whose used length is shorter than its
/// header's `len`; sizing reads from the guest's window alone overflowed
/// every full packet, which the guest then never credited back.
#[test]
fn next_rx_capacity_reads_the_next_chains_writable_bytes_without_consuming_it() {
    let q_size = 8usize;
    let mut memory = vec![0u8; 0x10000];
    let (desc_addr, avail_addr, used_addr) = setup_virtqueue_layout(&mut memory, 0x4000, q_size);
    let write = arcbox_virtio_core::queue::flags::WRITE;
    let next = arcbox_virtio_core::queue::flags::NEXT;

    // Empty ring: nothing to land in.
    assert_eq!(
        VirtioVsock::next_rx_capacity(&mut memory, desc_addr, avail_addr, used_addr, q_size, 0),
        0
    );

    // One 3776-byte buffer, as the driver posts.
    write_descriptor(&mut memory, desc_addr, 0, 0x8000, 3776, write, 0);
    avail_ring_push(&mut memory, avail_addr, q_size, 0);
    assert_eq!(
        VirtioVsock::next_rx_capacity(&mut memory, desc_addr, avail_addr, used_addr, q_size, 0),
        3776
    );
    // Reading again consumes nothing.
    assert_eq!(
        VirtioVsock::next_rx_capacity(&mut memory, desc_addr, avail_addr, used_addr, q_size, 0),
        3776
    );

    // A chain sums its writable descriptors and skips read-only ones.
    let mut memory2 = vec![0u8; 0x10000];
    let (d2, a2, u2) = setup_virtqueue_layout(&mut memory2, 0x4000, q_size);
    write_descriptor(&mut memory2, d2, 0, 0x8000, 100, next, 1);
    write_descriptor(&mut memory2, d2, 1, 0x9000, 1000, write | next, 2);
    write_descriptor(&mut memory2, d2, 2, 0xa000, 24, write, 0);
    avail_ring_push(&mut memory2, a2, q_size, 0);
    assert_eq!(
        VirtioVsock::next_rx_capacity(&mut memory2, d2, a2, u2, q_size, 0),
        1024
    );
}

/// The Linux driver's RX buffer: `SKB_WITH_OVERHEAD(4 KiB)` on a 4 KiB-page
/// kernel.
const RX_BUF_LEN: u32 = 3776;

/// Guest memory layout shared by the injection tests below: a 16-entry RX
/// ring at 0x1000 and RX buffers from 0x10000, one page apart.
const RX_RING_BASE: usize = 0x1000;
const RX_BUF_BASE: u64 = 0x10000;
const RX_Q_SIZE: usize = 16;

struct RxRing {
    desc: usize,
    avail: usize,
    used: usize,
    posted: usize,
}

impl RxRing {
    fn layout(memory: &mut Vec<u8>) -> Self {
        let (desc, avail, used) = setup_virtqueue_layout(memory, RX_RING_BASE, RX_Q_SIZE);
        Self {
            desc,
            avail,
            used,
            posted: 0,
        }
    }

    /// Posts `n` more driver-sized RX buffers.
    fn post(&mut self, memory: &mut [u8], n: usize) {
        for _ in 0..n {
            let idx = self.posted;
            assert!(idx < RX_Q_SIZE, "test ring exhausted");
            let addr = RX_BUF_BASE + (idx as u64) * 4096;
            write_descriptor(
                memory,
                self.desc,
                idx,
                addr,
                RX_BUF_LEN,
                arcbox_virtio_core::queue::flags::WRITE,
                0,
            );
            avail_ring_push(memory, self.avail, RX_Q_SIZE, idx as u16);
            self.posted += 1;
        }
    }

    fn config(&self) -> QueueConfig {
        QueueConfig {
            desc_addr: self.desc as u64,
            avail_addr: self.avail as u64,
            used_addr: self.used as u64,
            size: RX_Q_SIZE as u16,
            ready: true,
            gpa_base: 0,
        }
    }

    fn used_idx(&self, memory: &[u8]) -> usize {
        u16::from_le_bytes([memory[self.used + 2], memory[self.used + 3]]) as usize
    }

    /// Every packet the device completed, in used-ring order, as
    /// `(header, payload)`.
    fn completed(&self, memory: &[u8]) -> Vec<(VsockHeader, Vec<u8>)> {
        (0..self.used_idx(memory))
            .map(|k| {
                let entry = self.used + 4 + 8 * (k % RX_Q_SIZE);
                let head =
                    u32::from_le_bytes(memory[entry..entry + 4].try_into().unwrap()) as usize;
                let len =
                    u32::from_le_bytes(memory[entry + 4..entry + 8].try_into().unwrap()) as usize;
                let buf = RX_BUF_BASE as usize + head * 4096;
                let hdr = VsockHeader::from_bytes(&memory[buf..buf + VsockHeader::SIZE]).unwrap();
                assert_eq!(hdr.len as usize, len - VsockHeader::SIZE);
                (hdr, memory[buf + VsockHeader::SIZE..buf + len].to_vec())
            })
            .collect()
    }
}

fn make_socketpair() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` is a valid 2-element array for socketpair to fill.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    assert_eq!(ret, 0);
    // SAFETY: both fds are fresh from socketpair with sole ownership.
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// A device bound to `memory` (GPA base 0) plus `streams` connected
/// host→guest connections, each granted `credit` bytes of peer window.
/// Returns the daemon-side end of every stream in connection order.
fn bind_injection_device(
    memory: &mut [u8],
    streams: usize,
    credit: u32,
) -> (
    VirtioVsock,
    Arc<Mutex<manager::VsockConnectionManager>>,
    Vec<(manager::VsockConnectionId, std::os::fd::OwnedFd)>,
) {
    let mut vsock = VirtioVsock::new(VsockConfig::default());
    // SAFETY: `memory` outlives the device in every test below and is not
    // touched while `poll_rx_injection` runs.
    let mem =
        unsafe { arcbox_virtio_core::GuestMemWriter::new(memory.as_mut_ptr(), memory.len(), 0) };
    vsock.bind_ctx(arcbox_virtio_core::DeviceCtx {
        mem: Arc::new(mem),
        raise_irq: Arc::new(|_| {}),
    });
    let mgr = Arc::new(Mutex::new(manager::VsockConnectionManager::new()));
    vsock.bind_connection_manager(mgr.clone());

    let mut hosts = Vec::new();
    let mut m = mgr.lock().unwrap();
    for i in 0..streams {
        let (host, internal) = make_socketpair();
        // As `connect_vsock_hv` sets the pair up: non-blocking device end,
        // and 1 MiB socket buffers so a test payload fits in one write.
        let bufsize: libc::c_int = 1 << 20;
        for fd in [host.as_raw_fd(), internal.as_raw_fd()] {
            for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
                // SAFETY: `fd` is a live fd owned by this function; the
                // option value pointer is valid for the call.
                let rc = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        opt,
                        (&raw const bufsize).cast(),
                        std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                    )
                };
                assert_eq!(rc, 0);
            }
        }
        // SAFETY: `internal` is a live fd owned by this function.
        unsafe {
            let flags = libc::fcntl(internal.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(
                internal.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }
        let (id, _rx) = m.allocate(1024 + i as u32, 3, internal);
        let conn = m.get_mut(&id).unwrap();
        assert_eq!(conn.rx_queue.dequeue(), manager::RxOps::REQUEST);
        conn.connect = true;
        conn.update_peer_credit(credit, 0);
        hosts.push((id, host));
    }
    m.backend_rxq.clear();
    drop(m);
    (vsock, mgr, hosts)
}

fn write_all(fd: &std::os::fd::OwnedFd, byte: u8, len: usize) {
    let data = vec![byte; len];
    // SAFETY: `fd` is live and `data` is a valid buffer of the stated length.
    let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    assert_eq!(n as usize, len, "socketpair buffer too small for the test");
}

/// One round drains every queued stream in turn, one packet per visit, until
/// each has been read short — the host pays one interrupt for the lot.
#[test]
fn one_round_drains_every_stream_round_robin() {
    let mut memory = vec![0u8; 0x30000];
    let mut ring = RxRing::layout(&mut memory);
    ring.post(&mut memory, RX_Q_SIZE);
    let (mut vsock, mgr, hosts) = bind_injection_device(&mut memory, 2, 1 << 20);
    let (a, host_a) = &hosts[0];
    let (b, host_b) = &hosts[1];
    write_all(host_a, b'a', 10_000);
    write_all(host_b, b'b', 8_000);

    assert!(vsock.poll_rx_injection(&ring.config(), None, false).raise);

    let pkts = ring.completed(&memory);
    let ports: Vec<u32> = pkts.iter().map(|(h, _)| h.dst_port).collect();
    // Phase 1 walks a HashMap, so which stream goes first is arbitrary; the
    // property is alternation — neither stream is drained to exhaustion
    // while the other waits.
    assert_eq!(ports.len(), 6);
    assert!(
        ports.windows(2).all(|w| w[0] != w[1]),
        "streams are served alternately, not one to exhaustion: {ports:?}"
    );
    for (hdr, payload) in &pkts {
        assert_eq!(hdr.operation(), Some(VsockOp::Rw));
        let byte = if hdr.dst_port == a.guest_port {
            b'a'
        } else {
            b'b'
        };
        assert!(payload.iter().all(|&x| x == byte));
        assert!(payload.len() <= RX_BUF_LEN as usize - VsockHeader::SIZE);
    }
    let total = |port: u32| -> usize {
        pkts.iter()
            .filter(|(h, _)| h.dst_port == port)
            .map(|(_, p)| p.len())
            .sum()
    };
    assert_eq!(total(a.guest_port), 10_000);
    assert_eq!(total(b.guest_port), 8_000);

    // Both streams were read short, so nothing is left queued for the
    // next round; the readable fd re-arms the worker when more arrives.
    let m = mgr.lock().unwrap();
    assert!(m.backend_rxq.is_empty());
    assert!(!m.get(a).unwrap().rx_queue.pending());
    assert!(!m.get(b).unwrap().rx_queue.pending());
}

/// Running out of posted RX buffers ends the round with the stream still
/// queued (`rxq_starved`, so the caller interrupts the guest to refill) and
/// the next round resumes where it stopped.
#[test]
fn a_round_stops_at_the_last_posted_buffer_and_resumes() {
    let mut memory = vec![0u8; 0x30000];
    let mut ring = RxRing::layout(&mut memory);
    ring.post(&mut memory, 2);
    let (mut vsock, mgr, hosts) = bind_injection_device(&mut memory, 1, 1 << 20);
    let (id, host) = &hosts[0];
    write_all(host, b'x', 10_000);

    assert!(vsock.poll_rx_injection(&ring.config(), None, false).raise);
    assert_eq!(ring.used_idx(&memory), 2);
    {
        let m = mgr.lock().unwrap();
        assert_eq!(m.backend_rxq.front(), Some(id));
        assert!(m.get(id).unwrap().rx_queue.pending());
    }

    ring.post(&mut memory, 2);
    assert!(vsock.poll_rx_injection(&ring.config(), None, false).raise);
    assert_eq!(ring.used_idx(&memory), 3);
    let total: usize = ring.completed(&memory).iter().map(|(_, p)| p.len()).sum();
    assert_eq!(total, 10_000);
    assert!(mgr.lock().unwrap().backend_rxq.is_empty());
}

/// A stream that outruns the peer's window asks for credit as soon as it
/// crosses the half-window mark — ahead of the data still pending — then
/// parks at zero credit and resumes on the peer's CREDIT_UPDATE.
#[test]
fn a_closing_window_sends_the_credit_request_ahead_of_pending_data() {
    let mut memory = vec![0u8; 0x30000];
    let mut ring = RxRing::layout(&mut memory);
    ring.post(&mut memory, RX_Q_SIZE);
    let (mut vsock, mgr, hosts) = bind_injection_device(&mut memory, 1, 4_000);
    let (id, host) = &hosts[0];
    write_all(host, b'x', 10_000);

    assert!(vsock.poll_rx_injection(&ring.config(), None, false).raise);

    let ops: Vec<(Option<VsockOp>, usize)> = ring
        .completed(&memory)
        .iter()
        .map(|(h, p)| (h.operation(), p.len()))
        .collect();
    assert_eq!(
        ops,
        [
            (Some(VsockOp::Rw), RX_BUF_LEN as usize - VsockHeader::SIZE),
            (Some(VsockOp::CreditRequest), 0),
            (
                Some(VsockOp::Rw),
                4_000 - (RX_BUF_LEN as usize - VsockHeader::SIZE)
            ),
        ]
    );
    {
        let m = mgr.lock().unwrap();
        assert!(m.backend_rxq.is_empty(), "parked, not spinning");
        assert!(m.get(id).unwrap().credit_request_pending());
    }

    // The guest consumed everything and reopened the window.
    {
        let mut m = mgr.lock().unwrap();
        VsockHostConnections::update_peer_credit(
            &mut *m,
            id.guest_port,
            id.host_port,
            1 << 20,
            4_000,
        );
        assert_eq!(m.backend_rxq.front(), Some(id));
    }
    assert!(vsock.poll_rx_injection(&ring.config(), None, false).raise);
    let total: usize = ring
        .completed(&memory)
        .iter()
        .filter(|(h, _)| h.operation() == Some(VsockOp::Rw))
        .map(|(_, p)| p.len())
        .sum();
    assert_eq!(total, 10_000);
}

/// With EVENT_IDX the round interrupts only when the guest asked for it:
/// `used_event` inside the range it published means raise; a far-away
/// `used_event` (the guest is still draining) means data landed silently,
/// and a starved round that wrote nothing has nothing to report.
#[test]
fn an_event_idx_round_interrupts_only_when_the_guest_asked() {
    let mut memory = vec![0u8; 0x30000];
    let mut ring = RxRing::layout(&mut memory);
    ring.post(&mut memory, 2);
    let (mut vsock, mgr, hosts) = bind_injection_device(&mut memory, 1, 1 << 20);
    let (id, host) = &hosts[0];
    let used_event_off = ring.avail + 4 + 2 * RX_Q_SIZE;

    // The guest enabled callbacks at used.idx 0: it wants the next entry.
    memory[used_event_off..used_event_off + 2].copy_from_slice(&0u16.to_le_bytes());
    write_all(host, b'x', 1_000);
    assert_eq!(
        vsock.poll_rx_injection(&ring.config(), None, true),
        RxRound {
            wrote: true,
            raise: true
        }
    );
    assert_eq!(ring.used_idx(&memory), 1);

    // Now the guest is draining with callbacks disabled (used_event far off):
    // the data still lands, without an interrupt.
    memory[used_event_off..used_event_off + 2].copy_from_slice(&0x7fffu16.to_le_bytes());
    write_all(host, b'y', 1_000);
    assert_eq!(
        vsock.poll_rx_injection(&ring.config(), None, true),
        RxRound {
            wrote: true,
            raise: false
        }
    );
    assert_eq!(ring.used_idx(&memory), 2);

    // Out of descriptors with data pending: the stream stays queued
    // (rxq_starved) but nothing new was published, so no interrupt either.
    write_all(host, b'z', 1_000);
    assert_eq!(
        vsock.poll_rx_injection(&ring.config(), None, true),
        RxRound {
            wrote: false,
            raise: false
        }
    );
    assert_eq!(mgr.lock().unwrap().backend_rxq.front(), Some(id));

    // Without EVENT_IDX the same starved round interrupts so the guest refills.
    assert_eq!(
        vsock.poll_rx_injection(&ring.config(), None, false),
        RxRound {
            wrote: false,
            raise: true
        }
    );
}
