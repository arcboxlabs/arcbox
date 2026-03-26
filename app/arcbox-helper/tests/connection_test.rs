//! Integration tests for connection handling and version reporting.

mod common;

use arcbox_helper::client::{Client, ClientError};

#[tokio::test]
async fn version_returns_crate_version() {
    let (client, _dir) = common::setup().await;
    let version = client.version().await.unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn connection_refused_when_no_server() {
    let err = Client::connect_to("/tmp/arcbox-helper-nonexistent.sock").await;
    assert!(matches!(err, Err(ClientError::Connection(_))));
}
