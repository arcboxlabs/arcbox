use super::*;
use arcbox_dns::{NamePattern, registry::Lookup};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::time::timeout;

#[test]
fn network_changes_reinspect_the_container_instead_of_the_network() {
    for action in ["connect", "disconnect"] {
        let mut event = serde_json::json!({
            "Type": "network", "Action": action,
            "Actor": { "ID": "network-id", "Attributes": { "container": "container-id" } }
        });
        assert_eq!(network_container_id(&event), Some("container-id"));
        event["Action"] = "destroy".into();
        assert_eq!(network_container_id(&event), None);
        event["Action"] = action.into();
        event["Actor"]["Attributes"]["container"] = "".into();
        assert_eq!(network_container_id(&event), None);
    }
}

#[tokio::test]
async fn disconnecting_the_last_network_removes_only_that_containers_dns_ownership() {
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new());
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
    let dns = GuestDnsServer::new(Default::default(), CancellationToken::new());
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
