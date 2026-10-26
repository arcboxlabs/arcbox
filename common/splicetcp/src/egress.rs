//! Egress resolution seam.
//!
//! Decouples the "decide + dial an upstream transport for an outbound SYN"
//! policy from the TCP bridge. The bridge holds an [`EgressResolver`] trait
//! object and calls [`EgressResolver::resolve`] per flow; the default
//! [`DefaultEgress`] reproduces the bridge's historical inline behavior
//! (system-proxy aware connect via SOCKS5 / HTTP CONNECT, or a direct
//! `TcpStream::connect`, with the gateway→loopback translation and a connect
//! timeout). A consumer (e.g. an Inbound/Router/Outbound host) can replace the
//! resolver to inject its own egress policy.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use arcbox_fakeip::proxy_detect::ProxyEnvironment;
use tokio::sync::oneshot;

/// An IPv4 or IPv6 flow and the metadata needed to select egress.
#[derive(Clone, Debug)]
pub struct FlowMeta {
    pub src_ip: IpAddr,
    pub src_port: u16,
    pub dst_ip: IpAddr,
    pub dst_port: u16,
    /// Destination domain recovered from the DNS log, if known.
    pub domain: Option<String>,
}

/// A resolved egress transport.
///
/// Currently always a real fd, which the bridge splices on the fast path. A
/// boxed-stream variant for relayed (no-fd) encrypted protocols will be added
/// together with its bridge relay implementation, rather than reserved ahead of
/// a consumer.
pub enum EgressConn {
    /// Has a real fd → the bridge splices it on the fast path.
    Tcp(crate::NativeTcp<tokio::net::TcpStream>),
    /// A polled transport with explicit control over guest-side abandonment.
    ControlledTcp(crate::NativeTcp<tokio::net::TcpStream>, AbandonHandle),
}

impl EgressConn {
    /// Creates a transport that can stop relaying without closing the guest TCP connection.
    /// Call the returned handle before dropping the consumer's stream on a silent rejection.
    pub fn controlled_tcp(
        stream: impl Into<crate::NativeTcp<tokio::net::TcpStream>>,
    ) -> (Self, AbandonHandle) {
        let signal = Arc::new(AtomicBool::new(false));
        (
            Self::ControlledTcp(stream.into(), AbandonHandle(Arc::clone(&signal))),
            AbandonHandle(signal),
        )
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        crate::NativeTcp<tokio::net::TcpStream>,
        Option<Arc<AtomicBool>>,
    ) {
        match self {
            Self::Tcp(stream) => (stream, None),
            Self::ControlledTcp(stream, control) => (stream, Some(control.0)),
        }
    }
}

/// One-shot control for abandoning a guest TCP connection after inspection.
/// Dropping the handle without calling [`Self::abandon`] preserves ordinary close behavior.
pub struct AbandonHandle(Arc<AtomicBool>);

