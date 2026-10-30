//! Exercise the shared RPC barrier through real Connect requests and startup exits.

use std::task::Poll;

use arcbox_api::SetupState;
use arcbox_connect::v1::Empty;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use connectrpc::{RequestContext, Response, Router, handler_fn};
use tokio::sync::Notify;
use tower::ServiceExt as _;

use super::*;

#[tokio::test]
async fn failed_startup_closes_rpc_admission_and_waits_for_handlers() {
    startup_cleanup_waits_for_rpc(false).await;
}

#[tokio::test]
async fn interrupted_startup_closes_rpc_admission_and_waits_for_handlers() {
    startup_cleanup_waits_for_rpc(true).await;
}

fn request() -> Request<Body> {
    Request::post("/test.Lifecycle/Run")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

async fn startup_cleanup_waits_for_rpc(interrupted: bool) {
    let handles = StartupHandles::new(Arc::new(SetupState::new()));
    let started = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let router = Router::new().route(
        "test.Lifecycle",
        "Run",
        handler_fn({
            let started = Arc::clone(&started);
            let released = Arc::clone(&released);
            move |_: RequestContext, _: Empty| {
                let started = Arc::clone(&started);
                let released = Arc::clone(&released);
                async move {
                    started.notify_one();
                    released.notified().await;
                    Response::ok(Empty::default())
                }
            }
        }),
    );
    let app = crate::control_plane::into_app(router, handles.rpc_calls.clone());
    let admitted = tokio::spawn(app.clone().oneshot(request()));
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .unwrap();

    let mut cleanup = Box::pin(async {
        if interrupted {
            interrupt_startup(&handles).await.unwrap();
        } else {
            abort_failed_startup(&handles).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            () = &mut cleanup => panic!("startup cleanup abandoned the admitted RPC"),
            () = handles.shutdown.cancelled() => {}
        }
    })
    .await
    .unwrap();
    assert!(matches!(futures::poll!(&mut cleanup), Poll::Pending));
    drop(cleanup);

    let rejected = tokio::time::timeout(Duration::from_secs(3), app.oneshot(request()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(rejected.into_body(), 4096).await.unwrap();
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["code"], "unavailable");

    let survivor = handles.clone();
    drop(handles);
    let mut cleanup = Box::pin(abort_failed_startup(&survivor));
    assert!(matches!(futures::poll!(&mut cleanup), Poll::Pending));
    released.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(3), admitted)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    tokio::time::timeout(Duration::from_secs(3), &mut cleanup)
        .await
        .unwrap();
}
