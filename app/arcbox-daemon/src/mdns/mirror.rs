//! Reconciles the daemon's DNS table with the records a registrar holds.
//!
//! The mirror is pure: it owns the desired state (what the table says), the
//! live state (what the registrar accepted so far) and the retry rules, and
//! it drives any [`Registrar`]. `super::dns_sd::Connection` is the real one.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::hash::Hash;
use std::io;
use std::net::IpAddr;

use arcbox_net::DnsChange;

use super::dns_sd::ERR_POLICY_DENIED;

/// Where records are registered. Registration is asynchronous: the
/// registrar hands back a handle at once and the caller feeds the reply
/// for that handle into [`Mirror::handle_reply`] later.
pub trait Registrar {
    type Handle: Copy + Eq + Hash + Debug;

    fn register(&mut self, fqdn: &str, ip: IpAddr) -> io::Result<Self::Handle>;
    fn remove(&mut self, handle: Self::Handle);
}

/// What a reply changed, for the caller to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The record is live.
    Confirmed { fqdn: String },
    /// The registrar refused a record because the process has no Local
    /// Network access. Nothing is registered until [`Mirror::retry_denied`].
    Denied,
    /// A record went live after a denial.
    Recovered,
    /// The registrar refused one record for a reason other than policy; it
    /// is retried only when its address changes.
    Refused { fqdn: String, error: i32 },
}

struct Live<H> {
    handle: H,
    ip: IpAddr,
}

pub struct Mirror<R: Registrar> {
    registrar: R,
    /// What the DNS table says should exist.
    desired: BTreeMap<String, IpAddr>,
    /// Records the registrar holds for us and has not refused.
    live: HashMap<String, Live<R::Handle>>,
    by_handle: HashMap<R::Handle, String>,
    /// Records refused for a reason other than policy, with the address
    /// that was refused.
    refused: HashMap<String, IpAddr>,
    /// Set while the registrar refuses registrations for lack of Local
    /// Network access; cleared by [`Mirror::retry_denied`] or a live record.
    denied: bool,
}

impl<R: Registrar> Mirror<R> {
    pub fn new(registrar: R) -> Self {
        Self {
            registrar,
            desired: BTreeMap::new(),
            live: HashMap::new(),
            by_handle: HashMap::new(),
            refused: HashMap::new(),
            denied: false,
        }
    }

    pub fn registrar_mut(&mut self) -> &mut R {
        &mut self.registrar
    }

    pub const fn is_denied(&self) -> bool {
        self.denied
    }

    /// Replaces the desired state with a full snapshot of the table.
    pub fn resync(&mut self, entries: Vec<(String, IpAddr)>) {
        self.desired = entries.into_iter().collect();
        self.reconcile();
    }

    /// Applies one change from the table.
    pub fn apply(&mut self, change: DnsChange) {
        match change {
            DnsChange::Registered { fqdn, ip } => {
                self.desired.insert(fqdn, ip);
            }
            DnsChange::Removed { fqdn } => {
                self.desired.remove(&fqdn);
            }
        }
        self.reconcile();
    }

    /// Registers everything that is missing although the last attempt was
    /// refused for lack of Local Network access. The denial stands until a
    /// record goes live, so a repeat refusal is not reported again.
    pub fn retry_denied(&mut self) {
        if self.denied {
            self.reconcile_with(true);
        }
    }

    /// Settles the registration the registrar answered with `error`
    /// (`0` for a live record, otherwise its `kDNSServiceErr_*` code).
    pub fn handle_reply(&mut self, handle: R::Handle, error: i32) -> Option<Event> {
        let fqdn = self.by_handle.get(&handle)?.clone();
        if error == 0 {
            let recovered = std::mem::take(&mut self.denied);
            return Some(if recovered {
                Event::Recovered
            } else {
                Event::Confirmed { fqdn }
            });
        }
        // A refused record is still allocated and must be disposed of.
        let live = self.forget(&fqdn)?;
        self.registrar.remove(live.handle);
        if error == ERR_POLICY_DENIED {
            let already = std::mem::replace(&mut self.denied, true);
            return (!already).then_some(Event::Denied);
        }
        self.refused.insert(fqdn.clone(), live.ip);
        Some(Event::Refused { fqdn, error })
    }

    fn reconcile(&mut self) {
        self.reconcile_with(false);
    }

    fn reconcile_with(&mut self, retry_denied: bool) {
        // The address a refusal was about is the one not to retry.
        self.refused
            .retain(|fqdn, ip| self.desired.get(fqdn) == Some(ip));

        let stale: Vec<String> = self
            .live
            .iter()
            .filter(|(fqdn, live)| self.desired.get(*fqdn) != Some(&live.ip))
            .map(|(fqdn, _)| fqdn.clone())
            .collect();
        for fqdn in stale {
            if let Some(live) = self.forget(&fqdn) {
                self.registrar.remove(live.handle);
            }
        }

        if self.denied && !retry_denied {
            return;
        }
        let missing: Vec<(String, IpAddr)> = self
            .desired
            .iter()
            .filter(|(fqdn, _)| !self.live.contains_key(*fqdn) && !self.refused.contains_key(*fqdn))
            .map(|(fqdn, ip)| (fqdn.clone(), *ip))
            .collect();
        for (fqdn, ip) in missing {
            match self.registrar.register(&fqdn, ip) {
                Ok(handle) => {
                    self.by_handle.insert(handle, fqdn.clone());
                    self.live.insert(fqdn, Live { handle, ip });
                }
                Err(error) => {
                    tracing::warn!(fqdn, %error, "mDNS record registration failed");
                    self.refused.insert(fqdn, ip);
                }
            }
        }
    }

