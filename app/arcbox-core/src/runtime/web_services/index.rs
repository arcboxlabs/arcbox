//! Publishes the built-in index only from a validated snapshot of the current physical VM.

use arcbox_dns::{NamePattern, claims::Claim};
use arcbox_engine::vm::VmIncarnation;

use super::*;

impl Runtime {
    /// Refreshes the index address from a dedicated guest directory request.
    /// The caller must cancel through the token and await completion.
    ///
    /// # Errors
    /// Returns an error for an unavailable VM, cancelled request, or invalid guest endpoint.
    pub async fn refresh_web_index(&self, cancelled: &CancellationToken) -> Result<()> {
        let expected = self.container_network_incarnation()?;
        let agent = self.connect_system_agent_expected(expected.clone()).await?;
        let response = agent
            .list_guest_web_services(
                ListGuestWebServicesRequest {
                    hostname: self.config.network.dns_domain.to_string(),
                    ..Default::default()
                },
                cancelled,
            )
            .await?;
        self.apply_web_index(&expected, response.services, cancelled)
            .await
    }

    async fn apply_web_index(
        &self,
        expected: &VmIncarnation,
        services: Vec<GuestWebService>,
        cancelled: &CancellationToken,
    ) -> Result<()> {
        let apex = &self.config.network.dns_domain;
        let gateway = self.config.container.cidr.docker_bridge_gateway();
        let address = match services.as_slice() {
            [] => None,
            [
                GuestWebService {
                    route: Some(Route::Index(route)),
                    ..
                },
            ] if route.hostname == apex.as_str() && route.ipv4_address == gateway.to_string() => {
                Some(gateway.into())
            }
            _ => {
                return Err(CoreError::Vm(
                    "guest index does not match the configured apex and gateway".into(),
                ));
            }
        };
        let mut state = self.host_network.commit.lock().await;
        if cancelled.is_cancelled() {
            return Err(CoreError::Vm("index publication cancelled".into()));
        }
        self.with_host_network_incarnation(self.default_machine_name(), expected, || ())?;
        self.prepare_container_incarnation(&mut state, expected)
            .await?;
        self.with_host_network_incarnation(self.default_machine_name(), expected, || {
            if cancelled.is_cancelled() {
                return Err(CoreError::Vm("index publication cancelled".into()));
            }
            self.replace_dns_registration(
                "system:index",
                vec![Claim::exclusive(
                    NamePattern::Exact(apex.clone()),
                    "system:index".into(),
                    "index".into(),
                )],
                address,
            );
            Ok(())
        })?
    }

    /// Withdraws the index address while preserving its permanent apex reservation.
    pub async fn withdraw_web_index(&self) {
        let _state = self.host_network.commit.lock().await;
        self.remove_dns_registration("system:index");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NetworkFixture;

    fn endpoint(runtime: &Runtime) -> Vec<GuestWebService> {
        vec![GuestWebService {
            route: IndexWebService {
                hostname: runtime.config.network.dns_domain.to_string(),
                ipv4_address: runtime
                    .config
                    .container
                    .cidr
                    .docker_bridge_gateway()
                    .to_string(),
                managed_https: true,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }]
    }

    #[tokio::test]
    async fn publication_requires_the_captured_vm_and_retires_with_that_vm() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = NetworkFixture::default();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap()
        .with_network_fixture(fixture.clone());
        let expected = runtime.container_network_incarnation().unwrap();
        let cancelled = CancellationToken::new();
        runtime
            .apply_web_index(&expected, endpoint(&runtime), &cancelled)
            .await
            .unwrap();
        let revision = runtime
            .dns_registry
            .read()
            .unwrap()
            .get("system:index")
            .unwrap()
            .revision;
        let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
        runtime
            .apply_web_index(&expected, endpoint(&runtime), &cancelled)
            .await
            .unwrap();
        assert_eq!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .get("system:index")
                .unwrap()
                .revision,
            revision
        );
        assert!(snapshot.commit(&[]).await.unwrap());
        fixture.restart();
        assert!(
            runtime
                .apply_web_index(&expected, endpoint(&runtime), &cancelled)
                .await
                .is_err()
        );
        runtime
            .retire_system_vm_container_networking()
            .await
            .unwrap();
        assert!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .get("system:index")
                .is_none()
        );
        assert!(matches!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .resolve(&runtime.config.network.dns_domain),
            Lookup::Reserved(_)
        ));
        let current = runtime.container_network_incarnation().unwrap();
        runtime
            .apply_web_index(&current, endpoint(&runtime), &cancelled)
            .await
            .unwrap();
        let snapshot = runtime.capture_container_snapshot().await.unwrap().unwrap();
        runtime.withdraw_web_index().await;
        assert!(snapshot.commit(&[]).await.unwrap());
        assert!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .get("system:index")
                .is_none()
        );
        runtime.close_host_network_admission();
        assert!(
            runtime
                .apply_web_index(&current, endpoint(&runtime), &cancelled)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_cancelled_and_missing_routes_never_publish_an_address() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(crate::Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap()
        .with_network_fixture(NetworkFixture::default());
        let expected = runtime.container_network_incarnation().unwrap();
        let cancelled = CancellationToken::new();
        let mut wrong_apex = endpoint(&runtime);
        let Some(Route::Index(route)) = &mut wrong_apex[0].route else {
            panic!("index fixture")
        };
        route.hostname = "other.local".into();
        let mut wrong_gateway = endpoint(&runtime);
        let Some(Route::Index(route)) = &mut wrong_gateway[0].route else {
            panic!("index fixture")
        };
        route.ipv4_address = "172.17.0.2".into();
        for routes in [
            wrong_apex,
            wrong_gateway,
            [endpoint(&runtime), endpoint(&runtime)].concat(),
            vec![GuestWebService::default()],
            vec![GuestWebService {
                route: arcbox_connect::v1::MachineWebService {
                    machine_name: "default".into(),
                    hostname: runtime.config.network.dns_domain.to_string(),
                    ipv4_address: runtime
                        .config
                        .container
                        .cidr
                        .docker_bridge_gateway()
                        .to_string(),
                    http_port: 80,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }],
        ] {
            assert!(
                runtime
                    .apply_web_index(&expected, routes, &cancelled)
                    .await
                    .is_err()
            );
            assert!(
                runtime
                    .dns_registry
                    .read()
                    .unwrap()
                    .get("system:index")
                    .is_none()
            );
        }
        runtime
            .apply_web_index(&expected, endpoint(&runtime), &cancelled)
            .await
            .unwrap();
        runtime
            .apply_web_index(&expected, Vec::new(), &cancelled)
            .await
            .unwrap();
        assert_eq!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .get("system:index")
                .unwrap()
                .address,
            None
        );
        cancelled.cancel();
        assert!(
            runtime
                .apply_web_index(&expected, endpoint(&runtime), &cancelled)
                .await
                .is_err()
        );
        assert_eq!(
            runtime
                .dns_registry
                .read()
                .unwrap()
                .get("system:index")
                .unwrap()
                .address,
            None
        );
    }
}
