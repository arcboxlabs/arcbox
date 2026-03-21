//! Integration tests for DNS-related RPC methods.

mod common;

use arcbox_helper::client::ClientError;

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
    assert!(!installed); // mock always returns false
}
