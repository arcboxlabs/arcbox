use std::{sync::Arc, task::Poll, time::Duration};

use arcbox_connect::v1::Empty;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use connectrpc::{
    Response, ServiceStream, bidi_streaming_handler_fn, client_streaming_handler_fn, handler_fn,
};
use tokio::sync::{Notify, mpsc, oneshot};
use tower::ServiceExt as _;

use super::*;

#[tokio::test]
async fn close_rejects_partial_client_and_bidi_input() {
    let (input_seen, mut started) = mpsc::unbounded_channel();
    let release_bidi = Arc::new(Notify::new());
    let router = Router::new()
        .route_client_stream(
            "test.Input",
            "Client",
            client_streaming_handler_fn({
                let input_seen = input_seen.clone();
                move |ctx: RequestContext, mut input: ServiceStream<Empty>| {
                    let input_seen = input_seen.clone();
                    async move {
                        input.next().await.unwrap().unwrap();
                        input_seen.send(()).unwrap();
                        let error = input.next().await.unwrap().unwrap_err();
                        assert_eq!(error.code, connectrpc::ErrorCode::Unavailable);
                        assert!(
                            ctx.extensions()
                                .get::<CancellationToken>()
                                .unwrap()
                                .is_cancelled()
                        );
                        assert!(input.next().await.is_none());
                        Err::<Response<Empty>, _>(error)
                    }
                }
            }),
        )
        .route_bidi_stream(
            "test.Input",
            "Bidi",
            bidi_streaming_handler_fn({
                let release_bidi = Arc::clone(&release_bidi);
                move |ctx: RequestContext, mut input: ServiceStream<Empty>| {
                    let input_seen = input_seen.clone();
                    let release_bidi = Arc::clone(&release_bidi);
                    async move {
                        input.next().await.unwrap().unwrap();
                        input_seen.send(()).unwrap();
                        let error = input.next().await.unwrap().unwrap_err();
                        assert_eq!(error.code, connectrpc::ErrorCode::Unavailable);
                        assert!(
                            ctx.extensions()
                                .get::<CancellationToken>()
                                .unwrap()
                                .is_cancelled()
                        );
                        assert!(input.next().await.is_none());
                        release_bidi.notified().await;
                        Err::<Response<ServiceStream<Empty>>, _>(error)
                    }
                }
            }),
        );
    let calls = Calls::default();
    let dispatcher = calls.clone().dispatcher(router);
    let mut client = dispatcher.call_client_streaming(
        "test.Input/Client",
        RequestContext::new(Default::default()),
        Box::pin(futures::stream::iter([Ok(Bytes::new())]).chain(futures::stream::pending())),
        CodecFormat::Proto,
    );
    let mut bidi = dispatcher.call_bidi_streaming(
        "test.Input/Bidi",
        RequestContext::new(Default::default()),
        Box::pin(futures::stream::iter([Ok(Bytes::new())]).chain(futures::stream::pending())),
        CodecFormat::Proto,
    );
    assert!(matches!(futures::poll!(&mut client), Poll::Pending));
    assert!(matches!(futures::poll!(&mut bidi), Poll::Pending));
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(3), started.recv())
            .await
            .unwrap()
            .unwrap();
    }
    calls.close();
    let mut joined = Box::pin(calls.wait());
    assert!(matches!(futures::poll!(&mut joined), Poll::Pending));
    assert_eq!(
        client.await.err().unwrap().code,
        connectrpc::ErrorCode::Unavailable
    );
    assert!(matches!(futures::poll!(&mut joined), Poll::Pending));
    release_bidi.notify_one();
    assert_eq!(
        bidi.await.err().unwrap().code,
        connectrpc::ErrorCode::Unavailable
    );
    joined.await;
}

#[tokio::test]
async fn close_and_caller_drop_retain_native_work_and_async_cleanup() {
    let calls = Calls::default();
    let (native_started, started) = oneshot::channel();
    let (release_native, native_release) = oneshot::channel();
    let (cleanup_started, cleaning) = oneshot::channel();
    let (release_cleanup, cleanup_release) = oneshot::channel();
    let (cleanup_done, completed) = oneshot::channel();
    let mut caller = calls.track(|| {
        // Close between admission and task creation to exercise the admission race.
        calls.close();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                native_started.send(()).unwrap();
                native_release.blocking_recv().unwrap();
            })
            .await
            .unwrap();
            cleanup_started.send(()).unwrap();
            cleanup_release.await.unwrap();
            cleanup_done.send(()).unwrap();
            Ok(())
        })
    });
    assert!(matches!(futures::poll!(&mut caller), Poll::Pending));
    tokio::time::timeout(Duration::from_secs(3), started)
        .await
        .unwrap()
        .unwrap();
    drop(caller);

    let mut joined = Box::pin(calls.wait());
    assert!(matches!(futures::poll!(&mut joined), Poll::Pending));
    release_native.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), cleaning)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(futures::poll!(&mut joined), Poll::Pending));
    release_cleanup.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), joined)
        .await
        .unwrap();
    completed.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn connect_deadline_preserves_admitted_handler_completion() {
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let router = Router::new().route(
        "test.Lifecycle",
        "Run",
        handler_fn({
            let started = Arc::clone(&started);
            let released = Arc::clone(&released);
            move |ctx: RequestContext, _: Empty| {
                let started = Arc::clone(&started);
                let released = Arc::clone(&released);
                async move {
                    started.notify_one();
                    released.notified().await;
                    assert!(
                        ctx.extensions()
                            .get::<CancellationToken>()
                            .unwrap()
                            .is_cancelled()
                    );
                    Response::ok(Empty::default())
                }
            }
        }),
    );
    let calls = Calls::default();
    let app = crate::control_plane::into_app(router, calls.clone());
    let request = Request::post("/test.Lifecycle/Run")
        .header("content-type", "application/json")
        .header("connect-timeout-ms", "1000")
        .body(Body::from("{}"))
        .unwrap();
    let response = tokio::spawn(app.oneshot(request));
    started.notified().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let response = response.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["code"], "deadline_exceeded");

    calls.close();
    let mut joined = Box::pin(calls.wait());
    assert!(matches!(futures::poll!(&mut joined), Poll::Pending));
    released.notify_one();
    tokio::time::timeout(Duration::from_secs(3), joined)
        .await
        .unwrap();
}
