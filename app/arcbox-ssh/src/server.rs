//! Serving connections: the russh configuration and the accept loop.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PublicKey;
use russh::server::{Config, Server as _};
use russh::{MethodKind, MethodSet, SshId};
use tokio::net::TcpListener;

use crate::connection::Connection;
use crate::host::MachineHost;
use crate::keys::SshKeys;

/// An SSH server whose logins land in the machines `H` reaches.
pub struct SshServer<H> {
    host: Arc<H>,
    config: Arc<Config>,
    client_key: Arc<PublicKey>,
}

impl<H: MachineHost> SshServer<H> {
    /// A server identifying with `keys.host` that lets in `keys.client` only.
    #[must_use]
    pub fn new(host: Arc<H>, keys: SshKeys) -> Self {
        let config = Config {
            server_id: SshId::Standard(
                format!("SSH-2.0-ArcBox_{}", env!("CARGO_PKG_VERSION")).into(),
            ),
            methods: MethodSet::from(&[MethodKind::PublicKey][..]),
            // OpenSSH clients open with a `none` attempt to learn the
            // methods; refusing it at once spares every connection the
            // one-second delay other rejections keep.
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![keys.host],
            // Shells and editor connections sit idle for hours; the client
            // closes the connection when it is done with it.
            inactivity_timeout: None,
            nodelay: true,
            ..Config::default()
        };
        Self {
            host,
            config: Arc::new(config),
            client_key: Arc::new(keys.client),
        }
    }

    /// Serves `listener` until `shutdown` resolves, then disconnects every
    /// client.
    ///
    /// # Errors
    ///
    /// Returns an error if the listener fails.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> std::io::Result<()> {
        let mut acceptor = Acceptor {
            host: self.host,
            client_key: self.client_key,
            local: listener.local_addr()?,
        };
        let server = acceptor.run_on_socket(self.config, &listener);
        let handle = server.handle();
        tokio::select! {
            result = server => result,
            () = shutdown => {
                handle.shutdown("ArcBox is shutting down".to_owned());
                Ok(())
            }
        }
    }
}

/// Hands each accepted connection its own [`Connection`].
struct Acceptor<H> {
    host: Arc<H>,
    client_key: Arc<PublicKey>,
    local: SocketAddr,
}

impl<H: MachineHost> russh::server::Server for Acceptor<H> {
    type Handler = Connection<H>;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection<H> {
        Connection::new(
            Arc::clone(&self.host),
            Arc::clone(&self.client_key),
            peer,
            self.local,
        )
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        tracing::debug!(%error, "ssh connection ended with an error");
    }
}
