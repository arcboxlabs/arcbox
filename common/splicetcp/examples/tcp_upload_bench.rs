//! Measure IPv4/IPv6 uploads, including encoding, bridge writes, ACKs, and peer reads.
use arcbox_packet::{
    ethernet::{
        SynParams, TcpFrameParams, build_tcp_ack_frame, build_tcp_data_frame, build_tcp_syn_frame,
    },
    ip::{IpPacketView, TcpPacketView},
};
use splicetcp::tcp_bridge::TcpBridge;
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const GW_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
const GUEST_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
const PAYLOAD: [u8; 1400] = [0xa5; 1400];

fn tcp(frame: &[u8]) -> TcpPacketView<'_> {
    TcpPacketView::parse(IpPacketView::parse(&frame[14..]).unwrap()).unwrap()
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("mode,segments,sample,ns_per_batch");
    for (family, guest, remote) in [
        (
            "ipv4",
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ),
        (
            "ipv6",
            "2001:db8::2".parse().unwrap(),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ),
    ] {
        for bare in [false, true] {
            for segments in [1, 32] {
                let mut bridge = TcpBridge::new(Ipv4Addr::new(192, 0, 2, 1));
                bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
                let wake = Arc::new(Notify::new());
                bridge.set_handshake_waker(Arc::clone(&wake));
                let listener = tokio::net::TcpListener::bind((remote, 0)).await.unwrap();
                let mut params = TcpFrameParams {
                    src_ip: guest,
                    dst_ip: remote,
                    src_port: 42000,
                    dst_port: listener.local_addr().unwrap().port(),
                    seq: 101,
                    ack: 0,
                    window: 65535,
                    src_mac: GUEST_MAC,
                    dst_mac: GW_MAC,
                };
                let syn = build_tcp_syn_frame(&SynParams {
                    src_ip: guest,
                    dst_ip: remote,
                    src_port: params.src_port,
                    dst_port: params.dst_port,
                    seq: 100,
                    src_mac: GUEST_MAC,
                    dst_mac: GW_MAC,
                    mss: 1400,
                    wscale: None,
                    sack_permitted: false,
                });
                assert!(
                    bridge
                        .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
                        .is_none()
                );
                let (server, _) = listener.accept().await.unwrap();
                wake.notified().await;
                let syn_ack = bridge.poll_handshakes();
                assert_eq!(syn_ack.len(), 1);
                params.ack = tcp(&syn_ack[0]).sequence().wrapping_add(1);
                assert!(
                    bridge
                        .try_complete_handshake(&build_tcp_ack_frame(&params))
                        .is_some()
                );
                let mut server = server.into_std().unwrap();
                server.set_nonblocking(false).unwrap();
                server
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut received = vec![0; segments * PAYLOAD.len()];
                let mode = format!("{family}-{}", if bare { "ip" } else { "ethernet" });
                for sample in 0..8 {
                    let started = Instant::now();
                    for _ in 0..2000 {
                        let start_seq = params.seq;
                        let frames: Vec<_> = (0..segments)
                            .map(|_| {
                                let frame = build_tcp_data_frame(&params, &PAYLOAD);
                                params.seq = params.seq.wrapping_add(PAYLOAD.len() as u32);
                                frame
                            })
                            .collect();
                        let frames: Vec<_> = frames
                            .iter()
                            .map(|frame| &frame[if bare { 14 } else { 0 }..])
                            .collect();
                        let mut consumed = 0;
                        let mut bytes = 0;
                        let deadline = Instant::now();
                        while consumed < frames.len() || bytes < received.len() {
                            assert!(deadline.elapsed().as_secs() < 2, "upload did not drain");
                            let replies = if consumed < frames.len() {
                                let (count, ack) = if bare {
                                    bridge.try_fast_path_intercept_ip_batch(&frames[consumed..])
                                } else {
                                    bridge.try_fast_path_intercept_batch(&frames[consumed..])
                                }
                                .unwrap();
                                consumed += count;
                                vec![ack]
                            } else {
                                bridge.poll_fast_path()
                            };
                            for reply in replies {
                                let ack = tcp(&reply);
                                assert_eq!(ack.destination_port(), params.src_port);
                                let end = ack.acknowledgment().wrapping_sub(start_seq) as usize;
                                assert!(end >= bytes && end <= received.len());
                                server.read_exact(&mut received[bytes..end]).unwrap();
                                bytes = end;
                            }
                        }
                        assert!(received.iter().all(|byte| *byte == 0xa5));
                    }
                    if sample > 0 {
                        println!(
                            "{mode},{segments},{sample},{:.3}",
                            started.elapsed().as_secs_f64() * 1e9 / 2000.0
                        );
                    }
                }
            }
        }
    }
}
