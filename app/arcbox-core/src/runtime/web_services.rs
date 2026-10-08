//! Host-owned signing names and public URLs derived from installed guest web routes.

use arcbox_connect::v1::{
    ContainerWebService, ReconcileHttpsAccessRequest, ReconcileHttpsAccessResponse, WebService,
    container_web_service::HttpsBackend, web_service::HttpsMode,
};
use tokio_util::sync::CancellationToken;

use super::Runtime;
use crate::error::{CoreError, Result};

impl Runtime {
    /// Authorizes a local FQDN for signing and returns its container DNS registration revision.
    /// A new registration changes the revision even when the owner and address are unchanged.
    pub async fn https_signing_revision(&self, fqdn: &str) -> Option<u64> {
        let domain = self.network_manager.dns_domain()?;
        let hostname = fqdn.strip_suffix(&format!(".{domain}"))?;
        let entries = self.dns_entries.read().await;
        let (owner, entry) = entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .hostnames
                    .iter()
                    .any(|name| name == hostname || name == fqdn)
            })
            .max_by_key(|(_, entry)| entry.revision)?;
        if !Self::is_container_dns_owner(owner) {
            return None;
        }
        (self
            .network_manager
            .local_hosts_table()
            .read()
            .expect("local DNS hosts lock poisoned")
            .get(fqdn)
            == Some(&entry.ip))
        .then_some(entry.revision)
    }

    /// Renews the guest's leased observation of managed HTTPS access from the Mac.
    /// The caller must cancel through the token and await completion, including connection setup.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest cannot be reached or rejects the observation.
    pub async fn reconcile_https_access(
        &self,
        request: ReconcileHttpsAccessRequest,
        cancelled: &CancellationToken,
    ) -> Result<ReconcileHttpsAccessResponse> {
        self.connect_system_agent()
            .await?
            .reconcile_https_access(request, cancelled)
            .await
            .map_err(Into::into)
    }

    /// Lists container web services with their current DNS names and installed routes.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest cannot be reached or reports an invalid route.
    pub async fn list_web_services(&self) -> Result<Vec<WebService>> {
        let mut agent = self.connect_system_agent().await?;
        let response = if agent.is_blocking() {
            tokio::task::spawn_blocking(move || agent.list_container_web_services_blocking())
                .await
                .map_err(|error| CoreError::Vm(format!("web services task panicked: {error}")))??
        } else {
            agent.list_container_web_services().await?
        };
        response
            .services
            .into_iter()
            .map(container_service)
            .collect()
    }
}

