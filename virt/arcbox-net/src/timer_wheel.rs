//! Unified timer wheel for network flow timeout management.
//!
//! Replaces per-flow `tokio::time::timeout()` calls and periodic full-table
//! scans with a single shared timer that checks all registered deadlines on
//! a fixed tick interval, reducing wakeup count from O(N) to O(1).

use std::collections::HashMap;
use std::hash::Hash;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Action to take when a timer expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    /// TCP bridge: clean up a pending SYN whose connect task timed out.
    SynTimeout,
    /// Socket proxy: remove an expired outbound UDP flow.
    UdpFlowExpiry,
    /// Inbound relay: remove an expired inbound UDP flow.
    InboundUdpExpiry,
}

/// Unified key for all flow types tracked by the timer wheel.
///
/// Each variant carries enough information to identify the specific flow
/// in its owning subsystem (socket proxy, tcp bridge, inbound relay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerKey {
    /// Outbound UDP: (src_ip, src_port, dst_ip, dst_port)
    Udp {
        src_ip: Ipv4Addr,
        src_port: u16,
        dst_ip: Ipv4Addr,
        dst_port: u16,
    },
    /// Inbound UDP: (gateway_ip, ephemeral_port, guest_ip, container_port)
    InboundUdp {
        gateway_ip: Ipv4Addr,
        ephemeral_port: u16,
        guest_ip: Ipv4Addr,
        container_port: u16,
    },
    /// TCP SYN gate: four-tuple identifying the pending connection
    Syn {
        src_ip: Ipv4Addr,
        src_port: u16,
        dst_ip: Ipv4Addr,
        dst_port: u16,
    },
}

/// An entry returned when a timer expires.
#[derive(Debug)]
pub struct ExpiredEntry {
    /// The key identifying the flow.
    pub key: TimerKey,
    /// The action to take for this expired flow.
    pub action: TimerAction,
}

struct TimerEntry {
    deadline: Instant,
    action: TimerAction,
}

/// A coarse-grained timer wheel for network flow timeouts.
///
/// Instead of N independent `tokio::time::timeout()` objects (one per flow),
/// a single tick scans all registered deadlines. This reduces the tokio
/// runtime wakeup count from O(active_flows) to O(1).
///
/// # Usage
///
/// ```ignore
/// let mut wheel = TimerWheel::new(Duration::from_secs(1));
///
/// // On flow creation:
/// wheel.register(TimerKey::Udp { .. }, Duration::from_secs(60), TimerAction::UdpFlowExpiry);
///
/// // On flow activity (refresh deadline):
/// wheel.register(TimerKey::Udp { .. }, Duration::from_secs(60), TimerAction::UdpFlowExpiry);
///
/// // In the event loop, on each tick:
/// for expired in wheel.advance() {
///     handle_expired(expired.key, expired.action);
/// }
/// ```
pub struct TimerWheel {
    entries: HashMap<TimerKey, TimerEntry>,
    tick_interval: Duration,
}

