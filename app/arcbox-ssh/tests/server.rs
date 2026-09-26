//! The SSH server against an in-process russh client.

use std::net::SocketAddr;
use std::sync::Arc;

use arcbox_connect::v1::MachineExecRequest;
use arcbox_engine::agent_client::ExecSessionInput;
use arcbox_ssh::{CLIENT_KEY_FILE, ExecOutput, MachineHost, SshKeys, SshServer};
use russh::client;
use russh::keys::{Algorithm, PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// No machine to log into: these tests stop at authentication.
struct NoMachines;

impl MachineHost for NoMachines {
    async fn exec(
        &self,
        machine: &str,
        _request: MachineExecRequest,
        _input: mpsc::Receiver<ExecSessionInput>,
    ) -> anyhow::Result<ExecOutput> {
        anyhow::bail!("no machine named '{machine}'")
    }
}

struct TrustingClient;

impl client::Handler for TrustingClient {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Fixture {
    addr: SocketAddr,
    client_key: Arc<PrivateKey>,
    _dir: tempfile::TempDir,
}

async fn start_server() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let keys = SshKeys::load_or_generate(dir.path()).unwrap();
    let pem = std::fs::read_to_string(dir.path().join(CLIENT_KEY_FILE)).unwrap();
    let client_key = Arc::new(PrivateKey::from_openssh(pem).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = SshServer::new(Arc::new(NoMachines), keys);
    tokio::spawn(server.serve(listener, std::future::pending()));
    Fixture {
        addr,
        client_key,
        _dir: dir,
    }
}

async fn login(
    addr: SocketAddr,
    user: &str,
    key: Arc<PrivateKey>,
) -> (client::Handle<TrustingClient>, bool) {
    let config = Arc::new(client::Config::default());
    let mut handle = client::connect(config, addr, TrustingClient).await.unwrap();
    let auth = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, None))
        .await
        .unwrap();
    (handle, auth.success())
}

#[tokio::test]
async fn the_daemon_client_key_logs_in() {
    let fixture = start_server().await;
    let (_, ok) = login(fixture.addr, "dev@ubuntu", fixture.client_key).await;
    assert!(ok);
}

#[tokio::test]
async fn any_other_key_is_refused() {
    let fixture = start_server().await;
    let stranger = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let (_, ok) = login(fixture.addr, "ubuntu", Arc::new(stranger)).await;
    assert!(!ok);
}

#[tokio::test]
async fn a_user_name_naming_no_machine_is_refused() {
    let fixture = start_server().await;
    let (_, ok) = login(fixture.addr, "dev@", fixture.client_key).await;
    assert!(!ok);
}
