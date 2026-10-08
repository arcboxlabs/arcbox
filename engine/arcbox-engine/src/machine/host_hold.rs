//! Host-side holds on a running machine.
//!
//! The host may keep resources a machine serves: the daemon mounts a
//! machine's root over NFS from the machine's own agent. Killing the VM
//! while such a mount is up leaves the client with requests no server will
//! answer, and only a forced unmount, after its timeout, gets rid of them —
//! a `remove --force` took ~15 s that way. A holder therefore registers a
//! [`HostHold`] while it depends on the machine and drops it once the
//! `MachineStopping` event has made it let go; every stop waits, bounded,
//! for the holds to clear before it touches the VM. A timeout aborts the stop.
//! The graceful stop
//! waits too: its shutdown RPC kills the machine's export within moments
//! on alpine, before an unmount started at the event has finished.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The holds on every machine, by name.
#[derive(Default)]
pub(super) struct HostHolds {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    counts: Mutex<HashMap<String, usize>>,
    released: Condvar,
}

impl Inner {
    fn counts(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        // A holder that panicked mid-update leaves a plain map; keep serving.
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HostHolds {
    /// Registers a hold on `name`, released when the returned guard drops.
    pub fn hold(&self, name: &str) -> HostHold {
        *self.inner.counts().entry(name.to_owned()).or_insert(0) += 1;
        HostHold {
            inner: Arc::clone(&self.inner),
            name: name.to_owned(),
        }
    }

    /// Waits until no hold on `name` remains, or `timeout` passes. Returns
    /// whether the holds cleared.
    pub fn wait_released(&self, name: &str, timeout: Duration) -> bool {
        let counts = self.inner.counts();
        let (counts, _) = self
            .inner
            .released
            .wait_timeout_while(counts, timeout, |counts| counts.contains_key(name))
            .unwrap_or_else(PoisonError::into_inner);
        !counts.contains_key(name)
    }
}

/// A host-side dependency on a running machine that a force stop waits
/// for; see the module docs. Dropping it releases the hold.
#[derive(Debug)]
pub struct HostHold {
    inner: Arc<Inner>,
    name: String,
}

#[cfg(feature = "test-support")]
impl HostHold {
    /// Creates a hold for a registry fixture without admitting work to a VM.
    #[must_use]
    pub fn for_test() -> Self {
        HostHolds::default().hold("fixture")
    }
}

impl Drop for HostHold {
    fn drop(&mut self) {
        let mut counts = self.inner.counts();
        let Some(count) = counts.get_mut(&self.name) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            counts.remove(&self.name);
            self.inner.released.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::HostHolds;

    #[test]
    fn a_machine_without_holds_is_released_at_once() {
        let holds = HostHolds::default();
        let started = Instant::now();
        assert!(holds.wait_released("m", Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn every_hold_must_drop_before_the_wait_returns() {
        let holds = HostHolds::default();
        let first = holds.hold("m");
        let second = holds.hold("m");
        let other = holds.hold("other");

        assert!(!holds.wait_released("m", Duration::from_millis(20)));
        drop(first);
        assert!(!holds.wait_released("m", Duration::from_millis(20)));
        drop(second);
        assert!(holds.wait_released("m", Duration::from_millis(20)));
        // Another machine's hold is not this one's.
        assert!(!holds.wait_released("other", Duration::from_millis(20)));
        drop(other);
    }

    #[test]
    fn a_waiter_wakes_when_the_hold_drops() {
        let holds = HostHolds::default();
        let hold = holds.hold("m");
        let release_after = Duration::from_millis(200);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(release_after);
            drop(hold);
        });

        let started = Instant::now();
        assert!(holds.wait_released("m", Duration::from_secs(10)));
        let waited = started.elapsed();
        assert!(waited >= release_after, "returned after {waited:?}");
        assert!(waited < Duration::from_secs(5), "woke late: {waited:?}");
        releaser.join().unwrap();
    }
}