impl TimerWheel {
    /// Create a new timer wheel with the given tick interval.
    pub fn new(tick_interval: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            tick_interval,
        }
    }

    /// Register a new timer. If the key already exists, its deadline is
    /// updated (acts as a "refresh" for activity-based timeouts).
    pub fn register(&mut self, key: TimerKey, timeout: Duration, action: TimerAction) {
        let deadline = Instant::now() + timeout;
        self.entries.insert(key, TimerEntry { deadline, action });
    }

    /// Cancel a timer by key. Returns `true` if it existed.
    pub fn cancel(&mut self, key: &TimerKey) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Advance the wheel: collect and remove all expired entries.
    pub fn advance(&mut self) -> Vec<ExpiredEntry> {
        let now = Instant::now();
        let mut expired = Vec::new();
        self.entries.retain(|key, entry| {
            if entry.deadline <= now {
                expired.push(ExpiredEntry {
                    key: *key,
                    action: entry.action,
                });
                false
            } else {
                true
            }
        });
        expired
    }

    /// Number of registered timers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the wheel has no registered timers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The tick interval this wheel was configured with.
    #[must_use]
    pub fn tick_interval(&self) -> Duration {
        self.tick_interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn udp_key(port: u16) -> TimerKey {
        TimerKey::Udp {
            src_ip: Ipv4Addr::new(192, 168, 64, 2),
            src_port: port,
            dst_ip: Ipv4Addr::new(1, 1, 1, 1),
            dst_port: 53,
        }
    }

    fn syn_key(port: u16) -> TimerKey {
        TimerKey::Syn {
            src_ip: Ipv4Addr::new(192, 168, 64, 2),
            src_port: port,
            dst_ip: Ipv4Addr::new(10, 0, 0, 1),
            dst_port: 80,
        }
    }

    #[test]
    fn test_register_and_advance() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        wheel.register(
            udp_key(1000),
            Duration::from_millis(10),
            TimerAction::UdpFlowExpiry,
        );
        wheel.register(
            udp_key(2000),
            Duration::from_secs(60),
            TimerAction::UdpFlowExpiry,
        );
        assert_eq!(wheel.len(), 2);

        thread::sleep(Duration::from_millis(20));
        let expired = wheel.advance();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].key, udp_key(1000));
        assert_eq!(expired[0].action, TimerAction::UdpFlowExpiry);
        assert_eq!(wheel.len(), 1);
    }

    #[test]
    fn test_cancel() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        wheel.register(
            syn_key(5000),
            Duration::from_secs(60),
            TimerAction::SynTimeout,
        );
        assert!(wheel.cancel(&syn_key(5000)));
        assert!(!wheel.cancel(&syn_key(5000)));
        assert!(wheel.is_empty());
    }

    #[test]
    fn test_refresh_updates_deadline() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        wheel.register(
            udp_key(3000),
            Duration::from_millis(10),
            TimerAction::UdpFlowExpiry,
        );
        // Refresh with a longer deadline
        wheel.register(
            udp_key(3000),
            Duration::from_secs(60),
            TimerAction::UdpFlowExpiry,
        );
        assert_eq!(wheel.len(), 1);

        thread::sleep(Duration::from_millis(20));
        let expired = wheel.advance();
        assert!(expired.is_empty()); // refresh extended the deadline
    }

    #[test]
    fn test_advance_no_expired() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        wheel.register(
            udp_key(4000),
            Duration::from_secs(60),
            TimerAction::UdpFlowExpiry,
        );
        let expired = wheel.advance();
        assert!(expired.is_empty());
        assert_eq!(wheel.len(), 1);
    }

    #[test]
    fn test_empty_wheel() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        assert!(wheel.is_empty());
        assert_eq!(wheel.len(), 0);
        let expired = wheel.advance();
        assert!(expired.is_empty());
    }

    #[test]
    fn test_mixed_actions() {
        let mut wheel = TimerWheel::new(Duration::from_secs(1));
        wheel.register(
            udp_key(1000),
            Duration::from_millis(5),
            TimerAction::UdpFlowExpiry,
        );
        wheel.register(
            syn_key(2000),
            Duration::from_millis(5),
            TimerAction::SynTimeout,
        );
        wheel.register(
            TimerKey::InboundUdp {
                gateway_ip: Ipv4Addr::new(192, 168, 64, 1),
                ephemeral_port: 50000,
                guest_ip: Ipv4Addr::new(192, 168, 64, 2),
                container_port: 8080,
            },
            Duration::from_secs(60),
            TimerAction::InboundUdpExpiry,
        );

        thread::sleep(Duration::from_millis(15));
        let expired = wheel.advance();
        assert_eq!(expired.len(), 2); // UDP + SYN expired, inbound still live
        assert_eq!(wheel.len(), 1);
    }
}
