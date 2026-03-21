//! Integration tests for route_add / route_remove RPCs.

mod common;

use arcbox_helper::client::ClientError;

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
