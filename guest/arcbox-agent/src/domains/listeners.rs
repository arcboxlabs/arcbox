//! The TCP ports a container listens on.
//!
//! `/proc/<pid>/net/tcp` and `tcp6` list the sockets of the network
//! namespace `pid` lives in, so reading them through a container's init
//! process sees the container's listeners without entering its namespace.

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// `st` column value of a listening socket.
const TCP_LISTEN: &str = "0A";

/// Socket identities detect a replacement listener even when its port is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Snapshot(BTreeSet<(u16, u64)>);

impl Snapshot {
    pub(super) fn ports(&self) -> BTreeSet<u16> {
        self.0.iter().map(|(port, _)| *port).collect()
    }
}

/// The ports listened on at an address the container's IPv4 address
/// reaches, read through its init process `pid`.
///
/// Fails unless `pid` is in the container's network namespace `netns` (its
/// `SandboxKey`) both before and after the read: an init process that
/// switched namespaces, as `nsenter -t 1 -n` does, would otherwise show
/// another namespace's sockets as the container's.
pub(super) async fn read(pid: u32, netns: &Path) -> io::Result<Snapshot> {
    let proc = format!("/proc/{pid}");
    in_namespace(&proc, netns).await?;
    let tcp = tokio::fs::read_to_string(format!("{proc}/net/tcp")).await?;
    let tcp6 = tokio::fs::read_to_string(format!("{proc}/net/tcp6")).await?;
    in_namespace(&proc, netns).await?;
    Ok(reachable(&tcp, &tcp6))
}

async fn in_namespace(proc: &str, netns: &Path) -> io::Result<()> {
    let current = tokio::fs::metadata(format!("{proc}/ns/net")).await?;
    let container = tokio::fs::metadata(netns).await?;
    if (current.dev(), current.ino()) == (container.dev(), container.ino()) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{proc} has left the network namespace {}",
            netns.display()
        )))
    }
}

/// The ports the two tables list as listening on an address reachable
/// through the container's IPv4 address.
///
/// Loopback listeners are out: nothing outside the container reaches them.
/// So are IPv6 listeners bound to a specific IPv6 address, which IPv4
/// traffic never arrives at; `::` counts, since Linux binds it dual-stack
/// unless the socket opts out.
fn reachable(tcp: &str, tcp6: &str) -> Snapshot {
    let v4 = listening(tcp).filter_map(|(addr, port, inode)| {
        let ip = parse_v4(addr)?;
        (!ip.is_loopback()).then_some((port, inode))
    });
    let v6 = listening(tcp6).filter_map(|(addr, port, inode)| {
        let ip = parse_v6(addr)?;
        let reachable =
            ip.is_unspecified() || ip.to_ipv4_mapped().is_some_and(|v4| !v4.is_loopback());
        reachable.then_some((port, inode))
    });
    Snapshot(v4.chain(v6).collect())
}

/// `(hex address, port, inode)` of every listening socket in a `/proc/net/tcp*`
/// table.
fn listening(table: &str) -> impl Iterator<Item = (&str, u16, u64)> {
    table.lines().skip(1).filter_map(|line| {
        let mut fields = line.split_whitespace();
        let local = fields.nth(1)?;
        let state = fields.nth(1)?;
        if state != TCP_LISTEN {
            return None;
        }
        let (addr, port) = local.split_once(':')?;
        let inode = fields.nth(5)?.parse().ok()?;
        Some((addr, u16::from_str_radix(port, 16).ok()?, inode))
    })
}

/// The kernel prints each 32-bit word of an address as the number its
/// bytes form in memory, so the bytes come back in native order.
fn parse_word(hex: &str) -> Option<[u8; 4]> {
    if hex.len() != 8 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes)
}

fn parse_v4(hex: &str) -> Option<Ipv4Addr> {
    parse_word(hex).map(Ipv4Addr::from)
}

fn parse_v6(hex: &str) -> Option<Ipv6Addr> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, word) in bytes.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *word = parse_word(hex.get(i * 8..i * 8 + 8)?)?;
    }
    Some(Ipv6Addr::from(bytes))
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";

    fn table(rows: &[(&str, &str)]) -> String {
        let mut text = format!("{HEADER}\n");
        for (i, (local, state)) in rows.iter().enumerate() {
            writeln!(
                text,
                "   {i}: {local} 00000000:0000 {state} 00000000:00000000 00:00000000 00000000     0        0 1000{i} 1"
            )
            .unwrap();
        }
        text
    }

    #[test]
    fn only_listeners_reachable_over_ipv4_count() {
        let tcp = table(&[
            ("00000000:0BB8", "0A"), // 0.0.0.0:3000
            ("0100007F:1F90", "0A"), // 127.0.0.1:8080, loopback
            ("020011AC:0FA0", "0A"), // 172.17.0.2:4000
            ("020011AC:1770", "01"), // 172.17.0.2:6000, established
        ]);
        let tcp6 = table(&[
            ("00000000000000000000000000000000:1388", "0A"), // [::]:5000
            ("00000000000000000000000001000000:1B58", "0A"), // [::1]:7000
            ("0000000000000000FFFF00000100007F:2328", "0A"), // [::ffff:127.0.0.1]:9000
            ("0000000000000000FFFF0000020011AC:2710", "0A"), // [::ffff:172.17.0.2]:10000
            ("000080FE000000000000000001000000:2AF8", "0A"), // [fe80::1]:11000
        ]);
        assert_eq!(
            reachable(&tcp, &tcp6).ports(),
            BTreeSet::from([3000, 4000, 5000, 10000])
        );
    }

    #[test]
    fn nothing_listening_and_garbage_yield_nothing() {
        assert!(reachable(&table(&[]), &table(&[])).ports().is_empty());
        let garbage = format!("{HEADER}\n   0: nonsense\n   1: 00000000:ZZZZ 00000000:0000 0A\n");
        assert!(reachable(&garbage, &garbage).ports().is_empty());
    }

    #[test]
    fn rebinding_the_same_port_changes_the_snapshot() {
        let old = table(&[("00000000:0BB8", "0A")]);
        let new = old.replace("10000", "20000");
        let old = reachable(&old, &table(&[]));
        let new = reachable(&new, &table(&[]));
        assert_eq!(old.ports(), new.ports());
        assert_ne!(old, new);
    }
}
