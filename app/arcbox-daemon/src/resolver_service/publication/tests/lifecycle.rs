//! Exercises resolver connection ownership through a temporary helper transport.

use super::*;
use arcbox_helper::{HelperError, HelperServiceRequest, HelperServiceResponse};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixListener;

type Transport = tarpc::serde_transport::Transport<
    tokio::net::UnixStream,
    tarpc::ClientMessage<HelperServiceRequest>,
    tarpc::Response<HelperServiceResponse>,
    tarpc::tokio_serde::formats::Json<
        tarpc::ClientMessage<HelperServiceRequest>,
        tarpc::Response<HelperServiceResponse>,
    >,
>;

fn socket() -> (tempfile::TempDir, UnixListener, Publisher) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let mut publisher = publisher();
    publisher.socket = path.to_str().unwrap().into();
    publisher.request.revision = 0;
    (directory, listener, publisher)
}

async fn request(transport: &mut Transport) -> tarpc::Request<HelperServiceRequest> {
    loop {
        if let tarpc::ClientMessage::Request(request) = transport.next().await.unwrap().unwrap() {
            return request;
        }
    }
}

async fn reply(transport: &mut Transport, request_id: u64, message: HelperServiceResponse) {
    transport
        .send(tarpc::Response {
            request_id,
            message: Ok(message),
        })
        .await
        .unwrap();
}

async fn connect(listener: &UnixListener) -> Transport {
    let (stream, _) = listener.accept().await.unwrap();
    let codec = tarpc::tokio_util::codec::length_delimited::LengthDelimitedCodec::builder();
    let mut transport = tarpc::serde_transport::new(
        codec.new_framed(stream),
        tarpc::tokio_serde::formats::Json::default(),
    );
    let version = request(&mut transport).await;
    assert!(matches!(version.message, HelperServiceRequest::Version {}));
    reply(
        &mut transport,
        version.id,
        HelperServiceResponse::Version(format!(
            "arcbox-helper {}",
            arcbox_constants::helper::MIN_HELPER_VERSION
        )),
    )
    .await;
    transport
}

async fn eof(mut transport: Transport) {
    while let Some(message) = transport.next().await {
        assert!(matches!(
            message.unwrap(),
            tarpc::ClientMessage::Cancel { .. }
        ));
    }
}

#[tokio::test]
async fn shutdown_and_helper_eof_retire_authority_before_joining_the_mirror() {
    for (disconnected, cleanup_fails) in [(false, false), (true, false), (true, true)] {
        tokio::time::timeout(Duration::from_secs(5), retire(disconnected, cleanup_fails))
            .await
            .unwrap();
    }
}

