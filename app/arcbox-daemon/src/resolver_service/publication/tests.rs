mod lifecycle;

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

fn publisher() -> Publisher {
    Publisher {
        manager: Arc::new(NetworkManager::new(arcbox_net::NetConfig::default())),
        domain: "arcbox.local".parse().unwrap(),
        setup: Arc::new(arcbox_api::SetupState::new()),
        shutdown: CancellationToken::new(),
        socket: String::new(),
        request: ResolverLease {
            profile: "production".into(),
            nonce: [42; 16],
            revision: 1,
            port: 5553,
            roots: vec!["arcbox.local".into()],
        },
        authorities: Vec::new(),
    }
}

fn accepted(request: &ResolverLease) -> ResolverAuthoritySnapshot {
    ResolverAuthoritySnapshot {
        epoch: 1,
        roots: request
            .roots
            .iter()
            .map(|root| arcbox_helper::ResolverAuthorityRoot {
                root: root.clone(),
                owner: ResolverOwner::CurrentConnection,
                state: ResolverAuthorityState::Active {
                    port: request.port,
                    accepted_revision: request.revision,
                },
            })
            .collect(),
    }
}

fn instance(publisher: &Publisher, name: &str) -> (InstanceTarget, u64) {
    let registry = publisher.manager.dns_registry();
    let registry = registry.read().unwrap();
    match registry.resolve(&name.parse().unwrap()) {
        arcbox_dns::registry::Lookup::Instance {
            authority,
            revision,
        } => (authority.target, revision),
        lookup => panic!("expected instance authority, got {lookup:?}"),
    }
}

#[test]
fn invalid_snapshot_cannot_publish_an_unconfirmed_lease() {
    for mismatch in ["port", "revision", "root", "missing"] {
        let mut publisher = publisher();
        let mut snapshot = accepted(&publisher.request);
        match mismatch {
            "port" => {
                snapshot.roots[0].state = ResolverAuthorityState::Active {
                    port: 6000,
                    accepted_revision: publisher.request.revision,
                };
            }
            "revision" => {
                snapshot.roots[0].state = ResolverAuthorityState::Active {
                    port: publisher.request.port,
                    accepted_revision: 99,
                };
            }
            "root" => snapshot.roots[0].root = "unconfirmed.local".into(),
            "missing" => snapshot.roots.clear(),
            _ => unreachable!(),
        }
        let before = publisher.manager.dns_registry().read().unwrap().epoch();
        assert!(publisher.apply(snapshot).is_err(), "{mismatch}");
        assert_eq!(publisher.authorities, [] as [InstanceAuthority; 0]);
        assert_eq!(
            publisher.manager.dns_registry().read().unwrap().epoch(),
            before
        );
    }
}

#[test]
fn wire_revisions_preserve_effective_authority_and_external_changes_update_it() {
    let mut publisher = publisher();
    let mut snapshot = accepted(&publisher.request);
    snapshot.roots.push(arcbox_helper::ResolverAuthorityRoot {
        root: "team.arcbox.local".into(),
        owner: ResolverOwner::OtherConnection,
        state: ResolverAuthorityState::Active {
            port: 6000,
            accepted_revision: 7,
        },
    });
    assert!(publisher.apply(snapshot.clone()).unwrap());
    let original = instance(&publisher, "web.team.arcbox.local");
    assert_eq!(original.0, InstanceTarget::Other { port: 6000 });

    snapshot.epoch += 1;
    publisher.request.revision += 1;
    for root in &mut snapshot.roots {
        if let ResolverAuthorityState::Active {
            accepted_revision, ..
        } = &mut root.state
        {
            *accepted_revision += 1;
        }
    }
    assert!(publisher.apply(snapshot.clone()).unwrap());
    assert_eq!(instance(&publisher, "web.team.arcbox.local"), original);

    snapshot.epoch += 1;
    snapshot.roots[1].state = ResolverAuthorityState::RetainedUnavailable;
    assert!(publisher.apply(snapshot).unwrap());
    let changed = instance(&publisher, "web.team.arcbox.local");
    assert_eq!(changed.0, InstanceTarget::Unavailable);
    assert!(changed.1 > original.1);
}

#[test]
fn delayed_ack_requires_reconciliation_without_granting_an_unaccepted_root() {
    for (name, granted) in [("late.arcbox.local", true), ("late.local", false)] {
        let mut publisher = publisher();
        let snapshot = accepted(&publisher.request);
        let registry = publisher.manager.dns_registry();
        registry
            .write()
            .unwrap()
            .replace(
                "late".into(),
                vec![arcbox_dns::claims::Claim::exclusive(
                    NamePattern::Exact(name.parse().unwrap()),
                    "late".into(),
                    "containers".into(),
                )],
                Some("10.0.0.1".parse().unwrap()),
            )
            .unwrap();
        assert!(!publisher.apply(snapshot).unwrap());
        assert!(matches!(
            registry.read().unwrap().resolve(&name.parse().unwrap()),
            arcbox_dns::registry::Lookup::Matched { publishable, .. } if publishable == granted
        ));
    }
}

#[test]
fn failed_connection_retains_known_attempted_and_current_roots() {
    let mut publisher = publisher();
    let mut snapshot = accepted(&publisher.request);
    snapshot.roots.push(arcbox_helper::ResolverAuthorityRoot {
        root: "known.local".into(),
        owner: ResolverOwner::OtherConnection,
        state: ResolverAuthorityState::RetainedUnavailable,
    });
    assert!(publisher.apply(snapshot).unwrap());
    publisher.request.roots.push("attempted.local".into());
    let registry = publisher.manager.dns_registry();
    registry
        .write()
        .unwrap()
        .replace(
            "new".into(),
            vec![arcbox_dns::claims::Claim::exclusive(
                NamePattern::Exact("current.local".parse().unwrap()),
                "new".into(),
                "containers".into(),
            )],
            Some("10.0.0.1".parse().unwrap()),
        )
        .unwrap();
    publisher.unavailable();
    for root in [
        "arcbox.local",
        "known.local",
        "attempted.local",
        "current.local",
    ] {
        assert_eq!(instance(&publisher, root).0, InstanceTarget::Unavailable);
    }
    assert_eq!(
        publisher.manager.local_dns_entries(),
        [] as [(String, std::net::IpAddr); 0]
    );
}
