//! Integration tests for socket-related RPC methods.

mod common;

use arcbox_helper::client::ClientError;

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
