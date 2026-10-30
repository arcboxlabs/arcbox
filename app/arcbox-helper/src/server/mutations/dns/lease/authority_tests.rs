use std::fs;
use std::time::Duration;

use arcbox_atomic_file::{AtomicWriteError, write_public};
use arcbox_helper::{
    ResolverAuthorityRoot, ResolverAuthoritySnapshot, ResolverAuthorityState, ResolverLease,
    ResolverOwner,
};
use tokio_util::sync::CancellationToken;

use super::{HelperError, ResolverStore, Update};

fn fixture() -> (tempfile::TempDir, ResolverStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = ResolverStore::new(directory.path().to_path_buf());
    (directory, store)
}

fn update(revision: u64, port: u16, roots: &[&str]) -> Update {
    ResolverLease {
        profile: "production".into(),
        nonce: [7; 16],
        revision,
        port,
        roots: roots.iter().map(|root| (*root).into()).collect(),
    }
    .try_into()
    .unwrap()
}

fn root(name: &str, owner: ResolverOwner, state: ResolverAuthorityState) -> ResolverAuthorityRoot {
    ResolverAuthorityRoot {
        root: name.into(),
        owner,
        state,
    }
}

fn active(port: u16, accepted_revision: u64) -> ResolverAuthorityState {
    ResolverAuthorityState::Active {
        port,
        accepted_revision,
    }
}

async fn changed(
    store: &ResolverStore,
    connection: u64,
    epoch: u64,
    mutate: impl FnOnce(),
) -> ResolverAuthoritySnapshot {
    let closed = CancellationToken::new();
    let pending = store.watch(connection, Some(epoch), &closed);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    mutate();
    let snapshot = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.epoch > epoch);
    snapshot
}

