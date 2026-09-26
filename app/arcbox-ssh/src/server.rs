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
use crate::keys::SshKeys;

/// The ArcBox SSH server.
pub struct SshServer {
    config: Arc<Config>,
    client_key: Arc<PublicKey>,
}

impl SshServer {
    /// A server identifying with `keys.host` that lets in `keys.client` only.
    #[must_use]
    pub fn new(keys: SshKeys) -> Self {
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
            client_key: self.client_key,
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
struct Acceptor {
    client_key: Arc<PublicKey>,
}

impl russh::server::Server for Acceptor {
    type Handler = Connection;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Connection {
        Connection::new(Arc::clone(&self.client_key))
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        tracing::debug!(%error, "ssh connection ended with an error");
    }
}
