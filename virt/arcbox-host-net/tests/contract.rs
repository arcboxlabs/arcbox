//! The `GuestNetwork` contract, run against `HostNetwork` and against the
//! testkit's `FakeNetwork`, the port's reference network.
//!
//! The testkit has a driver contract (`driver_contract!`) but no network
//! one, so the checks the fake's own tests make are written here once and
//! run over both. Running them over the fake too is what keeps them the
//! port's contract rather than this adapter's description of itself. Both
//! networks lease from `10.200.0.0/16` so the expected addresses are the
//! same; what the port leaves to each adapter (the NIC attachment, host
//! ingress) is checked elsewhere.

#![cfg(target_os = "macos")]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use arcbox_host_net::{HostNetwork, NoDns, Pool};
use arcbox_vm_driver::net::{AttachMode, GuestNetwork, NetworkLease, NetworkMode, NetworkPolicy};
use arcbox_vm_driver::testkit::FakeNetwork;
use arcbox_vm_driver::{Error, VmId};

fn id(s: &str) -> VmId {
    VmId::new(s).unwrap()
}

fn policy() -> NetworkPolicy {
    NetworkPolicy {
        mode: NetworkMode::Nat,
    }
}

fn v4(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// What a previous process's durable record hands back for `vm`: a lease
/// this network never minted, on `.host`.
fn replayed(vm: &str, host: u8) -> NetworkLease {
    NetworkLease {
        vm: id(vm),
        ip: IpAddr::V4(Ipv4Addr::new(10, 200, 0, host)),
        prefix_len: 16,
        gateway: v4("10.200.0.1"),
        mac: "02:00:00:00:00:01".parse().unwrap(),
        cleanup_token: format!("boot-1-{vm}"),
    }
}

/// A fresh, settled network: whatever the owner had to replay is replayed.
trait Settled: GuestNetwork + Sized {
    fn settled() -> Self;
}

impl Settled for FakeNetwork {
    fn settled() -> Self {
        Self::new()
    }
}

impl Settled for HostNetwork {
    fn settled() -> Self {
        let net = Self::new(
            Pool::new(Ipv4Addr::new(10, 200, 0, 0), 16).unwrap(),
            Arc::new(NoDns),
        );
        net.reconcile().unwrap().replay_complete();
        net
    }
}

async fn leases_take_the_lowest_free_host_and_release_reuses_it<N: Settled>() {
    let net = N::settled();
    let a = net.reserve(&id("a"), policy()).await.unwrap();
    let b = net.reserve(&id("b"), policy()).await.unwrap();
    assert_eq!(a.ip, v4("10.200.0.2"));
    assert_eq!(b.ip, v4("10.200.0.3"));
    assert_eq!(a.prefix_len, 16);
    assert_eq!(a.gateway, v4("10.200.0.1"));
    assert!(a.mac.is_unicast() && a.mac != b.mac);
    assert_ne!(a.cleanup_token, b.cleanup_token);
    assert!(net.reserve(&id("a"), policy()).await.is_err());

    let nic = net.activate(&a, AttachMode::Invariant).await.unwrap();
    assert_eq!(nic.id, "eth0");
    assert_eq!(nic.mac, a.mac);
    assert_eq!(net.identity(&a, AttachMode::Invariant).mac, a.mac);

    // A lease that is not the one reserved does not activate.
    let mut forged = b.clone();
    forged.cleanup_token = "forged".into();
    assert!(net.activate(&forged, AttachMode::Invariant).await.is_err());
    // Nor does one for a VM that reserved nothing.
    let mut unknown = b.clone();
    unknown.vm = id("nobody");
    assert!(matches!(
        net.activate(&unknown, AttachMode::Invariant).await,
        Err(Error::NotFound(_))
    ));

    net.release(a.clone()).await.unwrap();
    let c = net.reserve(&id("c"), policy()).await.unwrap();
    assert_eq!(c.ip, a.ip);
}

async fn quarantine_holds_the_address_until_the_token_finalizes_it<N: Settled>() {
    let net = N::settled();
    let a = net.reserve(&id("a"), policy()).await.unwrap();
    net.activate(&a, AttachMode::Invariant).await.unwrap();
    net.quarantine(a.clone()).await.unwrap();
    net.quarantine(a.clone()).await.unwrap(); // idempotent
    let mut other = a.clone();
    other.cleanup_token = "other".into();
    assert!(matches!(
        net.quarantine(other).await,
        Err(Error::Unavailable(_))
    ));
    assert!(net.activate(&a, AttachMode::Invariant).await.is_err());

    let reconcile = net.reconcile().unwrap();
    assert_eq!(
        reconcile.pending_cleanups().await.unwrap(),
        vec![(id("a"), a.cleanup_token.clone())]
    );
    assert!(matches!(
        reconcile.validate_cleanup(&id("a"), "wrong").await,
        Err(Error::PreconditionFailed(_))
    ));
    assert!(matches!(
        reconcile.validate_cleanup(&id("b"), &a.cleanup_token).await,
        Err(Error::PreconditionFailed(_))
    ));
    assert_eq!(
        reconcile
            .validate_cleanup(&id("a"), &a.cleanup_token)
            .await
            .unwrap(),
        a
    );
    assert!(matches!(
        net.reserve(&id("a"), policy()).await,
        Err(Error::Unavailable(_))
    ));
    let b = net.reserve(&id("b"), policy()).await.unwrap();
    assert_ne!(b.ip, a.ip);

    assert!(matches!(
        reconcile.finalize_cleanup(&id("a"), "wrong").await,
        Err(Error::PreconditionFailed(_))
    ));
    reconcile
        .finalize_cleanup(&id("a"), &a.cleanup_token)
        .await
        .unwrap();
    assert_eq!(reconcile.pending_cleanups().await.unwrap(), []);
    let c = net.reserve(&id("c"), policy()).await.unwrap();
    assert_eq!(c.ip, a.ip);
}

async fn stale_leases_neither_free_nor_quarantine_a_reassigned_address<N: Settled>() {
    let net = N::settled();
    let a1 = net.reserve(&id("a"), policy()).await.unwrap();
    net.release(a1.clone()).await.unwrap();
    net.release(a1.clone()).await.unwrap(); // a no-op...
    let second = net.reserve(&id("b"), policy()).await.unwrap();
    assert_eq!(second.ip, a1.ip);
    // ...until the address belongs to someone else.
    assert!(net.release(a1.clone()).await.is_err());
    assert!(net.quarantine(a1.clone()).await.is_err());
    let third = net.reserve(&id("c"), policy()).await.unwrap();
    assert_ne!(third.ip, second.ip);

    // A VM's older generation cannot touch its current lease either.
    let a2 = net.reserve(&id("a"), policy()).await.unwrap();
    assert!(net.release(a1.clone()).await.is_err());
    assert!(net.quarantine(a1).await.is_err());
    net.activate(&a2, AttachMode::Invariant).await.unwrap();

    // A quarantined lease is released only by itself.
    net.quarantine(a2.clone()).await.unwrap();
    let mut forged = a2.clone();
    forged.cleanup_token = "forged".into();
    assert!(net.release(forged).await.is_err());
    net.release(a2.clone()).await.unwrap();
    assert_eq!(
        net.reconcile().unwrap().pending_cleanups().await.unwrap(),
        []
    );
    let reused = net.reserve(&id("d"), policy()).await.unwrap();
    assert_eq!(reused.ip, a2.ip);

    // A durable-record replay of an address nobody holds still lands in
    // quarantine and pins the address until its token finalizes it.
    let ghost = replayed("ghost", 5);
    net.quarantine(ghost.clone()).await.unwrap();
    let next = net.reserve(&id("e"), policy()).await.unwrap();
    assert_eq!(next.ip, v4("10.200.0.6"));
    net.reconcile()
        .unwrap()
        .finalize_cleanup(&id("ghost"), &ghost.cleanup_token)
        .await
        .unwrap();
    let unpinned = net.reserve(&id("f"), policy()).await.unwrap();
    assert_eq!(unpinned.ip, ghost.ip);
}

async fn an_adopted_lease_holds_its_address<N: Settled>() {
    let net = N::settled();
    let live = replayed("a", 2);
    net.adopt(&live, AttachMode::Invariant).await.unwrap();
    // Re-adopting a live lease is the address collision: not idempotent.
    assert!(matches!(
        net.adopt(&live, AttachMode::Invariant).await,
        Err(Error::Network(_))
    ));
    let next = net.reserve(&id("b"), policy()).await.unwrap();
    assert_ne!(next.ip, live.ip);

    // Two records claiming one address: adopting either is a guess, and
    // the refusal takes nothing.
    let mut twin = live.clone();
    twin.vm = id("twin");
    assert!(matches!(
        net.adopt(&twin, AttachMode::Invariant).await,
        Err(Error::Network(_))
    ));
    let after = net.reserve(&id("c"), policy()).await.unwrap();
    assert_eq!(after.ip, v4("10.200.0.4"));
    let mut foreign = live.clone();
    foreign.ip = v4("192.0.2.7");
    assert!(net.adopt(&foreign, AttachMode::Invariant).await.is_err());

    // Live and awaiting cleanup are exclusive.
    net.quarantine(live.clone()).await.unwrap();
    assert!(matches!(
        net.adopt(&live, AttachMode::Invariant).await,
        Err(Error::PreconditionFailed(_))
    ));
}

async fn a_held_address_is_never_handed_out<N: Settled>() {
    let net = N::settled();
    net.hold_address(v4("10.200.0.2"));
    net.hold_address(v4("10.200.0.2")); // idempotent
    net.hold_address(v4("192.0.2.1")); // not this pool's: nothing to hold
    net.hold_address(v4("fd00::2"));
    let a = net.reserve(&id("a"), policy()).await.unwrap();
    assert_eq!(a.ip, v4("10.200.0.3"));
    // Held is not a lease: nothing lists it, and no protocol call names it.
    assert_eq!(
        net.reconcile().unwrap().pending_cleanups().await.unwrap(),
        []
    );
}

async fn with_nothing_pending_the_startup_gate_is_open<N: Settled>() {
    let net = N::settled();
    let reconcile = net.reconcile().unwrap();
    assert_eq!(reconcile.startup_cleanup_token().await, None);
    assert!(matches!(
        reconcile.validate_startup_cleanup("boot-1").await,
        Err(Error::PreconditionFailed(_))
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reconcile.wait_startup_cleanup_complete(),
    )
    .await
    .expect("returns at once");
}

macro_rules! contract {
    ($module:ident, $network:ident: $($check:ident),+ $(,)?) => {
        mod $module {
            use super::*;
            $(
                #[tokio::test]
                async fn $check() {
                    super::$check::<$network>().await;
                }
            )+
        }
    };
}

macro_rules! contract_for {
    ($($module:ident: $network:ident),+) => {
        $(
            contract!($module, $network:
                leases_take_the_lowest_free_host_and_release_reuses_it,
                quarantine_holds_the_address_until_the_token_finalizes_it,
                stale_leases_neither_free_nor_quarantine_a_reassigned_address,
                an_adopted_lease_holds_its_address,
                a_held_address_is_never_handed_out,
                with_nothing_pending_the_startup_gate_is_open,
            );
        )+
    };
}

contract_for!(fake: FakeNetwork, host: HostNetwork);
