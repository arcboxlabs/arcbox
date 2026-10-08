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
            path: "/containers/web/json",
            status: 200,
            body: INSPECT_WEB,
        },
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
            path: "/containers/abc123",
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
async fn complete_snapshot_rejects_failed_or_mismatched_inspects() {
    const LIST: &str = r#"[{"Id":"zzz789"},{"Id":"abc123"}]"#;
    const SECOND: &str = r#"{"Id":"zzz789","Name":"/second"}"#;
    for (listing, status, body, expected) in [
        (LIST, 500, SECOND, None),
        (LIST, 200, "not json", None),
        (LIST, 200, r#"{"Id":"zzz789"}"#, None),
        (LIST, 200, INSPECT_WEB, None),
        (LIST, 200, SECOND, Some(vec!["abc123", "zzz789"])),
        ("[]", 200, SECOND, Some(vec![])),
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let guest = mock_guest::start_with_routes(
            tmp.path(),
            vec![
                MockRoute {
                    method: "GET",
                    path: "/containers/json",
                    status: 200,
                    body: listing,
                },
                MockRoute {
                    method: "GET",
                    path: "/containers/abc123/json",
                    status: 200,
                    body: INSPECT_WEB,
                },
                MockRoute {
                    method: "GET",
                    path: "/containers/zzz789/json",
                    status,
                    body,
                },
            ],
        )
        .await;
        let client = arcbox_docker::proxy::GuestHttpClient::new(Arc::new(
            mock_guest::UnixSocketConnector::new(guest.socket_path.clone()),
        ));
        let result = arcbox_docker::guest_query::running_container_snapshot(&client).await;
        match expected {
            Some(ids) => {
                let snapshot = result.unwrap();
                assert_eq!(
                    snapshot
                        .iter()
                        .map(|entry| entry.metadata.id.as_str())
                        .collect::<Vec<_>>(),
                    ids
                );
            }
            None => assert!(matches!(result, Err(arcbox_docker::DockerError::Server(_)))),
        }
        assert_eq!(
            guest.last_request_uri().await.as_deref(),
            Some(if listing == "[]" {
                "/containers/json"
            } else {
                "/containers/zzz789/json"
            })
        );
        guest.cancel.cancel();
    }
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

    // The guest resolves the name before the canonical entry is torn down.
    let (status, _) = send(&router, "POST", "/containers/web/stop").await;
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
                method: "GET",
                path: "/containers/web/json",
                status: 200,
                body: INSPECT_WEB,
            },
            MockRoute {
                method: "POST",
                path: "/containers/abc123/rename",
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
        runtime.register_container_dns(&metadata).await.unwrap();
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
async fn network_refresh_is_idempotent_and_rename_replaces_the_old_alias() {
    let (router, runtime, _guest, _tmp) = router_with_mock_guest(vec![
        MockRoute {
            method: "POST",
            path: "/networks/bridge/connect",
            status: 200,
            body: "",
        },
        MockRoute {
            method: "GET",
            path: "/containers/web/json",
            status: 200,
            body: INSPECT_WEB,
        },
        MockRoute {
            method: "POST",
            path: "/containers/abc123/rename",
            status: 204,
            body: "",
        },
        MockRoute {
            method: "GET",
            path: "/containers/abc123/json",
            status: 200,
            body: INSPECT_WEB,
        },
        MockRoute {
            method: "GET",
            path: "/containers/abc123/json",
            status: 200,
            body: r#"{"Id":"abc123","Name":"/renamed","NetworkSettings":{"Networks":{"bridge":{"IPAddress":"172.17.0.2"}}}}"#,
        },
    ])
    .await;
    let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(
        &serde_json::from_str(INSPECT_WEB).unwrap(),
    )
    .unwrap();
    assert!(
        runtime
            .capture_container_snapshot()
            .await
            .unwrap()
            .unwrap()
            .commit(&[arcbox_core::runtime::ContainerNetwork {
                metadata,
                bindings: Vec::new(),
            }])
            .await
            .unwrap()
    );
    let captured = runtime
        .capture_web_guest(runtime.default_machine_name())
        .unwrap();
    let dns_revision = |name| {
        runtime
            .with_web_guest_claim(&captured, name, |_, revision| revision)
            .unwrap()
    };
    let revision = dns_revision("web.arcbox.local").unwrap();
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/networks/bridge/connect")
                .body(Body::from(r#"{"Container":"web"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(dns_revision("web.arcbox.local"), Some(revision));

    let (status, _) = send(&router, "POST", "/containers/web/rename?name=renamed").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(runtime.resolve_registered_container("web").await, None);
    assert_eq!(
        runtime
            .resolve_registered_container("renamed")
            .await
            .as_deref(),
        Some("abc123")
    );
    assert_eq!(dns_revision("web.arcbox.local"), None);
    assert_ne!(dns_revision("renamed.arcbox.local").unwrap(), revision);
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
async fn lifecycle_mutations_share_canonical_lock_and_preserve_query() {
    let actions = [
        ("POST", "/containers/abc123/start"),
        ("POST", "/containers/abc123/restart"),
        ("POST", "/containers/abc123/stop"),
        ("POST", "/containers/abc123/kill"),
        ("POST", "/containers/abc123/rename"),
        ("DELETE", "/containers/abc123"),
    ];
    let mut routes: Vec<_> = [
        "/containers/reused-name/json",
        "/containers/json/json",
        "/containers/abc/json",
        "/containers/abc123/json",
    ]
    .into_iter()
    .map(|path| MockRoute {
        method: "GET",
        path,
        status: 200,
        body: INSPECT_WEB,
    })
    .collect();
    routes.extend(actions.into_iter().map(|(method, path)| MockRoute {
        method,
        path,
        status: 409,
        body: "canonical mutation",
    }));
    let (router, runtime, guest, _tmp) = router_with_mock_guest(routes).await;
    for token in ["reused-name", "json", "abc"] {
        for (method, path) in actions {
            let guard = runtime.lock_container_lifecycle("abc123").await;
            let expected = format!("/v1.47{path}?t=7&name=new%2Dname&signal=SIGTERM");
            let uri = expected.replace("/abc123", &format!("/{token}"));
            let router = router.clone();
            let mut request = tokio::spawn(async move { send(&router, method, &uri).await });
            guest
                .wait_for_request_uri(&format!("/containers/{token}/json"))
                .await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), &mut request)
                    .await
                    .is_err()
            );
            drop(guard);
            let (status, _) = request.await.unwrap();
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(guest.last_request_uri().await, Some(expected));
        }
    }
}

#[tokio::test]
async fn invalid_identity_cannot_reach_a_guest_mutation() {
    for (guest_status, body, expected) in [
        (404, r#"{"message":"missing"}"#, StatusCode::NOT_FOUND),
        (200, r#"{"Id":""}"#, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let (router, _runtime, guest, _tmp) = router_with_mock_guest(vec![MockRoute {
            method: "GET",
            path: "/containers/missing/json",
            status: guest_status,
            body,
        }])
        .await;
        let (status, _) = send(&router, "DELETE", "/containers/missing?force=true").await;
        assert_eq!(status, expected);
        assert_eq!(
            guest.last_request_uri().await.as_deref(),
            Some("/containers/missing/json")
        );
    }
}

#[tokio::test]
async fn network_mutations_hold_the_canonical_lock_and_preserve_options() {
    let (router, runtime, guest, _tmp) = router_with_mock_guest(lifecycle_routes()).await;
    for operation in ["connect", "disconnect"] {
        let body = r#"{"Container":"web","Force":true,"EndpointConfig":{"Aliases":["alias"],"IPAMConfig":{}},"Extra":[1,"two"]}"#;
        let uri = format!("/networks/bridge/{operation}");
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("Content-Length", body.len())
            .body(Body::from(body))
            .unwrap();
        let guard = runtime.lock_container_lifecycle("abc123").await;
        let mut response = tokio::spawn(router.clone().oneshot(request));
        guest.wait_for_request_uri("/containers/web/json").await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut response)
                .await
                .is_err()
        );
        drop(guard);
        let response = response.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let echoed = response.into_body().collect().await.unwrap().to_bytes();
        let mut expected: serde_json::Value = serde_json::from_str(body).unwrap();
        expected["Container"] = "abc123".into();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&echoed).unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn only_forced_disconnect_can_remove_an_orphan_endpoint() {
    for (operation, force, inspect_status, expected) in [
        ("disconnect", true, 404, StatusCode::OK),
        ("disconnect", false, 404, StatusCode::NOT_FOUND),
        ("connect", true, 404, StatusCode::NOT_FOUND),
        ("disconnect", true, 500, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let (router, runtime, guest, _tmp) = router_with_mock_guest(vec![MockRoute {
            method: "GET",
            path: "/containers/orphan/json",
            status: inspect_status,
            body: r#"{"message":"absent"}"#,
        }])
        .await;
        let metadata = arcbox_dns::container::ContainerMetadata::from_inspect(
            &serde_json::from_str(INSPECT_WEB).unwrap(),
        )
        .unwrap();
        runtime.register_container_dns(&metadata).await.unwrap();
        let previous = runtime.network_manager().local_dns_entries();
        let body = format!("{{ \"Container\": \"orphan\", \"Force\": {force} }}");
        let uri = format!("/networks/bridge/{operation}");
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                body
            );
            assert_eq!(
                guest.last_request_uri().await.as_deref(),
                Some(uri.as_str())
            );
        } else {
            assert_eq!(
                guest.last_request_uri().await.as_deref(),
                Some("/containers/orphan/json")
            );
        }
        assert_eq!(runtime.network_manager().local_dns_entries(), previous);
    }
}
