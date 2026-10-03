//! Docker API server.

use crate::api::{router_with_proxy, strip_api_version_prefix};
use crate::error::{DockerError, Result};
use crate::proxy::ProxyState;
use crate::proxy::VsockConnector;
use arcbox_core::Runtime;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower::Layer;
use tower_http::trace::TraceLayer;

/// Docker API server configuration.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Unix socket path.
    pub socket_path: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
        }
    }
}

fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".arcbox")
        .join("docker.sock")
}

/// Docker API server.
pub struct DockerApiServer {
    config: ServerConfig,
    runtime: Arc<Runtime>,
}

impl DockerApiServer {
    /// Creates a new Docker API server.
    #[must_use]
    pub const fn new(config: ServerConfig, runtime: Arc<Runtime>) -> Self {
        Self { config, runtime }
    }

    /// Returns the socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.config.socket_path
    }

    /// Binds the Docker API socket, returning the listener to serve on.
    ///
    /// Separate from [`Self::serve`] so a caller that spawns the serving task
    /// can still fail startup on a bind error: the daemon's Docker socket is
    /// its primary API, and a background task that only logs the failure
    /// leaves clients hitting connection-refused against a daemon that
    /// reported itself ready.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound.
    pub fn bind(&self) -> Result<UnixListener> {
        // Remove existing socket
        let _ = std::fs::remove_file(&self.config.socket_path);

        // Create parent directory if needed
        if let Some(parent) = self.config.socket_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let listener = UnixListener::bind(&self.config.socket_path)
            .map_err(|e| crate::error::DockerError::Server(e.to_string()))?;

        tracing::info!(
            "Docker API server listening on {}",
            self.config.socket_path.display()
        );
        tracing::info!("Docker API backend: smart proxy to guest dockerd");

        Ok(listener)
    }

    /// Serves on an already-bound listener until `shutdown` is cancelled.
    ///
    /// Cancellation stops accepting and shuts every connection down
    /// gracefully: an idle keep-alive connection closes at once, a connection
    /// with a request in flight finishes that response with
    /// `Connection: close`, and a connection hijacked by an upgrade
    /// (`attach`, `exec`) has already left this server for the proxy. The
    /// future then returns; only requests still running hold it open. The
    /// caller owns the deadline for those, and aborting this future aborts
    /// them with it.
    ///
    /// There is deliberately no `bind`-and-serve convenience wrapper: it
    /// would only be useful from a spawned task, which is exactly the shape
    /// that swallows the bind error and lets startup report READY for a
    /// daemon nobody can reach (CORE-71).
    ///
    /// # Errors
    ///
    /// Returns an error if serving fails.
    pub async fn serve(&self, listener: UnixListener, shutdown: CancellationToken) -> Result<()> {
        self.run_native_http(listener, shutdown).await
    }
}

impl DockerApiServer {
    async fn run_native_http(
        &self,
        listener: UnixListener,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let connector = Arc::new(VsockConnector::new(Arc::clone(&self.runtime)));
        let activity_hook: crate::proxy::ActivityHook = {
            let runtime = Arc::clone(&self.runtime);
            Arc::new(move || Box::new(runtime.begin_system_vm_activity()) as _)
        };
        let proxy = Arc::new(ProxyState::new(connector).with_activity_hook(activity_hook));

        // Keep host container networking in step with the guest for every
        // change that reaches no handler: containers that stop without a
        // stop/kill/remove call (natural exit, --rm, prune, OOM, guest-side
        // stop) and containers dockerd brings back by itself after a System
        // VM restart. It shares the router's ProxyState so its queries go
        // through the same pooled client — including the restart-generation
        // reset.
        crate::host_reconciler::spawn(
            Arc::clone(&self.runtime),
            Arc::clone(&proxy),
            shutdown.clone(),
        );

        // Wrap the Axum Router with a MapRequestLayer that strips API version
        // prefixes *before* route matching. `Router::layer` runs after routing
        // and cannot be used for URI rewriting.
        let version_layer = tower::util::MapRequestLayer::new(strip_api_version_prefix);
        let app = version_layer.layer(
            router_with_proxy(Arc::clone(&self.runtime), proxy).layer(TraceLayer::new_for_http()),
        );

        // Docker clients speak HTTP/1.1, and the attach/exec hijack is an
        // HTTP/1 upgrade, so protocol detection stays off. The auto builder
        // is here for its graceful-shutdown hook: hyper-util wires
        // `GracefulShutdown` to its own connection types, not to hyper's
        // bare `http1::UpgradeableConnection`.
        let builder = auto::Builder::new(TokioExecutor::new()).http1_only();
        let graceful = GracefulShutdown::new();
        // Connection tasks live in a set this future owns, so a caller that
        // aborts the future once its drain budget runs out aborts the
        // requests still running with it, instead of leaving them to run
        // until the process exits.
        let mut connections = JoinSet::new();

        loop {
            let stream = tokio::select! {
                result = listener.accept() => {
                    let (stream, _) = result.map_err(|e| DockerError::Server(e.to_string()))?;
                    stream
                }
                // Reap finished connections as they finish. Without this the
                // set keeps every handle since startup and its length says
                // nothing about what is still open.
                Some(_) = connections.join_next(), if !connections.is_empty() => continue,
                () = shutdown.cancelled() => break,
            };

            let service = TowerToHyperService::new(app.clone());
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
            let conn = graceful.watch(conn.into_owned());
            connections.spawn(async move {
                if let Err(err) = conn.await {
                    let err_str = err.to_string().to_lowercase();
                    if !err_str.contains("shutting down")
                        && !err_str.contains("connection reset")
                        && !err_str.contains("broken pipe")
                        && !err_str.contains("connection closed")
                        && !err_str.contains("incomplete")
                    {
                        tracing::error!("Error serving connection: {}", err);
                    }
                }
            });
        }

        while connections.try_join_next().is_some() {}
        tracing::info!(
            open_connections = connections.len(),
            "Docker API server shutting down, closing idle connections and finishing in-flight requests"
        );
        graceful.shutdown().await;
        tracing::info!("Docker API server drained");

        Ok(())
    }
}