impl AbandonHandle {
    /// Signals abandonment synchronously. Call before dropping the consumer's stream.
    /// The next bridge poll or guest packet releases the transport without emitting FIN or RST.
    pub fn abandon(self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The terminal result of resolving a guest TCP handshake.
pub enum EgressOutcome {
    /// Establish the guest handshake using this upstream transport.
    Connect(EgressConn),
    /// Refuse the guest handshake with a TCP reset.
    Reject,
    /// Remove the guest handshake without sending a response.
    Drop,
}

/// Decides + establishes egress for a flow. Injected into the bridge so a
/// consumer (e.g. an Inbound/Router/Outbound host) can replace the policy.
pub trait EgressResolver: Send + Sync {
    /// Resolves the flow. A closed result channel refuses the guest handshake.
    fn resolve(&self, flow: FlowMeta) -> oneshot::Receiver<EgressOutcome>;
}

/// An IPv4 or IPv6 flow's 4-tuple handed to a [`FlowObserver`] on termination.
/// A public mirror of the bridge's internal flow key (whose fields are private).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub src_ip: IpAddr,
    pub src_port: u16,
    pub dst_ip: IpAddr,
    pub dst_port: u16,
}

/// Observes the byte totals of fast-path flows the bridge spliced.
///
/// Those bytes are otherwise invisible to the [`EgressResolver`] host, which only
/// hands in the egress stream and never sees what transits it. Injected into the
/// bridge (mirroring [`EgressResolver`]) so a consumer can account per-flow traffic.
pub trait FlowObserver: Send + Sync {
    /// Called once when a fast-path flow is torn down (guest FIN/RST, host EOF,
    /// or an I/O error). `up_bytes` = guest→host (client→server), `down_bytes` =
    /// host→guest (server→client).
    fn on_flow_close(&self, key: FlowKey, up_bytes: u64, down_bytes: u64);
}

/// Default egress policy reproducing the TCP bridge's historical inline behavior.
///
/// Resolves a system-proxy CONNECT/SOCKS5 target (or direct), translates the
/// gateway IP to loopback, and dials with a connect timeout.
pub struct DefaultEgress {
    gateway_ip: Ipv4Addr,
    proxy_env: Option<ProxyEnvironment>,
    connect_timeout_secs: u64,
}

impl DefaultEgress {
    pub fn new(
        gateway_ip: Ipv4Addr,
        proxy_env: Option<ProxyEnvironment>,
        connect_timeout_secs: u64,
    ) -> Self {
        Self {
            gateway_ip,
            proxy_env,
            connect_timeout_secs,
        }
    }

    /// Determines whether to connect via a proxy tunnel for the given destination.
    ///
    /// Returns `Some((proxy_authority, target_host, target_port, protocol))` if a
    /// proxy should be used, or `None` for direct connection. The proxy authority
    /// is a `"host:port"` string that `TcpStream::connect` can resolve (supports
    /// both IP addresses and hostnames like `proxy.corp.com`).
    fn resolve_proxy_target(
        &self,
        dst_ip: IpAddr,
        dst_port: u16,
        domain: Option<&str>,
    ) -> Option<(String, String, u16, &'static str)> {
        // The gateway IP means "the host itself" (`host.docker.internal`):
        // it is translated to loopback at connect time and must never be
        // tunnelled through a proxy — the proxy would dial a virtual
        // address that exists only inside the datapath.
        if dst_ip == IpAddr::V4(self.gateway_ip) {
            return None;
        }

        let env = self.proxy_env.as_ref()?;

        // No proxy configured → always direct.
        if !env.has_usable_proxy() {
            return None;
        }

        // A usable proxy is configured (checked above), so every destination is
        // tunnelled through it — including IP literals and DNS-log misses, not
        // just Fake-IP. Resolve the CONNECT/SOCKS5 target host: the recovered
        // domain when known, otherwise the destination IP string, which the
        // proxy resolves on its end (Surge does this). Fake-IP destinations
        // (198.18.0.0/15) without a recovered domain fall here too and are
        // mapped back by the proxy.
        let host = match domain {
            Some(d) => d.to_string(),
            None => dst_ip.to_string(),
        };

        // Honor the bypass list (direct for excepted hosts).
        if env.should_bypass(&host) {
            return None;
        }

        // Prefer SOCKS5 (supports all protocols and avoids TLS issues),
        // then HTTPS proxy (HTTP CONNECT works on any port, not just 443),
        // then HTTP proxy as last resort.
        if let Some(ref socks) = env.socks_proxy {
            let authority = socks.to_string();
            return Some((authority, host, dst_port, "socks5"));
        }

        if let Some(ref https) = env.https_proxy {
            let authority = https.to_string();
            return Some((authority, host, dst_port, "http-connect"));
        }

        if let Some(ref http) = env.http_proxy {
            let authority = http.to_string();
            return Some((authority, host, dst_port, "http-connect"));
        }

        None
    }
}

impl EgressResolver for DefaultEgress {
    fn resolve(&self, flow: FlowMeta) -> oneshot::Receiver<EgressOutcome> {
        let proxy_target =
            self.resolve_proxy_target(flow.dst_ip, flow.dst_port, flow.domain.as_deref());
        // Direct connect target. Gateway IP → loopback for host.docker.internal.
        let target_ip = if flow.dst_ip == IpAddr::V4(self.gateway_ip) {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            flow.dst_ip
        };
        let connect_addr = SocketAddr::new(target_ip, flow.dst_port);
        let timeout_secs = self.connect_timeout_secs;
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let connect = async {
                match proxy_target {
                    // SOCKS5 (preferred): the proxy resolves the hostname.
                    Some((authority, host, port, "socks5")) => {
                        arcbox_proxy::socks5::connect_via_socks5(&authority, &host, port).await
                    }
                    // HTTP CONNECT (https/http system proxy).
                    Some((authority, host, port, _)) => {
                        arcbox_proxy::http_connect::connect_via_http_proxy(&authority, &host, port)
                            .await
                    }
                    // No proxy configured / bypassed → direct.
                    None => tokio::net::TcpStream::connect(connect_addr).await,
                }
            };
            let stream =
                tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), connect)
                    .await
                    .ok()
                    .and_then(Result::ok);
            let outcome = match stream {
                Some(stream) => EgressOutcome::Connect(EgressConn::Tcp(stream.into())),
                None => EgressOutcome::Reject,
            };
            let _ = tx.send(outcome);
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcbox_fakeip::proxy_detect::ProxyConfig;