fn container_service(route: ContainerWebService) -> Result<WebService> {
    let hostname = route
        .hostnames
        .first()
        .ok_or_else(|| CoreError::Vm("guest web route has no DNS names".into()))?;
    for domain in &route.hostnames {
        if domain
            .strip_suffix(arcbox_constants::dns::LOCAL_DOMAIN)
            .and_then(|prefix| prefix.strip_suffix('.'))
            .is_none_or(str::is_empty)
            || !domain
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            return Err(CoreError::Vm(format!(
                "guest web route has invalid domain {domain:?}"
            )));
        }
    }
    let (https_mode, https_port) = match route.https_backend {
        Some(HttpsBackend::NativeHttpsPort(port)) => (HttpsMode::Passthrough, port),
        Some(HttpsBackend::TerminatedHttpPort(port)) => (HttpsMode::Managed, port),
        None => (HttpsMode::None, 0),
    };
    if route.http_port > u32::from(u16::MAX)
        || https_port > u32::from(u16::MAX)
        || (https_mode != HttpsMode::None && https_port == 0)
        || (route.http_port == 0 && https_port == 0)
    {
        return Err(CoreError::Vm(
            "guest web route has invalid upstream ports".into(),
        ));
    }
    Ok(WebService {
        owner: format!("container:{}", route.container_id),
        hostname: hostname.clone(),
        http_url: if route.http_port == 0 {
            String::new()
        } else {
            format!("http://{hostname}")
        },
        https_url: if https_port == 0 {
            String::new()
        } else {
            format!("https://{hostname}")
        },
        domains: route.hostnames,
        https_mode: https_mode.into(),
        http_port: route.http_port,
        https_port,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signing_authorization_follows_current_container_dns_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        runtime.network_manager().set_dns_domain("arcbox.local");
        let name = "web.arcbox.local";
        assert_eq!(runtime.https_signing_revision(name).await, None);
        let ip = "10.0.2.3".parse().unwrap();
        let aliases = [
            "web".into(),
            "web.demo".into(),
            "absolute.arcbox.local".into(),
        ];
        runtime.register_dns("first", &aliases, ip).await;
        let first = runtime.https_signing_revision(name).await.unwrap();
        for alias in [name, "web.demo.arcbox.local", "absolute.arcbox.local"] {
            assert_eq!(runtime.https_signing_revision(alias).await, Some(first));
        }
        assert_eq!(
            runtime.https_signing_revision("web.example.com").await,
            None
        );

        runtime.register_dns("first", &aliases, ip).await;
        let renewed = runtime.https_signing_revision(name).await.unwrap();
        assert_ne!(renewed, first);
        runtime.register_dns("second", &["web".into()], ip).await;
        let second = runtime.https_signing_revision(name).await.unwrap();
        assert_ne!(second, renewed);
        for owner in ["sandbox:other", "machine:other", "system:host"] {
            runtime.register_dns(owner, &["web".into()], ip).await;
            assert_eq!(runtime.https_signing_revision(name).await, None, "{owner}");
            runtime.deregister_dns_by_id(owner).await;
            assert_eq!(runtime.https_signing_revision(name).await, Some(second));
        }
        runtime.deregister_dns_by_id("second").await;
        assert_eq!(runtime.https_signing_revision(name).await, Some(renewed));
        runtime
            .network_manager()
            .register_dns("web", "10.0.2.4".parse().unwrap());
        assert_eq!(runtime.https_signing_revision(name).await, None);
        runtime.deregister_dns_by_id("first").await;
        assert_eq!(runtime.https_signing_revision(name).await, None);
    }

    fn route() -> ContainerWebService {
        ContainerWebService {
            container_id: "abc123".into(),
            hostnames: vec![
                "web.demo.arcbox.local".into(),
                "demo-web-1.arcbox.local".into(),
            ],
            http_port: 3000,
            https_backend: Some(HttpsBackend::TerminatedHttpPort(3000)),
            ..Default::default()
        }
    }

    #[test]
    fn urls_use_the_owned_compose_alias_and_default_ingress_ports() {
        let service = container_service(route()).unwrap();
        assert_eq!(service.owner, "container:abc123");
        assert_eq!(service.hostname, "web.demo.arcbox.local");
        assert_eq!(service.domains.len(), 2);
        assert_eq!(service.http_url, "http://web.demo.arcbox.local");
        assert_eq!(service.https_url, "https://web.demo.arcbox.local");
        assert_eq!(service.https_mode, HttpsMode::Managed);
        assert_eq!(service.http_port, 3000);
        assert_eq!(service.https_port, 3000);
    }

    #[test]
    fn native_tls_without_http_preserves_the_backend_mode() {
        let mut route = route();
        route.http_port = 0;
        route.https_backend = Some(HttpsBackend::NativeHttpsPort(8443));
        let service = container_service(route).unwrap();
        assert_eq!(service.http_url, "");
        assert_eq!(service.https_url, "https://web.demo.arcbox.local");
        assert_eq!(service.https_mode, HttpsMode::Passthrough);
        assert_eq!(service.https_port, 8443);
    }

    #[test]
    fn absent_https_does_not_produce_an_https_url() {
        let mut route = route();
        route.https_backend = None;
        let service = container_service(route).unwrap();
        assert_eq!(service.https_url, "");
        assert_eq!(service.https_mode, HttpsMode::None);
    }

    #[test]
    fn invalid_guest_routes_fail_before_becoming_browser_urls() {
        for hostname in ["", "example.com", "evil.com/#.arcbox.local", "arcbox.local"] {
            let mut route = route();
            route.hostnames = vec![hostname.into()];
            assert!(container_service(route).is_err());
        }
        let mut overflow = route();
        overflow.http_port = u32::MAX;
        assert!(container_service(overflow).is_err());
        let mut zero_tls = route();
        zero_tls.https_backend = Some(HttpsBackend::NativeHttpsPort(0));
        assert!(container_service(zero_tls).is_err());
    }
}
