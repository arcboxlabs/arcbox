//! Integration tests for the arcbox-helper tarpc pipeline.
//!
//! Starts a mock server (reuses lib validation, skips actual privileged syscalls)
//! on a temp Unix socket, connects with the real `Client`, and verifies the
//! full serialize → transport → validate → respond round-trip.

use arcbox_helper::HelperService;
use arcbox_helper::client::{Client, ClientError};
use arcbox_helper::validate;
use futures::prelude::*;
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;

// =============================================================================
// Mock server — validates inputs but skips privileged ops
// =============================================================================

#[derive(Clone)]
struct MockHelperServer;

impl HelperService for MockHelperServer {
    async fn route_add(
        self,
        _: tarpc::context::Context,
        subnet: String,
        iface: String,
    ) -> Result<(), String> {
        validate::validate_subnet(&subnet)?;
        validate::validate_iface(&iface)?;
        Ok(())
    }

    async fn route_remove(self, _: tarpc::context::Context, subnet: String) -> Result<(), String> {
        validate::validate_subnet(&subnet)?;
        Ok(())
    }

    async fn dns_install(
        self,
        _: tarpc::context::Context,
        domain: String,
        port: u16,
    ) -> Result<(), String> {
        validate::validate_domain(&domain)?;
        validate::validate_port(port)?;
        Ok(())
    }

    async fn dns_uninstall(self, _: tarpc::context::Context, domain: String) -> Result<(), String> {
        validate::validate_domain(&domain)?;
        Ok(())
    }

    async fn dns_status(self, _: tarpc::context::Context, domain: String) -> Result<bool, String> {
        validate::validate_domain(&domain)?;
        Ok(false)
    }

    async fn socket_link(self, _: tarpc::context::Context, target: String) -> Result<(), String> {
        validate::validate_socket_target(&target)?;
        Ok(())
    }

    async fn socket_unlink(self, _: tarpc::context::Context) -> Result<(), String> {
        Ok(())
    }

    async fn version(self, _: tarpc::context::Context) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

// =============================================================================
// Test harness
// =============================================================================

/// Starts a mock server on a temp socket and returns a connected `Client`.
async fn setup() -> (Client, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("helper.sock");
    let sock_str = sock_path.to_str().unwrap().to_string();

    // Start server.
    let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
    let codec = tarpc::tokio_util::codec::length_delimited::LengthDelimitedCodec::builder();

    tokio::spawn(async move {
        loop {
            let Ok((conn, _)) = listener.accept().await else {
                break;
            };
            let transport = tarpc::serde_transport::new(codec.new_framed(conn), Bincode::default());
            tokio::spawn(
                BaseChannel::with_defaults(transport)
                    .execute(MockHelperServer.serve())
                    .for_each(|resp| async {
                        tokio::spawn(resp);
                    }),
            );
        }
    });

    // Connect client via explicit socket path (avoids env var race in parallel tests).
    let client = Client::connect_to(&sock_str).await.unwrap();

    (client, dir)
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn version_returns_crate_version() {
    let (client, _dir) = setup().await;
    let version = client.version().await.unwrap();
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn route_add_valid_input() {
    let (client, _dir) = setup().await;
    client.route_add("10.0.0.0/8", "bridge100").await.unwrap();
}

#[tokio::test]
async fn route_add_rejects_public_subnet() {
    let (client, _dir) = setup().await;
    let err = client.route_add("8.8.8.0/24", "bridge100").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("private range")));
}

#[tokio::test]
async fn route_add_rejects_invalid_iface() {
    let (client, _dir) = setup().await;
    let err = client.route_add("10.0.0.0/8", "eth0").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("bridge")));
}

#[tokio::test]
async fn route_remove_valid() {
    let (client, _dir) = setup().await;
    client.route_remove("172.16.0.0/12").await.unwrap();
}

#[tokio::test]
async fn dns_install_valid() {
    let (client, _dir) = setup().await;
    client.dns_install("arcbox.local", 5553).await.unwrap();
}

#[tokio::test]
async fn dns_install_rejects_privileged_port() {
    let (client, _dir) = setup().await;
    let err = client.dns_install("arcbox.local", 53).await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("1024")));
}

#[tokio::test]
async fn dns_install_rejects_invalid_domain() {
    let (client, _dir) = setup().await;
    let err = client.dns_install("UPPER.CASE", 5553).await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("invalid characters")));
}

#[tokio::test]
async fn dns_status_valid() {
    let (client, _dir) = setup().await;
    let installed = client.dns_status("arcbox.local").await.unwrap();
    assert!(!installed); // mock always returns false
}

#[tokio::test]
async fn socket_link_valid() {
    let (client, _dir) = setup().await;
    client
        .socket_link("/Users/test/.arcbox/run/docker.sock")
        .await
        .unwrap();
}

#[tokio::test]
async fn socket_link_rejects_bad_path() {
    let (client, _dir) = setup().await;
    let err = client.socket_link("/tmp/docker.sock").await;
    assert!(matches!(err, Err(ClientError::Helper(msg)) if msg.contains("/Users/")));
}

#[tokio::test]
async fn socket_unlink_valid() {
    let (client, _dir) = setup().await;
    client.socket_unlink().await.unwrap();
}

#[tokio::test]
async fn connection_refused_when_no_server() {
    // Point to a nonexistent socket via explicit path (no env var mutation).
    let err = Client::connect_to("/tmp/arcbox-helper-nonexistent.sock").await;
    assert!(matches!(err, Err(ClientError::Connection(_))));
}
