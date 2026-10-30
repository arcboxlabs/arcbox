use arcbox_constants::dns::Domain;
use arcbox_dns::{
    NamePattern,
    claims::{Claim, Resolution, resolve},
    container::ContainerMetadata,
};
use serde_json::{Value, json};

const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const AUTHORITY: &str = "docker:default";

fn container(id: &str, name: &str, labels: Value) -> ContainerMetadata {
    ContainerMetadata::from_inspect(&json!({
        "Id": id, "Name": format!("/{name}"), "Config": { "Labels": labels }
    }))
    .unwrap()
}

fn lookup<'a>(name: &str, claims: &'a [Claim]) -> Resolution<'a> {
    resolve(&name.parse().unwrap(), claims, &[])
}

fn assert_owner(name: &str, claims: &[Claim], owner: &str) {
    let Resolution::Matched(claim) = lookup(name, claims) else {
        panic!("no owner for {name}");
    };
    assert_eq!(claim.owner(), owner);
    assert_eq!(claim.authority(), AUTHORITY);
}

#[test]
fn generated_names_are_canonical_and_include_nested_descendants() {
    let container = container(ID, "My-Web", Value::Null);
    let result = container.claims(&"arcbox.local".parse().unwrap(), AUTHORITY);
    assert_eq!(result.warnings, Vec::<String>::new());
    assert_eq!(result.claims.len(), 2);
    assert_eq!(container.name, "My-Web");
    for name in ["my-web.arcbox.local", "a.b.my-web.arcbox.local"] {
        assert_owner(name, &result.claims, ID);
    }
    assert_eq!(
        lookup("othermy-web.arcbox.local", &result.claims),
        Resolution::Unmatched
    );
}

#[test]
fn compose_replicas_share_service_names_but_custom_names_remain_exclusive() {
    let smaller = format!("{}0", "a".repeat(63));
    let larger = format!("{}1", "a".repeat(63));
    let labels = json!({
        "com.docker.compose.project": "Project",
        "com.docker.compose.service": "Web",
        "dev.arcbox.domains": "shared.local"
    });
    let suffix = "arcbox.local".parse().unwrap();
    let mut claims = Vec::new();
    for (id, name) in [(&larger, "project-web-2"), (&smaller, "project-web-1")] {
        let result = container(id, name, labels.clone()).claims(&suffix, AUTHORITY);
        assert_eq!(result.warnings, Vec::<String>::new());
        claims.extend(result.claims);
    }
    for _ in 0..2 {
        for name in [
            "web.project.arcbox.local",
            "nested.web.project.arcbox.local",
        ] {
            assert_owner(name, &claims, &smaller);
        }
        assert_owner("project-web-2.arcbox.local", &claims, &larger);
        assert_eq!(lookup("shared.local", &claims), Resolution::Conflict);
        claims.reverse();
    }
}

#[test]
fn custom_csv_retains_valid_entries_and_diagnoses_each_rejected_entry() {
    let result = container(
        ID,
        "web",
        json!({ "dev.arcbox.domains": " APP.LOCAL , *.DEV.LOCAL, bad_name.local,api.test," }),
    )
    .claims(&"arcbox.local".parse().unwrap(), AUTHORITY);
    assert_eq!(result.claims.len(), 4);
    for name in ["app.local", "x.dev.local", "a.b.dev.local"] {
        assert_owner(name, &result.claims, ID);
    }
    for name in ["x.app.local", "dev.local", "bad-name.local", "api.test"] {
        assert_eq!(lookup(name, &result.claims), Resolution::Unmatched);
    }
    assert_eq!(result.warnings.len(), 3);
    for rejected in ["bad_name.local", "api.test", "entry \"\""] {
        assert!(result.warnings.iter().any(|warning| {
            warning.contains("dev.arcbox.domains") && warning.contains(rejected)
        }));
    }
}

#[test]
fn oneoff_containers_claim_only_their_unique_name() {
    for oneoff in ["true", "True", "tRuE"] {
        let container = container(
            ID,
            "project-web-run-123",
            json!({
                "com.docker.compose.project": "project",
                "com.docker.compose.service": "web",
                "com.docker.compose.oneoff": oneoff,
                "dev.arcbox.domains": "app.local,*.dev.local,bad_name.local"
            }),
        );
        assert_eq!(container.default_aliases(), ["project-web-run-123"]);
        let result = container.claims(&"arcbox.local".parse().unwrap(), AUTHORITY);
        assert_eq!(result.warnings, Vec::<String>::new());
        assert_eq!(result.claims.len(), 2);
        assert_owner("project-web-run-123.arcbox.local", &result.claims, ID);
        assert_owner("a.project-web-run-123.arcbox.local", &result.claims, ID);
        for name in ["web.project.arcbox.local", "app.local", "x.dev.local"] {
            assert_eq!(lookup(name, &result.claims), Resolution::Unmatched);
        }
    }
}

#[test]
fn invalid_generated_names_do_not_discard_other_claims_or_gain_a_fallback() {
    for (name, service, valid, rejected, rewritten) in [
        (
            "project_web_1",
            "web",
            "web.project.arcbox.local",
            "project_web_1",
            "project-web-1.arcbox.local",
        ),
        (
            "project-web-1",
            "web_api",
            "project-web-1.arcbox.local",
            "web_api.project",
            "web-api.project.arcbox.local",
        ),
    ] {
        let result = container(
            ID,
            name,
            json!({
                "com.docker.compose.project": "project",
                "com.docker.compose.service": service,
                "dev.arcbox.domains": "app.local"
            }),
        )
        .claims(&"arcbox.local".parse().unwrap(), AUTHORITY);
        assert_eq!(result.claims.len(), 3);
        assert_owner(valid, &result.claims, ID);
        assert_owner("app.local", &result.claims, ID);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains(rejected));
        assert_eq!(lookup(rewritten, &result.claims), Resolution::Unmatched);
        assert_eq!(
            lookup(&format!("{}.arcbox.local", &ID[..12]), &result.claims),
            Resolution::Unmatched
        );
    }
}

#[test]
fn maximum_length_generated_names_keep_exact_claims_without_impossible_wildcards() {
    let suffix: Domain = format!(
        "{}.{}.{}.local",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63)
    )
    .parse()
    .unwrap();
    for length in [251, 252, 253, 254] {
        let alias_length = length - suffix.as_str().len() - 1;
        let name = "n".repeat(alias_length);
        let project = "p".repeat(alias_length - 2);
        let result = container(
            ID,
            &name,
            json!({
                "com.docker.compose.project": project,
                "com.docker.compose.service": "s"
            }),
        )
        .claims(&suffix, AUTHORITY);
        if length > 253 {
            assert_eq!(result.claims, Vec::<Claim>::new());
            assert_eq!(result.warnings.len(), 2);
            continue;
        }
        assert_eq!(result.warnings, Vec::<String>::new());
        assert_eq!(result.claims.len(), if length == 251 { 4 } else { 2 });
        for alias in [name, format!("s.{project}")] {
            let domain: Domain = format!("{alias}.{suffix}").parse().unwrap();
            assert_eq!(domain.as_str().len(), length);
            assert!(
                result
                    .claims
                    .iter()
                    .any(|claim| { claim.pattern() == &NamePattern::Exact(domain.clone()) })
            );
            if length == 251 {
                assert_owner(&format!("x.{domain}"), &result.claims, ID);
            }
        }
    }
}
