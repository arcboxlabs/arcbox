//! The daemon's control-plane socket.
//!
//! Every service is served over Connect (CORE-53 for the sandbox API,
//! CORE-68 for the rest), so one set of handlers answers Connect, gRPC, and
//! gRPC-Web at a single address.
//!
//! Connections are served with hyper's protocol-detecting builder rather
//! than an HTTP/2-only server, which is what makes all three formats
//! reachable at once: gRPC clients arrive over HTTP/2 while
//! `curl --unix-socket` posting JSON arrives over HTTP/1.1, and the same
//! handlers answer both.

mod calls;

pub use calls::Calls;

use anyhow::{Context, Result};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

/// Concurrent HTTP/2 streams allowed per client connection.
///
/// tonic's server left this unlimited; hyper's builder defaults it to 200.
/// Keeping a cap is deliberate — a desktop client multiplexes every watch,
/// exec, and read stream over one connection, and a stream leak there
/// should back-pressure that client instead of growing the daemon without
/// bound — but the number is pinned here so it is a decision rather than
/// an inherited default.
const MAX_HTTP2_STREAMS: u32 = 200;

/// Accepts connections on `listener` until `shutdown` fires, serving `app`.
///
/// Returns once every in-flight connection has finished, so the socket is
/// free before the daemon drops its lock.
pub async fn serve(listener: UnixListener, app: axum::Router, shutdown: CancellationToken) {
    let graceful = GracefulShutdown::new();
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder.http2().max_concurrent_streams(MAX_HTTP2_STREAMS);

    loop {
        let stream = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => stream,
                Err(e) => {
                    // A single failed accept (EMFILE, a client that vanished
                    // between the SYN and the accept) must not take the
                    // control plane down with it.
                    debug!(error = %e, "control-plane accept failed");
                    continue;
                }
            },
        };

        let service = TowerToHyperService::new(app.clone());
        let conn = builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
        let conn = graceful.watch(conn.into_owned());
        drop(tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!(error = %e, "control-plane connection ended");
            }
        }));
    }

    info!("control plane draining in-flight connections");
    graceful.shutdown().await;
}

/// Binds the control-plane Unix socket, replacing any stale one.
///
/// Each server owns its socket (remove-before-bind) rather than relying on
/// a central cleanup pass, so a crashed predecessor cannot block startup.
pub fn bind(socket_path: &std::path::Path) -> Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create socket directory")?;
    }
    UnixListener::bind(socket_path).context(format!(
        "Failed to bind gRPC socket: {}",
        socket_path.display()
    ))
}

/// Builds the router: every service, plus server reflection.
///
/// Reflection covers gRPC, gRPC-Web, and Connect alike. Both `v1` and the
/// legacy `v1alpha` are registered, since `grpcurl` still asks for
/// `v1alpha` by default.
///
/// One limit is worth knowing before chasing it: `ServerReflectionInfo` is
/// bidi-streaming, and the Connect protocol carries bidi streams only over
/// HTTP/2. A plain HTTP/1.1 Connect client therefore gets `501` from
/// reflection — a property of that RPC's shape, not of this wiring. Unary
/// calls are unaffected, which is why `curl --unix-socket` still works for
/// the services themselves.
///
/// The descriptor set is `arcbox-connect`'s own — emitted by the same
/// build script that generates the served types, so it tracks the
/// GENERATED surface exactly. That is deliberately a superset of the
/// served one — reflection advertises every service the protos declare:
/// the schema-only `AgentService`, the never-implemented Container,
/// Image, Network, and Volume services, and (off macOS) the cfg-gated
/// `MacosService`. A reflected service 404ing on every format is one of
/// these, not a router bug. The prost-built set advertised the same
/// superset.
///
/// # Errors
///
/// Returns an error if the embedded descriptor set cannot be parsed, which
/// would mean the build emitted a corrupt one.
pub fn connect_router(
    system_runtime: arcbox_api::SharedRuntime,
    host_runtime: arcbox_api::SharedRuntime,
    early_runtime: arcbox_api::SharedRuntime,
    system: arcbox_api::SystemServiceImpl,
) -> Result<connectrpc::Router> {
    let reflector = connectrpc_reflection::Reflector::from_descriptor_set_bytes(
        arcbox_connect::FILE_DESCRIPTOR_SET,
    )
    .context("parsing the embedded file descriptor set for reflection")?;
    Ok(connectrpc_reflection::install(
        arcbox_api::connect::router_with_system(
            system_runtime,
            host_runtime,
            early_runtime,
            system,
        ),
        reflector,
    ))
}

