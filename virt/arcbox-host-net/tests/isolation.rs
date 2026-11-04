//! Two VMs' datapaths cannot reach each other (ADR 0011, D1).
//!
//! Each test plays two guests on the VMM ends of the sockets `HostNetwork`
//! hands its driver, with real datapaths behind them. A guest's own
//! datapath answering it is the positive control: it is what makes
//! "nothing arrived at the other guest" mean isolation rather than a dead
//! link. A shared datapath or bridge fails these in one of two ways — the
//! other guest sees the frame, or the answer goes to whichever guest
//! attached last.
//!
//! The forwards also pin the rule's guest port: a host sandbox has no
//! in-guest DNAT, so a forward must arrive on that port and not on the one
//! its host listener bound.

#![cfg(target_os = "macos")]

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcbox_host_net::{DatapathPlacement, DatapathServices, HostNetwork, NoDns, Pool};
use arcbox_net::dhcp::DhcpPacket;
use arcbox_vm_driver::net::{
    AttachMode, GuestNetwork, NetworkIdentity, NetworkMode, NetworkPolicy,
};
use arcbox_vm_driver::{ForwardProtocol, ForwardRule, MacAddr, NicAttachment, VmId};

const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;

/// One guest: the VMM end of its NIC, and what its network says it is.
struct Guest {
    vm: VmId,
    link: UnixDatagram,
    mac: MacAddr,
    identity: NetworkIdentity,
}

impl Guest {
    fn send(&self, frame: &[u8]) {
        self.link.send(frame).unwrap();
    }

    /// The first frame within `timeout` that `matches` accepts; frames it
    /// rejects (ARP probes, router chatter) are skipped.
    fn recv_matching(&self, timeout: Duration, matches: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 65536];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            self.link
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .unwrap();
            match self.link.recv(&mut buf) {
                Ok(n) if matches(&buf[..n]) => return Some(buf[..n].to_vec()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(e) => panic!("{}: recv: {e}", self.vm),
            }
        }
        None
    }

    /// Everything that arrives within `timeout`.
    fn drain(&self, timeout: Duration) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.recv_matching(left, |_| true) {
                Some(frame) => frames.push(frame),
                None => break,
            }
        }
        frames
    }
}

async fn two_guests() -> (HostNetwork, Guest, Guest) {
    let net = HostNetwork::new(
        Pool::new(Ipv4Addr::new(10, 200, 0, 0), 16).unwrap(),
        DatapathPlacement::Network,
        DatapathServices::default(),
        Arc::new(NoDns),
    );
    net.reconcile().unwrap().replay_complete();
    let mut guests = Vec::new();
    for name in ["a", "b"] {
        let vm = VmId::new(name).unwrap();
        let lease = net
            .reserve(
                &vm,
                NetworkPolicy {
                    mode: NetworkMode::Nat,
                },
            )
            .await
            .unwrap();
        let nic = net.activate(&lease, AttachMode::Invariant).await.unwrap();
        let NicAttachment::FileHandle { fd } = nic.attachment else {
            panic!("expected a file handle, got {:?}", nic.attachment);
        };
        // What a VMM does with the lent descriptor: keep its own copy.
        // SAFETY: the network keeps `fd` open for the lease's life; the
        // borrow ends at the end of this statement.
        let owned = unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .unwrap();
        // SAFETY: `owned` is a fresh descriptor of a unix datagram socket,
        // handed over whole.
        let link = unsafe { UnixDatagram::from_raw_fd(owned.into_raw_fd()) };
        link.set_nonblocking(false).unwrap();
        guests.push(Guest {
            identity: net.identity(&lease, AttachMode::Invariant),
            mac: lease.mac,
            vm,
            link,
        });
    }
    let b = guests.pop().unwrap();
    let a = guests.pop().unwrap();
    (net, a, b)
}

/// A broadcast DHCPDISCOVER from `mac`.
fn dhcp_discover(mac: MacAddr, xid: u32) -> Vec<u8> {
    let mut bootp = vec![0u8; 240];
    bootp[0] = 1; // BOOTREQUEST
    bootp[1] = 1; // Ethernet
    bootp[2] = 6;
    bootp[4..8].copy_from_slice(&xid.to_be_bytes());
    bootp[10] = 0x80; // broadcast flag
    bootp[28..34].copy_from_slice(&mac.octets());
    bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bootp.extend_from_slice(&[53, 1, 1, 255]); // DHCPDISCOVER, end
    udp_frame(
        mac.octets(),
        [0xff; 6],
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        DHCP_CLIENT_PORT,
        DHCP_SERVER_PORT,
        &bootp,
    )
}

