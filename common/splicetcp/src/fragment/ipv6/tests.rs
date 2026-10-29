use super::*;
use std::time::Duration;

fn fragment(id: u32, offset: u16, more: bool, payload: &[u8], extensions: bool) -> Vec<u8> {
    let mut frame = vec![0; ETH_HEADER_LEN + 40];
    frame[12..14].copy_from_slice(&[0x86, 0xdd]);
    let ip = &mut frame[ETH_HEADER_LEN..];
    ip[0] = 0x60;
    ip[6] = if extensions { 0 } else { 44 };
    ip[7] = 64;
    ip[23] = 1;
    ip[39] = 2;
    if extensions {
        frame.extend_from_slice(&[44, 0, 0, 0, 0, 0, 0, 0]);
    }
    frame.extend_from_slice(&[if extensions { 60 } else { 17 }, 0]);
    frame.extend_from_slice(&(offset | u16::from(more)).to_be_bytes());
    frame.extend_from_slice(&id.to_be_bytes());
    frame.extend_from_slice(payload);
    let length = (frame.len() - ETH_HEADER_LEN - 40) as u16;
    frame[ETH_HEADER_LEN + 4..ETH_HEADER_LEN + 6].copy_from_slice(&length.to_be_bytes());
    frame
}

fn udp() -> Vec<u8> {
    let mut data = vec![0; 32];
    data[..8].copy_from_slice(&[0x12, 0x34, 0, 53, 0, 32, 0xab, 0xcd]);
    for (index, byte) in data[8..].iter_mut().enumerate() {
        *byte = index as u8;
    }
    data
}

#[test]
fn reassembles_out_of_order_with_extensions_and_retains_transport_bytes() {
    let mut data = vec![17, 0, 0, 0, 0, 0, 0, 0];
    data.extend_from_slice(&udp());
    let first = fragment(7, 0, true, &data[..24], true);
    let last = fragment(7, 24, false, &data[24..], true);
    let mut reassembler = Ipv6Reassembler::default();
    let now = Instant::now();
    assert!(reassembler.push(&last, now).is_none());
    assert!(reassembler.push(&last, now).is_none());
    let full = reassembler.push(&first, now).unwrap();
    let ip = IpPacketView::parse(&full[ETH_HEADER_LEN..]).unwrap();
    assert_eq!(ip.transport().unwrap(), udp());
    assert_eq!(ip.protocol(), 17);
    assert!(ip.fragment().is_none());
    assert_eq!(full[ETH_HEADER_LEN + 6], 0);
    assert_eq!(full[ETH_HEADER_LEN + 40], 60);
    assert_eq!(full.len(), ETH_HEADER_LEN + 40 + 8 + data.len());
    assert!(reassembler.entries.is_empty());
}

#[test]
fn overlap_poison_survives_later_fragments_but_atomic_is_independent() {
    let data = udp();
    let now = Instant::now();
    let first = fragment(1, 0, true, &data[..16], false);
    let last = fragment(1, 16, false, &data[16..], false);
    let overlap = fragment(1, 8, true, &data[8..24], false);
    let atomic = fragment(1, 0, false, &data, false);
    let mut reassembler = Ipv6Reassembler::default();
    assert!(reassembler.push(&first, now).is_none());
    assert!(reassembler.push(&atomic, now).is_some());
    assert!(reassembler.push(&overlap, now).is_none());
    assert!(reassembler.push(&last, now).is_none());
    assert!(reassembler.push(&first, now).is_none());
    assert!(reassembler.entries.values().next().unwrap().poisoned);
    assert!(reassembler.push(&atomic, now).is_some());
    let expired = now + REASSEMBLY_TIMEOUT;
    assert!(reassembler.push(&last, expired).is_none());
    assert!(reassembler.push(&first, expired).is_some());
}

#[test]
fn rejects_missing_ranges_inconsistent_final_and_oversized_payloads() {
    let now = Instant::now();
    let data = udp();
    let mut reassembler = Ipv6Reassembler::default();
    assert!(
        reassembler
            .push(&fragment(2, 0, true, &data[..8], false), now)
            .is_none()
    );
    assert!(
        reassembler
            .push(&fragment(2, 16, false, &data[16..], false), now)
            .is_none()
    );
    assert!(
        reassembler
            .push(&fragment(2, 24, false, &data[24..28], false), now)
            .is_none()
    );
    assert!(
        reassembler
            .push(&fragment(2, 8, true, &data[8..16], false), now)
            .is_none()
    );
    assert!(reassembler.entries.values().next().unwrap().poisoned);
    let oversized = fragment(3, 65528, false, &[0; 8], false);
    assert!(reassembler.push(&oversized, now).is_none());
    assert!(reassembler.entries[&("::1".parse().unwrap(), "::2".parse().unwrap(), 3)].poisoned);
}

#[test]
fn bounds_reassembly_contexts_and_expires_missing_datagrams() {
    let now = Instant::now();
    let mut reassembler = Ipv6Reassembler::default();
    for id in 0..MAX_ENTRIES as u32 + 10 {
        reassembler.push(&fragment(id, 16, false, &[0; 8], false), now);
    }
    assert_eq!(reassembler.entries.len(), MAX_ENTRIES);
    reassembler.push(
        &fragment(99, 16, false, &[0; 8], false),
        now + REASSEMBLY_TIMEOUT + Duration::from_millis(1),
    );
    assert_eq!(reassembler.entries.len(), 1);
}

#[test]
fn rejects_nested_fragments_and_malformed_first_transport_header() {
    let now = Instant::now();
    let mut reassembler = Ipv6Reassembler::default();
    let mut nested = fragment(5, 0, false, &[17, 0, 0, 0, 0, 0, 0, 6], false);
    nested[ETH_HEADER_LEN + 40] = 44;
    assert!(reassembler.push(&nested, now).is_none());
    let mut tcp = fragment(6, 0, true, &[0; 8], false);
    tcp[ETH_HEADER_LEN + 40] = 6;
    assert!(reassembler.push(&tcp, now).is_none());
    assert!(reassembler.entries.values().next().unwrap().poisoned);
}

#[test]
fn combines_ecn_and_rejects_not_ect_mixture() {
    let now = Instant::now();
    let data = udp();
    for (first_ecn, last_ecn, expected) in [(2, 3, Some(3)), (1, 2, Some(2)), (0, 3, None)] {
        let mut first = fragment(11, 0, true, &data[..16], false);
        let mut last = fragment(11, 16, false, &data[16..], false);
        first[ETH_HEADER_LEN + 1] = first_ecn << 4;
        last[ETH_HEADER_LEN + 1] = last_ecn << 4;
        let mut reassembler = Ipv6Reassembler::default();
        assert!(reassembler.push(&first, now).is_none());
        let result = reassembler.push(&last, now);
        assert_eq!(
            result.map(|frame| (frame[ETH_HEADER_LEN + 1] >> 4) & 3),
            expected
        );
    }
}
