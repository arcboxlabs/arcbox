use super::*;
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
        "Name": "/web-1",
        "Config": { "Labels": { "com.docker.compose.project": "demo", "com.docker.compose.service": "web" } },
        "NetworkSettings": { "Networks": { "app": { "IPAddress": "172.18.0.2" } } }
    });
    sync_dns(&dns, &inspect).await;
    dns.register_container("web.demo", "web-2", "172.18.0.3".parse().unwrap())
        .await;
    inspect["NetworkSettings"]["Networks"] = serde_json::json!({});
    sync_dns(&dns, &inspect).await;
    let entries = dns.containers();
    let entries = entries.read().await;
    assert!(!entries.contains_key("web-1"));
    assert_eq!(
        entries.get("web.demo"),
        Some(&"172.18.0.3".parse().unwrap())
    );
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