    fn forget(&mut self, fqdn: &str) -> Option<Live<R::Handle>> {
        let live = self.live.remove(fqdn)?;
        self.by_handle.remove(&live.handle);
        Some(live)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// `kDNSServiceErr_NameConflict`: the shape of a refusal that is about one
    /// record rather than the whole process.
    const ERR_NAME_CONFLICT: i32 = -65548;

    #[derive(Default)]
    struct FakeRegistrar {
        next: usize,
        registered: Vec<(usize, String, IpAddr)>,
        removed: Vec<usize>,
    }

    impl Registrar for FakeRegistrar {
        type Handle = usize;

        fn register(&mut self, fqdn: &str, ip: IpAddr) -> io::Result<usize> {
            self.next += 1;
            self.registered.push((self.next, fqdn.to_owned(), ip));
            Ok(self.next)
        }

        fn remove(&mut self, handle: usize) {
            self.removed.push(handle);
        }
    }

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 2, last))
    }

    fn registered(mirror: &Mirror<FakeRegistrar>) -> Vec<(String, IpAddr)> {
        mirror
            .registrar
            .registered
            .iter()
            .map(|(_, fqdn, ip)| (fqdn.clone(), *ip))
            .collect()
    }

    #[test]
    fn a_snapshot_is_registered_and_removals_follow() {
        let mut mirror = Mirror::new(FakeRegistrar::default());
        mirror.resync(vec![
            ("a.arcbox.local".into(), ip(2)),
            ("b.arcbox.local".into(), ip(3)),
        ]);
        assert_eq!(
            registered(&mirror),
            vec![
                ("a.arcbox.local".into(), ip(2)),
                ("b.arcbox.local".into(), ip(3))
            ]
        );

        mirror.apply(DnsChange::Removed {
            fqdn: "a.arcbox.local".into(),
        });
        assert_eq!(mirror.registrar.removed, vec![1]);
        assert_eq!(
            registered(&mirror).len(),
            2,
            "no re-registration on removal"
        );
    }

    #[test]
    fn a_changed_address_replaces_the_record() {
        let mut mirror = Mirror::new(FakeRegistrar::default());
        mirror.resync(vec![("web.arcbox.local".into(), ip(2))]);
        mirror.apply(DnsChange::Registered {
            fqdn: "web.arcbox.local".into(),
            ip: ip(9),
        });
        assert_eq!(mirror.registrar.removed, vec![1]);
        assert_eq!(registered(&mirror)[1], ("web.arcbox.local".into(), ip(9)));

        mirror.apply(DnsChange::Registered {
            fqdn: "web.arcbox.local".into(),
            ip: ip(9),
        });
        assert_eq!(
            registered(&mirror).len(),
            2,
            "an unchanged address is left alone"
        );
    }

    #[test]
    fn a_policy_denial_pauses_registration_until_retried() {
        let mut mirror = Mirror::new(FakeRegistrar::default());
        mirror.resync(vec![("a.arcbox.local".into(), ip(2))]);

        let denied = mirror.handle_reply(1, ERR_POLICY_DENIED);
        assert_eq!(denied, Some(Event::Denied));
        assert_eq!(
            mirror.registrar.removed,
            vec![1],
            "a refused record is disposed of"
        );
        assert!(mirror.is_denied());

        mirror.apply(DnsChange::Registered {
            fqdn: "b.arcbox.local".into(),
            ip: ip(3),
        });
        assert_eq!(
            registered(&mirror).len(),
            1,
            "nothing is registered while denied"
        );

        mirror.retry_denied();
        assert_eq!(
            &registered(&mirror)[1..],
            &[
                ("a.arcbox.local".into(), ip(2)),
                ("b.arcbox.local".into(), ip(3))
            ]
        );

        let second_denial = mirror.handle_reply(2, ERR_POLICY_DENIED);
        assert_eq!(
            second_denial, None,
            "a repeat refusal is not reported again"
        );
        assert!(
            mirror.is_denied(),
            "the denial stands until a record goes live"
        );
        mirror.retry_denied();
        let live = mirror.handle_reply(4, 0);
        assert_eq!(live, Some(Event::Recovered));
        assert!(!mirror.is_denied());
    }

    #[test]
    fn a_refused_name_is_retried_only_with_a_new_address() {
        let mut mirror = Mirror::new(FakeRegistrar::default());
        mirror.resync(vec![("dup.arcbox.local".into(), ip(2))]);
        let refused = mirror.handle_reply(1, ERR_NAME_CONFLICT);
        assert_eq!(
            refused,
            Some(Event::Refused {
                fqdn: "dup.arcbox.local".into(),
                error: ERR_NAME_CONFLICT
            })
        );

        mirror.apply(DnsChange::Registered {
            fqdn: "dup.arcbox.local".into(),
            ip: ip(2),
        });
        assert_eq!(
            registered(&mirror).len(),
            1,
            "the refused address is not retried"
        );

        mirror.apply(DnsChange::Registered {
            fqdn: "dup.arcbox.local".into(),
            ip: ip(5),
        });
        assert_eq!(registered(&mirror)[1], ("dup.arcbox.local".into(), ip(5)));
    }

    #[test]
    fn a_reply_for_an_unknown_handle_is_ignored() {
        let mut mirror = Mirror::new(FakeRegistrar::default());
        assert_eq!(mirror.handle_reply(7, 0), None);
    }
}
