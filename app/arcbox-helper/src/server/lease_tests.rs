use std::sync::Arc;
use std::time::Duration;

use arcbox_helper::{
    HelperService, ResolverAuthorityRoot, ResolverAuthorityState, ResolverLease, ResolverOwner,
    client::Client,
};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use super::{
    finish_connection, handler::HelperServer, mutations::dns::ResolverStore,
    peer_auth::VerifiedPeer, serve_connection,
};

fn request(revision: u64, root: &str) -> ResolverLease {
    ResolverLease {
        profile: "production".into(),
        nonce: [17; 16],
        revision,
        port: 5553,
        roots: vec![root.into()],
    }
}

#[tokio::test]
async fn resolver_rpc_uses_peer_uid_and_releases_files_on_socket_eof() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ResolverStore::new(directory.path().join("resolvers")));
    let socket = directory.path().join("helper.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (uid_tx, uid_rx) = oneshot::channel();
    let serving = tokio::spawn(async move {
        let (conn, _) = listener.accept().await.unwrap();
        let uid = conn.peer_cred().unwrap().uid();
        uid_tx.send(uid).unwrap();
        serve_connection(
            conn,
            HelperServer::new(VerifiedPeer::test_daemon(uid, true), store),
        )
        .await
    });
    let client = Client::connect_to(socket.to_str().unwrap()).await.unwrap();
    client
        .resolver_lease(request(1, "rpc.local"))
        .await
        .unwrap();
    let resolver = directory.path().join("resolvers/rpc.local");
    let content = std::fs::read_to_string(&resolver).unwrap();
    assert!(content.contains(&format!("# lease {} production ", uid_rx.await.unwrap())));
    assert!(content.contains("port 5553\n"));
    drop(client);
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!resolver.exists());
}

#[tokio::test]
async fn resolver_boundary_failures_cannot_reopen_the_connection() {
    for (signed, profile, root) in [
        (false, "production", "a.local"),
        (true, "development", "a.local"),
        (true, "production", "local"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(ResolverStore::new(directory.path().to_path_buf()));
        let server = HelperServer::new(VerifiedPeer::test_daemon(501, signed), store);
        let mut invalid = request(1, root);
        invalid.profile = profile.into();
        assert!(
            server
                .clone()
                .resolver_lease(tarpc::context::current(), invalid)
                .await
                .is_err()
        );
        assert!(
            server
                .clone()
                .resolver_lease(tarpc::context::current(), request(2, "a.local"))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        server.release().unwrap();
    }
}

#[tokio::test]
async fn resolver_eof_drains_in_flight_mutations_before_releasing_the_lease() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ResolverStore::new(directory.path().to_path_buf()));
    let server = HelperServer::new(VerifiedPeer::test_daemon(501, true), Arc::clone(&store));
    server
        .clone()
        .resolver_lease(tarpc::context::current(), request(1, "old.local"))
        .await
        .unwrap();
    let (proceed, pending) = oneshot::channel();
    let (completed_tx, completed_rx) = oneshot::channel();
    let mutation_server = server.clone();
    let observer = store.connection();
    let mutation_store = Arc::clone(&store);
    let next_path = directory.path().join("next.local");
    let mut in_flight = JoinSet::new();
    in_flight.spawn(async move {
        pending.await.unwrap();
        mutation_server
            .resolver_lease(tarpc::context::current(), request(2, "next.local"))
            .await
            .unwrap();
        assert!(next_path.exists());
        assert_eq!(
            mutation_store.snapshot(observer).unwrap().roots,
            [ResolverAuthorityRoot {
                root: "next.local".into(),
                owner: ResolverOwner::OtherConnection,
                state: ResolverAuthorityState::RetainedUnavailable,
            }]
        );
        completed_tx.send(()).unwrap();
    });
    let finishing = finish_connection(server, in_flight);
    tokio::pin!(finishing);
    assert!(futures::poll!(&mut finishing).is_pending());
    assert!(directory.path().join("old.local").exists());
    proceed.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), finishing)
        .await
        .unwrap()
        .unwrap();
    completed_rx.await.unwrap();
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let replacement = HelperServer::new(VerifiedPeer::test_daemon(501, true), store);
    replacement
        .clone()
        .resolver_lease(tarpc::context::current(), request(1, "old.local"))
        .await
        .unwrap();
    replacement.release().unwrap();
}

#[tokio::test]
async fn resolver_eof_reports_task_and_cleanup_errors_after_attempting_every_release() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ResolverStore::new(directory.path().to_path_buf()));
    let server = HelperServer::new(VerifiedPeer::test_daemon(501, true), store);
    let mut update = request(1, "blocked.local");
    update.roots.push("clean.local".into());
    server
        .clone()
        .resolver_lease(tarpc::context::current(), update)
        .await
        .unwrap();
    let blocked = directory.path().join("blocked.local");
    std::fs::remove_file(&blocked).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(&blocked).unwrap();
    let mut in_flight = JoinSet::new();
    in_flight.spawn(async { panic!("held request failed") });
    in_flight.spawn(std::future::pending()).abort();
    let error = finish_connection(server.clone(), in_flight)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("panicked"), "{error}");
    assert!(error.contains("cancelled"), "{error}");
    assert!(error.contains("resolver lease cleanup failed"), "{error}");
    assert!(error.contains("blocked.local"), "{error}");
    assert!(!directory.path().join("clean.local").exists());
    assert!(blocked.exists());
    std::fs::remove_file(blocked).unwrap();
    server.release().unwrap();
}
