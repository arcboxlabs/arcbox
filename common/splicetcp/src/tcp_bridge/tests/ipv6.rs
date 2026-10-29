//! Independent IPv6 packets exercise bridge parsing and real sockets.

use super::*;
use crate::{EgressConn, EgressResolver, FlowMeta, NativeTcp};
use std::{
    net::Ipv6Addr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct GuestPacket<'a> {
    seq: u32,
    ack: u32,
    flags: u8,
    mss: Option<u16>,
    payload: &'a [u8],
}

fn packet(key: SynFlowKey, p: GuestPacket<'_>) -> Vec<u8> {
    let (IpAddr::V6(src), IpAddr::V6(dst)) = (key.src_ip, key.dst_ip) else {
        panic!("IPv6 fixture");
    };
    let options = p.mss.map(|mss| [2, 4, (mss >> 8) as u8, mss as u8]);
    let header_len = 20 + options.as_ref().map_or(0, |_| 4);
    let mut frame = vec![0; ETH_HEADER_LEN + 40 + header_len];
    frame[..6].copy_from_slice(&GW_MAC);
    frame[6..12].copy_from_slice(&GUEST_MAC);
    frame[12..14].copy_from_slice(&[0x86, 0xdd]);
    let ip = &mut frame[ETH_HEADER_LEN..];
    ip[0] = 0x60;
    ip[4..6].copy_from_slice(&((header_len + p.payload.len()) as u16).to_be_bytes());
    ip[6] = 6;
    ip[7] = 64;
    ip[8..24].copy_from_slice(&src.octets());
    ip[24..40].copy_from_slice(&dst.octets());
    ip[40..42].copy_from_slice(&key.src_port.to_be_bytes());
    ip[42..44].copy_from_slice(&key.dst_port.to_be_bytes());
    ip[44..48].copy_from_slice(&p.seq.to_be_bytes());
    ip[48..52].copy_from_slice(&p.ack.to_be_bytes());
    ip[52] = ((header_len / 4) as u8) << 4;
    ip[53] = p.flags;
    ip[54..56].copy_from_slice(&65535u16.to_be_bytes());
    if let Some(options) = options {
        ip[60..64].copy_from_slice(&options);
    }
    frame.extend_from_slice(p.payload);
    let checksum = !tcp_sum(&frame[ETH_HEADER_LEN..]);
    frame[ETH_HEADER_LEN + 56..ETH_HEADER_LEN + 58].copy_from_slice(&checksum.to_be_bytes());
    frame
}

fn tcp_sum(ip: &[u8]) -> u16 {
    let tcp = &ip[40..];
    let mut bytes = ip[8..40].to_vec();
    bytes.extend_from_slice(&(tcp.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 6]);
    bytes.extend_from_slice(tcp);
    if !bytes.len().is_multiple_of(2) {
        bytes.push(0);
    }
    let mut sum: u64 = bytes
        .chunks_exact(2)
        .map(|word| u64::from(u16::from_be_bytes([word[0], word[1]])))
        .sum();
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    sum as u16
}

fn reply(frame: &[u8]) -> TcpPacketView<'_> {
    assert_eq!(&frame[12..14], &[0x86, 0xdd]);
    assert_eq!(tcp_sum(&frame[ETH_HEADER_LEN..]), 65535);
    super::super::tcp_at(frame, ETH_HEADER_LEN).unwrap()
}

#[test]
fn borrowed_ipv6_length_limit_preserves_maximum_packet_and_ethernet_padding() {
    let key = SynFlowKey {
        src_ip: "2001:db8::2".parse().unwrap(),
        src_port: 41000,
        dst_ip: Ipv6Addr::LOCALHOST.into(),
        dst_port: 80,
    };
    let payload = vec![0x5a; 65_515];
    let mut frame = packet(
        key,
        GuestPacket {
            seq: 1,
            flags: 0x18,
            payload: &payload,
            ..Default::default()
        },
    );
    let ip = &frame[ETH_HEADER_LEN..];
    assert_eq!(ip.len(), 65_575);
    assert_eq!(super::super::tcp_at(ip, 0).unwrap().payload(), payload);

    frame.push(0);
    assert!(super::super::tcp_at(&frame[ETH_HEADER_LEN..], 0).is_none());
    assert_eq!(
        super::super::tcp_at(&frame, ETH_HEADER_LEN)
            .unwrap()
            .payload(),
        payload
    );
}

