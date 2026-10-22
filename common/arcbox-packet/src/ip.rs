//! Validated borrowed IP and TCP views shared by native and Ethernet datapaths.
//!
//! Header validity does not imply checksum validity: VM packets can carry offload checksums.

use std::net::IpAddr;

use etherparse::{
    Ipv4HeaderSlice, Ipv6FragmentHeaderSlice, Ipv6Header, Ipv6HeaderSlice, TcpSlice, ip_number,
};

/// A fragment and the IP prefix needed to reconstruct its complete datagram.
#[derive(Clone, Copy, Debug)]
pub struct FragmentView<'a> {
    /// IPv4 identification or IPv6 Fragment identification.
    pub id: u32,
    /// Offset in bytes within the fragmentable payload.
    pub offset: usize,
    /// Whether another fragment follows this fragment.
    pub more: bool,
    /// Protocol after the IPv4 header or IPv6 Fragment header.
    pub protocol: u8,
    /// Bytes before the fragmentable payload, excluding an IPv6 Fragment header.
    pub prefix: &'a [u8],
    /// Fragmentable payload carried by this packet.
    pub payload: &'a [u8],
    /// IPv6 prefix byte that must bypass the removed Fragment header.
    pub previous_next_header: Option<usize>,
}

/// A structurally valid IP packet trimmed to its declared length.
#[derive(Clone, Copy, Debug)]
pub struct IpPacketView<'a> {
    bytes: &'a [u8],
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    payload: &'a [u8],
    fragment: Option<FragmentView<'a>>,
}

impl<'a> IpPacketView<'a> {
    /// Parses ordinary IPv4/IPv6 packets; jumbograms and malformed extensions are rejected.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        match bytes.first()? >> 4 {
            4 => {
                let header = Ipv4HeaderSlice::from_slice(bytes).ok()?;
                let total = usize::from(header.total_len());
                if total < header.slice().len() {
                    return None;
                }
                let bytes = bytes.get(..total)?;
                let payload = &bytes[header.slice().len()..];
                let protocol = header.protocol().0;
                let fragment = header.is_fragmenting_payload().then(|| FragmentView {
                    id: u32::from(header.identification()),
                    offset: usize::from(header.fragments_offset().byte_offset()),
                    more: header.more_fragments(),
                    protocol,
                    prefix: header.slice(),
                    payload,
                    previous_next_header: None,
                });
                Some(Self {
                    bytes,
                    source: header.source_addr().into(),
                    destination: header.destination_addr().into(),
                    protocol,
                    payload,
                    fragment,
                })
            }
            6 => {
                let header = Ipv6HeaderSlice::from_slice(bytes).ok()?;
                let bytes = bytes.get(..40 + usize::from(header.payload_length()))?;
                let mut next = header.next_header();
                let mut offset = 40;
                let mut previous = 6;
                let mut fragment = None;
                while Ipv6Header::is_skippable_header_extension(next) {
                    if next == ip_number::IPV6_FRAG {
                        let part = Ipv6FragmentHeaderSlice::from_slice(&bytes[offset..]).ok()?;
                        if part.next_header() == ip_number::IPV6_HOP_BY_HOP {
                            return None;
                        }
                        fragment = Some(FragmentView {
                            id: part.identification(),
                            offset: usize::from(part.fragment_offset().byte_offset()),
                            more: part.more_fragments(),
                            protocol: part.next_header().0,
                            prefix: &bytes[..offset],
                            payload: &bytes[offset + 8..],
                            previous_next_header: Some(previous),
                        });
                        next = part.next_header();
                        offset += 8;
                        break;
                    }
                    if next == ip_number::IPV6_HOP_BY_HOP && offset != 40 {
                        return None;
                    }
                    let rest = &bytes[offset..];
                    if next == ip_number::AUTH && rest.get(1) == Some(&0) {
                        return None;
                    }
                    // Active source routing changes the checksum destination; this endpoint does not route those headers.
                    if next == ip_number::IPV6_ROUTE
                        && rest.get(3).is_some_and(|segments| *segments != 0)
                    {
                        return None;
                    }
                    let (following, remaining) =
                        Ipv6Header::skip_header_extension_in_slice(rest, next).ok()?;
                    let extension_len = rest.len() - remaining.len();
                    if matches!(
                        next,
                        ip_number::IPV6_HOP_BY_HOP | ip_number::IPV6_DEST_OPTIONS
                    ) && !valid_options(&rest[..extension_len])
                    {
                        return None;
                    }
                    previous = offset;
                    offset += extension_len;
                    next = following;
                }
                Some(Self {
                    bytes,
                    source: header.source_addr().into(),
                    destination: header.destination_addr().into(),
                    protocol: next.0,
                    payload: &bytes[offset..],
                    fragment,
                })
            }
            _ => None,
        }
    }

    /// Returns the complete on-wire source address.
    pub fn source(&self) -> IpAddr {
        self.source
    }
    /// Returns the complete on-wire destination address.
    pub fn destination(&self) -> IpAddr {
        self.destination
    }
    /// Returns bytes through the declared IP end, without Ethernet padding.
    pub fn packet(&self) -> &'a [u8] {
        self.bytes
    }
    /// Returns the protocol after the parsed extension headers.
    pub fn protocol(&self) -> u8 {
        self.protocol
    }
    /// Returns a complete transport segment only when no Fragment header remains.
    pub fn transport(&self) -> Option<&'a [u8]> {
        self.fragment.is_none().then_some(self.payload)
    }
    /// Returns fragment metadata before transport parsing.
    pub fn fragment(&self) -> Option<FragmentView<'a>> {
        self.fragment
    }
}

