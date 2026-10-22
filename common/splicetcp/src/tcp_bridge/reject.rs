//! Validated reset responses for rejected SYN frames.

use crate::ethernet::{ETH_HEADER_LEN, EthernetHeader, TcpFrameParams, build_tcp_rst_frame};

/// Swaps endpoints and acknowledges the SYN plus its declared payload.
pub(super) fn build_rst_from_syn(frame: &[u8], gateway_mac: [u8; 6]) -> Option<Vec<u8>> {
    let ethernet = EthernetHeader::parse(frame)?;
    let tcp = super::packet::tcp_at(frame, ETH_HEADER_LEN)?;
    let ip = tcp.ip();
    // Ordinary IP lengths bound the payload below u32::MAX; Ethernet padding is excluded.
    let ack = tcp
        .sequence()
        .wrapping_add(1)
        .wrapping_add(u32::try_from(tcp.payload().len()).ok()?);
    Some(build_tcp_rst_frame(&TcpFrameParams {
        src_ip: ip.destination(),
        dst_ip: ip.source(),
        src_port: tcp.destination_port(),
        dst_port: tcp.source_port(),
        seq: 0,
        ack,
        window: 0,
        src_mac: gateway_mac,
        dst_mac: ethernet.src_mac,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_packet::checksum::{checksum_add_scalar, checksum_fold};
    use arcbox_packet::ip::{IpPacketView, TcpPacketView};

    const GATEWAY: [u8; 6] = [2, 0, 0, 0, 0, 1];
    const GUEST: [u8; 6] = [2, 0, 0, 0, 0, 2];

    fn syn(ipv6: bool) -> Vec<u8> {
        let ip_len = if ipv6 { 40 } else { 20 };
        let mut frame = vec![0; 14 + ip_len + 23];
        frame[..6].copy_from_slice(&GATEWAY);
        frame[6..12].copy_from_slice(&GUEST);
        let ip = &mut frame[14..];
        if ipv6 {
            ip[0] = 0x60;
            ip[4..6].copy_from_slice(&23u16.to_be_bytes());
            ip[6] = 6;
            ip[8..24].copy_from_slice(
                &"2001:db8::1"
                    .parse::<std::net::Ipv6Addr>()
                    .unwrap()
                    .octets(),
            );
            ip[24..40].copy_from_slice(
                &"2001:db8::2"
                    .parse::<std::net::Ipv6Addr>()
                    .unwrap()
                    .octets(),
            );
        } else {
            ip[0] = 0x45;
            ip[2..4].copy_from_slice(&43u16.to_be_bytes());
            ip[9] = 6;
            ip[12..16].copy_from_slice(&[192, 0, 2, 1]);
            ip[16..20].copy_from_slice(&[192, 0, 2, 2]);
        }
        let tcp = &mut ip[ip_len..];
        tcp[..4].copy_from_slice(&[0x9c, 0x40, 0x01, 0xbb]);
        tcp[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
        tcp[12] = 0x50;
        tcp[13] = 0x02;
        tcp[20..].copy_from_slice(b"syn");
        frame[12..14].copy_from_slice(if ipv6 { &[0x86, 0xdd] } else { &[8, 0] });
        frame
    }

    #[test]
    fn rejected_syn_resets_both_families_and_acknowledges_payload() {
        for ipv6 in [false, true] {
            let mut input = syn(ipv6);
            let original = IpPacketView::parse(&input[14..]).unwrap();
            let (source, destination) = (original.source(), original.destination());
            input.extend_from_slice(&[0xff; 16]);
            let output = build_rst_from_syn(&input, GATEWAY).unwrap();
            assert_eq!(&output[..6], &GUEST);
            assert_eq!(&output[6..12], &GATEWAY);
            assert_eq!(&output[12..14], &input[12..14]);
            let ip = IpPacketView::parse(&output[14..]).unwrap();
            assert_eq!((ip.source(), ip.destination()), (destination, source));
            let tcp = TcpPacketView::parse(ip).unwrap();
            assert_eq!((tcp.source_port(), tcp.destination_port()), (443, 40000));
            assert_eq!(
                (
                    tcp.sequence(),
                    tcp.acknowledgment(),
                    tcp.flags(),
                    tcp.window()
                ),
                (0, 3, 0x14, 0)
            );
            assert_eq!(tcp.segment().len(), 20);
            let mut pseudo = if ipv6 {
                output[22..54].to_vec()
            } else {
                output[26..34].to_vec()
            };
            if ipv6 {
                pseudo.extend_from_slice(&20u32.to_be_bytes());
                pseudo.extend_from_slice(&[0, 0, 0, 6]);
            } else {
                assert_eq!(checksum_fold(checksum_add_scalar(&output[14..34])), 0);
                pseudo.extend_from_slice(&[0, 6, 0, 20]);
            }
            pseudo.extend_from_slice(tcp.segment());
            assert_eq!(checksum_fold(checksum_add_scalar(&pseudo)), 0);
        }
    }

    #[test]
    fn rejection_parses_ipv6_extensions_but_requires_complete_transport() {
        let mut input = syn(true);
        input.splice(54..54, [6, 0, 0, 0, 0, 0, 0, 0]);
        input[18..20].copy_from_slice(&31u16.to_be_bytes());
        input[20] = 0;
        assert!(build_rst_from_syn(&input, GATEWAY).is_some());
        // Replace Hop-by-Hop with a real first fragment; no transport parsing is permitted yet.
        input[20] = 44;
        input[57] = 1;
        input.push(0);
        input[18..20].copy_from_slice(&32u16.to_be_bytes());
        assert!(build_rst_from_syn(&input, GATEWAY).is_none());
        for ipv6 in [false, true] {
            let input = syn(ipv6);
            for end in 0..input.len() {
                assert!(build_rst_from_syn(&input[..end], GATEWAY).is_none());
            }
            let mut wrong_family = input.clone();
            wrong_family[12..14].copy_from_slice(if ipv6 { &[8, 0] } else { &[0x86, 0xdd] });
            assert!(build_rst_from_syn(&wrong_family, GATEWAY).is_none());
            let mut malformed_tcp = input;
            let tcp_start = if ipv6 { 54 } else { 34 };
            malformed_tcp[tcp_start + 12] = 0x40;
            assert!(build_rst_from_syn(&malformed_tcp, GATEWAY).is_none());
        }
    }
}
