//! Shared test infrastructure for arcbox-helper integration tests.
//!
//! Provides a mock tarpc server that validates inputs but skips privileged
//! operations, plus a `setup()` helper that spins up the mock on a temp
//! Unix socket and returns a connected `Client`.

use arcbox_helper::HelperService;
use arcbox_helper::client::Client;
use arcbox_helper::validate::{
    BridgeIface, CliName, CliTarget, DnsPort, Domain, SocketTarget, Subnet,
};
use futures::prelude::*;
use tarpc::server::{BaseChannel, Channel};
use tarpc::tokio_serde::formats::Bincode;

/// Mock server — parses inputs into validated types (mirrors real handler)
/// but skips privileged ops.
#[derive(Clone)]
pub struct MockHelperServer;

impl HelperService for MockHelperServer {
    async fn route_add(
        self,
        _: tarpc::context::Context,
        subnet: String,
        iface: String,
    ) -> Result<(), String> {
        let _subnet: Subnet = subnet.parse()?;
        let _iface: BridgeIface = iface.parse()?;
        Ok(())
    }

    async fn route_remove(self, _: tarpc::context::Context, subnet: String) -> Result<(), String> {
        let _subnet: Subnet = subnet.parse()?;
        Ok(())
    }

    async fn dns_install(
        self,
        _: tarpc::context::Context,
        domain: String,
        port: u16,
    ) -> Result<(), String> {
        let _domain: Domain = domain.parse()?;
        let _port = DnsPort::try_from(port)?;
        Ok(())
    }

    async fn dns_uninstall(self, _: tarpc::context::Context, domain: String) -> Result<(), String> {
        let _domain: Domain = domain.parse()?;
        Ok(())
    }

    async fn dns_status(self, _: tarpc::context::Context, domain: String) -> Result<bool, String> {
        let _domain: Domain = domain.parse()?;
        Ok(false)
    }

    async fn socket_link(self, _: tarpc::context::Context, target: String) -> Result<(), String> {
        let _target: SocketTarget = target.parse()?;
        Ok(())
    }

    async fn socket_unlink(self, _: tarpc::context::Context) -> Result<(), String> {
        Ok(())
    }

    async fn cli_link(
        self,
        _: tarpc::context::Context,
        name: String,
        target: String,
    ) -> Result<(), String> {
        let _name: CliName = name.parse()?;
        let _target: CliTarget = target.parse()?;
        Ok(())
    }

    async fn cli_unlink(self, _: tarpc::context::Context, name: String) -> Result<(), String> {
        let _name: CliName = name.parse()?;
        Ok(())
    }

    async fn version(self, _: tarpc::context::Context) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

/// Starts a mock server on a temp socket and returns a connected `Client`.
///
/// The returned `TempDir` must be kept alive for the duration of the test
/// so the socket file is not cleaned up prematurely.
pub async fn setup() -> (Client, tempfile::TempDir) {
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
