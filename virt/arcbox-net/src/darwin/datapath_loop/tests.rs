use super::fd::{fd_read, fd_write, set_nonblocking, write_to_guest};
use super::guest_tx::{DeliveryClass, GuestTx, LOSSY_QUEUE_CAP};
use super::intercept::build_dns_servfail_response;
use super::*;
use std::os::fd::{FromRawFd, RawFd};

/// Creates a SOCK_DGRAM socketpair, returning (fd_a, fd_b) as OwnedFds.
fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds: [i32; 2] = [0; 2];
    // SAFETY: valid pointer to 2-element array.
    let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
    assert_eq!(ret, 0, "socketpair() failed");
    // SAFETY: fds are valid file descriptors from socketpair.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Shrinks a socket's send/receive buffers so overflow is reached quickly.
fn shrink_socket_buffers(fd: RawFd) {
    let size: libc::c_int = 8192;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: setsockopt on a valid fd with a valid c_int payload.
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const size).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(ret, 0, "setsockopt failed");
    }
}

/// Sends reliable frames until the socketpair overflows and a backlog forms.
/// Returns the number of frames sent. Panics if overflow is never reached.
fn send_until_backlog(tx: &mut GuestTx, fd: RawFd, frame: &[u8]) -> usize {
    let mut sent = 0usize;
    for _ in 0..10_000 {
        tx.send(frame.to_vec(), DeliveryClass::Reliable);
        sent += 1;
        tx.flush(fd);
        if tx.has_backlog() {
            return sent;
        }
    }
    panic!("socketpair never overflowed; cannot exercise backpressure");
}

