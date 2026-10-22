//! Measure established ACK handling and polled downloads through public bridge APIs.
use std::{hint::black_box, io::Write, net::Ipv4Addr, time::Instant};

use arcbox_packet::{
    ethernet::{SynAckParams, TcpFrameParams, build_tcp_ack_frame, build_tcp_syn_ack_frame},
    ip::{IpPacketView, TcpPacketView},
};
use splicetcp::tcp_bridge::TcpBridge;

const GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
const GUEST: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
const GW_MAC: [u8; 6] = [2, 0, 0, 0, 0, 1];
const GUEST_MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("mode,flows,sample,ns_per_operation");
    for flow_count in [1usize, 64] {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let mut bridge = TcpBridge::new(GATEWAY);
        bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
        let mut peers = Vec::new();
        let mut acknowledgments = Vec::new();
        let mut params = Vec::new();
        for index in 0..flow_count {
            let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (server, _) = listener.accept().await.unwrap();
            let server = server.into_std().unwrap();
            server.set_nonblocking(false).unwrap();
            server.set_nodelay(true).unwrap();
            bridge.initiate_inbound(8000 + u16::try_from(index).unwrap(), client, GUEST, GATEWAY);
            let syns = bridge.poll_handshakes();
            assert_eq!(syns.len(), 1);
            let syn = TcpPacketView::parse(IpPacketView::parse(&syns[0][14..]).unwrap()).unwrap();
            let initial_ack = syn.sequence().wrapping_add(1);
            let response = build_tcp_syn_ack_frame(&SynAckParams {
                src_ip: GUEST,
                dst_ip: GATEWAY,
                src_port: syn.destination_port(),
                dst_port: syn.source_port(),
                seq: 100,
                ack: initial_ack,
                src_mac: GUEST_MAC,
                dst_mac: GW_MAC,
                mss: 1460,
                wscale: None,
                sack_permitted: false,
            });
            assert!(bridge.try_complete_handshake(&response).is_some());
            let p = TcpFrameParams {
                src_ip: GUEST,
                dst_ip: GATEWAY,
                src_port: syn.destination_port(),
                dst_port: syn.source_port(),
                seq: 101,
                ack: initial_ack,
                window: 65535,
                src_mac: GUEST_MAC,
                dst_mac: GW_MAC,
            };
            acknowledgments.push(build_tcp_ack_frame(&p));
            params.push(p);
            peers.push(server);
        }
        assert_eq!(bridge.fast_path_count(), flow_count);
        for mode in ["ack", "download"] {
            let iterations = if mode == "ack" { 200_000 } else { 10_000 };
            for sample in 0..8 {
                let started = Instant::now();
                for iteration in 0..iterations {
                    let index = iteration % flow_count;
                    if mode == "download" {
                        peers[index].write_all(&[0xa5; 1400]).unwrap();
                        let wait = Instant::now();
                        let mut bytes = 0;
                        while bytes < 1400 {
                            assert!(
                                wait.elapsed().as_secs() < 2,
                                "download did not reach the bridge"
                            );
                            for frame in bridge.poll_fast_path() {
                                let tcp = TcpPacketView::parse(
                                    IpPacketView::parse(&frame[14..]).unwrap(),
                                )
                                .unwrap();
                                assert_eq!(tcp.destination_port(), params[index].src_port);
                                assert_eq!(tcp.sequence(), params[index].ack);
                                assert!(tcp.payload().iter().all(|byte| *byte == 0xa5));
                                bytes += tcp.payload().len();
                                params[index].ack = params[index]
                                    .ack
                                    .wrapping_add(u32::try_from(tcp.payload().len()).unwrap());
                                acknowledgments[index] = build_tcp_ack_frame(&params[index]);
                                assert!(
                                    bridge
                                        .try_fast_path_intercept(&acknowledgments[index])
                                        .unwrap()
                                        .is_empty()
                                );
                            }
                        }
                        assert_eq!(bytes, 1400);
                    } else {
                        assert!(
                            black_box(&mut bridge)
                                .try_fast_path_intercept(black_box(&acknowledgments[index]))
                                .unwrap()
                                .is_empty()
                        );
                    }
                }
                let nanos = started.elapsed().as_secs_f64() * 1e9 / iterations as f64;
                if sample > 0 {
                    println!("{mode},{flow_count},{sample},{nanos:.3}");
                }
            }
        }
    }
}
