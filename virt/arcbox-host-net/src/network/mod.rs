//! [`HostNetwork`]: the port's [`GuestNetwork`] and [`NetworkReconcile`]
//! for VMs that run on the macOS host.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use arcbox_net::darwin::guest_nat;
use arcbox_vm_driver::net::{
    AttachMode, GuestNetwork, HostIngress, NetworkIdentity, NetworkLease, NetworkMode,
    NetworkPolicy, NetworkReconcile,
};
use arcbox_vm_driver::{Error, NicAttachment, NicSpec, Result, VmId};
use async_trait::async_trait;

use crate::dns::DnsPublisher;
use crate::pool::Pool;

mod reconcile;

/// The device name every guest sees its one NIC under.
pub const NIC_ID: &str = "eth0";

/// The prefix of the guest link: the datapath's DHCP hands out a /24.
const GUEST_PREFIX_LEN: u8 = 24;

/// The macOS guest network for host sandboxes.
///
/// Every guest boots on the same fixed link — `10.0.2.2/24` behind
/// `10.0.2.1`, which is also its resolver — alone behind its own datapath,
/// which the driver builds in the VMM's process from the
/// [`NicAttachment::UserNat`] NIC this network hands out. There is no host
/// bridge between guests, no guest can address another (ADR 0010, D1), and
/// a checkpoint restores into any lease. The lease's pool address is the
/// VM's identity on the host: it is what the [`DnsPublisher`] publishes and
/// what the sandbox API reports, and it is never routed.
///
/// Cheap to clone; clones share one ledger.
///
/// Nothing this network installs outlives its process — the datapath is
/// the VMM's and DNS lives in the composer — so it has no startup sweep:
/// [`NetworkReconcile::startup_cleanup_token`] is always `None`. Quarantine
/// still holds an address until the composer confirms its own host state
/// for that lease (port forwards, DNS it published by other means) is
/// gone. `reserve` waits for [`NetworkReconcile::replay_complete`], so a
/// fresh lease cannot take the address of a guest that is still running
/// and not yet adopted.
#[derive(Clone)]
pub struct HostNetwork {
    shared: Arc<Shared>,
}

struct Shared {
    pool: Pool,
    dns: Arc<dyn DnsPublisher>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Host numbers in use: live, quarantined, or held.
    used: BTreeSet<u32>,
    live: HashMap<VmId, Live>,
    quarantined: HashMap<VmId, NetworkLease>,
    /// The owner has replayed its durable leases; `reserve` is open.
    replayed: bool,
}

struct Live {
    lease: NetworkLease,
}

