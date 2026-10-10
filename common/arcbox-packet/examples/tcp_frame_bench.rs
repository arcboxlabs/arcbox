//! Measure allocation, framing, payload copy, and checksum cost together.
//!
//! Run with `cargo run -p arcbox-packet --example tcp_frame_bench --release`.

use std::hint::black_box;
use std::net::Ipv4Addr;
use std::time::Instant;

use arcbox_packet::ethernet::{
    TcpFrameParams, build_tcp_data_frame, build_tcp_data_frame_partial_csum,
};

fn main() {
    let params = TcpFrameParams {
        src_ip: Ipv4Addr::new(192, 168, 1, 1),
        dst_ip: Ipv4Addr::new(192, 168, 1, 2),
        src_port: 443,
        dst_port: 12345,
        seq: 1000,
        ack: 2000,
        window: 65535,
        src_mac: [2, 1, 2, 3, 4, 5],
        dst_mac: [2, 6, 7, 8, 9, 10],
    };
    println!("round,checksum,payload_bytes,iterations,ns_per_frame");
    for round in 0..7 {
        for len in [0usize, 1, 64, 1460, 3960, 32768, 65495] {
            let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let iterations = (268_435_456 / len.max(1)).clamp(20_000, 1_000_000);
            for (name, build) in [
                (
                    "full",
                    build_tcp_data_frame as fn(&TcpFrameParams, &[u8]) -> Vec<u8>,
                ),
                ("partial", build_tcp_data_frame_partial_csum),
            ] {
                assert_eq!(&build(&params, &payload)[54..], &payload);
                for _ in 0..1000 {
                    black_box(build(black_box(&params), black_box(&payload)));
                }
                let start = Instant::now();
                for _ in 0..iterations {
                    black_box(build(black_box(&params), black_box(&payload)));
                }
                let ns = start.elapsed().as_nanos() as f64 / iterations as f64;
                println!("{round},{name},{len},{iterations},{ns:.3}");
            }
        }
    }
}
