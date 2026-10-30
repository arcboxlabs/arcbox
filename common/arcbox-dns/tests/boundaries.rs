use arcbox_constants::dns::Domain;
use arcbox_dns::NamePattern;
use arcbox_dns::claims::{Boundary, Claim, Resolution, resolve};

fn domain(name: &str) -> Domain {
    name.parse().unwrap()
}

fn claim(pattern: &str, authority: &str) -> Claim {
    Claim::exclusive(
        NamePattern::parse_custom(pattern).unwrap(),
        format!("resource:{authority}"),
        authority.into(),
    )
}

fn reserved(root: &str, authority: &str) -> Boundary {
    Boundary::ReservedTree {
        root: domain(root),
        authority: authority.into(),
    }
}

fn delegated(root: &str, instance: &str) -> Boundary {
    Boundary::DelegatedTree {
        root: domain(root),
        instance: instance.into(),
    }
}

fn lookup<'a>(name: &str, claims: &'a [Claim], boundaries: &'a [Boundary]) -> Resolution<'a> {
    resolve(&domain(name), claims, boundaries)
}

#[test]
fn reservations_outlive_routes_and_exclude_unrelated_claims() {
    let boundaries = [reserved("vm.local", "machine")];
    let mut claims = vec![claim("vm.local", "docker"), claim("*.vm.local", "docker")];
    for name in ["vm.local", "x.vm.local", "a.b.vm.local"] {
        assert_eq!(
            lookup(name, &[], &boundaries),
            Resolution::Reserved(&boundaries[0])
        );
        assert_eq!(
            lookup(name, &claims, &boundaries),
            Resolution::Reserved(&boundaries[0])
        );
    }
    let machine = claim("vm.local", "machine");
    claims.push(machine.clone());
    assert_eq!(
        lookup("vm.local", &claims, &boundaries),
        Resolution::Matched(&machine)
    );
    assert_eq!(
        lookup("x.vm.local", &claims, &boundaries),
        Resolution::Reserved(&boundaries[0])
    );
}

#[test]
fn reservation_authority_is_checked_before_claim_specificity() {
    let boundaries = [reserved("vm.local", "machine")];
    let machine = claim("*.vm.local", "machine");
    let claims = [claim("api.vm.local", "docker"), machine.clone()];
    assert_eq!(
        lookup("api.vm.local", &claims, &boundaries),
        Resolution::Matched(&machine)
    );
}

#[test]
fn closest_reservation_controls_its_namespace() {
    let mut boundaries = [
        reserved("k8s.local", "cluster"),
        reserved("svc.k8s.local", "service"),
    ];
    let service = claim("*.svc.k8s.local", "service");
    let claims = [claim("*.k8s.local", "cluster"), service.clone()];
    for _ in 0..2 {
        assert_eq!(
            lookup("a.b.svc.k8s.local", &claims, &boundaries),
            Resolution::Matched(&service)
        );
        boundaries.reverse();
    }
}

#[test]
fn reservations_do_not_inherit_ancestor_wildcards_from_the_same_authority() {
    let claims = [claim("*.k8s.local", "cluster")];
    for boundary in [
        reserved("svc.k8s.local", "cluster"),
        Boundary::ReservedName {
            name: domain("svc.k8s.local"),
            authority: "cluster".into(),
        },
    ] {
        let boundaries = [boundary.clone()];
        assert_eq!(
            lookup("svc.k8s.local", &claims, &boundaries),
            Resolution::Reserved(&boundary)
        );
        if matches!(&boundary, Boundary::ReservedTree { .. }) {
            assert_eq!(
                lookup("deep.svc.k8s.local", &claims, &boundaries),
                Resolution::Reserved(&boundary)
            );
        }
    }
}

