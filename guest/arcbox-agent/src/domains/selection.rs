//! The selected protocols, independent of the NAT representation.

use std::collections::BTreeSet;

use super::http_port::{HTTPS_PORT, Pin};

pub(super) const HTTPS_LABEL: &str = "dev.arcbox.https-port";
pub(super) const ALT_HTTPS_PORT: u16 = 8443;

/// The verified protocol spoken by a cleartext HTTP listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProtocol {
    Http1,
    Http2,
}

/// A selected HTTP endpoint; explicit ports can await successful discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpBackend {
    pub port: u16,
    pub protocol: Option<HttpProtocol>,
}

/// The upstream protocol served through a container's port 443.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpsBackend {
    /// Preserve the application's TLS handshake and certificate.
    Native(u16),
    /// Terminate TLS with the ArcBox CA, then relay HTTP to this port.
    Terminate(HttpBackend),
}

/// The verified or explicitly selected web services for a container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectedRoutes {
    pub http: Option<u16>,
    pub https: Option<HttpsBackend>,
}

impl SelectedRoutes {
    pub(super) fn choose(
        http: Option<HttpBackend>,
        pin: Option<Pin>,
        native: &BTreeSet<u16>,
    ) -> Self {
        let https = match pin {
            Some(Pin::Off) => None,
            Some(Pin::Port(port)) => Some(HttpsBackend::Native(port)),
            None => [HTTPS_PORT, ALT_HTTPS_PORT]
                .into_iter()
                .find(|port| native.contains(port))
                .map(HttpsBackend::Native)
                .or_else(|| http.map(HttpsBackend::Terminate)),
        };
        Self {
            http: http.map(|backend| backend.port),
            https,
        }
    }

    pub(super) fn terminated_port(self) -> Option<u16> {
        self.terminated_backend().map(|backend| backend.port)
    }

    pub(super) fn terminated_backend(self) -> Option<HttpBackend> {
        match self.https {
            Some(HttpsBackend::Terminate(backend)) => Some(backend),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(port: u16) -> HttpBackend {
        HttpBackend {
            port,
            protocol: Some(HttpProtocol::Http1),
        }
    }

    #[test]
    fn native_https_wins_and_does_not_require_http() {
        let native = BTreeSet::from([443, 8443]);
        assert_eq!(
            SelectedRoutes::choose(Some(http(80)), None, &native).https,
            Some(HttpsBackend::Native(443))
        );
        assert_eq!(
            SelectedRoutes::choose(None, None, &BTreeSet::from([8443])).https,
            Some(HttpsBackend::Native(8443))
        );
        assert_eq!(
            SelectedRoutes::choose(Some(http(8080)), None, &BTreeSet::new()).https,
            Some(HttpsBackend::Terminate(http(8080)))
        );
        assert_eq!(
            SelectedRoutes::choose(None, None, &BTreeSet::new()).https,
            None
        );
    }

    #[test]
    fn managed_routes_preserve_verified_and_unknown_protocols() {
        for protocol in [None, Some(HttpProtocol::Http1), Some(HttpProtocol::Http2)] {
            let backend = HttpBackend {
                port: 8080,
                protocol,
            };
            let selected = SelectedRoutes::choose(Some(backend), None, &BTreeSet::new());
            assert_eq!(selected.http, Some(8080));
            assert_eq!(selected.terminated_backend(), Some(backend));
        }
    }

    #[test]
    fn https_override_controls_native_and_terminated_routes() {
        let native = BTreeSet::from([443]);
        assert_eq!(
            SelectedRoutes::choose(Some(http(80)), Some(Pin::Off), &native).https,
            None
        );
        assert_eq!(
            SelectedRoutes::choose(Some(http(80)), Some(Pin::Port(9443)), &native).https,
            Some(HttpsBackend::Native(9443))
        );
    }
}
