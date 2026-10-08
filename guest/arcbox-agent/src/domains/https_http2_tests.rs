use super::*;
use crate::domains::{
    EndpointFacts, HttpBackend, HttpProtocol, HttpsBackend, SelectedRoutes, http_tasks,
};
use bytes::Bytes;
use http_body_util::{BodyExt as _, Empty};
use hyper_util::rt::TokioIo;

#[tokio::test]
async fn managed_h2_rejects_replaced_pinned_routes_without_closing_an_existing_stream() {
    for protocol in [Some(HttpProtocol::Http1), None] {
        within(async {
            let incoming = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let address = incoming.local_addr().unwrap();
            let backend = Arc::new(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap());
            let port = backend.local_addr().unwrap().port();
            let dns = DnsRegistry::default();
            let services = WebServices::new(dns.clone());
            publish(&services, &dns, "a", port, "arcbox.local");
            let inspect = serde_json::json!({
                "Id": "a", "Name": "/web", "State": {"Pid": 1},
                "Config": {"Labels": {"dev.arcbox.http-port": port.to_string()}},
                "NetworkSettings": {
                    "SandboxKey": "/var/run/docker/netns/web",
                    "Networks": {"bridge": {"IPAddress": "127.0.0.1"}}
                }
            });
            services.publish(
                &EndpointFacts::from_inspect(&inspect, &"arcbox.local".parse().unwrap()).unwrap(),
                SelectedRoutes {
                    http: Some(port),
                    https: Some(HttpsBackend::Terminate(HttpBackend { port, protocol })),
                },
            );
            let server = tokio::spawn({
                let backend = backend.clone();
                async move {
                    if protocol.is_none() {
                        let (mut probe, _) = backend.accept().await.unwrap();
                        let mut preface = [0; 24];
                        probe.read_exact(&mut preface).await.unwrap();
                        assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                        drop(probe);
                        let (mut probe, _) = backend.accept().await.unwrap();
                        assert!(head(&mut probe).await.starts_with("HEAD / HTTP/1.1\r\n"));
                        probe
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    }
                    let (mut upstream, _) = backend.accept().await.unwrap();
                    assert!(
                        head(&mut upstream)
                            .await
                            .starts_with("GET /stream HTTP/1.1\r\n")
                    );
                    upstream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
                        )
                        .await
                        .unwrap();
                    upstream.read_to_end(&mut Vec::new()).await.unwrap();
                }
            });
            let cancelled = CancellationToken::new();
            let relay = tokio::spawn({
                let services = services.clone();
                let cancelled = cancelled.clone();
                async move {
                    let mut config = (*configuration()).clone();
                    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                    http_tasks::run(&cancelled, async |executor| {
                        relay_to(
                            incoming.accept().await.unwrap().0,
                            Ipv4Addr::LOCALHOST,
                            &services,
                            async |_| Ok(Arc::new(config)),
                            async |address| TcpStream::connect(address).await,
                            executor,
                        )
                        .await
                    })
                    .await
                }
            });
            let tls = TlsConnector::from(crate::domains::probe::configuration())
                .connect(
                    NAME.try_into().unwrap(),
                    TcpStream::connect(address).await.unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
            let tasks = http_tasks::Executor::default();
            let (mut requests, driver) =
                hyper::client::conn::http2::handshake(tasks.clone(), TokioIo::new(tls))
                    .await
                    .unwrap();
            let driver = tokio::spawn(driver);
            let request = |path: &str| {
                hyper::Request::get(format!("https://{NAME}{path}"))
                    .body(Empty::<Bytes>::new())
                    .unwrap()
            };
            let mut held = requests
                .send_request(request("/stream"))
                .await
                .unwrap()
                .into_body();
            assert_eq!(
                held.frame().await.unwrap().unwrap().into_data().unwrap(),
                b"x"[..]
            );
            replace_owner(&services, &dns, port);
            assert!(requests.send_request(request("/replaced")).await.is_err());
            std::future::poll_fn(|context| {
                assert!(
                    backend.poll_accept(context).is_pending(),
                    "replaced route opened another upstream"
                );
                std::task::Poll::Ready(())
            })
            .await;
            // A refused sibling leaves the existing stream open until listener cancellation.
            std::future::poll_fn(|context| {
                let frame = held.frame();
                tokio::pin!(frame);
                assert!(frame.poll(context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            cancelled.cancel();
            relay.await.unwrap().unwrap();
            server.await.unwrap();
            let _peer_close = driver.await.unwrap();
            tasks.shutdown().await;
        })
        .await;
    }
}
