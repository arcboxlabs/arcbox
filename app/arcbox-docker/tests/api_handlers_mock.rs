//! Handler-level Docker API tests against a mock guest dockerd — run in CI.
//!
//! Unlike the `api_*.rs` suites (which need a running daemon + guest and are
//! `#[ignore]`d), these build the real `create_router` with a `skip_vm_check`
//! runtime and a mock guest serving canned responses, so the full
//! handler → readiness → proxy path executes offline, including host-side
//! effects (DNS registration, name aliases, teardown).

#[path = "support/api.rs"]
mod api_support;
use api_support::*;
use http_body_util::BodyExt;

const INSPECT_WEB: &str = r#"{
    "Id": "abc123",
    "Name": "/web",
    "Config": {"Labels": {}},
    "HostConfig": {"ReadonlyRootfs": false},
    "State": {"Running": false},
    "Mounts": [],
    "NetworkSettings": {"Networks": {"bridge": {"IPAddress": "172.17.0.2"}}}
}"#;

fn lifecycle_routes() -> Vec<MockRoute> {
    vec![
        MockRoute {
            method: "GET",
            path: "/containers/json",
            status: 200,
            body: "[]",
        },
        MockRoute {
            method: "GET",
            path: "/containers/abc123/json",
            status: 200,
            body: INSPECT_WEB,
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/start",
            status: 204,
            body: "",
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/stop",
            status: 204,
            body: "",
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/kill",
            status: 204,
            body: "",
        },
        MockRoute {
            method: "DELETE",
            path: "/containers/web",
            status: 204,
            body: "",
        },
        MockRoute {
            method: "GET",
            path: "/containers/nope/json",
            status: 404,
            body: r#"{"message":"No such container: nope"}"#,
        },
    ]
}

async fn send(router: &axum::Router, method: &str, uri: &str) -> (StatusCode, bytes::Bytes) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    // Mirror the production `MapRequestLayer(strip_api_version_prefix)` that
    // wraps the router in DockerApiServer — without it, versioned paths would
    // bypass the local handlers and silently fall through to the proxy.
    let request = arcbox_docker::api::strip_api_version_prefix(request);
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body)
}