impl HostNetwork {
    /// A network leasing from `pool`, whose live leases are published
    /// through `dns`.
    pub fn new(pool: Pool, dns: Arc<dyn DnsPublisher>) -> Self {
        Self {
            shared: Arc::new(Shared {
                pool,
                dns,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn pool(&self) -> Pool {
        self.shared.pool
    }

    /// The pool host number of `lease`, refusing a lease of another network.
    fn host_of(&self, lease: &NetworkLease) -> Result<u32> {
        self.pool().host(lease.ip).ok_or_else(|| {
            Error::Network(format!(
                "vm {} lease {} is not an address of this network",
                lease.vm, lease.ip
            ))
        })
    }

    /// Withdraws `vm`'s name unless a live lease has published it again in
    /// the meantime.
    async fn deregister_unless_live(&self, vm: &VmId) {
        if !self.state().live.contains_key(vm) {
            self.shared.dns.deregister(vm).await;
        }
    }
}

/// Only fresh boots and invariant restores exist on the host: no checkpoint
/// of a host VM predates the fixed guest link.
fn require_invariant(lease: &NetworkLease, mode: AttachMode) -> Result<()> {
    match mode {
        AttachMode::Invariant => Ok(()),
        AttachMode::LegacySnapshot => Err(Error::Network(format!(
            "vm {}: a legacy-snapshot attachment needs the guest on its pool address, \
             which a host datapath cannot serve",
            lease.vm
        ))),
    }
}

impl State {
    /// The error for a lease that is not the VM's current one — an older
    /// generation, or one whose address has since gone to another VM.
    fn stale(&self, lease: &NetworkLease) -> Error {
        let other_holder = self
            .live
            .iter()
            .map(|(vm, live)| (vm, &live.lease))
            .chain(self.quarantined.iter())
            .find(|(vm, held)| **vm != lease.vm && held.ip == lease.ip)
            .map(|(vm, _)| vm);
        match other_holder {
            Some(vm) => Error::Network(format!(
                "vm {} lease {} is stale: {} is now held by vm {vm}",
                lease.vm, lease.cleanup_token, lease.ip
            )),
            None => Error::Network(format!(
                "vm {} lease {} is not its current lease",
                lease.vm, lease.cleanup_token
            )),
        }
    }

    fn is_live(&self, lease: &NetworkLease) -> bool {
        self.live.get(&lease.vm).is_some_and(|l| l.lease == *lease)
    }

    /// A token that does not name `vm`'s pending generation is a failed
    /// precondition: no retry of it can succeed.
    fn validate_cleanup(&self, vm: &VmId, token: &str) -> Result<&NetworkLease> {
        let lease = self
            .quarantined
            .get(vm)
            .ok_or_else(|| Error::PreconditionFailed(format!("vm {vm} has no pending cleanup")))?;
        if lease.cleanup_token != token {
            return Err(Error::PreconditionFailed(format!(
                "vm {vm} cleanup token does not name the pending generation"
            )));
        }
        Ok(lease)
    }
}

#[async_trait]
impl GuestNetwork for HostNetwork {
    /// Reserves the lowest free pool address for `vm`. Only
    /// [`NetworkMode::Nat`] is offered: every datapath egresses through the
    /// host, so `Isolated` would be a promise it cannot keep, and there is
    /// no host link to bridge onto.
    async fn reserve(&self, vm: &VmId, policy: NetworkPolicy) -> Result<NetworkLease> {
        if policy.mode != NetworkMode::Nat {
            return Err(Error::Network(format!(
                "network mode {:?} is not offered for host VMs; only nat is",
                policy.mode
            )));
        }
        let pool = self.pool();
        let mut state = self.state();
        if !state.replayed {
            return Err(Error::Unavailable(
                "the network's owner has not finished replaying its durable leases".into(),
            ));
        }
        if state.quarantined.contains_key(vm) {
            return Err(Error::Unavailable(format!(
                "vm {vm} cleanup is awaiting host finalization"
            )));
        }
        if state.live.contains_key(vm) {
            return Err(Error::Network(format!("vm {vm} already holds a lease")));
        }
        let host = pool
            .hosts()
            .find(|host| !state.used.contains(host))
            .ok_or_else(|| Error::Network("host network address pool exhausted".into()))?;
        state.used.insert(host);
        let lease = NetworkLease {
            vm: vm.clone(),
            ip: IpAddr::V4(pool.address(host)),
            prefix_len: pool.prefix_len(),
            gateway: IpAddr::V4(pool.gateway()),
            mac: Pool::mac(host),
            cleanup_token: uuid::Uuid::new_v4().to_string(),
        };
        state.live.insert(
            vm.clone(),
            Live {
                lease: lease.clone(),
            },
        );
        Ok(lease)
    }

    /// Publishes the lease's name and hands the driver a `UserNat` NIC.
    async fn activate(&self, lease: &NetworkLease, mode: AttachMode) -> Result<NicSpec> {
        require_invariant(lease, mode)?;
        {
            let state = self.state();
            if state.quarantined.contains_key(&lease.vm) {
                return Err(Error::Network(format!(
                    "vm {} lease is quarantined; reserve a new one",
                    lease.vm
                )));
            }
            match state.live.get(&lease.vm) {
                None => return Err(Error::NotFound(lease.vm.clone())),
                Some(live) if live.lease != *lease => {
                    return Err(Error::Network(format!(
                        "vm {} lease does not match the one reserved",
                        lease.vm
                    )));
                }
                Some(_) => {}
            }
        }
        self.shared.dns.register(&lease.vm, lease.ip).await?;
        if !self.state().is_live(lease) {
            // Quarantined or released while the name was being published.
            self.deregister_unless_live(&lease.vm).await;
            return Err(Error::Network(format!(
                "vm {} lease changed while it was being activated",
                lease.vm
            )));
        }
        Ok(NicSpec {
            id: NIC_ID.to_owned(),
            mac: lease.mac,
            attachment: NicAttachment::UserNat,
        })
    }

    /// Re-registers a lease whose guest is still running and republishes
    /// its name; the datapath lives in the VMM's process and survived.
    async fn adopt(&self, lease: &NetworkLease, mode: AttachMode) -> Result<()> {
        require_invariant(lease, mode)?;
        let host = self.host_of(lease)?;
        {
            let mut state = self.state();
            if state.quarantined.contains_key(&lease.vm) {
                return Err(Error::PreconditionFailed(format!(
                    "vm {} lease is quarantined; it cannot also be live",
                    lease.vm
                )));
            }
            // Address before VM: a re-adopted live lease is the
            // duplicate-address refusal, as on every other network.
            if state.used.contains(&host) {
                return Err(Error::Network(format!(
                    "vm {} cannot adopt {}: the address is already held",
                    lease.vm, lease.ip
                )));
            }
            if state.live.contains_key(&lease.vm) {
                return Err(Error::PreconditionFailed(format!(
                    "vm {} already holds a live lease",
                    lease.vm
                )));
            }
            state.used.insert(host);
            state.live.insert(
                lease.vm.clone(),
                Live {
                    lease: lease.clone(),
                },
            );
        }
        if let Err(error) = self.shared.dns.register(&lease.vm, lease.ip).await {
            let mut state = self.state();
            if state.is_live(lease) {
                state.live.remove(&lease.vm);
                state.used.remove(&host);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Withdraws the lease's name and keeps the address until the cleanup
    /// protocol finalizes it. Idempotent.
    async fn quarantine(&self, lease: NetworkLease) -> Result<()> {
        let host = self.host_of(&lease)?;
        {
            let mut state = self.state();
            if let Some(existing) = state.quarantined.get(&lease.vm) {
                if existing.cleanup_token == lease.cleanup_token {
                    return Ok(());
                }
                return Err(Error::Unavailable(format!(
                    "vm {} already has a different quarantined lease",
                    lease.vm
                )));
            }
            match state.live.get(&lease.vm) {
                Some(live) if live.lease == lease => {
                    state.live.remove(&lease.vm);
                }
                Some(_) => return Err(state.stale(&lease)),
                // A lease replayed from a durable record after a restart is
                // quarantined even though this process never handed it out
                // — unless its address has gone to another VM since.
                None => {
                    if !state.used.insert(host) {
                        return Err(state.stale(&lease));
                    }
                }
            }
            state.quarantined.insert(lease.vm.clone(), lease.clone());
        }
        self.shared.dns.deregister(&lease.vm).await;
        Ok(())
    }

    /// Withdraws the lease's name and returns the address outright. Releasing what is already released does nothing.
    async fn release(&self, lease: NetworkLease) -> Result<()> {
        let host = self.host_of(&lease)?;
        {
            let mut state = self.state();
            let current = state
                .live
                .get(&lease.vm)
                .map(|live| &live.lease)
                .or_else(|| state.quarantined.get(&lease.vm));
            match current {
                Some(current) if *current == lease => {
                    state.quarantined.remove(&lease.vm);
                    state.used.remove(&host);
                    state.live.remove(&lease.vm);
                }
                Some(_) => return Err(state.stale(&lease)),
                None if state.used.contains(&host) => return Err(state.stale(&lease)),
                None => return Ok(()),
            }
        }
        self.shared.dns.deregister(&lease.vm).await;
        Ok(())
    }

    /// Takes the address out of the pool for this process's lifetime. An
    /// address outside the pool could never be handed out; nothing to do.
    fn hold_address(&self, address: IpAddr) {
        if let Some(host) = self.pool().host(address) {
            self.state().used.insert(host);
        }
    }

    /// The fixed guest link, whatever the lease: `10.0.2.2/24` behind
    /// `10.0.2.1`, which is also the resolver. The mode does not change the
    /// answer because a legacy-snapshot attachment is refused at `activate`.
    fn identity(&self, lease: &NetworkLease, _mode: AttachMode) -> NetworkIdentity {
        NetworkIdentity {
            ip: IpAddr::V4(guest_nat::GUEST_IP),
            prefix_len: GUEST_PREFIX_LEN,
            gateway: IpAddr::V4(guest_nat::GATEWAY_IP),
            dns: vec![IpAddr::V4(guest_nat::GATEWAY_IP)],
            mac: lease.mac,
        }
    }

    /// Refused: nothing on the host routes to a host VM, so there is no
    /// address a DNAT rule could target. Ports reach the guest through a
    /// `PortForward`: the driver's.
    fn host_ingress(&self, lease: &NetworkLease) -> Result<HostIngress> {
        Err(Error::Network(format!(
            "vm {}: host VMs take no DNAT ingress; forward ports through PortForward",
            lease.vm
        )))
    }

    fn reconcile(&self) -> Option<&dyn NetworkReconcile> {
        Some(self)
    }
}
