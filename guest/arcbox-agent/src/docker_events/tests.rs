use super::*;
use arcbox_dns::{NamePattern, registry::Lookup};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::time::timeout;

async fn sync_dns(dns: &GuestDnsServer, info: &serde_json::Value) -> anyhow::Result<()> {
    let metadata =
        ContainerMetadata::from_inspect(info).context("test inspect omitted identity")?;
    super::sync_dns(
        &mut *dns.containers().write().await,
        dns.domain(),
        &metadata,
    )
}

const OLD_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WEB_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OFFLINE_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn inspect(id: &str, name: &str, ip: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": id, "Name": name, "State": { "Pid": 1 },
        "NetworkSettings": {
            "SandboxKey": "/var/run/docker/netns/test",
            "Networks": { "bridge": { "IPAddress": ip } }
        }
    })
}

#[test]
fn network_changes_reinspect_the_container_instead_of_the_network() {
    for action in ["connect", "disconnect"] {
        let mut event = serde_json::json!({
            "Type": "network", "Action": action,
            "Actor": { "ID": "network-id", "Attributes": { "container": "container-id" } }
        });
        assert_eq!(network_container_id(&event).unwrap(), Some("container-id"));
        event["Action"] = "destroy".into();
        assert_eq!(network_container_id(&event).unwrap(), None);
        event["Action"] = action.into();
        event["Actor"]["Attributes"]["container"] = "".into();
        assert!(network_container_id(&event).is_err());
    }
}

#[tokio::test]
async fn failed_rule_cleanup_retains_the_full_owner_until_retry_succeeds() {
    let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    sync_dns(
        &dns,
        &serde_json::json!({
            "Id": id, "Name": "/web",
            "NetworkSettings": { "Networks": { "bridge": { "IPAddress": "172.17.0.2" } } }
        }),
    )
    .await
    .unwrap();
    let registry = dns.containers();
    let (domains, mut commands) = DomainRoutes::test_channel();
    let mut sync = ContainerSync {
        dns: &dns,
        mirror: PublishMirror::new("eth0".into()),
        domains,
    };
    for (action, fail) in [("die", true), ("destroy", false)] {
        let event =
            serde_json::json!({ "Type": "container", "Action": action, "Actor": { "ID": id } })
                .to_string();
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(handle_event(&mut sync, &event), async {
                let crate::domains::Command::Forget(owner, reply) = commands.recv().await.unwrap()
                else {
                    panic!("cleanup must send Forget");
                };
                assert_eq!(owner, id);
                let entries = registry.read().await;
                let unavailable = entries
                    .get(id)
                    .expect("owner survives until cleanup completes");
                assert_eq!(unavailable.address, None);
                assert_eq!(unavailable.claims, []);
                drop(entries);
                reply
                    .send(if fail {
                        Err(anyhow::anyhow!("rule deletion failed"))
                    } else {
                        Ok(())
                    })
                    .unwrap();
            })
        })
        .await
        .unwrap();
        if fail {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("rule deletion failed")
            );
            assert!(registry.read().await.get(id).is_some());
        } else {
            result.unwrap();
            assert!(registry.read().await.get(id).is_none());
        }
    }
}

#[tokio::test]
async fn stopped_domain_task_cannot_accept_updates_or_discard_cleanup_ownership() {
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    let (domains, commands) = DomainRoutes::test_channel();
    drop(commands);
    let facts = ContainerFacts::from_inspect(
        &serde_json::json!({
            "Id": "owner", "Name": "/web", "State": { "Pid": 1 },
            "NetworkSettings": {
                "SandboxKey": "/var/run/docker/netns/web",
                "Networks": { "bridge": { "IPAddress": "172.17.0.2" } }
            }
        }),
        dns.domain(),
    )
    .unwrap();
    assert!(domains.update("owner", Some(facts)).await.is_err());
    let mut sync = ContainerSync {
        dns: &dns,
        mirror: PublishMirror::new("eth0".into()),
        domains,
    };
    let error = forget_container(&mut sync, "owner").await.unwrap_err();
    assert!(error.to_string().contains("rules task stopped"));
    assert!(dns.containers().read().await.get("owner").is_some());
}

