//! Measure classification, retained-frame copies, and queue draining without socket I/O.
use arcbox_packet::ethernet::{
    SynParams, TcpFrameParams, build_tcp_data_frame, build_tcp_syn_frame,
};
use splicetcp::{
    classifier::FrameClassifier,
    shim::{GATEWAY_MAC, HOST_MAC},
};
use std::{
    hint::black_box,
    net::{IpAddr, Ipv4Addr},
    time::Instant,
};

fn main() {
    let ipv6 = std::env::args().nth(1).is_some_and(|arg| arg == "ipv6");
    let (guest, remote): (IpAddr, IpAddr) = if ipv6 {
        (
            "2001:db8::2".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
        )
    } else {
        (
            Ipv4Addr::new(192, 168, 64, 2).into(),
            Ipv4Addr::new(192, 168, 64, 1).into(),
        )
    };
    println!("family,mode,entry,sample,ns_per_frame");
    for (mode, size) in [("syn", 0), ("data64", 64), ("data1400", 1400)] {
        let frame = if mode == "syn" {
            build_tcp_syn_frame(&SynParams {
                src_ip: guest,
                dst_ip: remote,
                src_port: 42001,
                dst_port: 443,
                seq: 100,
                src_mac: HOST_MAC,
                dst_mac: GATEWAY_MAC,
                mss: 1400,
                wscale: None,
                sack_permitted: false,
            })
        } else {
            build_tcp_data_frame(
                &TcpFrameParams {
                    src_ip: guest,
                    dst_ip: remote,
                    src_port: 42001,
                    dst_port: 443,
                    seq: 101,
                    ack: 1000,
                    window: 65535,
                    src_mac: HOST_MAC,
                    dst_mac: GATEWAY_MAC,
                },
                &vec![0xa5; size],
            )
        };
        for entry in ["ethernet", "ip"] {
            let mut classifier =
                FrameClassifier::with_pool_capacity(Ipv4Addr::new(192, 168, 64, 1), 1500, 4);
            let mut mac = None;
            for sample in 0..8 {
                let started = Instant::now();
                for _ in 0..100_000 {
                    if entry == "ip" {
                        classifier.classify_ip_packet(black_box(&frame[14..]), &mut mac);
                    } else {
                        classifier.classify_frame(black_box(&frame), &mut mac);
                    }
                    if mode == "syn" {
                        let syns = classifier.take_gated_syns();
                        assert_eq!(syns.len(), 1);
                        assert_eq!(syns[0].frame, frame);
                    } else {
                        let mut retained = 0;
                        assert!(
                            classifier
                                .drain_fast_path(|bytes| {
                                    assert_eq!(bytes, frame);
                                    retained += 1;
                                    Some(Vec::new())
                                })
                                .is_empty()
                        );
                        assert_eq!(retained, 1);
                    }
                }
                if sample > 0 {
                    println!(
                        "{},{mode},{entry},{sample},{:.3}",
                        if ipv6 { 6 } else { 4 },
                        started.elapsed().as_secs_f64() * 1e9 / 100_000.0
                    );
                }
            }
        }
    }
}