#[test]
fn generated_claims_at_the_reservation_root_remain_eligible() {
    let boundaries = [reserved("svc.k8s.local", "cluster")];
    let mut claims = Vec::from(Claim::generated(
        domain("svc.k8s.local"),
        "service".into(),
        "cluster".into(),
    ));
    claims.push(claim("*.k8s.local", "cluster"));
    for name in ["svc.k8s.local", "a.b.svc.k8s.local"] {
        assert!(matches!(
            lookup(name, &claims, &boundaries),
            Resolution::Matched(claim) if claim.owner() == "service"
        ));
    }
}

#[test]
fn exact_reservation_is_narrower_than_a_tree_with_the_same_root() {
    let index = Boundary::ReservedName {
        name: domain("arcbox.local"),
        authority: "index".into(),
    };
    let mut boundaries = [reserved("arcbox.local", "docker"), index.clone()];
    let child = claim("app.arcbox.local", "docker");
    let mut claims = vec![claim("arcbox.local", "docker"), child.clone()];
    for _ in 0..2 {
        assert_eq!(
            lookup("arcbox.local", &claims, &boundaries),
            Resolution::Reserved(&index)
        );
        assert_eq!(
            lookup("app.arcbox.local", &claims, &boundaries),
            Resolution::Matched(&child)
        );
        boundaries.reverse();
    }
    let index_claim = claim("arcbox.local", "index");
    claims.push(index_claim.clone());
    assert_eq!(
        lookup("arcbox.local", &claims, &boundaries),
        Resolution::Matched(&index_claim)
    );
}

#[test]
fn delegation_blocks_more_specific_local_claims_and_reservations() {
    let child = delegated("dev.arcbox.local", "child");
    let mut boundaries = [
        child.clone(),
        Boundary::ReservedName {
            name: domain("x.dev.arcbox.local"),
            authority: "docker".into(),
        },
    ];
    let parent = claim("*.arcbox.local", "docker");
    let claims = [parent.clone(), claim("x.dev.arcbox.local", "docker")];
    for _ in 0..2 {
        for name in [
            "dev.arcbox.local",
            "x.dev.arcbox.local",
            "a.b.dev.arcbox.local",
        ] {
            assert_eq!(
                lookup(name, &claims, &boundaries),
                Resolution::Delegated(&child)
            );
        }
        boundaries.reverse();
    }
    assert_eq!(
        lookup("notdev.arcbox.local", &claims, &boundaries),
        Resolution::Matched(&parent)
    );
    assert_eq!(
        lookup("dev.arcbox.local.example", &claims, &boundaries),
        Resolution::Unmatched
    );
}

#[test]
fn nested_delegations_select_the_closest_external_instance() {
    let child = delegated("nested.dev.local", "child");
    let mut boundaries = [delegated("dev.local", "parent"), child.clone()];
    for _ in 0..2 {
        for name in ["nested.dev.local", "a.b.nested.dev.local"] {
            assert_eq!(
                lookup(name, &[], &boundaries),
                Resolution::Delegated(&child)
            );
        }
        boundaries.reverse();
    }
}

#[test]
fn incompatible_boundaries_conflict_and_identical_boundaries_are_idempotent() {
    for mut boundaries in [
        [reserved("dev.local", "a"), reserved("dev.local", "b")],
        [delegated("dev.local", "a"), delegated("dev.local", "b")],
        [
            Boundary::ReservedName {
                name: domain("dev.local"),
                authority: "a".into(),
            },
            Boundary::ReservedName {
                name: domain("dev.local"),
                authority: "b".into(),
            },
        ],
    ] {
        let claims = [claim("dev.local", "a")];
        for _ in 0..2 {
            assert_eq!(
                lookup("dev.local", &claims, &boundaries),
                Resolution::Conflict
            );
            boundaries.reverse();
        }
        boundaries[1] = boundaries[0].clone();
        assert!(matches!(
            lookup("dev.local", &[], &boundaries),
            Resolution::Reserved(_) | Resolution::Delegated(_)
        ));
    }
}
