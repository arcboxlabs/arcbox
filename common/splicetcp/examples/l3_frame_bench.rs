//! Measures validated L3 wrapping with reusable buffers; excludes sockets and classification.
use std::{hint::black_box, time::Instant};

use arcbox_packet::ethernet::ETH_HEADER_LEN;
use splicetcp::shim::{l2_to_l3, l3_to_l2, synthetic_l2_header};

fn main() {
    let ipv6 = std::env::args().nth(1).is_some_and(|value| value == "ipv6");
    let header_len = if ipv6 { 40 } else { 20 };
    let header = synthetic_l2_header();
    println!("family,payload_bytes,sample,ns_per_frame");
    for payload in [0usize, 64, 1460, 32768] {
        let mut packet = vec![0xa5; header_len + payload];
        packet[..header_len].fill(0);
        if ipv6 {
            packet[0] = 0x60;
            packet[4..6].copy_from_slice(&u16::try_from(payload).unwrap().to_be_bytes());
            packet[6] = 17;
        } else {
            packet[0] = 0x45;
            packet[2..4]
                .copy_from_slice(&u16::try_from(header_len + payload).unwrap().to_be_bytes());
            packet[9] = 17;
        }
        let mut buffer = vec![0; ETH_HEADER_LEN + packet.len()];
        let frame = l3_to_l2(&packet, &header, &mut buffer).unwrap();
        assert_eq!(l2_to_l3(frame), Some(packet.as_slice()));
        let iterations = if payload < 2000 { 2_000_000 } else { 100_000 };
        for sample in 0..8 {
            let started = Instant::now();
            for _ in 0..iterations {
                black_box(
                    l3_to_l2(
                        black_box(&packet),
                        black_box(&header),
                        black_box(&mut buffer),
                    )
                    .unwrap(),
                );
            }
            let nanos = started.elapsed().as_secs_f64() * 1e9 / f64::from(iterations);
            if sample > 0 {
                println!("{},{payload},{sample},{nanos:.3}", if ipv6 { 6 } else { 4 });
            }
        }
    }
}
