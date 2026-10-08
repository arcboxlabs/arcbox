//! Binary archive guest used by CA tests; all filesystem writes stay in memory.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use arcbox_core::{Config, Runtime, VmLifecycleConfig};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use hyper::{Request, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use percent_encoding::percent_decode_str;
use tokio::net::{UnixListener, UnixStream};

use super::*;
use crate::proxy::{
    GuestConnector, GuestStream, HalfCloseStream, ProxyState, VsockShutdown, VsockStream,
};

pub(super) struct Data {
    pub bundle: Vec<u8>,
    pub writes: usize,
    pub symlink: Option<String>,
    pub reject_write: bool,
    pub operations: Vec<String>,
    pub metadata: serde_json::Value,
}

pub(super) struct Guest {
    pub state: AppState,
    pub data: Arc<Mutex<Data>>,
    _directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Guest {
    pub async fn new(metadata: serde_json::Value) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let data = Arc::new(Mutex::new(Data {
            bundle: b"original roots\n".to_vec(),
            writes: 0,
            symlink: None,
            reject_write: false,
            operations: Vec::new(),
            metadata,
        }));
        let shared = Arc::clone(&data);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    let service = service_fn(move |req| handle(req, Arc::clone(&shared)));
                    http1::Builder::new()
                        .serve_connection(TokioIo::new(HalfCloseStream::new(socket)), service)
                        .await
                        .unwrap();
                });
            }
        });
        let runtime = Runtime::with_vm_lifecycle_config(
            Config {
                data_dir: directory.path().into(),
                ..Config::default()
            },
            VmLifecycleConfig {
                skip_vm_check: true,
                ..VmLifecycleConfig::default()
            },
        )
        .unwrap();
        Self {
            state: AppState {
                runtime: Arc::new(runtime),
                proxy: Arc::new(ProxyState::new(Arc::new(Connector(socket)))),
            },
            data,
            _directory: directory,
            task,
        }
    }
}

struct Connector(PathBuf);
impl GuestConnector for Connector {
    fn connect(&self) -> Pin<Box<dyn Future<Output = Result<TokioIo<GuestStream>>> + Send + '_>> {
        Box::pin(async {
            let socket = UnixStream::connect(&self.0).await?;
            Ok(TokioIo::new(HalfCloseStream::new(
                VsockStream::from_unix_stream_with_shutdown(socket, VsockShutdown::CloseOnDropOnly),
            )))
        })
    }
}

async fn handle(
    request: Request<Incoming>,
    data: Arc<Mutex<Data>>,
) -> std::result::Result<Response, hyper::Error> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let body = request.into_body().collect().await?.to_bytes();
    let mut data = data.lock().unwrap();
    let mut response = Response::builder().status(200);
    let body = if path == "/_ping" {
        Body::from("OK")
    } else if path == "/containers/test/json" || path == "/containers/canonical-test/json" {
        Body::from(data.metadata.to_string())
    } else if path.ends_with("/start") || path.ends_with("/restart") {
        assert_eq!(method, Method::POST);
        assert!(path.starts_with("/containers/canonical-test/"));
        data.operations.push(path);
        data.metadata["State"]["Running"] = json!(true);
        response = response.status(204);
        Body::empty()
    } else {
        assert!(path == "/containers/test/archive" || path == "/containers/canonical-test/archive");
        let target = percent_decode_str(
            query
                .strip_prefix("path=")
                .unwrap()
                .split('&')
                .next()
                .unwrap(),
        )
        .decode_utf8()
        .unwrap();
        if method == Method::HEAD {
            if target != BUNDLES[0] && !BUNDLES[0].starts_with(&format!("{target}/")) {
                response = response.status(404);
            } else {
                let mode = if data.symlink.as_deref() == Some(&target) {
                    1_u32 << 27
                } else if target == BUNDLES[0] {
                    0o644
                } else {
                    1_u32 << 31
                };
                response = response.header(
                    "X-Docker-Container-Path-Stat",
                    STANDARD.encode(json!({"mode":mode}).to_string()),
                );
            }
            Body::empty()
        } else if method == Method::GET {
            assert_eq!(target, BUNDLES[0]);
            Body::from(tar(&data.bundle))
        } else {
            assert_eq!(method, Method::PUT);
            assert_eq!(target, "/etc/ssl/certs");
            assert!(query.ends_with("&noOverwriteDirNonDir=true"));
            if data.reject_write {
                response = response.status(403);
            } else {
                data.bundle = untar(&body).1;
                data.writes += 1;
            }
            Body::empty()
        }
    };
    Ok(response.body(body).unwrap())
}
