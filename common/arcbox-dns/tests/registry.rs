use std::net::IpAddr;

use arcbox_constants::dns::Domain;
use arcbox_dns::NamePattern;
use arcbox_dns::claims::{Boundary, Claim};
use arcbox_dns::registry::{Lookup, OwnerMismatch, Registry};

fn domain(name: &str) -> Domain {
    name.parse().unwrap()
}

fn claim(name: &str, owner: &str) -> Claim {
    Claim::exclusive(
        NamePattern::Exact(domain(name)),
        owner.into(),
        "docker".into(),
    )
}

fn ip(last: u8) -> IpAddr {
    [172, 17, 0, last].into()
}

#[test]
fn identical_registration_is_idempotent_and_rename_replaces_old_claims() {
    let mut registry = Registry::default();
    let first = registry
        .replace("a".into(), vec![claim("old.local", "a")], Some(ip(2)))
        .unwrap();
    assert_eq!(
        registry.replace("a".into(), vec![claim("old.local", "a")], Some(ip(2))),
        Ok(first)
    );
    assert_eq!(registry.epoch(), first);
    let renamed = registry
        .replace("a".into(), vec![claim("new.local", "a")], Some(ip(3)))
        .unwrap();
    assert!(renamed > first);
    assert_eq!(registry.resolve(&domain("old.local")), Lookup::Unmatched);
    let Lookup::Matched {
        claim,
        address,
        revision,
    } = registry.resolve(&domain("new.local"))
    else {
        panic!("renamed resource must resolve")
    };
    assert_eq!((claim.owner(), address, revision), ("a", ip(3), renamed));
}

#[test]
fn losing_last_network_keeps_identity_without_participating_in_claim_selection() {
    let mut registry = Registry::default();
    let claims = vec![claim("app.local", "a")];
    let connected = registry
        .replace("a".into(), claims.clone(), Some(ip(2)))
        .unwrap();
    let disconnected = registry.replace("a".into(), claims.clone(), None).unwrap();
    assert!(disconnected > connected);
    assert_eq!(registry.owners().collect::<Vec<_>>(), ["a"]);
    assert_eq!(registry.get("a").unwrap().claims, claims);
    assert_eq!(registry.resolve(&domain("app.local")), Lookup::Unmatched);
    let selected = registry
        .replace("b".into(), vec![claim("app.local", "b")], Some(ip(3)))
        .unwrap();
    assert!(
        matches!(registry.resolve(&domain("app.local")), Lookup::Matched { claim, address, revision }
            if claim.owner() == "b" && address == ip(3) && revision == selected)
    );
    registry.replace("a".into(), claims, Some(ip(2))).unwrap();
    assert_eq!(registry.resolve(&domain("app.local")), Lookup::Conflict);
}

#[test]
fn removal_and_reinsertion_never_reuse_a_revision() {
    let mut registry = Registry::default();
    let claims = vec![claim("app.local", "a")];
    let first = registry
        .replace("a".into(), claims.clone(), Some(ip(2)))
        .unwrap();
    assert!(registry.remove("a"));
    let removed = registry.epoch();
    assert!(removed > first);
    assert!(registry.get("a").is_none());
    assert!(!registry.remove("a"));
    assert_eq!(registry.epoch(), removed);
    let restored = registry.replace("a".into(), claims, Some(ip(2))).unwrap();
    assert!(restored > removed);
    assert_eq!(registry.get("a").unwrap().revision, restored);
}

#[test]
fn foreign_claim_rejects_the_whole_replacement_without_mutating_state() {
    let mut registry = Registry::default();
    registry
        .replace("a".into(), vec![claim("old.local", "a")], Some(ip(2)))
        .unwrap();
    let original = registry.get("a").unwrap().clone();
    let epoch = registry.epoch();
    let replacement = vec![claim("new.local", "a"), claim("foreign.local", "b")];
    assert_eq!(
        registry.replace("a".into(), replacement, Some(ip(3))),
        Err(OwnerMismatch)
    );
    assert_eq!(registry.get("a"), Some(&original));
    assert_eq!(registry.epoch(), epoch);
    assert_eq!(registry.resolve(&domain("new.local")), Lookup::Unmatched);
    assert_eq!(
        registry.resolve(&domain("foreign.local")),
        Lookup::Unmatched
    );
}

#[test]
fn lookup_preserves_conflicts_reservations_and_delegations() {
    let reserved = Boundary::ReservedName {
        name: domain("index.local"),
        authority: "index".into(),
    };
    let delegated = Boundary::DelegatedTree {
        root: domain("remote.local"),
        instance: "remote".into(),
    };
    let mut registry = Registry::new(vec![reserved.clone(), delegated.clone()]);
    for (owner, address) in [("a", ip(2)), ("b", ip(3))] {
        registry
            .replace(owner.into(), vec![claim("app.local", owner)], Some(address))
            .unwrap();
    }
    assert_eq!(registry.resolve(&domain("app.local")), Lookup::Conflict);
    assert_eq!(
        registry.resolve(&domain("index.local")),
        Lookup::Reserved(&reserved)
    );
    assert_eq!(
        registry.resolve(&domain("x.remote.local")),
        Lookup::Delegated(&delegated)
    );
}