/// An Ethernet/IPv4/UDP frame; the UDP checksum is left 0 (none).
fn udp_frame(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let ip_len = 20 + udp_len;
    let mut f = Vec::with_capacity(14 + ip_len);
    f.extend_from_slice(&dst_mac);
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&0x0800u16.to_be_bytes());
    let ip_start = f.len();
    f.extend_from_slice(&[0x45, 0]);
    f.extend_from_slice(&(ip_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0]);
    f.extend_from_slice(&src.octets());
    f.extend_from_slice(&dst.octets());
    let sum = checksum(&f[ip_start..ip_start + 20]);
    f[ip_start + 10..ip_start + 12].copy_from_slice(&sum.to_be_bytes());
    f.extend_from_slice(&src_port.to_be_bytes());
    f.extend_from_slice(&dst_port.to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(payload);
    f
}

fn checksum(header: &[u8]) -> u16 {
    let mut sum: u32 = header
        .chunks(2)
        .map(|w| u32::from(u16::from_be_bytes([w[0], w[1]])))
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The UDP datagram in `frame`, as (source, destination, dst port, payload).
fn parse_udp(frame: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u16, u16, &[u8])> {
    if frame.len() < 14 + 20 + 8 || frame[12..14] != [0x08, 0x00] || frame[14 + 9] != 17 {
        return None;
    }
    let ip = &frame[14..];
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    let src = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let dst = Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]);
    let udp = ip.get(ihl..)?;
    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
    let len = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    Some((src, dst, src_port, dst_port, udp.get(8..len)?))
}

/// The DHCP reply to `xid` in `frame`, if it is one.
fn dhcp_reply(frame: &[u8], xid: u32) -> Option<DhcpPacket> {
    let (_, _, src_port, dst_port, payload) = parse_udp(frame)?;
    if (src_port, dst_port) != (DHCP_SERVER_PORT, DHCP_CLIENT_PORT) {
        return None;
    }
    DhcpPacket::parse(payload).ok().filter(|p| p.xid == xid)
}

fn v4(ip: IpAddr) -> Ipv4Addr {
    match ip {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(v6) => panic!("expected IPv4, got {v6}"),
    }
}

/// A UDP port nothing on loopback holds right now.
fn free_udp_port() -> u16 {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const ANSWER: Duration = Duration::from_secs(5);
/// A guest port no free host port the tests bind can equal: it is below
/// the OS's ephemeral range.
const GUEST_PORT: u16 = 7777;
const QUIET: Duration = Duration::from_millis(500);

/// Broadcasts stay on the sender's link: each guest's DHCP discover is
/// answered by its own datapath, with the address the network told that
/// guest, and the other guest sees neither the broadcast nor the answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_broadcast_is_answered_on_its_own_link_only() {
    let (_net, a, b) = two_guests().await;
    for (me, other, xid) in [(&a, &b, 0x0a0a_0001), (&b, &a, 0x0b0b_0001)] {
        let (me_link, other_link) = (me, other);
        let offer = tokio::task::block_in_place(|| {
            me_link.send(&dhcp_discover(me_link.mac, xid));
            me_link.recv_matching(ANSWER, |f| dhcp_reply(f, xid).is_some())
        })
        .unwrap_or_else(|| panic!("{}: no DHCP offer from its own datapath", me.vm));
        let offer = dhcp_reply(&offer, xid).unwrap();
        // The datapath serves exactly the link the guest is told about.
        assert_eq!(IpAddr::V4(offer.yiaddr), me.identity.ip, "{}", me.vm);
        assert_eq!(offer.chaddr[..6], me.mac.octets(), "{}", me.vm);

        let leaked = tokio::task::block_in_place(|| other_link.drain(QUIET));
        assert!(
            leaked.is_empty(),
            "{} saw {} frame(s) of {}'s exchange",
            other.vm,
            leaked.len(),
            me.vm
        );
    }
}