async fn retire(disconnected: bool, cleanup_fails: bool) {
    let (_directory, listener, mut publisher) = socket();
    let manager = Arc::clone(&publisher.manager);
    let setup = Arc::clone(&publisher.setup);
    let shutdown = publisher.shutdown.clone();
    let (ready, started) = oneshot::channel();
    let (entered, entering) = oneshot::channel();
    let (stopped, stopping) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let running = tokio::spawn(async move {
        publisher
            .connection(&mut Some(ready), |stop| async move {
                entered.send(()).unwrap();
                stop.cancelled().await;
                stopped.send(()).unwrap();
                released.await.unwrap();
                if cleanup_fails {
                    Err(std::io::Error::other("mirror cleanup failed"))
                } else {
                    Ok(())
                }
            })
            .await
    });
    let mut transport = connect(&listener).await;
    let lease = request(&mut transport).await;
    let HelperServiceRequest::ResolverLease { update } = lease.message else {
        panic!("expected the initial complete lease");
    };
    reply(
        &mut transport,
        lease.id,
        HelperServiceResponse::ResolverLease(Ok(())),
    )
    .await;
    let watch = request(&mut transport).await;
    assert!(matches!(
        watch.message,
        HelperServiceRequest::ResolverWatch { after_epoch: None }
    ));
    reply(
        &mut transport,
        watch.id,
        HelperServiceResponse::ResolverWatch(Ok(accepted(&update))),
    )
    .await;
    started.await.unwrap().unwrap();
    entering.await.unwrap();
    assert!(setup.current().dns_resolver_installed);
    let watch = request(&mut transport).await;
    assert!(matches!(
        watch.message,
        HelperServiceRequest::ResolverWatch {
            after_epoch: Some(1)
        }
    ));
    let mut changes = manager.subscribe_dns_registry();
    let mut snapshot = accepted(&update);
    snapshot.epoch = 2;
    snapshot.roots.push(arcbox_helper::ResolverAuthorityRoot {
        root: "external.local".into(),
        owner: ResolverOwner::OtherConnection,
        state: ResolverAuthorityState::Active {
            port: 6000,
            accepted_revision: 1,
        },
    });
    reply(
        &mut transport,
        watch.id,
        HelperServiceResponse::ResolverWatch(Ok(snapshot)),
    )
    .await;
    changes.changed().await.unwrap();
    assert!(matches!(
        manager.dns_registry().read().unwrap().resolve(&"external.local".parse().unwrap()),
        arcbox_dns::registry::Lookup::Instance { authority, .. }
            if authority.target == InstanceTarget::Other { port: 6000 }
    ));
    let watch = request(&mut transport).await;
    assert!(matches!(
        watch.message,
        HelperServiceRequest::ResolverWatch {
            after_epoch: Some(2)
        }
    ));
    let mut closing = if disconnected {
        drop(transport);
        None
    } else {
        shutdown.cancel();
        Some(tokio::spawn(eof(transport)))
    };
    stopping.await.unwrap();
    assert!(!setup.current().dns_resolver_installed);
    for root in ["arcbox.local", "external.local"] {
        assert!(matches!(
            manager.dns_registry().read().unwrap().resolve(&root.parse().unwrap()),
            arcbox_dns::registry::Lookup::Instance { authority, .. }
                if authority.target == InstanceTarget::Unavailable
        ));
    }
    assert!(!running.is_finished());
    if let Some(closing) = closing.as_mut() {
        assert!(
            tokio::time::timeout(Duration::from_millis(25), closing)
                .await
                .is_err(),
            "the helper connection must outlive mirror cleanup"
        );
    }
    release.send(()).unwrap();
    let result = running.await.unwrap();
    assert_eq!(result.is_err(), disconnected);
    if cleanup_fails {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("helper dispatcher stopped"), "{error}");
        assert!(error.contains("mirror cleanup failed"), "{error}");
    }
    if let Some(closing) = closing {
        closing.await.unwrap();
    }
}

#[tokio::test]
async fn shutdown_during_initial_lease_wait_never_starts_the_mirror() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (_directory, listener, publisher) = socket();
        let setup = Arc::clone(&publisher.setup);
        let shutdown = publisher.shutdown.clone();
        let (ready, started) = oneshot::channel();
        let running = tokio::spawn(publisher.run(ready, true, |_| async {
            panic!("an unconfirmed lease must not start the mirror");
        }));
        let mut transport = connect(&listener).await;
        assert!(matches!(
            request(&mut transport).await.message,
            HelperServiceRequest::ResolverLease { .. }
        ));
        shutdown.cancel();
        eof(transport).await;
        running.await.unwrap().unwrap();
        assert!(started.await.is_err());
        assert!(!setup.current().dns_resolver_installed);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn required_failure_finishes_cleanup_and_canonical_failure_reconnects() {
    for required in [true, false] {
        tokio::time::timeout(Duration::from_secs(5), failed_setup(required))
            .await
            .unwrap();
    }
}

async fn failed_setup(required: bool) {
    let (_directory, listener, publisher) = socket();
    let setup = Arc::clone(&publisher.setup);
    let shutdown = publisher.shutdown.clone();
    let (ready, mut started) = oneshot::channel();
    let running = tokio::spawn(publisher.run(ready, required, |_| async {
        panic!("a rejected lease must not start the mirror");
    }));
    let mut previous_nonce = None;
    for _ in 0..if required { 1 } else { 2 } {
        let mut transport = connect(&listener).await;
        let lease = request(&mut transport).await;
        let HelperServiceRequest::ResolverLease { update } = lease.message else {
            panic!("a new connection must reacquire its complete lease");
        };
        assert_eq!(update.revision, 1);
        assert_eq!(update.roots, ["arcbox.local"]);
        if let Some(previous) = previous_nonce {
            assert_eq!(update.nonce, previous);
        }
        previous_nonce = Some(update.nonce);
        reply(
            &mut transport,
            lease.id,
            HelperServiceResponse::ResolverLease(Err(HelperError::other("lease rejected"))),
        )
        .await;
        eof(transport).await;
        assert!(!setup.current().dns_resolver_installed);
    }
    if required {
        let error = started.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("lease rejected"));
    } else {
        assert!(matches!(
            started.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        shutdown.cancel();
    }
    running.await.unwrap().unwrap();
}