#[test]
fn test_set_nonblocking() {
    let (a, _b) = socketpair();
    set_nonblocking(a.as_raw_fd()).unwrap();

    // SAFETY: fcntl on a valid fd.
    let flags = unsafe { libc::fcntl(a.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_ne!(flags & libc::O_NONBLOCK, 0, "O_NONBLOCK should be set");
}

#[test]
fn test_fd_read_write_roundtrip() {
    let (a, b) = socketpair();
    let data = b"hello network";

    // SAFETY: writing from valid buffer to valid fd.
    let n = unsafe { libc::write(b.as_raw_fd(), data.as_ptr().cast(), data.len()) };
    assert_eq!(n as usize, data.len());

    let mut buf = [0u8; 64];
    let n = fd_read(a.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(&buf[..n], data);
}

#[tokio::test]
async fn test_write_to_guest_roundtrip() {
    let (a, b) = socketpair();

    set_nonblocking(a.as_raw_fd()).unwrap();
    let guest_async = AsyncFd::new(FdWrapper(a)).unwrap();

    let frame = b"test ethernet frame data";
    write_to_guest(&guest_async, frame);

    let mut buf = [0u8; 128];
    let n = fd_read(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(n, frame.len());
    assert_eq!(&buf[..n], frame.as_slice());
}

#[test]
fn test_fd_write_roundtrip() {
    let (a, b) = socketpair();
    let data = b"fd_write test data";
    let n = fd_write(b.as_raw_fd(), data).unwrap();
    assert_eq!(n, data.len());

    let mut buf = [0u8; 64];
    let n = fd_read(a.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(&buf[..n], data);
}

#[test]
fn guest_tx_flush_delivers_a_queued_frame() {
    let (a, b) = socketpair();
    set_nonblocking(a.as_raw_fd()).unwrap();

    let mut tx = GuestTx::new(None);
    let frame_data = b"queued frame".to_vec();
    tx.send(frame_data.clone(), DeliveryClass::Reliable);
    assert_eq!(tx.stats.tx_syscalls, 0, "send must not touch the fd");

    tx.flush(a.as_raw_fd());
    assert!(!tx.has_backlog(), "queue should be empty after a flush");

    let mut buf = [0u8; 128];
    let n = fd_read(b.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(&buf[..n], frame_data.as_slice());
}

/// The point of ABX-313: a burst produced within one event-loop iteration
/// costs one syscall, not one per frame — and still arrives in order.
#[test]
fn guest_tx_flush_batches_a_burst_into_one_syscall() {
    let (a, b) = socketpair();
    set_nonblocking(a.as_raw_fd()).unwrap();

    let mut tx = GuestTx::new(None);
    const BURST: u8 = 32;
    for i in 0..BURST {
        tx.send(vec![i; 64], DeliveryClass::Reliable);
    }

    tx.flush(a.as_raw_fd());

    assert!(!tx.has_backlog(), "the whole burst should fit");
    assert_eq!(u64::from(BURST), tx.stats.tx_frames);
    assert_eq!(
        tx.stats.tx_syscalls, 1,
        "{BURST} frames must cost a single sendmsg_x"
    );

    let mut buf = [0u8; 128];
    for i in 0..BURST {
        let n = fd_read(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(n, 64);
        assert_eq!(buf[0], i, "batched frames must arrive in send order");
    }
}

/// Regression test for the container-egress black hole: when the socketpair
/// overflows (macOS returns `ENOBUFS`, not `EAGAIN`), reliable frames must
/// be queued — never dropped — and delivered in order once the peer drains.
/// The shim has no retransmission, so a single lost frame stalls its TCP
/// connection permanently.
#[test]
fn guest_tx_reliable_survives_socketpair_overflow() {
    let (a, b) = socketpair();
    set_nonblocking(a.as_raw_fd()).unwrap();
    set_nonblocking(b.as_raw_fd()).unwrap();
    shrink_socket_buffers(a.as_raw_fd());
    shrink_socket_buffers(b.as_raw_fd());

    let mut tx = GuestTx::new(None);
    // Each frame carries its sequence number so ordering is verifiable.
    let make_frame = |seq: u32| {
        let mut f = vec![0xAB_u8; 2048];
        f[..4].copy_from_slice(&seq.to_be_bytes());
        f
    };

    // Queue in bursts so the batched write — not just the single-write
    // fallback — is what overruns the socketpair.
    let mut sent = 0usize;
    for _ in 0..1_000 {
        for _ in 0..16 {
            tx.send(make_frame(sent as u32), DeliveryClass::Reliable);
            sent += 1;
        }
        tx.flush(a.as_raw_fd());
        if tx.has_backlog() {
            break;
        }
    }
    assert!(tx.has_backlog(), "socketpair never overflowed");
    // The retry timer must always be armed for a pending backlog — it is
    // the safety net against unreliable datagram writability signals.
    assert!(tx.awaits_retry(), "backlog must arm the retry timer");
    // Keep producing while blocked — everything must queue.
    for _ in 0..16 {
        tx.send(make_frame(sent as u32), DeliveryClass::Reliable);
        sent += 1;
    }

    // Alternately drain the peer and retry until everything is delivered.
    let mut buf = vec![0u8; 4096];
    let mut received = 0usize;
    let mut idle_rounds = 0;
    while received < sent && idle_rounds < 1_000 {
        let mut progressed = false;
        while let Ok(n) = fd_read(b.as_raw_fd(), &mut buf) {
            if n == 0 {
                break;
            }
            assert_eq!(
                u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
                received as u32,
                "frames must arrive in send order"
            );
            received += 1;
            progressed = true;
        }
        tx.flush(a.as_raw_fd());
        if !progressed {
            idle_rounds += 1;
        }
    }

    assert_eq!(received, sent, "reliable frames were lost across overflow");
    assert!(!tx.has_backlog(), "queue must fully drain");
    assert!(
        tx.stats.enobufs_events + tx.stats.would_block_events >= 1,
        "the overflow path was never exercised"
    );
    assert_eq!(tx.stats.lossy_dropped, 0);
    assert_eq!(tx.stats.io_errors, 0);
    assert_eq!(tx.stats.short_writes, 0);
}

/// Lossy frames are bounded at `LOSSY_QUEUE_CAP` and dropped beyond it;
/// reliable frames are still accepted past the cap.
#[test]
fn guest_tx_lossy_capped_reliable_uncapped() {
    let (a, b) = socketpair();
    set_nonblocking(a.as_raw_fd()).unwrap();
    set_nonblocking(b.as_raw_fd()).unwrap();
    shrink_socket_buffers(a.as_raw_fd());
    shrink_socket_buffers(b.as_raw_fd());

    let mut tx = GuestTx::new(None);
    let frame = vec![0xCD_u8; 2048];
    send_until_backlog(&mut tx, a.as_raw_fd(), &frame);

    for _ in 0..(LOSSY_QUEUE_CAP + 100) {
        tx.send(frame.clone(), DeliveryClass::Lossy);
    }
    assert!(
        tx.stats.lossy_dropped >= 100,
        "lossy frames must drop at the cap (dropped: {})",
        tx.stats.lossy_dropped
    );

    let hwm_before = tx.stats.queue_high_water;
    tx.send(frame.clone(), DeliveryClass::Reliable);
    assert!(
        tx.stats.queue_high_water > hwm_before,
        "reliable frames must still be accepted past the lossy cap"
    );
}

#[test]
fn test_build_dns_servfail_response() {
    let query = vec![
        0x12, 0x34, // ID
        0x01, 0x00, // Flags (RD)
        0x00, 0x01, // QDCOUNT
        0x00, 0x00, // ANCOUNT
        0x00, 0x00, // NSCOUNT
        0x00, 0x00, // ARCOUNT
        0x01, b'a', // QNAME label "a"
        0x00, // root
        0x00, 0x01, // QTYPE A
        0x00, 0x01, // QCLASS IN
    ];

    let response = build_dns_servfail_response(&query).expect("should build servfail");
    assert_eq!(response[0..2], query[0..2]); // ID preserved
    assert_eq!(response[2] & 0x80, 0x80); // QR=1
    assert_eq!(response[3] & 0x0F, 0x02); // RCODE=SERVFAIL
    assert_eq!(&response[4..6], &1u16.to_be_bytes()); // QDCOUNT=1
    assert_eq!(&response[6..8], &0u16.to_be_bytes()); // ANCOUNT=0
    assert_eq!(&response[8..10], &0u16.to_be_bytes()); // NSCOUNT=0
    assert_eq!(&response[10..12], &0u16.to_be_bytes()); // ARCOUNT=0
    assert_eq!(&response[12..], &query[12..]); // Question echoed
}

#[tokio::test(flavor = "current_thread")]
async fn guest_batch_promotes_handshake_before_following_payload() {
    use arcbox_packet::ethernet::{
        SynParams, TcpFrameParams, build_tcp_ack_frame, build_tcp_data_frame, build_tcp_fin_frame,
        build_tcp_syn_frame,
    };
    use tokio::io::AsyncReadExt;

    let gateway_ip = Ipv4Addr::new(192, 168, 64, 1);
    let guest_ip = Ipv4Addr::new(192, 168, 64, 2);
    let gateway_mac = [2, 0, 0, 0, 0, 1];
    let guest_mac = [2, 0, 0, 0, 0, 2];
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let (host_fd, guest_fd) = socketpair();
    let guest = std::os::unix::net::UnixDatagram::from(guest_fd);
    guest.set_nonblocking(true).unwrap();
    let guest = tokio::net::UnixDatagram::from_std(guest).unwrap();
    let cancel = CancellationToken::new();
    let (reply_tx, reply_rx) = mpsc::channel(16);
    let (_cmd_tx, cmd_rx) = mpsc::channel(1);
    let datapath = NetworkDatapath::new(
        host_fd,
        HostEgress::new(
            gateway_ip,
            gateway_mac,
            guest_ip,
            reply_tx,
            cancel.clone(),
            1500,
        ),
        reply_rx,
        cmd_rx,
        DhcpServer::new(crate::dhcp::DhcpConfig::new(
            gateway_ip,
            Ipv4Addr::new(255, 255, 255, 0),
        )),
        DnsForwarder::new(crate::dns::DnsConfig::default()),
        gateway_ip,
        guest_ip,
        gateway_mac,
        cancel.clone(),
        1500,
    );
    let running = tokio::spawn(datapath.run());
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut params = TcpFrameParams {
            src_ip: guest_ip,
            dst_ip: Ipv4Addr::LOCALHOST,
            src_port: 40400,
            dst_port: listener.local_addr().unwrap().port(),
            seq: 1001,
            ack: 0,
            window: 65535,
            src_mac: guest_mac,
            dst_mac: gateway_mac,
        };
        let syn = build_tcp_syn_frame(&SynParams {
            src_ip: params.src_ip,
            dst_ip: params.dst_ip,
            src_port: params.src_port,
            dst_port: params.dst_port,
            seq: params.seq - 1,
            src_mac: guest_mac,
            dst_mac: gateway_mac,
            mss: 1460,
            wscale: None,
            sack_permitted: false,
        });
        assert_eq!(fd_write(guest.as_raw_fd(), &syn).unwrap(), syn.len());
        let (mut upstream, _) = listener.accept().await.unwrap();
        let mut frame = [0; 1500];
        let n = guest.recv(&mut frame).await.unwrap();
        assert!(n >= 54);
        assert_eq!(frame[47], 0x12);
        params.ack = u32::from_be_bytes(frame[38..42].try_into().unwrap()).wrapping_add(1);

        // No await here: the current-thread runtime delivers all four frames in one source drain.
        let ack = build_tcp_ack_frame(&params);
        assert_eq!(fd_write(guest.as_raw_fd(), &ack).unwrap(), ack.len());
        let payload = b"first segment;second segment";
        for part in [&payload[..14], &payload[14..]] {
            let data = build_tcp_data_frame(&params, part);
            assert_eq!(fd_write(guest.as_raw_fd(), &data).unwrap(), data.len());
            params.seq += u32::try_from(part.len()).unwrap();
        }
        let fin = build_tcp_fin_frame(&params);
        assert_eq!(fd_write(guest.as_raw_fd(), &fin).unwrap(), fin.len());

        let mut received = Vec::new();
        upstream.read_to_end(&mut received).await.unwrap();
        assert_eq!(
            received, payload,
            "payload must arrive once, without guest retransmission"
        );
        loop {
            let n = guest.recv(&mut frame).await.unwrap();
            assert!(n >= 54);
            let ack = u32::from_be_bytes(frame[42..46].try_into().unwrap());
            assert!(ack <= params.seq + 1, "ACK must not exceed payload and FIN");
            if ack == params.seq + 1 {
                break;
            }
        }
    })
    .await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    result.expect("one guest batch must deliver payload and FIN after handshake promotion");
}
