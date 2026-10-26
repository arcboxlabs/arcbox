use super::*;

fn ipv6(next: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = vec![0; 40];
    packet[0] = 0x60;
    packet[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    packet[6] = next;
    packet[23] = 1;
    packet[39] = 2;
    packet.extend_from_slice(payload);
    packet
}

#[test]
fn ipv4_length_options_addresses_and_fragments() {
    let mut packet = vec![0; 32];
    packet[0] = 0x46;
    packet[3] = 28;
    packet[4..8].copy_from_slice(&[0x12, 0x34, 0, 0]);
    packet[9] = 17;
    packet[12..20].copy_from_slice(&[192, 0, 2, 1, 198, 51, 100, 2]);
    packet[24..28].copy_from_slice(b"data");
    let view = IpPacketView::parse(&packet).unwrap();
    assert_eq!(view.source(), "192.0.2.1".parse::<IpAddr>().unwrap());
    assert_eq!(
        view.destination(),
        "198.51.100.2".parse::<IpAddr>().unwrap()
    );
    assert_eq!(view.packet(), &packet[..28]);
    assert_eq!(view.protocol(), 17);
    assert_eq!(view.transport(), Some(&b"data"[..]));
    for (flags, offset, more) in [(0x2000u16, 0, true), (3, 24, false)] {
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        let view = IpPacketView::parse(&packet).unwrap();
        let fragment = view.fragment().unwrap();
        assert!(view.transport().is_none());
        assert_eq!(
            (fragment.id, fragment.offset, fragment.more),
            (0x1234, offset, more)
        );
        assert_eq!(fragment.protocol, 17);
        assert_eq!(fragment.prefix, &packet[..24]);
        assert_eq!(fragment.payload, b"data");
        assert_eq!(fragment.previous_next_header, None);
    }
    for length in 0..28 {
        assert!(IpPacketView::parse(&packet[..length]).is_none());
    }
    packet[3] = 23;
    assert!(IpPacketView::parse(&packet).is_none());
}

#[test]
fn ipv6_length_addresses_and_extension_chain() {
    let mut packet = ipv6(0, &[60, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 42]);
    packet.extend_from_slice(&[0xee; 12]);
    let view = IpPacketView::parse(&packet).unwrap();
    assert_eq!(view.source(), "::1".parse::<IpAddr>().unwrap());
    assert_eq!(view.destination(), "::2".parse::<IpAddr>().unwrap());
    assert_eq!(view.packet(), &packet[..57]);
    assert_eq!(view.protocol(), 17);
    assert_eq!(view.transport(), Some(&[42][..]));
    for length in 0..57 {
        assert!(IpPacketView::parse(&packet[..length]).is_none());
    }
    for next in [0, 43, 44, 51, 60] {
        for length in 0..8 {
            assert!(IpPacketView::parse(&ipv6(next, &vec![0; length])).is_none());
        }
    }
    assert!(IpPacketView::parse(&[0x70; 40]).is_none());
}

#[test]
fn ipv6_fragments_preserve_reassembly_prefix_and_atomic_packets() {
    for extended in [false, true] {
        for (flags, offset, more) in [(1u16, 0, true), (24, 24, false), (0, 0, false)] {
            let mut payload = if extended {
                vec![44, 0, 0, 0, 0, 0, 0, 0]
            } else {
                vec![]
            };
            payload.extend_from_slice(&[17, 0]);
            payload.extend_from_slice(&flags.to_be_bytes());
            payload.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 42]);
            let packet = ipv6(if extended { 0 } else { 44 }, &payload);
            let view = IpPacketView::parse(&packet).unwrap();
            assert!(
                view.transport().is_none(),
                "atomic fragments also require reassembly"
            );
            let fragment = view.fragment().unwrap();
            assert_eq!(
                (fragment.id, fragment.offset, fragment.more),
                (0x12345678, offset, more)
            );
            assert_eq!(fragment.protocol, 17);
            assert_eq!(fragment.payload, &[42]);
            assert_eq!(fragment.prefix, &packet[..if extended { 48 } else { 40 }]);
            assert_eq!(
                fragment.previous_next_header,
                Some(if extended { 40 } else { 6 })
            );
        }
    }
}

#[test]
fn ipv6_options_obey_lengths_and_discard_actions() {
    for (extension, accepted) in [
        ([17, 0, 0x22, 2, 1, 2, 0, 0], true),
        ([17, 0, 1, 7, 0, 0, 0, 0], false),
        ([17, 0, 0, 0, 0, 0, 0, 2], false),
        ([17, 0, 0x40, 2, 0, 0, 0, 0], false),
        ([17, 0, 0x80, 2, 0, 0, 0, 0], false),
        ([17, 0, 0xc2, 4, 0, 1, 0, 0], false),
    ] {
        for next in [0, 60] {
            assert_eq!(
                IpPacketView::parse(&ipv6(next, &extension)).is_some(),
                accepted
            );
        }
    }
}

#[test]
fn ipv6_rejects_late_hop_by_hop_active_routing_and_short_auth() {
    for next in [0, 60, 44] {
        let payload = [0, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0];
        assert!(IpPacketView::parse(&ipv6(next, &payload)).is_none());
    }
    for segments in [0, 1, 255] {
        let payload = [17, 0, 255, segments, 0, 0, 0, 0];
        assert_eq!(
            IpPacketView::parse(&ipv6(43, &payload)).is_some(),
            segments == 0
        );
    }
    for length in [0, 1, 255] {
        let mut payload = [0; 12];
        payload[0] = 17;
        payload[1] = length;
        assert_eq!(
            IpPacketView::parse(&ipv6(51, &payload)).is_some(),
            length == 1
        );
    }
}
