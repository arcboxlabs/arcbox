//! The selected protocols, independent of the NAT representation.

use std::collections::BTreeSet;

use super::http_port::{HTTPS_PORT, Pin};

pub(super) const HTTPS_LABEL: &str = "dev.arcbox.https-port";
pub(super) const ALT_HTTPS_PORT: u16 = 8443;

/// The upstream protocol served through a container's port 443.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpsBackend {
    /// Preserve the application's TLS handshake and certificate.
    Native(u16),
    /// Terminate TLS with the ArcBox CA, then relay HTTP to this port.
    Terminate(u16),
}

/// The verified or explicitly selected web services for a container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelectedRoutes {
    pub http: Option<u16>,
    pub https: Option<HttpsBackend>,
}

impl SelectedRoutes {
    pub(super) fn choose(http: Option<u16>, pin: Option<Pin>, native: &BTreeSet<u16>) -> Self {
        let https = match pin {
            Some(Pin::Off) => None,
            Some(Pin::Port(port)) => Some(HttpsBackend::Native(port)),
            None => [HTTPS_PORT, ALT_HTTPS_PORT]
                .into_iter()
                .find(|port| native.contains(port))
                .map(HttpsBackend::Native)
                .or_else(|| http.map(HttpsBackend::Terminate)),
        };
        Self { http, https }
    }

    pub(super) fn terminated_port(self) -> Option<u16> {
        match self.https {
            Some(HttpsBackend::Terminate(port)) => Some(port),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_https_wins_and_does_not_require_http() {
        let native = BTreeSet::from([443, 8443]);
        assert_eq!(
            SelectedRoutes::choose(Some(80), None, &native).https,
            Some(HttpsBackend::Native(443))
        );
        assert_eq!(
            SelectedRoutes::choose(None, None, &BTreeSet::from([8443])).https,
            Some(HttpsBackend::Native(8443))
        );
        assert_eq!(
            SelectedRoutes::choose(Some(8080), None, &BTreeSet::new()).https,
            Some(HttpsBackend::Terminate(8080))
        );
        assert_eq!(
            SelectedRoutes::choose(None, None, &BTreeSet::new()).https,
            None
        );
    }

    #[test]
    fn https_override_controls_native_and_terminated_routes() {
        let native = BTreeSet::from([443]);
        assert_eq!(
            SelectedRoutes::choose(Some(80), Some(Pin::Off), &native).https,
            None
        );
        assert_eq!(
            SelectedRoutes::choose(Some(80), Some(Pin::Port(9443)), &native).https,
            Some(HttpsBackend::Native(9443))
        );
    }
}
