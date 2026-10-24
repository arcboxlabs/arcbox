//! Reproducible branch-cost probe; this does not measure network throughput.

use super::*;
use std::hint::black_box;

fn sample<const WRAPPED: bool>(bridge: &mut TcpBridge, frame: &[u8], iterations: u32) -> f64 {
    let frames = std::slice::from_ref(&frame);
    let start = StdInstant::now();
    for _ in 0..iterations {
        let bridge = black_box(&mut *bridge);
        let frames = black_box(frames);
        let reply = if WRAPPED {
            bridge.intercept_at::<ETH_HEADER_LEN, _>(frames)
        } else {
            bridge.intercept_open_at::<ETH_HEADER_LEN, _>(frames)
        };
        let _ = black_box(reply);
    }
    start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations)
}

#[tokio::test]
#[ignore = "manual release-mode comparison; run with --ignored --nocapture"]
async fn ordinary_ack_abandon_branch_cost() {
    let (mut bridge, _owner, key) = inline_fixture(40130, Ipv4Addr::new(198, 18, 30, 170)).await;
    let frame = make_guest_ack_frame(key.src_port, key.dst_ip, key.dst_port, 2000, 1000);
    assert!(bridge.flow_controls.is_empty());
    assert!(bridge.abandoned_conns.is_empty());
    assert!(bridge.try_fast_path_intercept(&frame).unwrap().is_empty());
    sample::<false>(&mut bridge, &frame, 10_000);
    sample::<true>(&mut bridge, &frame, 10_000);

    const ITERATIONS: u32 = 1_000_000;
    let mut original = [0.0; 9];
    let mut wrapped = [0.0; 9];
    for round in 0..original.len() {
        if round % 2 == 0 {
            original[round] = sample::<false>(&mut bridge, &frame, ITERATIONS);
            wrapped[round] = sample::<true>(&mut bridge, &frame, ITERATIONS);
        } else {
            wrapped[round] = sample::<true>(&mut bridge, &frame, ITERATIONS);
            original[round] = sample::<false>(&mut bridge, &frame, ITERATIONS);
        }
    }
    eprintln!("original ns/ACK: {original:.3?}; wrapper ns/ACK: {wrapped:.3?}");
    original.sort_by(f64::total_cmp);
    wrapped.sort_by(f64::total_cmp);
    eprintln!(
        "ordinary ACK median of 9 alternating {ITERATIONS}-iteration rounds: original={:.3} ns, wrapper={:.3} ns, delta={:.2}%",
        original[4],
        wrapped[4],
        (wrapped[4] / original[4] - 1.0) * 100.0
    );
}
