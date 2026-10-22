//! Bounded IPv6 reassembly with atomic-fragment isolation and overlap quarantine.

use super::{ETH_HEADER_LEN, MAX_ENTRIES, MAX_RANGES, REASSEMBLY_TIMEOUT, merge_range};
use arcbox_packet::ip::{FragmentView, IpPacketView, TcpPacketView};
use std::{collections::HashMap, net::IpAddr, time::Instant};

type Key = (IpAddr, IpAddr, u32);

struct Entry {
    first_seen: Instant,
    prefix: Vec<u8>,
    ethernet: [u8; ETH_HEADER_LEN],
    previous: usize,
    protocol: u8,
    payload: Vec<u8>,
    ranges: Vec<(usize, usize)>,
    total: Option<usize>,
    ecn: u8,
    poisoned: bool,
}

impl Entry {
    fn poison(&mut self) {
        self.poisoned = true;
        self.payload.clear();
        self.ranges.clear();
    }
}

#[derive(Default)]
pub(super) struct Ipv6Reassembler {
    entries: HashMap<Key, Entry>,
}

impl Ipv6Reassembler {
    pub(super) fn push(&mut self, frame: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.entries
            .retain(|_, entry| now.duration_since(entry.first_seen) < REASSEMBLY_TIMEOUT);
        let ip = IpPacketView::parse(frame.get(ETH_HEADER_LEN..)?)?;
        let fragment = ip.fragment()?;
        let previous = fragment.previous_next_header?;
        let ethernet = frame[..ETH_HEADER_LEN].try_into().ok()?;
        // RFC 6946: atomic fragments do not read or modify a same-ID reassembly context.
        if fragment.offset == 0 && !fragment.more {
            return rebuild(
                &ethernet,
                fragment.prefix,
                previous,
                fragment.protocol,
                fragment.payload,
            );
        }
        let key = (ip.source(), ip.destination(), fragment.id);
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_ENTRIES {
            return None;
        }
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            first_seen: now,
            prefix: fragment.prefix.to_vec(),
            ethernet,
            previous,
            protocol: fragment.protocol,
            payload: Vec::new(),
            ranges: Vec::new(),
            total: None,
            ecn: 0,
            poisoned: false,
        });
        if entry.poisoned {
            return None;
        }
        let end = fragment.offset + fragment.payload.len();
        if fragment.payload.is_empty()
            || (fragment.more && !fragment.payload.len().is_multiple_of(8))
            || fragment.prefix.len() - 40 + end > usize::from(u16::MAX)
            || entry.previous != previous
            || entry.protocol != fragment.protocol
            || !same_prefix(&entry.prefix, fragment.prefix)
            || entry
                .total
                .is_some_and(|total| end > total || (fragment.more && end >= total))
            || (!fragment.more
                && (entry.total.is_some_and(|total| total != end)
                    || entry.ranges.last().is_some_and(|range| range.1 > end)))
            || (fragment.offset == 0 && !complete_first_header(&ethernet, fragment))
        {
            entry.poison();
            return None;
        }
        entry.ecn |= 1 << ((fragment.prefix[1] >> 4) & 3);
        if entry.ecn & 1 != 0 && entry.ecn != 1 {
            entry.poison();
            return None;
        }
        if !fragment.more {
            entry.total = Some(end);
        }
        if fragment.offset == 0 {
            entry.prefix.copy_from_slice(fragment.prefix);
            entry.ethernet = ethernet;
        }
        for &(start, stop) in &entry.ranges {
            if fragment.offset < stop && start < end {
                // An exact byte-identical duplicate has no new coverage. Every other overlap poisons the ID.
                if fragment.offset == start
                    && end == stop
                    && entry.payload[start..stop] == *fragment.payload
                {
                    return None;
                }
                entry.poison();
                return None;
            }
        }
        entry.payload.resize(entry.payload.len().max(end), 0);
        entry.payload[fragment.offset..end].copy_from_slice(fragment.payload);
        merge_range(&mut entry.ranges, fragment.offset, end);
        if entry.ranges.len() > MAX_RANGES {
            entry.poison();
            return None;
        }
        if entry.total.is_none_or(|total| entry.ranges != [(0, total)]) {
            return None;
        }
        let mut entry = self.entries.remove(&key)?;
        let ecn = if entry.ecn & 8 != 0 {
            3
        } else if entry.ecn & 4 != 0 {
            2
        } else {
            u8::from(entry.ecn & 2 != 0)
        };
        entry.prefix[1] = (entry.prefix[1] & !0x30) | (ecn << 4);
        rebuild(
            &entry.ethernet,
            &entry.prefix,
            entry.previous,
            entry.protocol,
            &entry.payload,
        )
    }
}

fn same_prefix(first: &[u8], next: &[u8]) -> bool {
    first.len() == next.len()
        && first.iter().zip(next).enumerate().all(|(index, (a, b))| {
            match index {
                1 => a & !0x30 == b & !0x30,
                4 | 5 | 7 => true, // Payload length, hop limit and ECN can differ across fragments.
                _ => a == b,
            }
        })
}

fn complete_first_header(ethernet: &[u8; ETH_HEADER_LEN], fragment: FragmentView<'_>) -> bool {
    let Some(frame) = rebuild(
        ethernet,
        fragment.prefix,
        fragment.previous_next_header.unwrap(),
        fragment.protocol,
        fragment.payload,
    ) else {
        return false;
    };
    let ip = IpPacketView::parse(&frame[ETH_HEADER_LEN..]).unwrap();
    match ip.protocol() {
        6 => TcpPacketView::parse(ip).is_some(),
        17 => ip.transport().is_some_and(|payload| payload.len() >= 8),
        _ => true,
    }
}

fn rebuild(
    ethernet: &[u8; ETH_HEADER_LEN],
    prefix: &[u8],
    previous: usize,
    protocol: u8,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let length = u16::try_from(prefix.len() - 40 + payload.len()).ok()?;
    let mut frame = Vec::with_capacity(ETH_HEADER_LEN + prefix.len() + payload.len());
    frame.extend_from_slice(ethernet);
    frame.extend_from_slice(prefix);
    frame[ETH_HEADER_LEN + previous] = protocol;
    frame[ETH_HEADER_LEN + 4..ETH_HEADER_LEN + 6].copy_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    // A second Fragment header or malformed post-fragment extension cannot reach transport consumers.
    let ip = IpPacketView::parse(&frame[ETH_HEADER_LEN..])?;
    if ip.fragment().is_some() {
        return None;
    }
    Some(frame)
}

#[cfg(test)]
mod tests;
