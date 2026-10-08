use super::*;

#[test]
fn requested_roots_keep_offline_wildcards_conflicts_and_nested_names() {
    let mut registry = Registry::default();
    for (owner, pattern, address) in [
        ("offline", "*.child.app.local", None),
        ("first", "app.local", Some("10.0.0.1")),
        ("second", "app.local", Some("10.0.0.2")),
    ] {
        registry
            .replace(
                owner.into(),
                vec![arcbox_dns::claims::Claim::exclusive(
                    NamePattern::parse_custom(pattern).unwrap(),
                    owner.into(),
                    "containers".into(),
                )],
                address.map(|ip| ip.parse().unwrap()),
            )
            .unwrap();
    }
    assert_eq!(
        roots(&registry, &"configured.test".parse().unwrap()),
        ["app.local", "child.app.local", "configured.test"]
    );
}