#[tokio::test]
async fn snapshots_project_each_connection_and_notify_acquire_replace_and_release() {
    use ResolverOwner::{CurrentConnection, OtherConnection};

    let (_directory, store) = fixture();
    let first = store.connection();
    let second = store.connection();
    let initial = store
        .watch(second, None, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(initial.roots, []);
    let acquired = changed(&store, second, initial.epoch, || {
        store
            .update(first, 501, update(1, 5553, &["child.a.local", "a.local"]))
            .unwrap();
    })
    .await;
    assert_eq!(
        acquired.roots,
        [
            root("a.local", OtherConnection, active(5553, 1)),
            root("child.a.local", OtherConnection, active(5553, 1)),
        ]
    );
    let own = store.snapshot(first).unwrap();
    assert_eq!(own.epoch, acquired.epoch);
    assert_eq!(
        own.roots,
        [
            root("a.local", CurrentConnection, active(5553, 1)),
            root("child.a.local", CurrentConnection, active(5553, 1)),
        ]
    );
    let both = changed(&store, first, own.epoch, || {
        store
            .update(second, 501, update(7, 6553, &["other.local"]))
            .unwrap();
    })
    .await;
    assert_eq!(
        both.roots.last(),
        Some(&root("other.local", OtherConnection, active(6553, 7)))
    );
    let replaced = changed(&store, second, both.epoch, || {
        store
            .update(first, 501, update(2, 5554, &["a.local"]))
            .unwrap();
    })
    .await;
    assert_eq!(
        replaced.roots,
        [
            root("a.local", OtherConnection, active(5554, 2)),
            root("other.local", CurrentConnection, active(6553, 7)),
        ]
    );
    let released = changed(&store, second, replaced.epoch, || {
        store.release(first).unwrap();
    })
    .await;
    assert_eq!(
        released.roots,
        [root("other.local", CurrentConnection, active(6553, 7))]
    );
    assert!(store.snapshot(first).is_err());
}

#[tokio::test]
async fn coalesced_owner_replacement_keeps_a_lifetime_revision_without_metadata_churn() {
    let (_directory, store) = fixture();
    let owner = store.connection();
    let observer = store.connection();
    store
        .update(owner, 501, update(1, 5553, &["a.local"]))
        .unwrap();
    let initial = store.snapshot(observer).unwrap();
    let metadata = changed(&store, observer, initial.epoch, || {
        store
            .update(owner, 501, update(2, 5553, &["a.local"]))
            .unwrap();
    })
    .await;
    assert_eq!(metadata.authority_revision, initial.authority_revision);
    assert_ne!(metadata.roots, initial.roots);

    let replaced = changed(&store, observer, metadata.epoch, || {
        store.release(owner).unwrap();
        let next = store.connection();
        store
            .update(next, 501, update(2, 5553, &["a.local"]))
            .unwrap();
    })
    .await;
    assert_eq!(replaced.roots, metadata.roots);
    assert!(replaced.authority_revision > metadata.authority_revision);
}

#[tokio::test]
async fn rejected_updates_retain_only_previous_roots_without_an_active_revision() {
    for preflight_failure in [false, true] {
        let (directory, store) = fixture();
        let owner = store.connection();
        let observer = store.connection();
        store
            .update(owner, 501, update(1, 5553, &["old.local"]))
            .unwrap();
        let accepted = fs::read(directory.path().join("old.local")).unwrap();
        let attempt = if preflight_failure {
            fs::write(directory.path().join("z.local"), "foreign resolver").unwrap();
            update(9, 6553, &["next.local", "z.local"])
        } else {
            update(1, 6553, &["next.local"])
        };
        let before = store.snapshot(observer).unwrap();
        let failed = changed(&store, observer, before.epoch, || {
            assert!(store.update(owner, 501, attempt).is_err());
        })
        .await;
        assert_eq!(
            failed.roots,
            [root(
                "old.local",
                ResolverOwner::OtherConnection,
                ResolverAuthorityState::RetainedUnavailable,
            )]
        );
        assert_eq!(
            fs::read(directory.path().join("old.local")).unwrap(),
            accepted
        );
        assert!(!directory.path().join("next.local").exists());
    }
}

#[tokio::test]
async fn uncertain_publication_retains_the_root_union_until_release() {
    let (directory, store) = fixture();
    let owner = store.connection();
    let observer = store.connection();
    store
        .update(owner, 501, update(1, 5553, &["old.local", "shared.local"]))
        .unwrap();
    let before = store.snapshot(observer).unwrap();
    let failed = changed(&store, observer, before.epoch, || {
        let error = store.update_with(
            owner,
            501,
            update(2, 6553, &["next.local", "shared.local"]),
            |path, bytes| {
                write_public(path, bytes).unwrap();
                if path.ends_with("shared.local") {
                    return Err(HelperError::io(
                        "write",
                        path,
                        AtomicWriteError::DurabilityUncertain {
                            path: path.to_path_buf(),
                            source: std::io::Error::other("injected fsync failure"),
                        },
                    ));
                }
                Ok(())
            },
        );
        assert!(
            error
                .unwrap_err()
                .to_string()
                .contains("injected fsync failure")
        );
    })
    .await;
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    assert_eq!(
        failed.roots,
        ["next.local", "old.local", "shared.local"].map(|name| root(
            name,
            ResolverOwner::OtherConnection,
            ResolverAuthorityState::RetainedUnavailable,
        ))
    );
    assert!(
        store
            .update(store.connection(), 501, update(1, 5554, &["next.local"]))
            .is_err()
    );
    let released = changed(
        &store,
        observer,
        store.snapshot(observer).unwrap().epoch,
        || {
            store.release(owner).unwrap();
        },
    )
    .await;
    assert_eq!(released.roots, []);
    store
        .update(store.connection(), 501, update(1, 5554, &["next.local"]))
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn unchanged_watch_returns_after_thirty_seconds_and_cancellation_wakes_it() {
    let (_directory, store) = fixture();
    let connection = store.connection();
    let closed = CancellationToken::new();
    let initial = store.snapshot(connection).unwrap();
    let waiting = store.watch(connection, Some(initial.epoch), &closed);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(futures::poll!(&mut waiting).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(waiting.await.unwrap(), initial);
    assert_eq!(
        store
            .watch(connection, Some(initial.epoch + 1), &closed)
            .await
            .unwrap(),
        initial
    );
    let cancelled = store.watch(connection, Some(initial.epoch), &closed);
    tokio::pin!(cancelled);
    assert!(futures::poll!(&mut cancelled).is_pending());
    closed.cancel();
    assert!(cancelled.await.is_err());
}

#[tokio::test]
async fn socket_eof_cancels_an_unchanged_watch_before_releasing_resolvers() {
    use std::sync::Arc;

    use crate::server::{handler::HelperServer, peer_auth::VerifiedPeer, serve_connection};
    use arcbox_helper::HelperServiceClient;

    let (directory, store) = fixture();
    let store = Arc::new(store);
    let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
    let uid = server_stream.peer_cred().unwrap().uid();
    let server = HelperServer::new(VerifiedPeer::test_daemon(uid, true), Arc::clone(&store));
    let serving = tokio::spawn(serve_connection(server_stream, server));
    let codec = tarpc::tokio_util::codec::length_delimited::LengthDelimitedCodec::builder();
    let transport = tarpc::serde_transport::new(
        codec.new_framed(client_stream),
        tarpc::tokio_serde::formats::Json::default(),
    );
    let tarpc::client::NewClient { client, dispatch } =
        HelperServiceClient::new(tarpc::client::Config::default(), transport);
    let dispatch = tokio::spawn(dispatch);
    client
        .resolver_lease(
            tarpc::context::current(),
            ResolverLease {
                profile: "production".into(),
                nonce: [7; 16],
                revision: 1,
                port: 5553,
                roots: vec!["socket.local".into()],
            },
        )
        .await
        .unwrap()
        .unwrap();
    let initial = client
        .resolver_watch(tarpc::context::current(), None)
        .await
        .unwrap()
        .unwrap();
    let pending = tokio::spawn(async move {
        client
            .resolver_watch(tarpc::context::current(), Some(initial.epoch))
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.authority_epoch.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(directory.path().join("socket.local").exists());
    dispatch.abort();
    assert!(dispatch.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(pending.await.unwrap().is_err());
    assert!(!directory.path().join("socket.local").exists());
}