#[tokio::test]
async fn ping_and_list_proxy_through() {
    let (router, _runtime, _guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;

    // /_ping is answered by the mock's echo fallback (200, empty body).
    let (status, _) = send(&router, "GET", "/_ping").await;
    assert_eq!(status, StatusCode::OK);

    // Container list proxies to the canned empty list.
    let (status, body) = send(&router, "GET", "/containers/json").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().map(Vec::len), Some(0));

    // A versioned path proxies too (guest dockerd strips its own prefix).
    let (status, _) = send(&router, "GET", "/v1.47/containers/json").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn guest_error_status_passes_through() {
    let (router, _runtime, _guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;

    let (status, body) = send(&router, "GET", "/containers/nope/json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("No such container"));
}

#[tokio::test]
async fn malformed_successful_listing_is_a_query_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let guest = mock_guest::start_with_routes(
        tmp.path(),
        vec![MockRoute {
            method: "GET",
            path: "/containers/json",
            status: 200,
            body: "not json",
        }],
    )
    .await;
    let client = arcbox_docker::proxy::GuestHttpClient::new(Arc::new(
        mock_guest::UnixSocketConnector::new(guest.socket_path.clone()),
    ));

    let result = arcbox_docker::guest_query::list_running_container_ids(&client).await;

    assert!(matches!(result, Err(arcbox_docker::DockerError::Server(_))));
    guest.cancel.cancel();
}

#[tokio::test]
async fn start_stop_cycle_manages_host_networking() {
    let (router, runtime, _guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;

    // Start: proxied 204, then the handler inspects the container and
    // registers host state — DNS keyed by canonical ID plus the name alias.
    let (status, _) = send(&router, "POST", "/containers/abc123/start").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(runtime.registered_container_ids().await.contains("abc123"));
    assert_eq!(
        runtime.resolve_registered_container("web").await.as_deref(),
        Some("abc123"),
        "name alias must resolve after start"
    );

    // Stop BY NAME: the registry resolves the alias without a guest inspect
    // and tears down the canonical entry.
    let (status, _) = send(&router, "POST", "/containers/abc123/stop").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        runtime.registered_container_ids().await.is_empty(),
        "stop must tear down host networking"
    );
    assert_eq!(runtime.resolve_registered_container("web").await, None);
}

#[tokio::test]
async fn successful_rename_with_invalid_inspect_preserves_previous_host_ownership() {
    for body in ["not json", r#"{"Name":"/renamed"}"#, r#"{"Id":"abc123"}"#] {
        let routes = vec![
            MockRoute {
                method: "POST",
                path: "/containers/web/rename",
                status: 204,
                body: "",
            },
            MockRoute {
                method: "GET",
                path: "/containers/abc123/json",
                status: 200,
                body,
            },
        ];
        let (router, runtime, guest, _tmp) = router_with_mock_guest(routes).await;
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(
            &serde_json::from_str(INSPECT_WEB).unwrap(),
        )
        .unwrap();
        runtime.register_container_dns(&metadata).await;
        let previous = runtime.network_manager().local_dns_entries();

        let (status, error) = send(&router, "POST", "/containers/web/rename?name=renamed").await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(String::from_utf8_lossy(&error).contains("parse /containers/abc123/json body"));
        assert_eq!(
            guest.last_request_uri().await.as_deref(),
            Some("/containers/abc123/json")
        );
        assert_eq!(runtime.network_manager().local_dns_entries(), previous);
        assert_eq!(
            runtime.resolve_registered_container("web").await.as_deref(),
            Some("abc123")
        );
        assert_eq!(runtime.resolve_registered_container("renamed").await, None);
    }
}

#[tokio::test]
async fn versioned_remove_by_name_runs_local_teardown() {
    let (router, runtime, _guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;

    let (status, _) = send(&router, "POST", "/containers/abc123/start").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(runtime.registered_container_ids().await.contains("abc123"));

    // DELETE by NAME through a VERSIONED path. This proves two things at the
    // behavior level: the version-strip layer routes versioned requests into
    // the local handlers (not silently past them to the fallback), and
    // `DELETE /containers/{id}` reaches the local remove handler whose
    // teardown clears the registry via alias resolution.
    let (status, _) = send(&router, "DELETE", "/v1.47/containers/web").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        runtime.registered_container_ids().await.is_empty(),
        "remove must run local host teardown, resolved from the name alias"
    );
}

#[tokio::test]
async fn non_fatal_kill_keeps_host_networking() {
    let (router, runtime, _guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;

    let (status, _) = send(&router, "POST", "/containers/abc123/start").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(runtime.registered_container_ids().await.contains("abc123"));

    // SIGHUP reloads the container; it keeps running — host networking must
    // survive even though Docker returns 204.
    let (status, _) = send(&router, "POST", "/containers/abc123/kill?signal=SIGHUP").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        runtime.registered_container_ids().await.contains("abc123"),
        "a non-fatal kill signal must not tear down host networking"
    );

    // Default kill (SIGKILL) terminates → teardown.
    let (status, _) = send(&router, "POST", "/containers/abc123/kill").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(runtime.registered_container_ids().await.is_empty());
}

#[tokio::test]
async fn start_and_restart_pin_identity_and_preserve_query() {
    let routes = vec![
        MockRoute {
            method: "GET",
            path: "/containers/reused-name/json",
            status: 200,
            body: INSPECT_WEB,
        },
        MockRoute {
            method: "GET",
            path: "/containers/abc123/json",
            status: 200,
            body: INSPECT_WEB,
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/start",
            status: 409,
            body: "canonical start reached",
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/restart",
            status: 409,
            body: "canonical restart reached",
        },
    ];
    let (router, _runtime, guest, _tmp) = router_with_mock_guest(routes).await;
    for action in ["start", "restart"] {
        let uri = format!("/v1.47/containers/reused-name/{action}?t=7&signal=SIGTERM");
        let (status, _) = send(&router, "POST", &uri).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            guest.last_request_uri().await,
            Some(format!(
                "/v1.47/containers/abc123/{action}?t=7&signal=SIGTERM"
            ))
        );
    }
}
