//! Shutdown behaviour of the Docker API server.
//!
//! A Docker client that keeps an HTTP/1.1 connection open between requests
//! must not be able to hold the daemon's shutdown hostage. Before the server
//! drained through hyper's graceful shutdown, `serve` waited for every
//! accepted connection task to end, and an idle connection ends only when
//! the *client* closes it — so a parked client pinned the daemon until its
//! drain timeout aborted everything, in-flight requests included.

use arcbox_core::{Config, Runtime, VmLifecycleConfig};
use arcbox_docker::{DockerApiServer, ServerConfig};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// A runtime that never boots a VM; the server only needs it for wiring.
fn offline_runtime(data_dir: &Path) -> Arc<Runtime> {
    let config = Config {
        data_dir: data_dir.to_path_buf(),
        ..Default::default()
    };
    let vm_lifecycle_config = VmLifecycleConfig {
        skip_vm_check: true,
        ..Default::default()
    };
    Arc::new(
        Runtime::with_vm_lifecycle_config(config, vm_lifecycle_config)
            .expect("Failed to create runtime"),
    )
}

#[tokio::test]
async fn idle_connections_do_not_hold_shutdown_open() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let socket_path = tmp.path().join("docker.sock");
    let server = DockerApiServer::new(
        ServerConfig {
            socket_path: socket_path.clone(),
        },
        offline_runtime(tmp.path()),
    );
    let listener = server.bind().expect("bind");
    let shutdown = CancellationToken::new();
    let serve = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { server.serve(listener, shutdown).await }
    });

    // Three clients connect and then say nothing, the shape of a pooled
    // keep-alive connection between two Docker commands.
    let mut idle = Vec::new();
    for _ in 0..3 {
        idle.push(UnixStream::connect(&socket_path).await.expect("connect"));
    }
    // Let the accept loop pick them up so the drain has to deal with them.
    tokio::time::sleep(Duration::from_millis(100)).await;

    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(2), serve)
        .await
        .expect("serve must return without waiting for idle clients to hang up")
        .expect("serve task panicked")
        .expect("serve failed");

    // The server hung up on each idle client, rather than the other way round.
    for mut client in idle {
        let mut buf = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("server must close the idle connection")
            .expect("read after server close");
        assert_eq!(read, 0, "idle connection must see EOF from the server");
    }
}
