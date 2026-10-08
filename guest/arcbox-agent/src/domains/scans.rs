//! When a followed container's listeners are read.
//!
//! Right after `start` a server may not have bound yet, and dev servers
//! commonly take seconds to, so the listeners are read at the start, then
//! after [`FIRST_RESCAN`], each delay doubling up to [`MAX_SCAN_INTERVAL`],
//! then continue at that interval until the container stops.

use std::time::Duration;

use tokio::time::Instant;

/// Delay before the first rescan.
const FIRST_RESCAN: Duration = Duration::from_millis(250);

/// Longest delay between two rescans.
const MAX_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// One container's scan schedule.
#[derive(Debug)]
pub(super) struct Scans {
    /// When the next scan is due.
    pub(super) next: Instant,
    interval: Duration,
}

impl Scans {
    /// A schedule whose first scan is due at `now`.
    pub(super) fn starting(now: Instant) -> Self {
        Self {
            next: now,
            interval: FIRST_RESCAN,
        }
    }

    /// Moves past a scan taken at `now`.
    pub(super) fn advance(&mut self, now: Instant) {
        let next = now + self.interval;
        self.interval = (self.interval * 2).min(MAX_SCAN_INTERVAL);
        self.next = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_slow_down_and_continue_after_two_minutes() {
        let start = Instant::now();
        let mut scans = Scans::starting(start);
        let mut taken = Vec::new();
        for _ in 0..40 {
            let due = scans.next;
            taken.push(due.duration_since(start));
            scans.advance(due);
        }
        let ms = |ms| Duration::from_millis(ms);
        assert_eq!(
            taken[..7],
            [
                ms(0),
                ms(250),
                ms(750),
                ms(1750),
                ms(3750),
                ms(7750),
                ms(12750)
            ]
        );
        assert!(taken.windows(2).all(|w| w[0] + MAX_SCAN_INTERVAL >= w[1]));
        let last = *taken.last().unwrap();
        assert!(last > Duration::from_secs(120));
        assert_eq!(scans.next - start, last + MAX_SCAN_INTERVAL);
    }
}