#[tokio::test]
async fn disconnecting_the_last_network_removes_only_that_containers_dns_ownership() {
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    let mut inspect = serde_json::json!({
        "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Name": "/web-1",
        "Config": { "Labels": { "com.docker.compose.project": "demo", "com.docker.compose.service": "web" } },
        "NetworkSettings": { "Networks": { "app": { "IPAddress": "172.18.0.2" } } }
    });
    sync_dns(&dns, &inspect).await.unwrap();
    let mut replica = inspect.clone();
    replica["Id"] = "1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
    replica["Name"] = "/web-2".into();
    replica["NetworkSettings"]["Networks"]["app"]["IPAddress"] = "172.18.0.3".into();
    sync_dns(&dns, &replica).await.unwrap();
    inspect["NetworkSettings"]["Networks"] = serde_json::json!({});
    sync_dns(&dns, &inspect).await.unwrap();
    let entries = dns.containers();
    let entries = entries.read().await;
    assert_eq!(
        entries
            .get(inspect["Id"].as_str().unwrap())
            .unwrap()
            .address,
        None
    );
    assert!(matches!(
        entries.resolve(&"web-1.arcbox.local".parse().unwrap()),
        Lookup::Unmatched
    ));
    assert!(
        matches!(entries.resolve(&"web.demo.arcbox.local".parse().unwrap()), Lookup::Matched { claim, address, .. } if claim.owner() == replica["Id"].as_str().unwrap() && address == "172.18.0.3".parse::<std::net::IpAddr>().unwrap())
    );
}

#[tokio::test]
async fn dns_uses_network_name_order_and_rejects_invalid_names_individually() {
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    let mut inspect = serde_json::json!({
        "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Name": "/MyProject_Web_1",
        "Config": { "Labels": {
            "com.docker.compose.project": "MyProject",
            "com.docker.compose.service": "Web",
            "dev.arcbox.domains": "API.LOCAL, bad_name.local",
        } },
        "NetworkSettings": {}
    });
    for networks in [
        serde_json::json!({
            "z": { "IPAddress": "172.17.0.2" },
            "a": { "IPAddress": "172.18.0.3" },
        }),
        serde_json::json!({
            "a": { "IPAddress": "172.18.0.3" },
            "z": { "IPAddress": "172.17.0.2" },
        }),
    ] {
        inspect["NetworkSettings"]["Networks"] = networks;
        let metadata = ContainerMetadata::from_inspect(&inspect).unwrap();
        assert_eq!(
            metadata.default_aliases(),
            ["Web.MyProject", "MyProject_Web_1"]
        );
        assert_eq!(metadata.claims(dns.domain(), "container").warnings.len(), 2);
        sync_dns(&dns, &inspect).await.unwrap();
        let entries = dns.containers();
        let entries = entries.read().await;
        let registration = entries.get(metadata.id.as_str()).unwrap();
        assert_eq!(registration.address, Some("172.18.0.3".parse().unwrap()));
        let names: Vec<_> = registration
            .claims
            .iter()
            .filter_map(|claim| match claim.pattern() {
                NamePattern::Exact(name) => Some(name.as_str()),
                NamePattern::Descendants(_) => None,
            })
            .collect();
        assert_eq!(names, ["web.myproject.arcbox.local", "api.local"]);
    }
}

async fn request(listener: &UnixListener) -> (BufReader<UnixStream>, String) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut stream = BufReader::new(stream);
    let mut request = String::new();
    stream.read_line(&mut request).await.unwrap();
    loop {
        let mut line = String::new();
        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            return (stream, request);
        }
    }
}

