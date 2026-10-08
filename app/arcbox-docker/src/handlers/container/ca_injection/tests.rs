//! Isolated guest protocol checks; no host trust changes.

mod concurrency;
mod guest;

use axum::http::Method;
use serde_json::json;

use super::test_support::{root, tar, untar};
use super::*;

fn metadata() -> serde_json::Value {
    json!({"Id":"canonical-test", "Config":{"Env":null,"Labels":null}, "HostConfig":{"ReadonlyRootfs":false}, "State":{"Running":false}, "Mounts":[]})
}

async fn reconcile_with_pem(state: &AppState, id: &str, pem: &[u8]) -> Result<Vec<String>> {
    let pem = pem.to_vec();
    reconcile_with_root(state, id, move || Ok(Some(pem))).await
}

#[tokio::test]
async fn guest_reconciliation_is_idempotent_and_rotates_the_owned_root() {
    let guest = guest::Guest::new(metadata()).await;
    let old = root();
    let new = root();
    assert_eq!(
        reconcile_with_pem(&guest.state, "test", &old)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        reconcile_with_pem(&guest.state, "test", &old)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(guest.data.lock().unwrap().writes, 1);
    assert_eq!(
        reconcile_with_pem(&guest.state, "test", &new)
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    let data = guest.data.lock().unwrap();
    assert_eq!(data.writes, 2);
    assert!(data.bundle.starts_with(b"original roots\n"));
    assert!(
        !String::from_utf8_lossy(&data.bundle).contains(std::str::from_utf8(&old).unwrap().trim())
    );
}

#[tokio::test]
async fn caller_policy_and_symbolic_paths_prevent_writes() {
    for (pointer, value) in [
        ("/Config/Labels", json!({OPT_OUT:"false"})),
        ("/Config/Env", json!(["SSL_CERT_FILE=/custom.pem"])),
        ("/HostConfig/ReadonlyRootfs", json!(true)),
        ("/State/Running", json!(true)),
        ("/Mounts", json!([{"Destination":"/etc/ssl/certs"}])),
    ] {
        let mut config = metadata();
        *config.pointer_mut(pointer).unwrap() = value;
        let guest = guest::Guest::new(config).await;
        let warnings = reconcile_with_pem(&guest.state, "test", &root())
            .await
            .unwrap();
        assert_eq!(warnings.is_empty(), pointer == "/Config/Labels");
        assert_eq!(guest.data.lock().unwrap().writes, 0);
    }
    let guest = guest::Guest::new(metadata()).await;
    guest.data.lock().unwrap().symlink = Some("/etc/ssl".into());
    assert_ne!(
        reconcile_with_pem(&guest.state, "test", &root())
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(guest.data.lock().unwrap().writes, 0);
}

#[tokio::test]
async fn write_failures_are_errors_and_missing_publication_keeps_create_id() {
    let guest = guest::Guest::new(metadata()).await;
    guest.data.lock().unwrap().reject_write = true;
    assert!(
        reconcile_with_pem(&guest.state, "test", &root())
            .await
            .unwrap_err()
            .to_string()
            .contains("403")
    );
    let response = Response::builder()
        .status(201)
        .header(header::CONTENT_LENGTH, "57")
        .body(Body::from(
            r#"{"Id":"test","Warnings":["original warning"]}"#,
        ))
        .unwrap();
    let response = after_create(&guest.state, response).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let value: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(value["Id"], "test");
    assert_eq!(value["Warnings"][0], "original warning");
    assert_eq!(value["Warnings"][1], UNAVAILABLE);
}

#[tokio::test]
async fn opt_out_precedes_root_loader_failures() {
    let mut config = metadata();
    config["Config"]["Labels"] = json!({OPT_OUT: "false"});
    let guest = guest::Guest::new(config).await;
    let warnings = reconcile_with_root(&guest.state, "test", || {
        Err(DockerError::Server("Keychain is unavailable".into()))
    })
    .await
    .unwrap();
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(guest.data.lock().unwrap().writes, 0);
}

async fn operation(state: AppState, id: &str, action: &str) -> Result<Response> {
    use axum::{
        extract::{OriginalUri, State},
        http::Request,
    };
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/containers/{id}/{action}"))
        .body(Body::empty())
        .unwrap();
    let uri = OriginalUri(request.uri().clone());
    if action == "start" {
        super::super::start_container(State(state), uri, request).await
    } else {
        super::super::restart_container(State(state), uri, request).await
    }
}

#[tokio::test]
async fn start_obeys_opt_out_and_running_restart_reports_a_skip() {
    let mut config = metadata();
    config["Config"]["Labels"] = json!({OPT_OUT: "false"});
    let guest = guest::Guest::new(config).await;
    let tls = guest.state.runtime.config().data_dir.join("tls");
    tokio::fs::create_dir(&tls).await.unwrap();
    tokio::fs::write(tls.join("ca.pem"), b"invalid PEM")
        .await
        .unwrap();
    let response = operation(guest.state.clone(), "test", "start")
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!response.headers().contains_key(header::WARNING));
    guest.data.lock().unwrap().metadata["Config"]["Labels"] = json!(null);
    let response = operation(guest.state.clone(), "test", "restart")
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response.headers()[header::WARNING]
            .to_str()
            .unwrap()
            .contains("already running")
    );
    let data = guest.data.lock().unwrap();
    assert_eq!(data.writes, 0);
    assert_eq!(
        data.operations,
        [
            "/containers/canonical-test/start",
            "/containers/canonical-test/restart"
        ]
    );
}
