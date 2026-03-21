//! Integration tests for the arcbox-helper tarpc pipeline.
//!
//! Starts a mock server (reuses lib validation, skips actual privileged syscalls)
//! on a temp Unix socket, connects with the real `Client`, and verifies the
//! full serialize → transport → validate → respond round-trip.

mod common;

use arcbox_helper::client::{Client, ClientError};

#[tokio::test]
async fn version_returns_crate_version() {
    let (client, _dir) = common::setup().await;
    let version = client.version().await.unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn route_add_valid_input() {
    let (client, _dir) = common::setup().await;
    client.route_add("10.0.0.0/8", "bridge100").await.unwrap();
}

#[tokio::test]
async fn route_add_rejects_public_subnet() {
    let (client, _dir) = common::setup().await;
    let err = client.route_add("8.8.8.0/24", "bridge100").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("private range")));
}

#[tokio::test]
async fn route_add_rejects_invalid_iface() {
    let (client, _dir) = common::setup().await;
    let err = client.route_add("10.0.0.0/8", "eth0").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("bridge")));
}

#[tokio::test]
async fn route_remove_valid() {
    let (client, _dir) = common::setup().await;
    client.route_remove("172.16.0.0/12").await.unwrap();
}

#[tokio::test]
async fn dns_install_valid() {
    let (client, _dir) = common::setup().await;
    client.dns_install("arcbox.local", 5553).await.unwrap();
}

#[tokio::test]
async fn dns_install_rejects_privileged_port() {
    let (client, _dir) = common::setup().await;
    let err = client.dns_install("arcbox.local", 53).await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("1024")));
}

#[tokio::test]
async fn dns_install_rejects_invalid_domain() {
    let (client, _dir) = common::setup().await;
    let err = client.dns_install("UPPER.CASE", 5553).await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("invalid characters")));
}

#[tokio::test]
async fn dns_status_valid() {
    let (client, _dir) = common::setup().await;
    let installed = client.dns_status("arcbox.local").await.unwrap();
    assert!(!installed);
}

#[tokio::test]
async fn socket_link_valid() {
    let (client, _dir) = common::setup().await;
    client
        .socket_link("/Users/test/.arcbox/run/docker.sock")
        .await
        .unwrap();
}

#[tokio::test]
async fn socket_link_rejects_bad_path() {
    let (client, _dir) = common::setup().await;
    let err = client.socket_link("/tmp/docker.sock").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("/Users/")));
}

#[tokio::test]
async fn socket_unlink_valid() {
    let (client, _dir) = common::setup().await;
    client.socket_unlink().await.unwrap();
}

#[tokio::test]
async fn connection_refused_when_no_server() {
    let err = Client::connect_to("/tmp/arcbox-helper-nonexistent.sock").await;
    assert!(matches!(err, Err(ClientError::Connection(_))));
}
