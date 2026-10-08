//! Public URLs derived from the guest's installed web routes.

use arcbox_connect::v1::{
    ContainerWebService, WebService, container_web_service::HttpsBackend, web_service::HttpsMode,
};

use super::Runtime;
use crate::error::{CoreError, Result};

impl Runtime {
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