// RFC 8200: unknown options with nonzero action bits require discarding the packet.
// Jumbo Payload also has these bits; ordinary packet views never accept jumbograms.
fn valid_options(extension: &[u8]) -> bool {
    let mut offset = 2;
    while offset < extension.len() {
        let kind = extension[offset];
        if kind == 0 {
            offset += 1; // Pad1 has no length byte.
            continue;
        }
        let Some(length) = extension.get(offset + 1) else {
            return false;
        };
        if kind & 0xc0 != 0 {
            return false;
        }
        offset += 2 + usize::from(*length);
        if offset > extension.len() {
            return false;
        }
    }
    true
}

/// A validated complete TCP segment and its IP addresses.
#[derive(Clone, Debug)]
pub struct TcpPacketView<'a> {
    ip: IpPacketView<'a>,
    tcp: TcpSlice<'a>,
}

impl<'a> TcpPacketView<'a> {
    /// Parses TCP only after fragment reassembly and IP length validation.
    pub fn parse(ip: IpPacketView<'a>) -> Option<Self> {
        if ip.protocol() != 6 {
            return None;
        }
        Some(Self {
            tcp: TcpSlice::from_slice(ip.transport()?).ok()?,
            ip,
        })
    }
    /// Returns the validated IP view.
    pub fn ip(&self) -> IpPacketView<'a> {
        self.ip
    }
    /// Returns the source port.
    pub fn source_port(&self) -> u16 {
        self.tcp.source_port()
    }
    /// Returns the destination port.
    pub fn destination_port(&self) -> u16 {
        self.tcp.destination_port()
    }
    /// Returns the sequence number.
    pub fn sequence(&self) -> u32 {
        self.tcp.sequence_number()
    }
    /// Returns the acknowledgment number.
    pub fn acknowledgment(&self) -> u32 {
        self.tcp.acknowledgment_number()
    }
    /// Returns the eight TCP control flags.
    pub fn flags(&self) -> u8 {
        self.tcp.slice()[13]
    }
    /// Returns the unscaled receive window.
    pub fn window(&self) -> u16 {
        self.tcp.window_size()
    }
    /// Returns the complete TCP segment, including options and payload.
    pub fn segment(&self) -> &'a [u8] {
        self.tcp.slice()
    }
    /// Returns the TCP payload after options.
    pub fn payload(&self) -> &'a [u8] {
        self.tcp.payload()
    }
}

#[cfg(test)]
mod tcp_tests;
#[cfg(test)]
mod tests;