    fn env(http: bool, https: bool, socks: bool) -> ProxyEnvironment {
        let cfg = |port| {
            Some(ProxyConfig {
                host: "127.0.0.1".to_string(),
                port,
            })
        };
        ProxyEnvironment {
            fake_ip_active: false,
            http_proxy: if http { cfg(3128) } else { None },
            https_proxy: if https { cfg(3129) } else { None },
            socks_proxy: if socks { cfg(1080) } else { None },
            bypass_domains: vec![],
        }
    }

    fn egress(env: Option<ProxyEnvironment>) -> DefaultEgress {
        DefaultEgress::new(Ipv4Addr::new(10, 0, 0, 1), env, 5)
    }

    // Regression: a domain-less, non-Fake-IP destination must still be proxied
    // (via the IP literal) when a system proxy is configured, instead of
    // bypassing to a direct connect.
    #[test]
    fn domainless_ip_dst_proxied_via_socks() {
        let eg = egress(Some(env(false, false, true)));
        assert_eq!(
            eg.resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 443, None),
            Some((
                "127.0.0.1:1080".to_string(),
                "1.2.3.4".to_string(),
                443,
                "socks5"
            )),
        );
    }

    #[test]
    fn domainless_ip_dst_proxied_via_https_then_http() {
        // HTTPS preferred over HTTP.
        let eg = egress(Some(env(true, true, false)));
        assert_eq!(
            eg.resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 80, None),
            Some((
                "127.0.0.1:3129".to_string(),
                "1.2.3.4".to_string(),
                80,
                "http-connect"
            )),
        );
        // HTTP only.
        let eg = egress(Some(env(true, false, false)));
        assert_eq!(
            eg.resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 80, None),
            Some((
                "127.0.0.1:3128".to_string(),
                "1.2.3.4".to_string(),
                80,
                "http-connect"
            )),
        );
    }

    // SOCKS5 wins when several proxies are present.
    #[test]
    fn socks_preferred_over_http_proxies() {
        let eg = egress(Some(env(true, true, true)));
        let got = eg.resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 443, None);
        assert_eq!(got.unwrap().3, "socks5");
    }

    // Regression: the gateway IP (`host.docker.internal`) is translated to
    // loopback at connect time and must stay direct even when a system proxy
    // is configured — tunnelling it would make the proxy dial a virtual
    // address that exists only inside the datapath.
    #[test]
    fn gateway_dst_is_direct_despite_proxy() {
        let eg = egress(Some(env(true, true, true)));
        assert_eq!(
            eg.resolve_proxy_target(Ipv4Addr::new(10, 0, 0, 1).into(), 8080, None),
            None,
        );
    }

    // No usable proxy → direct, regardless of domain.
    #[test]
    fn no_proxy_is_direct() {
        let eg = egress(Some(env(false, false, false)));
        assert_eq!(
            eg.resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 443, Some("example.com")),
            None,
        );
        assert_eq!(
            egress(None).resolve_proxy_target(Ipv4Addr::new(1, 2, 3, 4).into(), 443, None),
            None,
        );
    }

    #[test]
    fn every_system_proxy_kind_formats_ipv6_authorities() {
        for (http, https, socks, port) in [
            (true, false, false, 3128),
            (false, true, false, 3129),
            (false, false, true, 1080),
        ] {
            let mut proxies = env(http, https, socks);
            for config in [
                &mut proxies.http_proxy,
                &mut proxies.https_proxy,
                &mut proxies.socks_proxy,
            ]
            .into_iter()
            .flatten()
            {
                config.host = "::1".into();
            }
            let target = egress(Some(proxies))
                .resolve_proxy_target(
                    Ipv4Addr::new(192, 0, 2, 1).into(),
                    443,
                    Some("service.example"),
                )
                .unwrap();
            assert_eq!(target.0, format!("[::1]:{port}"));
            assert_eq!((target.1.as_str(), target.2), ("service.example", 443));
        }
    }

    #[test]
    fn ipv6_proxy_targets_preserve_literals_domains_and_bypass() {
        let ip = "2001:db8::99".parse::<IpAddr>().unwrap();
        let mut proxy = env(true, true, true);
        assert_eq!(
            egress(Some(proxy.clone())).resolve_proxy_target(ip, 443, None),
            Some((
                "127.0.0.1:1080".into(),
                "2001:db8::99".into(),
                443,
                "socks5"
            ))
        );
        assert_eq!(
            egress(Some(proxy.clone())).resolve_proxy_target(ip, 443, Some("service.example")),
            Some((
                "127.0.0.1:1080".into(),
                "service.example".into(),
                443,
                "socks5"
            ))
        );
        proxy.bypass_domains.push("2001:db8::99".into());
        assert!(
            egress(Some(proxy))
                .resolve_proxy_target(ip, 443, None)
                .is_none()
        );
    }

    #[tokio::test]
    async fn direct_ip_families_and_ipv4_gateway_exchange_bytes() {
        use std::net::Ipv6Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let gateway = Ipv4Addr::new(10, 0, 0, 1);
        for (listen_ip, target_ip) in [
            (
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            ),
            (
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ),
            (IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V4(gateway)),
        ] {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let listener = tokio::net::TcpListener::bind(SocketAddr::new(listen_ip, 0))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let trap = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                trap.set_nonblocking(true).unwrap();
                let proxy = (target_ip == IpAddr::V4(gateway)).then(|| {
                    let mut proxy = env(false, false, true);
                    proxy.socks_proxy.as_mut().unwrap().port = trap.local_addr().unwrap().port();
                    proxy
                });
                let resolver = DefaultEgress::new(gateway, proxy, 2);
                let flow = FlowMeta {
                    src_ip: listen_ip,
                    src_port: 40000,
                    dst_ip: target_ip,
                    dst_port: address.port(),
                    domain: None,
                };
                let EgressOutcome::Connect(EgressConn::Tcp(mut client)) =
                    resolver.resolve(flow).await.unwrap()
                else {
                    panic!("direct egress must connect");
                };
                let (mut server, _) = listener.accept().await.unwrap();
                assert_eq!(client.peer_addr().unwrap(), address);
                client.write_all(b"request").await.unwrap();
                let mut request = [0; 7];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"request");
                server.write_all(b"reply").await.unwrap();
                let mut reply = [0; 5];
                client.read_exact(&mut reply).await.unwrap();
                assert_eq!(&reply, b"reply");
                assert_eq!(
                    trap.accept().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
            })
            .await
            .expect("egress must complete within five seconds");
        }
    }
}
