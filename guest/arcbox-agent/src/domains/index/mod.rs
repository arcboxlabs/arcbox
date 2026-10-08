//! HTTP responses pinned to the current built-in directory registration.

mod page;
mod publisher;

pub(super) use publisher::Publisher;

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use arcbox_constants::dns::Domain;
use arcbox_dns::{
    NamePattern,
    registry::{Lookup, Registry},
};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode, Version, header};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use super::WebServices;
use super::http_port::HTTPS_PORT;
use super::http_tasks::Executor;

/// A snapshot of the trusted publisher's unique apex and gateway registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Route {
    name: Domain,
    pub(super) destination: Ipv4Addr,
    revision: u64,
}

impl Route {
    pub(super) fn name(&self) -> &Domain {
        &self.name
    }

    pub(super) fn resolve(registry: &Registry, destination: Ipv4Addr) -> Option<Self> {
        let registration = registry.get("system:index")?;
        let [claim] = registration.claims.as_slice() else {
            return None;
        };
        let NamePattern::Exact(name) = claim.pattern() else {
            return None;
        };
        let Lookup::Matched {
            claim,
            address,
            revision,
            publishable: true,
        } = registry.resolve(name)
        else {
            return None;
        };
        (claim.owner() == "system:index"
            && claim.authority() == "index"
            && address == IpAddr::V4(destination))
        .then(|| Self {
            name: name.clone(),
            destination,
            revision,
        })
    }
}

pub(super) async fn serve(
    client: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    services: WebServices,
    route: Route,
    port: u16,
) -> anyhow::Result<()> {
    let service = hyper::service::service_fn(move |request| {
        std::future::ready(Ok::<_, Infallible>(response(
            request, &services, &route, port,
        )))
    });
    hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .serve_connection(TokioIo::new(client), service)
        .await?;
    Ok(())
}

pub(super) async fn serve_tls(
    client: TlsStream<TcpStream>,
    services: WebServices,
    route: Route,
    executor: Executor,
) -> anyhow::Result<()> {
    if client.get_ref().1.alpn_protocol() != Some(b"h2") {
        return serve(client, services, route, HTTPS_PORT).await;
    }
    let service = hyper::service::service_fn(move |request| {
        std::future::ready(Ok::<_, Infallible>(response(
            request, &services, &route, HTTPS_PORT,
        )))
    });
    hyper::server::conn::http2::Builder::new(executor)
        .serve_connection(TokioIo::new(client), service)
        .await?;
    Ok(())
}

fn accepts_authority<B>(request: &Request<B>, route: &Route, port: u16) -> bool {
    let matches = |value: &str| {
        value.eq_ignore_ascii_case(route.name.as_str())
            || value.rsplit_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case(route.name.as_str()) && value.parse::<u16>() == Ok(port)
            })
    };
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return false;
    }
    let matches_host = |host: &header::HeaderValue| host.to_str().is_ok_and(matches);
    if request.version() == Version::HTTP_2 {
        request
            .uri()
            .authority()
            .is_some_and(|authority| matches(authority.as_str()))
            && host.is_none_or(matches_host)
    } else {
        host.is_some_and(matches_host)
    }
}

fn response<B>(
    request: Request<B>,
    services: &WebServices,
    route: &Route,
    port: u16,
) -> Response<Full<Bytes>> {
    let directory = services.directory_urls(route);
    let status = if directory.is_err() {
        StatusCode::SERVICE_UNAVAILABLE
    } else if !accepts_authority(&request, route, port) {
        StatusCode::MISDIRECTED_REQUEST
    } else if request.uri().path() != "/" {
        StatusCode::NOT_FOUND
    } else if request.method() != Method::GET && request.method() != Method::HEAD {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::OK
    };
    let body = if status == StatusCode::OK {
        page::render(directory.unwrap().as_deref())
    } else {
        status.to_string()
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_LENGTH, body.len());
    if status == StatusCode::METHOD_NOT_ALLOWED {
        response = response.header(header::ALLOW, "GET, HEAD");
    }
    response
        .body(Full::new(if request.method() == Method::HEAD {
            Bytes::new()
        } else {
            body.into()
        }))
        .unwrap()
}