struct CaptureResolver(Mutex<Vec<FlowMeta>>);
impl EgressResolver for CaptureResolver {
    fn resolve(&self, flow: FlowMeta) -> tokio::sync::oneshot::Receiver<Option<EgressConn>> {
        self.0.lock().unwrap().push(flow.clone());
        crate::egress::DefaultEgress::new(GW_IP, None, 3).resolve(flow)
    }
}

struct Ipv4OnlySink;
impl crate::direct_rx::ConnSink for Ipv4OnlySink {
    fn send_conn(&self, _: crate::direct_rx::PromotedConn) -> bool {
        panic!("an IPv6 flow must not reach the IPv4 inline sink")
    }
}

#[tokio::test]
async fn ipv6_handshake_dns_upload_download_and_fin_use_real_socket() {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let key = SynFlowKey {
        src_ip: "2001:db8::2".parse().unwrap(),
        src_port: 41000,
        dst_ip: Ipv6Addr::LOCALHOST.into(),
        dst_port: listener.local_addr().unwrap().port(),
    };
    let resolver = Arc::new(CaptureResolver(Mutex::new(Vec::new())));
    let dns = arcbox_fakeip::dns_log::DnsResolutionLog::new();
    dns.record("ipv6.fixture.test", &[Ipv6Addr::LOCALHOST]);
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.set_fast_path_mtu(1280);
    bridge.enable_large_frames();
    bridge.set_conn_sink(Arc::new(Ipv4OnlySink));
    bridge.set_dns_log(dns);
    bridge.set_egress_resolver(resolver.clone());
    let syn = packet(
        key,
        GuestPacket {
            seq: 100,
            flags: 2,
            mss: Some(900),
            ..Default::default()
        },
    );
    assert!(
        bridge
            .handle_outbound_syn(&syn, GW_MAC, GUEST_MAC)
            .is_none()
    );
    let (mut server, _) = tokio::time::timeout(TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    let syn_ack = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Some(frame) = bridge.poll_handshakes().into_iter().next() {
                break frame;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let tcp = reply(&syn_ack);
    assert_eq!(tcp.flags(), 0x12);
    assert_eq!(tcp.acknowledgment(), 101);
    assert_eq!(
        crate::ethernet::parse_tcp_syn_options(tcp.segment()).mss,
        Some(1220)
    );
    assert_eq!(
        resolver.0.lock().unwrap()[0].domain.as_deref(),
        Some("ipv6.fixture.test")
    );
    assert_eq!(resolver.0.lock().unwrap()[0].dst_ip, key.dst_ip);
    let our_seq = tcp.sequence().wrapping_add(1);
    let ack = packet(
        key,
        GuestPacket {
            seq: 101,
            ack: our_seq,
            flags: 0x18,
            payload: b"first",
            ..Default::default()
        },
    );
    let replies = bridge.try_complete_handshake(&ack).unwrap();
    assert_eq!(reply(&replies[0]).acknowledgment(), 106);
    assert!(!bridge.fast_path_conns[&key].inline_owned);
    assert_eq!(bridge.fast_path_conns[&key].peer_mss, 900);
    let uploads = [
        packet(
            key,
            GuestPacket {
                seq: 106,
                ack: our_seq,
                flags: 0x18,
                payload: b"second",
                ..Default::default()
            },
        ),
        packet(
            key,
            GuestPacket {
                seq: 112,
                ack: our_seq,
                flags: 0x18,
                payload: b"third",
                ..Default::default()
            },
        ),
    ];
    let borrowed: Vec<_> = uploads
        .iter()
        .map(|frame| &frame[ETH_HEADER_LEN..])
        .collect();
    let (count, ack) = bridge.try_fast_path_intercept_ip_batch(&borrowed).unwrap();
    assert_eq!(count, 2);
    assert_eq!(reply(&ack).acknowledgment(), 117);
    let mut received = [0; 16];
    tokio::time::timeout(TIMEOUT, server.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&received, b"firstsecondthird");
    let fin = packet(
        key,
        GuestPacket {
            seq: 117,
            ack: our_seq,
            flags: 0x11,
            ..Default::default()
        },
    );
    assert_eq!(
        reply(&bridge.try_fast_path_intercept(&fin).unwrap()).acknowledgment(),
        118
    );
    assert_eq!(
        tokio::time::timeout(TIMEOUT, server.read_u8())
            .await
            .unwrap()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    let download: Vec<_> = (0..3001).map(|index| (index % 251) as u8).collect();
    server.write_all(&download).await.unwrap();
    server.shutdown().await.unwrap();
    let mut received = Vec::new();
    let fin_seq = tokio::time::timeout(TIMEOUT, async {
        'outer: loop {
            for frame in bridge.poll_fast_path() {
                let tcp = reply(&frame);
                assert!(frame.len() <= ETH_HEADER_LEN + 1280);
                assert!(tcp.payload().len() <= 900);
                assert_eq!(tcp.sequence(), our_seq.wrapping_add(received.len() as u32));
                received.extend_from_slice(tcp.payload());
                if tcp.flags() & 1 != 0 {
                    break 'outer tcp.sequence();
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(received, download);
    let final_ack = packet(
        key,
        GuestPacket {
            seq: 118,
            ack: fin_seq.wrapping_add(1),
            flags: 0x10,
            ..Default::default()
        },
    );
    bridge.try_fast_path_intercept(&final_ack).unwrap();
    bridge.poll_fast_path();
    assert_eq!(bridge.fast_path_count(), 0);
}

#[tokio::test]
async fn ipv6_no_mss_defaults_and_cancellation_release_native_owner() {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut server, _) = listener.accept().await.unwrap();
    let key = SynFlowKey {
        src_ip: "2001:db8::3".parse().unwrap(),
        src_port: 41001,
        dst_ip: Ipv6Addr::LOCALHOST.into(),
        dst_port: listener.local_addr().unwrap().port(),
    };
    let lease = Arc::new(());
    let released = Arc::downgrade(&lease);
    let cancelled = CancellationToken::new();
    let native = NativeTcp::with_lifecycle(client.into_std().unwrap(), lease, cancelled.clone());
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.set_conn_sink(Arc::new(Ipv4OnlySink));
    bridge.enable_large_frames();
    bridge.promote_to_fast_path(key, native, 1000, 2000, 9000, None);
    server.write_all(&[0x5a; 5000]).await.unwrap();
    let frames = tokio::time::timeout(TIMEOUT, async {
        loop {
            let frames = bridge.poll_fast_path();
            if !frames.is_empty() {
                break frames;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for frame in frames {
        reply(&frame);
        assert!(
            frame.len() <= UNIX_DGRAM_MAX_FRAME_LEN,
            "IPv6 must not use the large/GSO path"
        );
    }
    cancelled.cancel();
    let frames = bridge.poll_fast_path();
    assert_eq!(frames.len(), 1);
    assert_ne!(reply(&frames[0]).flags() & 4, 0);
    assert_eq!(bridge.fast_path_count(), 0);
    assert!(released.upgrade().is_none());

    let mut bridge = TcpBridge::new(GW_IP);
    let syn = packet(
        key,
        GuestPacket {
            seq: 1,
            flags: 2,
            ..Default::default()
        },
    );
    bridge.handle_outbound_syn(&syn, GW_MAC, GUEST_MAC);
    assert_eq!(bridge.handshake_conns[&key].peer_mss, 1220);
}

#[tokio::test]
async fn ipv6_active_open_without_peer_mss_uses_ipv6_default() {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (_server, _) = listener.accept().await.unwrap();
    let key = SynFlowKey {
        src_ip: "2001:db8::4".parse().unwrap(),
        src_port: 8080,
        dst_ip: Ipv6Addr::LOCALHOST.into(),
        dst_port: 61500,
    };
    let mut bridge = TcpBridge::new(GW_IP);
    bridge.set_fast_path_macs(GW_MAC, GUEST_MAC);
    bridge.initiate_active_handshake(key, client.into_std().unwrap(), GW_MAC, GUEST_MAC);
    let frames = bridge.poll_handshakes();
    assert_eq!(frames.len(), 1);
    let syn = reply(&frames[0]);
    assert_eq!(syn.flags(), 2);
    assert_eq!(
        crate::ethernet::parse_tcp_syn_options(syn.segment()).mss,
        Some(1440)
    );
    let response = packet(
        key,
        GuestPacket {
            seq: 100,
            ack: syn.sequence().wrapping_add(1),
            flags: 0x12,
            ..Default::default()
        },
    );
    let frames = bridge.try_complete_handshake(&response).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(reply(&frames[0]).acknowledgment(), 101);
    assert_eq!(bridge.fast_path_conns[&key].peer_mss, 1220);
}
