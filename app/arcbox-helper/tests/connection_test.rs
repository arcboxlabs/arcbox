//! Integration tests for connection handling and version reporting.

mod common;

use std::process::Command;
use std::time::Duration;

use arcbox_helper::client::{Client, ClientError};
use arcbox_helper::{HelperServiceRequest, HelperServiceResponse};
use futures::{SinkExt, StreamExt};

#[test]
fn version_flag_reports_the_helper_version_on_stdout() {
    let output = Command::new(env!("CARGO_BIN_EXE_arcbox-helper"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("arcbox-helper {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(output.stderr, b"");
}

#[tokio::test]
async fn version_returns_helper_crate_version() {
    let (client, _dir) = common::setup().await;
    let version = client.version().await.unwrap();
    assert_eq!(
        version,
        format!("arcbox-helper {}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        arcbox_constants::helper::parse_helper_version(&version),
        arcbox_constants::helper::parse_semver_triple(env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn mutation_client_rejects_an_old_wire_version() {
    let (client, _dir) = common::setup_with_version("arcbox-helper 0.4.24").await;

    assert!(matches!(
        client,
        Err(ClientError::IncompatibleVersion {
            installed,
            required,
        }) if installed == "arcbox-helper 0.4.24"
            && required == arcbox_constants::helper::MIN_HELPER_VERSION
    ));
}

#[tokio::test]
async fn mutation_client_rejects_an_unrecognized_version() {
    let (client, _dir) = common::setup_with_version("arcbox-helper broken").await;

    assert!(matches!(
        client,
        Err(ClientError::UnrecognizedVersion(version)) if version == "arcbox-helper broken"
    ));
}

/// Package version, workspace path-dep pin, and MIN_HELPER_VERSION must move
/// together. Catches "bumped Cargo.toml but forgot MIN" before release.
#[test]
fn helper_version_pins_are_aligned() {
    let pkg = env!("CARGO_PKG_VERSION");
    let min = arcbox_constants::helper::MIN_HELPER_VERSION;
    assert_eq!(
        pkg, min,
        "arcbox-helper Cargo.toml version ({pkg}) must equal \
         arcbox_constants::helper::MIN_HELPER_VERSION ({min}); bump both \
         (and the workspace path-dep pin) together"
    );

    // Workspace path-dep pin in the root Cargo.toml.
    let root_toml = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"));
    let needle = format!("arcbox-helper = {{ version = \"{pkg}\"");
    assert!(
        root_toml.contains(&needle),
        "root Cargo.toml must pin arcbox-helper = {{ version = \"{pkg}\", ... }}"
    );

    // Independent of the workspace package version.
    assert_eq!(pkg, "1.2.0");
}

#[tokio::test]
async fn connection_refused_when_no_server() {
    let err = Client::connect_to("/tmp/arcbox-helper-nonexistent.sock").await;
    assert!(matches!(err, Err(ClientError::Connection(_))));
}

type VersionTransport = tarpc::serde_transport::Transport<
    tokio::net::UnixStream,
    tarpc::ClientMessage<HelperServiceRequest>,
    tarpc::Response<HelperServiceResponse>,
    tarpc::tokio_serde::formats::Json<
        tarpc::ClientMessage<HelperServiceRequest>,
        tarpc::Response<HelperServiceResponse>,
    >,
>;

async fn version_request(listener: tokio::net::UnixListener) -> (VersionTransport, u64) {
    let (stream, _) = listener.accept().await.unwrap();
    let codec = tarpc::tokio_util::codec::length_delimited::LengthDelimitedCodec::builder();
    let mut transport = tarpc::serde_transport::new(
        codec.new_framed(stream),
        tarpc::tokio_serde::formats::Json::default(),
    );
    let tarpc::ClientMessage::Request(request) = transport.next().await.unwrap().unwrap() else {
        panic!("expected version request");
    };
    assert!(matches!(request.message, HelperServiceRequest::Version {}));
    (transport, request.id)
}

#[tokio::test]
async fn owned_connect_releases_transport_after_rejection_or_cancellation() {
    for rejected in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("helper.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let connecting =
            tokio::spawn(async move { Client::connect_owned_to(path.to_str().unwrap()).await });
        let (mut transport, request_id) = version_request(listener).await;
        if rejected {
            transport
                .send(tarpc::Response {
                    request_id,
                    message: Ok(HelperServiceResponse::Version(
                        "arcbox-helper broken".into(),
                    )),
                })
                .await
                .unwrap();
            assert!(matches!(
                connecting.await.unwrap(),
                Err(ClientError::UnrecognizedVersion(_))
            ));
        } else {
            connecting.abort();
            assert!(connecting.await.is_err());
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(message) = transport.next().await {
                assert!(matches!(
                    message.unwrap(),
                    tarpc::ClientMessage::Cancel { .. }
                ));
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn owned_dispatch_reports_idle_eof_without_another_rpc() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("helper.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let connecting =
        tokio::spawn(async move { Client::connect_owned_to(path.to_str().unwrap()).await });
    let (mut transport, request_id) = version_request(listener).await;
    transport
        .send(tarpc::Response {
            request_id,
            message: Ok(HelperServiceResponse::Version(format!(
                "arcbox-helper {}",
                env!("CARGO_PKG_VERSION")
            ))),
        })
        .await
        .unwrap();
    let (client, dispatch) = connecting.await.unwrap().unwrap();
    drop(transport);
    tokio::time::timeout(Duration::from_secs(2), dispatch)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(client.version().await.is_err());
}