/// Turns the router into the HTTP service the socket serves.
///
/// Kept separate from [`connect_router`] so tests can inspect the routing
/// table — once it is an `axum::Router` the registered method paths are no
/// longer visible.
pub fn into_app(router: connectrpc::Router, calls: Calls) -> axum::Router {
    axum::Router::new()
        .fallback_service(connectrpc::ConnectRpcService::new(calls.dispatcher(router)))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, OnceLock};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::UnixStream;

    use super::*;

    /// Brings up the real control plane on a temp socket.
    ///
    /// The runtime handle is deliberately left empty: every sandbox call
    /// then answers `unavailable`, which is enough to prove the transport —
    /// the assertions below are about which wire formats reach a handler and
    /// how its error comes back, not about sandbox behaviour. Keeping it
    /// VM-free is what lets this run in CI.
    async fn spawn_control_plane() -> (tempfile::TempDir, std::path::PathBuf, CancellationToken) {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("arcbox.sock");
        let runtime: arcbox_api::SharedRuntime = Arc::new(OnceLock::new());

        // The Connect half is built by the same function the daemon uses, so
        // a service or reflection wired up only in production would fail
        // here too.
        let system = arcbox_api::SystemServiceImpl::new(
            Arc::new(arcbox_api::SetupState::new()),
            Arc::clone(&runtime),
            Arc::clone(&runtime),
            Arc::clone(&runtime),
        );
        let app = into_app(
            connect_router(runtime.clone(), runtime.clone(), runtime, system)
                .expect("connect router"),
            Calls::default(),
        );

        let listener = bind(&socket).expect("bind temp socket");
        let shutdown = CancellationToken::new();
        drop(tokio::spawn(serve(listener, app, shutdown.clone())));
        (dir, socket, shutdown)
    }

    /// Issues a raw HTTP/1.1 request and returns the whole response.
    ///
    /// Written at the byte level on purpose: this is exactly what
    /// `curl --unix-socket` puts on the wire, so a pass here is the
    /// "callers with no proto toolchain can reach us" claim, not a
    /// client-library artifact.
    async fn http1_request(socket: &std::path::Path, request: &str) -> String {
        let mut stream = UnixStream::connect(socket).await.expect("connect");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        stream.flush().await.expect("flush");

        let mut response = Vec::new();
        // The daemon keeps HTTP/1.1 connections alive, so read only until
        // the body arrives rather than to EOF. Transport failures panic
        // with their own message here — otherwise they surface as a JSON
        // parse error on an empty body, which points at the one thing that
        // is not broken.
        let deadline = std::time::Duration::from_secs(5);
        tokio::time::timeout(deadline, async {
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.expect("read response");
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&buf[..n]);
                if response.windows(4).any(|w| w == b"\r\n\r\n")
                    && String::from_utf8_lossy(&response).contains('}')
                {
                    break;
                }
            }
        })
        .await
        .expect("daemon did not answer within 5s");
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Reassembles a `Transfer-Encoding: chunked` body from a raw response.
    ///
    /// The handler has no Content-Length to declare (the body is produced as
    /// it is encoded), so hyper chunks it — the same framing any streaming
    /// HTTP/1.1 response carries. Real clients handle this transparently;
    /// this test reads the socket directly, so it has to.
    fn dechunk(response: &str) -> String {
        let Some((_, mut rest)) = response.split_once("\r\n\r\n") else {
            return String::new();
        };
        let mut body = String::new();
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
                break;
            };
            if size == 0 || tail.len() < size {
                break;
            }
            body.push_str(&tail[..size]);
            rest = tail[size..].trim_start_matches("\r\n");
        }
        body
    }

    /// The acceptance criterion for CORE-53: one endpoint, three formats.
    ///
    /// All three target the same path on the same socket. A format that
    /// reached no handler would surface as a 404 or a connection reset
    /// rather than a service-level `unavailable`.
    #[tokio::test]
    async fn one_endpoint_answers_connect_grpc_and_grpc_web() {
        let (_dir, socket, shutdown) = spawn_control_plane().await;
        let path = "/arcbox.sandbox.v1.SandboxService/ListExposedPorts";

        // 1. Connect, JSON over HTTP/1.1 — the `curl` case.
        let response = http1_request(
            &socket,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
                 Content-Length: 2\r\n\r\n{{}}"
            ),
        )
        .await;
        assert!(
            response.contains("HTTP/1.1 503"),
            "Connect JSON should map unavailable to HTTP 503, got: {response}"
        );
        let body = dechunk(&response);
        let json: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("Connect errors are a JSON body: {e}; got {body:?}"));
        assert_eq!(
            json.get("code").and_then(serde_json::Value::as_str),
            Some("unavailable"),
            "Connect puts the error code in the JSON body: {json}"
        );

        // 2. gRPC over HTTP/2 — driven by a generated tonic client, which
        //    is now purely a test dependency: it is the independent
        //    implementation that keeps the gRPC claim honest.
        let channel = tonic::transport::Endpoint::try_from("http://localhost")
            .expect("endpoint")
            .connect_with_connector(tower::service_fn({
                let socket = socket.clone();
                move |_: tonic::transport::Uri| {
                    let socket = socket.clone();
                    async move {
                        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(socket).await?))
                    }
                }
            }))
            .await
            .expect("gRPC connect over the same socket");
        let status = arcbox_grpc::SandboxServiceClient::new(channel)
            .list_exposed_ports(tonic::Request::new(
                arcbox_grpc::arcbox_protocol::sandbox_v1::ListExposedPortsRequest::default(),
            ))
            .await
            .expect_err("empty runtime is unavailable");
        assert_eq!(
            status.code(),
            tonic::Code::Unavailable,
            "gRPC must reach the same handler and carry the same code"
        );

        // 3. gRPC-Web over HTTP/1.1 — a 5-byte-prefixed empty message.
        let body = [0u8, 0, 0, 0, 0];
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\n\
             Content-Type: application/grpc-web+proto\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        let mut stream = UnixStream::connect(&socket).await.expect("connect");
        stream.write_all(&request).await.expect("write");
        let mut response = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_end(&mut response),
        )
        .await;
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.contains("HTTP/1.1 200"),
            "gRPC-Web carries its status in the body, not the HTTP status: {response}"
        );
        assert!(
            response.contains("grpc-status: 14") || response.contains("grpc-status:14"),
            "gRPC-Web trailers should report unavailable: {response}"
        );

        shutdown.cancel();
    }

    /// Every service must actually be registered, or its path 404s: the
    /// router serves exactly what it was given, and a service dropped
    /// during the tonic migration (CORE-68) would fail nowhere else.
    /// Asserting the routes directly is hermetic; driving Icon end-to-end
    /// would reach the network.
    #[test]
    fn migrated_daemon_services_are_registered_on_the_connect_router() {
        let runtime: arcbox_api::SharedRuntime = Arc::new(OnceLock::new());
        let system = arcbox_api::SystemServiceImpl::new(
            Arc::new(arcbox_api::SetupState::new()),
            Arc::clone(&runtime),
            Arc::clone(&runtime),
            Arc::clone(&runtime),
        );
        let router = connect_router(runtime.clone(), runtime.clone(), runtime, system)
            .expect("connect router");

        for path in [
            "arcbox.v1.IconService/GetImageIcon",
            "arcbox.v1.StatsService/Watch",
            "arcbox.v1.SystemService/WatchSetupStatus",
            "arcbox.v1.SystemService/SetSystemVmResources",
            "arcbox.v1.KubernetesService/Status",
            "arcbox.v1.MigrationService/RunMigration",
            "arcbox.v1.MachineService/ExecSession",
            // Post-migration additions live under the same guarantee: a
            // service missing from the router 404s and fails nowhere else.
            "arcbox.sandbox.v1.TemplateService/Get",
            #[cfg(target_os = "macos")]
            "arcbox.v1.MacosService/List",
        ] {
            assert!(
                router.has_method(path),
                "{path} has moved to Connect; available: {:?}",
                router.methods().collect::<Vec<_>>()
            );
        }
    }

    #[tokio::test]
    async fn web_service_queries_validate_hostname_before_runtime_access() {
        let (_dir, socket, shutdown) = spawn_control_plane().await;
        for (body, code) in [
            ("{}", "unavailable"),
            (r#"{"hostname":"API.LOCAL"}"#, "unavailable"),
            (r#"{"hostname":"*.api.local"}"#, "invalid_argument"),
            (r#"{"hostname":"https://api.local"}"#, "invalid_argument"),
        ] {
            let response = http1_request(&socket, &format!(
                "POST /arcbox.v1.SystemService/ListWebServices HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()
            )).await;
            let response: serde_json::Value = serde_json::from_str(&dechunk(&response)).unwrap();
            assert_eq!(response["code"], code, "{body}");
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn prepared_host_admits_machine_updates_without_system_admission() {
        use arcbox_connect::v1::{setup_status, system_runtime_status::State};
        use arcbox_core::machine::MachineConfig;

        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(
            arcbox_core::Runtime::new(arcbox_core::config::Config::for_test(
                &directory.path().join("data"),
            ))
            .unwrap(),
        );
        runtime
            .machine_manager()
            .create(MachineConfig {
                name: "web".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let system_runtime: arcbox_api::SharedRuntime = Arc::default();
        let host_runtime: arcbox_api::SharedRuntime = Arc::default();
        let early_runtime = Arc::new(OnceLock::from(runtime.clone()));
        let setup = Arc::new(arcbox_api::SetupState::new());
        let system = arcbox_api::SystemServiceImpl::new(
            setup.clone(),
            system_runtime.clone(),
            host_runtime.clone(),
            early_runtime.clone(),
        );
        let app = into_app(
            connect_router(
                system_runtime.clone(),
                host_runtime.clone(),
                early_runtime,
                system,
            )
            .unwrap(),
            Calls::default(),
        );
        let socket = directory.path().join("api.sock");
        let listener = bind(&socket).unwrap();
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve(listener, app, shutdown.clone()));
        let set = "/arcbox.v1.MachineService/SetWebPorts";
        let body = r#"{"id":"web","httpPort":{"port":8080}}"#;
        assert_eq!(
            json_request(&socket, set, body).await["code"],
            "unavailable"
        );

        assert!(host_runtime.set(runtime.clone()).is_ok());
        setup.set_phase(setup_status::Phase::Ready, "host ready");
        for (state, reason) in [
            (State::Disabled, ""),
            (State::Starting, "waiting for System VM"),
            (State::Failed, "storage recovery required"),
        ] {
            setup.set_system_runtime(state, reason);
            let updated = json_request(&socket, set, body).await;
            assert_eq!(updated["httpPort"]["port"], 8080);
            let inspected = json_request(
                &socket,
                "/arcbox.v1.MachineService/Inspect",
                r#"{"id":"web"}"#,
            )
            .await;
            assert_eq!(inspected["webPorts"]["httpPort"]["port"], 8080);
            let backend =
                json_request(&socket, "/arcbox.v1.SystemService/GetSystemVmBackend", "{}").await;
            assert!(backend.get("backend").is_some(), "{backend}");
            for path in [
                "/arcbox.v1.KubernetesService/Status",
                "/arcbox.v1.MigrationService/PrepareMigration",
                "/arcbox.v1.SystemService/ResolveContainerFs",
                "/arcbox.sandbox.v1.SandboxService/ListExposedPorts",
            ] {
                assert_eq!(
                    json_request(&socket, path, "{}").await["code"],
                    "unavailable",
                    "{path}"
                );
            }
            assert!(system_runtime.get().is_none());
        }
        #[cfg(target_os = "macos")]
        assert!(
            json_request(&socket, "/arcbox.v1.MacosService/List", "{}")
                .await
                .get("code")
                .is_none()
        );

        for (path, request) in [
            ("Create", r#"{"name":"default"}"#),
            ("CloneMachine", r#"{"id":"web","name":"default"}"#),
            (
                "ImportMachine",
                r#"{"name":"default","path":"/missing.tar"}"#,
            ),
        ] {
            let rejected = json_request(
                &socket,
                &format!("/arcbox.v1.MachineService/{path}"),
                request,
            )
            .await;
            assert_eq!(rejected["code"], "invalid_argument", "{path}: {rejected}");
            assert!(
                rejected["message"].as_str().unwrap().contains("reserved"),
                "{path}: {rejected}"
            );
            assert!(!runtime.machine_manager().exists("default"));
        }
        runtime
            .machine_manager()
            .create(MachineConfig {
                name: "default".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let saved = directory.path().join("data/machines/default/config.toml");
        let before = std::fs::read(&saved).unwrap();
        let record = runtime.machine_manager().get("default").unwrap();
        for path in ["Start", "Stop", "Remove"] {
            let rejected = json_request(
                &socket,
                &format!("/arcbox.v1.MachineService/{path}"),
                r#"{"id":"default"}"#,
            )
            .await;
            assert_eq!(rejected["code"], "invalid_argument", "{path}: {rejected}");
            assert!(
                rejected["message"].as_str().unwrap().contains("reserved"),
                "{path}: {rejected}"
            );
            assert_eq!(std::fs::read(&saved).unwrap(), before);
            let after = runtime.machine_manager().get("default").unwrap();
            assert_eq!(after.state, record.state);
            assert_eq!(after.vm_id, record.vm_id);
        }

        let hold = runtime.machine_manager().storage_hold_path();
        std::fs::create_dir_all(hold.parent().unwrap()).unwrap();
        std::fs::write(hold, "offline-check").unwrap();
        let blocked = json_request(
            &socket,
            "/arcbox.v1.MachineService/Start",
            r#"{"id":"default"}"#,
        )
        .await;
        assert_eq!(blocked["code"], "failed_precondition");
        assert_eq!(
            json_request(&socket, set, body).await["httpPort"]["port"],
            8080
        );

        shutdown.cancel();
        server.await.unwrap();
    }

    async fn json_request(socket: &std::path::Path, path: &str, body: &str) -> serde_json::Value {
        let response = http1_request(socket, &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()
        )).await;
        serde_json::from_str(&dechunk(&response)).unwrap()
    }

    /// Reflection has to keep answering through the real router, or
    /// `grpcurl` and `buf curl` lose the ability to discover the API without
    /// vendoring the protos.
    #[tokio::test]
    async fn reflection_answers_through_the_real_router() {
        let (_dir, socket, shutdown) = spawn_control_plane().await;

        let channel = tonic::transport::Endpoint::try_from("http://localhost")
            .expect("endpoint")
            .connect_with_connector(tower::service_fn({
                let socket = socket.clone();
                move |_: tonic::transport::Uri| {
                    let socket = socket.clone();
                    async move {
                        Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(socket).await?))
                    }
                }
            }))
            .await
            .expect("connect");

        let mut client =
            tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                channel,
            );
        let request = tonic_reflection::pb::v1::ServerReflectionRequest {
            host: String::new(),
            message_request: Some(
                tonic_reflection::pb::v1::server_reflection_request::MessageRequest::ListServices(
                    String::new(),
                ),
            ),
        };
        let mut stream = client
            .server_reflection_info(tokio_stream::iter(vec![request]))
            .await
            .expect("reflection call")
            .into_inner();

        let response = tokio_stream::StreamExt::next(&mut stream)
            .await
            .expect("a reflection response")
            .expect("reflection is not an error");
        let services = match response.message_response {
            Some(tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ListServicesResponse(r)) => r.service,
            other => panic!("expected a service listing, got {other:?}"),
        };
        let names: Vec<_> = services.into_iter().map(|s| s.name).collect();
        assert!(
            names
                .iter()
                .any(|n| n == "arcbox.sandbox.v1.SandboxService"),
            "the sandbox services must stay discoverable: {names:?}"
        );

        shutdown.cancel();
    }

    /// A bidirectional call driven from both directions at once, over the
    /// socket the daemon really serves.
    ///
    /// This is the wire shape behind `abctl machine exec -it`: the client
    /// splits the stream into owned halves, a sender task writes the Init
    /// frame while the receiver awaits the response. The runtime handle is
    /// empty, so the deterministic answer is `unavailable` — but for that to
    /// arrive at all, the h2c bidi stream must open, the send half's frame
    /// must reach the handler (it reads Init before checking readiness), and
    /// the handler's error must come back through the receive half.
    #[tokio::test]
    async fn a_split_bidi_stream_reaches_the_exec_session_handler() {
        use arcbox_connect::v1 as pb;

        let (_dir, socket, shutdown) = spawn_control_plane().await;

        let authority = "http://localhost".parse().expect("static authority parses");
        let transport =
            connectrpc::client::Http2Connection::lazy_unix(&socket, authority).shared(4);
        let config = connectrpc::client::ClientConfig::new(
            "http://localhost".parse().expect("static base URI parses"),
        );
        let client = pb::MachineServiceClient::new(transport, config);

        let (mut send, mut recv) = client
            .exec_session()
            .await
            .expect("open the bidi stream")
            .into_split();

        let sender = tokio::spawn(async move {
            let init = pb::MachineExecInput {
                payload: Some(pb::machine_exec_input::Payload::Init(Box::new(
                    pb::MachineExecRequest {
                        id: "default".into(),
                        tty: true,
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            };
            // The send may race the server finishing the RPC; either way the
            // receive half below sees the handler's verdict.
            let _ = send.send(init).await;
            send.close_send();
        });

        let err = recv
            .message::<pb::MachineExecOutput>()
            .await
            .expect_err("an empty runtime answers every call with an error");
        assert_eq!(
            err.code,
            connectrpc::ErrorCode::Unavailable,
            "the handler should consume Init and fail on readiness: {err:?}"
        );
        sender.await.expect("sender task");

        shutdown.cancel();
    }
}