async fn reply(stream: &mut BufReader<UnixStream>, status: u16, body: &str) {
    let response = format!(
        "HTTP/1.0 {status} Test\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .get_mut()
        .write_all(response.as_bytes())
        .await
        .unwrap();
    stream.get_mut().shutdown().await.unwrap();
}

#[tokio::test]
async fn subscription_precedes_snapshot_and_preserves_events_during_listing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let event = r#"{"Type":"container","Action":"die","Actor":{"ID":"full-id","Attributes":{"name":"web"}}}"#;
    let client = async {
        let mut lines = subscribe_and_reconcile(&socket, &cancel, async {
            docker_get_at(&socket, "/containers/json").await?;
            Ok(())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            read_line_or_cancel(&mut lines, &cancel).await.unwrap(),
            Some(format!("{:x}", event.len() + 1))
        );
        assert_eq!(
            read_line_or_cancel(&mut lines, &cancel)
                .await
                .unwrap()
                .as_deref(),
            Some(event)
        );
    };
    let server = async {
        let (mut events, path) = request(&listener).await;
        assert!(path.starts_with("GET /events?"));
        events
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\n")
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        events
            .get_mut()
            .write_all(b"Transfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        let (mut listing, path) = request(&listener).await;
        assert_eq!(path, "GET /containers/json HTTP/1.0\r\n");
        let chunk = format!("{:x}\r\n{event}\n\r\n", event.len() + 1);
        events.get_mut().write_all(chunk.as_bytes()).await.unwrap();
        reply(&mut listing, 200, "[]").await;
    };
    timeout(Duration::from_secs(2), async {
        tokio::join!(client, server)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_listing_returns_while_the_event_stream_remains_open() {
    for (status, body) in [(500, "[]"), (200, "not json")] {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket = tmp.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        let client = async {
            let result = subscribe_and_reconcile(&socket, &cancel, async {
                docker_get_at(&socket, "/containers/json").await?;
                Ok(())
            })
            .await;
            let error = format!("{:#}", result.expect_err("listing must fail"));
            assert!(error.contains("/containers/json"));
            if status == 500 {
                assert!(error.contains("500"));
            }
            cancel.cancel();
        };
        let server = async {
            let (mut events, _) = request(&listener).await;
            events
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\n\r\n")
                .await
                .unwrap();
            let (mut listing, _) = request(&listener).await;
            reply(&mut listing, status, body).await;
            cancel.cancelled().await;
        };
        timeout(Duration::from_secs(2), async {
            tokio::join!(client, server)
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn rejected_subscription_does_not_start_reconciliation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let cancel = CancellationToken::new();
    let client = async {
        let result = subscribe_and_reconcile(&socket, &cancel, async {
            panic!("a rejected subscription must not run reconciliation")
        })
        .await;
        assert!(result.is_err());
    };
    let server = async {
        let (mut events, _) = request(&listener).await;
        reply(&mut events, 500, "[]").await;
    };
    timeout(Duration::from_secs(2), async {
        tokio::join!(client, server)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn incomplete_snapshots_never_publish_a_valid_prefix_or_discard_old_owners() {
    let listing = serde_json::json!([{ "Id": WEB_ID }, { "Id": OFFLINE_ID }]).to_string();
    let first = inspect(WEB_ID, "/new-web", "172.17.0.4").to_string();
    let mut cases = vec![
        (500, "[]".into(), vec![]),
        (200, "not json".into(), vec![]),
        (200, "{}".into(), vec![]),
        (200, r#"[{"Id":""}]"#.into(), vec![]),
        (
            200,
            serde_json::json!([{ "Id": WEB_ID }, {}]).to_string(),
            vec![],
        ),
    ];
    for (status, body) in [
        (500, "{}".into()),
        (200, "not json".into()),
        (200, serde_json::json!({ "Id": OFFLINE_ID }).to_string()),
        (200, inspect(OFFLINE_ID, "", "172.17.0.5").to_string()),
        (
            200,
            inspect(WEB_ID, "/wrong-owner", "172.17.0.5").to_string(),
        ),
        (
            200,
            serde_json::json!({ "Name": "/missing-id" }).to_string(),
        ),
    ] {
        cases.push((
            200,
            listing.clone(),
            vec![(200, first.clone()), (status, body)],
        ));
    }
    for (status, listing, inspections) in cases {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket = tmp.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
        for (id, name) in [(OLD_ID, "/absent"), (WEB_ID, "/old-web")] {
            sync_dns(&dns, &inspect(id, name, "172.17.0.2"))
                .await
                .unwrap();
        }
        let registry = dns.containers();
        let epoch = registry.read().await.epoch();
        let (domains, mut commands) = DomainRoutes::test_channel();
        let mut sync = ContainerSync {
            dns: &dns,
            mirror: PublishMirror::new("eth0".into()),
            domains,
        };
        let server = async {
            let (mut stream, path) = request(&listener).await;
            assert_eq!(path, "GET /containers/json HTTP/1.0\r\n");
            reply(&mut stream, status, &listing).await;
            for (id, (status, body)) in [WEB_ID, OFFLINE_ID].into_iter().zip(inspections) {
                let (mut stream, path) = request(&listener).await;
                assert_eq!(path, format!("GET /containers/{id}/json HTTP/1.0\r\n"));
                assert_eq!(registry.read().await.epoch(), epoch);
                assert!(matches!(
                    commands.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));
                reply(&mut stream, status, &body).await;
            }
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(reconcile_existing(&mut sync, &socket), server)
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert_eq!(registry.read().await.epoch(), epoch);
        assert!(matches!(
            commands.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn complete_snapshot_refreshes_known_owners_and_keeps_unaddressed_identity() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    for (id, name) in [(OLD_ID, "/absent"), (WEB_ID, "/old-web")] {
        sync_dns(&dns, &inspect(id, name, "172.17.0.2"))
            .await
            .unwrap();
    }
    let registry = dns.containers();
    let epoch = registry.read().await.epoch();
    let web = inspect(WEB_ID, "/new-web", "172.17.0.4");
    let (domains, mut commands) = DomainRoutes::test_channel();
    let mut sync = ContainerSync {
        dns: &dns,
        mirror: PublishMirror::new("eth0".into()),
        domains,
    };
    let server = async {
        let (mut stream, _) = request(&listener).await;
        reply(
            &mut stream,
            200,
            &serde_json::json!([{ "Id": WEB_ID }, { "Id": OFFLINE_ID }]).to_string(),
        )
        .await;
        for info in [&web, &inspect(OFFLINE_ID, "/offline", "")] {
            let (mut stream, path) = request(&listener).await;
            assert_eq!(
                path,
                format!(
                    "GET /containers/{}/json HTTP/1.0\r\n",
                    info["Id"].as_str().unwrap()
                )
            );
            reply(&mut stream, 200, &info.to_string()).await;
        }
        let crate::domains::Command::Reconcile(facts, reply) = commands.recv().await.unwrap()
        else {
            panic!("snapshot must replace actor tracking before publication");
        };
        assert_eq!(
            facts,
            [ContainerFacts::from_inspect(&web, dns.domain()).unwrap()]
        );
        assert_eq!(registry.read().await.epoch(), epoch);
        reply.send(()).unwrap();
        let crate::domains::Command::Forget(id, reply) = commands.recv().await.unwrap() else {
            panic!("absent owner must be cleaned up");
        };
        assert_eq!(id, OLD_ID);
        let entries = registry.read().await;
        assert_eq!(entries.get(OLD_ID).unwrap().address, None);
        assert_eq!(entries.get(OFFLINE_ID).unwrap().address, None);
        assert_ne!(entries.get(OFFLINE_ID).unwrap().claims, []);
        assert!(
            matches!(entries.resolve(&"new-web.arcbox.local".parse().unwrap()), Lookup::Matched { claim, address, .. }
            if claim.owner() == WEB_ID && address == "172.17.0.4".parse::<std::net::IpAddr>().unwrap())
        );
        assert!(matches!(
            entries.resolve(&"old-web.arcbox.local".parse().unwrap()),
            Lookup::Unmatched
        ));
        drop(entries);
        reply.send(Ok(())).unwrap();
        let crate::domains::Command::Forget(id, reply) = commands.recv().await.unwrap() else {
            panic!("unaddressed owner must remove routes");
        };
        assert_eq!(id, OFFLINE_ID);
        reply.send(Ok(())).unwrap();
    };
    let (result, ()) = timeout(Duration::from_secs(2), async {
        tokio::join!(reconcile_existing(&mut sync, &socket), server)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(registry.read().await.get(OLD_ID).is_none());
    assert!(registry.read().await.get(OFFLINE_ID).is_some());
}

#[tokio::test]
async fn empty_snapshot_disables_all_absent_names_and_retries_failed_cleanup() {
    let tmp = tempfile::TempDir::new().unwrap();
    let socket = tmp.path().join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
    for (id, name) in [(OLD_ID, "/absent"), (WEB_ID, "/web")] {
        sync_dns(&dns, &inspect(id, name, "172.17.0.2"))
            .await
            .unwrap();
    }
    let registry = dns.containers();
    let (domains, mut commands) = DomainRoutes::test_channel();
    let mut sync = ContainerSync {
        dns: &dns,
        mirror: PublishMirror::new("eth0".into()),
        domains,
    };
    for fail in [true, false] {
        let server = async {
            let (mut stream, path) = request(&listener).await;
            assert_eq!(path, "GET /containers/json HTTP/1.0\r\n");
            reply(&mut stream, 200, "[]").await;
            let crate::domains::Command::Reconcile(facts, reply) = commands.recv().await.unwrap()
            else {
                panic!("empty snapshot must stop actor tracking before cleanup");
            };
            assert_eq!(facts, []);
            reply.send(()).unwrap();
            let mut cleaned = BTreeSet::new();
            for _ in 0..if fail { 1 } else { 2 } {
                let crate::domains::Command::Forget(id, reply) = commands.recv().await.unwrap()
                else {
                    panic!("empty snapshot must remove routes");
                };
                assert!([OLD_ID, WEB_ID].contains(&id.as_str()));
                cleaned.insert(id);
                let entries = registry.read().await;
                for owner in entries.owners() {
                    let entry = entries.get(owner).unwrap();
                    assert_eq!(entry.address, None);
                    assert_eq!(entry.claims, []);
                }
                for name in ["absent.arcbox.local", "web.arcbox.local"] {
                    assert!(matches!(
                        entries.resolve(&name.parse().unwrap()),
                        Lookup::Unmatched
                    ));
                }
                drop(entries);
                reply
                    .send(if fail {
                        Err(anyhow::anyhow!("cleanup failed"))
                    } else {
                        Ok(())
                    })
                    .unwrap();
            }
            if !fail {
                assert_eq!(
                    cleaned,
                    BTreeSet::from([OLD_ID.to_owned(), WEB_ID.to_owned()])
                );
            }
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(reconcile_existing(&mut sync, &socket), server)
        })
        .await
        .unwrap();
        if fail {
            assert!(result.unwrap_err().to_string().contains("cleanup failed"));
            assert_eq!(registry.read().await.owners().count(), 2);
        } else {
            result.unwrap();
            assert_eq!(registry.read().await.owners().count(), 0);
        }
    }
}

#[tokio::test]
async fn inspected_events_wait_for_actor_ack_before_dns_and_keep_unavailable_owners_on_cleanup_error()
 {
    for ip in ["172.17.0.4", ""] {
        let tmp = tempfile::TempDir::new().unwrap();
        let socket = tmp.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let dns = GuestDnsServer::new(Default::default(), CancellationToken::new()).unwrap();
        sync_dns(&dns, &inspect(WEB_ID, "/old-web", "172.17.0.2"))
            .await
            .unwrap();
        let registry = dns.containers();
        let epoch = registry.read().await.epoch();
        let info = inspect(WEB_ID, "/new-web", ip);
        let (domains, mut commands) = DomainRoutes::test_channel();
        let mut sync = ContainerSync {
            dns: &dns,
            mirror: PublishMirror::new("eth0".into()),
            domains,
        };
        let server = async {
            let (mut stream, path) = request(&listener).await;
            assert_eq!(path, format!("GET /containers/{WEB_ID}/json HTTP/1.0\r\n"));
            reply(&mut stream, 200, &info.to_string()).await;
            let crate::domains::Command::Update(id, facts, reply) = commands.recv().await.unwrap()
            else {
                panic!("inspect event must await actor replacement");
            };
            assert_eq!(id, WEB_ID);
            assert_eq!(facts, ContainerFacts::from_inspect(&info, dns.domain()));
            assert_eq!(registry.read().await.epoch(), epoch);
            reply.send(()).unwrap();
            if ip.is_empty() {
                let crate::domains::Command::Forget(id, reply) = commands.recv().await.unwrap()
                else {
                    panic!("unaddressed container must remove rules");
                };
                assert_eq!(id, WEB_ID);
                assert_eq!(registry.read().await.get(WEB_ID).unwrap().address, None);
                reply.send(Err(anyhow::anyhow!("cleanup failed"))).unwrap();
            }
        };
        let (result, ()) = timeout(Duration::from_secs(2), async {
            tokio::join!(register_container_by_id(&mut sync, &socket, WEB_ID), server)
        })
        .await
        .unwrap();
        if ip.is_empty() {
            assert!(result.unwrap_err().to_string().contains("cleanup failed"));
        } else {
            result.unwrap();
        }
        let entries = registry.read().await;
        let expected = ip.parse::<std::net::IpAddr>().ok();
        assert_eq!(entries.get(WEB_ID).unwrap().address, expected);
        assert!(matches!(
            entries.resolve(&"old-web.arcbox.local".parse().unwrap()),
            Lookup::Unmatched
        ));
    }
}