/// A port forwarded into one guest delivers into that guest and no other,
/// whichever of the two attached last.
#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_port_reaches_only_its_own_guest() {
    let (net, a, b) = two_guests().await;
    // Each datapath learns its guest's MAC from the guest's first frame,
    // the way a booting guest's DHCP exchange teaches it.
    for (guest, xid) in [(&a, 1), (&b, 2)] {
        tokio::task::block_in_place(|| {
            guest.send(&dhcp_discover(guest.mac, xid));
            guest
                .recv_matching(ANSWER, |f| dhcp_reply(f, xid).is_some())
                .expect("dhcp offer");
        });
    }

    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    for (me, other) in [(&a, &b), (&b, &a)] {
        let mut forward = net.take_port_forward(&me.vm).unwrap();
        let port = free_udp_port();
        let rule = ForwardRule {
            protocol: ForwardProtocol::Udp,
            host_ip: Ipv4Addr::LOCALHOST,
            host_port: port,
            guest_port: GUEST_PORT,
        };
        assert_eq!(forward.add(rule).await.unwrap(), port);
        let payload = format!("for {}", me.vm);
        sender
            .send_to(payload.as_bytes(), (Ipv4Addr::LOCALHOST, port))
            .unwrap();

        // Delivered on the rule's guest port, not the host listener's.
        let delivered = tokio::task::block_in_place(|| {
            me.recv_matching(ANSWER, |f| {
                parse_udp(f).is_some_and(|(_, _, _, dst_port, body)| {
                    dst_port == GUEST_PORT && body == payload.as_bytes()
                })
            })
        })
        .unwrap_or_else(|| panic!("{}: its forwarded datagram never arrived", me.vm));
        let (_, dst, _, _, _) = parse_udp(&delivered).unwrap();
        assert_eq!(dst, v4(me.identity.ip), "{}", me.vm);

        let leaked = tokio::task::block_in_place(|| other.drain(QUIET));
        assert!(
            leaked.is_empty(),
            "{} saw {} frame(s) forwarded to {}",
            other.vm,
            leaked.len(),
            me.vm
        );
        forward.clear().await;
    }
}

/// A TCP forward whose host port is 0 opens its connection to the rule's
/// guest port: the guest sees a SYN on that port, not on the port the host
/// listener bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_tcp_forward_connects_to_the_rules_guest_port() {
    let (net, a, _b) = two_guests().await;
    tokio::task::block_in_place(|| {
        a.send(&dhcp_discover(a.mac, 3));
        a.recv_matching(ANSWER, |f| dhcp_reply(f, 3).is_some())
            .expect("dhcp offer");
    });

    let mut forward = net.take_port_forward(&a.vm).unwrap();
    let host_port = forward
        .add(ForwardRule {
            protocol: ForwardProtocol::Tcp,
            host_ip: Ipv4Addr::LOCALHOST,
            host_port: 0,
            guest_port: GUEST_PORT,
        })
        .await
        .unwrap();
    assert_ne!(host_port, 0, "add must report the port it bound");
    assert_ne!(host_port, GUEST_PORT);

    let _client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, host_port))
        .await
        .unwrap();
    let syn = tokio::task::block_in_place(|| a.recv_matching(ANSWER, |f| parse_syn(f).is_some()))
        .expect("the guest never saw the forwarded connection's SYN");
    let (dst, dst_port) = parse_syn(&syn).unwrap();
    assert_eq!(dst, v4(a.identity.ip));
    assert_eq!(dst_port, GUEST_PORT, "SYN went to the host listener's port");
    forward.clear().await;
}

/// The destination of the TCP SYN (without ACK) in `frame`, if it is one.
fn parse_syn(frame: &[u8]) -> Option<(Ipv4Addr, u16)> {
    if frame.len() < 14 + 20 + 20 || frame[12..14] != [0x08, 0x00] || frame[14 + 9] != 6 {
        return None;
    }
    let ip = &frame[14..];
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    let tcp = ip.get(ihl..ihl + 20)?;
    let flags = tcp[13];
    (flags & 0x12 == 0x02).then(|| {
        (
            Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]),
            u16::from_be_bytes([tcp[2], tcp[3]]),
        )
    })
}
