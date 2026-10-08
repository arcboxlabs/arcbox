use arcbox_constants::dns::Domain;
use arcbox_dns::NamePattern;
use arcbox_dns::claims::{Claim, Resolution, resolve};

fn domain(name: &str) -> Domain {
    name.parse().unwrap()
}

fn claim(pattern: &str, owner: &str) -> Claim {
    Claim::exclusive(
        NamePattern::parse_custom(pattern).unwrap(),
        owner.into(),
        "docker".into(),
    )
}

fn lookup<'a>(name: &str, claims: &'a [Claim]) -> Resolution<'a> {
    resolve(&domain(name), claims)
}

fn owner(result: Resolution<'_>) -> &str {
    let Resolution::Matched(claim) = result else {
        panic!("expected owner, got {result:?}")
    };
    claim.owner()
}

fn replica(id: &str, project: &str) -> [Claim; 2] {
    Claim::compose_service(
        domain("web.app.local"),
        id.into(),
        "docker".into(),
        (project.into(), "web".into()),
    )
}

#[test]
fn generated_claims_include_exact_and_nested_names() {
    let claims = Claim::generated(domain("app.local"), "app".into(), "docker".into());
    for name in ["app.local", "x.app.local", "a.b.app.local"] {
        assert_eq!(owner(lookup(name, &claims)), "app");
    }
    assert_eq!(lookup("other.local", &claims), Resolution::Unmatched);
}

#[test]
fn exact_then_most_specific_wildcard_wins_in_any_order() {
    let mut claims = [
        claim("*.app.local", "wide"),
        claim("*.api.app.local", "narrow"),
        claim("x.api.app.local", "exact"),
    ];
    for _ in 0..2 {
        for (name, expected) in [
            ("x.api.app.local", "exact"),
            ("y.api.app.local", "narrow"),
            ("api.app.local", "wide"),
        ] {
            assert_eq!(owner(lookup(name, &claims)), expected);
        }
        claims.reverse();
    }
}

#[test]
fn custom_claims_cannot_replace_equally_specific_generated_claims() {
    for (name, custom) in [("app.local", "app.local"), ("x.app.local", "*.app.local")] {
        let generated = Claim::generated(domain("app.local"), "first".into(), "docker".into());
        let mut claims = Vec::from(generated);
        claims.push(claim(custom, "second"));
        for _ in 0..2 {
            assert_eq!(lookup(name, &claims), Resolution::Conflict);
            claims.reverse();
        }
    }
}

#[test]
fn conflicts_never_fall_back_and_duplicate_owners_are_idempotent() {
    for pattern in ["x.api.app.local", "*.api.app.local"] {
        let mut claims = [
            claim("*.app.local", "fallback"),
            claim(pattern, "a"),
            claim(pattern, "b"),
        ];
        assert_eq!(lookup("x.api.app.local", &claims), Resolution::Conflict);
        claims[2] = claims[1].clone();
        assert_eq!(owner(lookup("x.api.app.local", &claims)), "a");
    }
}

#[test]
fn compose_replica_selection_is_stable_and_follows_removal() {
    let mut claims: Vec<_> = replica("b", "app")
        .into_iter()
        .chain(replica("a", "app"))
        .collect();
    for _ in 0..2 {
        for name in ["web.app.local", "deep.web.app.local"] {
            assert_eq!(owner(lookup(name, &claims)), "a");
        }
        claims.reverse();
    }
    claims.retain(|claim| claim.owner() != "a");
    assert_eq!(owner(lookup("web.app.local", &claims)), "b");
}

#[test]
fn compose_sharing_rejects_other_groups_and_exclusive_claims() {
    for extra in [
        replica("c", "other")[0].clone(),
        claim("web.app.local", "c"),
    ] {
        let mut claims = Vec::from(replica("b", "app"));
        claims.push(extra);
        assert_eq!(lookup("web.app.local", &claims), Resolution::Conflict);
    }
    let mut claims: Vec<_> = replica("b", "app")
        .into_iter()
        .chain(replica("a", "app"))
        .collect();
    claims.push(claim("web.app.local", "a"));
    for _ in 0..claims.len() {
        assert_eq!(lookup("web.app.local", &claims), Resolution::Conflict);
        claims.rotate_left(1);
    }
}

#[test]
fn equal_owner_or_compose_group_cannot_cross_authorities() {
    let exact = claim("web.app.local", "same");
    let other_exact = Claim::exclusive(
        exact.pattern().clone(),
        exact.owner().into(),
        "other-instance".into(),
    );
    let other_replica = Claim::compose_service(
        domain("web.app.local"),
        "b".into(),
        "other-instance".into(),
        ("app".into(), "web".into()),
    );
    for claims in [
        [exact, other_exact],
        [replica("a", "app")[0].clone(), other_replica[0].clone()],
    ] {
        assert_eq!(lookup("web.app.local", &claims), Resolution::Conflict);
    }
}