#[cfg(test)]
mod tests_tls;

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_dns::{
        claims::Claim,
        registry::{InstanceAuthority, InstanceTarget},
    };
    use http_body_util::BodyExt as _;

    #[tokio::test]
    async fn keepalive_requests_require_current_apex_gateway_and_authority() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let apex: Domain = "arcbox.local".parse().unwrap();
            let gateway = Ipv4Addr::new(172, 17, 0, 1);
            let dns: crate::dns_server::DnsRegistry = Default::default();
            let services = WebServices::new(dns.clone());
            let claim = Claim::exclusive(
                NamePattern::Exact(apex.clone()),
                "system:index".into(),
                "index".into(),
            );
            dns.write()
                .unwrap()
                .replace(
                    "system:index".into(),
                    vec![claim.clone()],
                    Some(gateway.into()),
                )
                .unwrap();
            assert!(services.index_route(gateway).is_none());
            dns.write()
                .unwrap()
                .replace_instance_authorities(vec![InstanceAuthority {
                    root: apex,
                    target: InstanceTarget::Current,
                }]);
            for (patterns, authority) in [
                (&["*.arcbox.local"][..], "index"),
                (&["arcbox.local"][..], "container"),
                (&["arcbox.local", "other.local"][..], "index"),
            ] {
                let claims = patterns
                    .iter()
                    .map(|pattern| {
                        Claim::exclusive(
                            NamePattern::parse_custom(pattern).unwrap(),
                            "system:index".into(),
                            authority.into(),
                        )
                    })
                    .collect();
                dns.write()
                    .unwrap()
                    .replace("system:index".into(), claims, Some(gateway.into()))
                    .unwrap();
                assert!(services.index_route(gateway).is_none());
            }
            dns.write()
                .unwrap()
                .replace("system:index".into(), vec![claim], Some(gateway.into()))
                .unwrap();
            assert!(services.index_route(Ipv4Addr::new(172, 17, 0, 2)).is_none());
            let route = services.index_route(gateway).unwrap();
            let (client, server) = tokio::io::duplex(16 * 1024);
            let task = tokio::spawn(serve(server, services.clone(), route, 80));
            let (mut requests, driver) =
                hyper::client::conn::http1::handshake(TokioIo::new(client))
                    .await
                    .unwrap();
            let driver = tokio::spawn(driver);
            for (host, expected) in [
                ("arcbox.local", StatusCode::OK),
                ("ARCBOX.LOCAL:80", StatusCode::OK),
                ("arcbox.local:443", StatusCode::MISDIRECTED_REQUEST),
                ("web.arcbox.local", StatusCode::MISDIRECTED_REQUEST),
                ("arcbox.local.evil", StatusCode::MISDIRECTED_REQUEST),
            ] {
                let request = Request::builder()
                    .uri("/")
                    .header(header::HOST, host)
                    .body(Full::new(Bytes::new()))
                    .unwrap();
                let response = requests.send_request(request).await.unwrap();
                assert_eq!(response.status(), expected, "{host}");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                if expected == StatusCode::OK {
                    assert!(String::from_utf8_lossy(&body).contains("directory is unavailable"));
                }
            }
            let directory = services.directory_source();
            directory.ready();
            let request = || {
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "arcbox.local")
                    .body(Full::new(Bytes::new()))
                    .unwrap()
            };
            let mut duplicate = request();
            duplicate
                .headers_mut()
                .append(header::HOST, header::HeaderValue::from_static("evil"));
            let response = requests.send_request(duplicate).await.unwrap();
            assert_eq!(response.status(), StatusCode::MISDIRECTED_REQUEST);
            response.into_body().collect().await.unwrap();
            let response = requests.send_request(request()).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("No running web services"));
            dns.write().unwrap().invalidate_instance_authorities();
            let response = requests.send_request(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            response.into_body().collect().await.unwrap();
            drop(requests);
            driver.await.unwrap().unwrap();
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }
}
