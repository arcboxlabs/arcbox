//! Owned web queries retain captured VMs until native and RPC cancellation completes.

use std::{future::Future, sync::Arc, time::Duration};

use arcbox_connect::v1::{ListGuestWebServicesRequest, ListGuestWebServicesResponse};
use arcbox_engine::machine::{MachineInfo, MachineState};
use tokio_util::sync::CancellationToken;

use super::{
    CapturedMachineWeb, CapturedWebGuest, CoreError, Domain, IndexEndpoint, Result, Runtime,
    WebService, selected_services,
};

#[cfg(test)]
mod ownership_tests;

#[cfg(test)]
mod tests;

enum WebGuest {
    System(CapturedWebGuest),
    Machine(Box<CapturedMachineWeb>),
}

impl WebGuest {
    fn guest(&self) -> &CapturedWebGuest {
        match self {
            Self::System(guest) => guest,
            Self::Machine(machine) => &machine.guest,
        }
    }

    fn project(
        self,
        runtime: &Runtime,
        response: ListGuestWebServicesResponse,
        hostname: Option<&Domain>,
    ) -> Result<Vec<WebService>> {
        match self {
            Self::System(guest) => {
                runtime.with_host_network_incarnation(&guest.machine, &guest.incarnation, || {
                    let registry = runtime
                        .dns_registry
                        .read()
                        .expect("DNS registry lock poisoned");
                    selected_services(
                        response.services,
                        hostname,
                        &registry,
                        &IndexEndpoint {
                            apex: &runtime.config.network.dns_domain,
                            gateway: runtime.config.container.cidr.docker_bridge_gateway(),
                        },
                    )
                })?
            }
            Self::Machine(machine) => {
                let services = machine_services(response.services, &machine)?;
                runtime
                    .with_web_guest_claim(
                        &machine.guest,
                        &machine.open.hostname,
                        |address, revision| {
                            if address.to_string() != machine.open.ipv4_address
                                || revision != machine.claim_revision
                            {
                                return Err(CoreError::invalid_state("machine web claim changed"));
                            }
                            Ok(services)
                        },
                    )?
                    .ok_or_else(|| CoreError::invalid_state("machine web claim disappeared"))?
            }
        }
    }
}

impl Runtime {
    /// Lists guest web services with their current DNS names and installed routes.
    ///
    /// # Errors
    /// Returns an error if a guest query fails or the captured identity changes.
    pub async fn list_web_services(
        self: &Arc<Self>,
        hostname: Option<Domain>,
    ) -> Result<Vec<WebService>> {
        let runtime = Arc::clone(self);
        owned_query(move |cancelled| async move {
            runtime.ensure_host_network_open()?;
            let targets = runtime
                .web_guest_names(runtime.machine_manager.list(), hostname.as_ref())
                .into_iter()
                .map(|name| {
                    if name == runtime.default_machine_name() {
                        Ok(WebGuest::System(runtime.capture_web_guest(&name)?))
                    } else {
                        Ok(WebGuest::Machine(Box::new(
                            runtime.capture_machine_web(&name)?,
                        )))
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            let mut queries = tokio::task::JoinSet::new();
            for target in targets {
                let runtime = Arc::clone(&runtime);
                let hostname = hostname.clone();
                let cancelled = cancelled.clone();
                queries.spawn(async move {
                    let response = runtime
                        .query_web_services(target.guest(), hostname.as_ref(), &cancelled)
                        .await?;
                    Ok((target, response))
                });
            }
            let responses = drain_queries(queries, &cancelled).await?;
            let mut services = Vec::new();
            for (guest, response) in responses {
                services.extend(guest.project(&runtime, response, hostname.as_ref())?);
            }
            services.sort_by(|left, right| {
                (&left.owner, &left.hostname).cmp(&(&right.owner, &right.hostname))
            });
            Ok(services)
        })
        .await
    }

    fn web_guest_names(
        &self,
        mut machines: Vec<MachineInfo>,
        hostname: Option<&Domain>,
    ) -> Vec<String> {
        machines.retain(|machine| {
            machine.name != self.default_machine_name() && machine.distro.is_some()
        });
        let exact_machine = hostname.is_some_and(|hostname| {
            machines.iter().any(|machine| {
                self.machine_dns_name(&machine.name).as_deref() == Some(hostname.as_str())
            })
        });
        machines.retain(|machine| {
            machine.state == MachineState::Running
                && hostname.is_none_or(|hostname| {
                    self.machine_dns_name(&machine.name).as_deref() == Some(hostname.as_str())
                })
        });
        let mut names: Vec<_> = machines.into_iter().map(|machine| machine.name).collect();
        if !exact_machine && self.subscribe_system_vm_state().borrow().is_ready() {
            names.push(self.default_machine_name().into());
        }
        names.sort();
        names
    }

    async fn query_web_services(
        self: &Arc<Self>,
        guest: &CapturedWebGuest,
        hostname: Option<&Domain>,
        cancelled: &CancellationToken,
    ) -> Result<ListGuestWebServicesResponse> {
        let query_cancelled = cancelled.child_token();
        let query = async {
            let (agent, _hold) = self
                .connect_web_guest_query(guest, &query_cancelled)
                .await?;
            let request = ListGuestWebServicesRequest {
                hostname: hostname.map_or_else(String::new, ToString::to_string),
                ..Default::default()
            };
            Ok(agent
                .list_guest_web_services(request, &query_cancelled)
                .await?)
        };
        query_deadline(query, &query_cancelled).await
    }
}

async fn owned_query<T: Send + 'static, F: Future<Output = Result<T>> + Send + 'static>(
    operation: impl FnOnce(CancellationToken) -> F,
) -> Result<T> {
    let cancelled = CancellationToken::new();
    let _cancel_on_drop = cancelled.clone().drop_guard();
    tokio::spawn(operation(cancelled))
        .await
        .map_err(|error| CoreError::Vm(format!("web query owner: {error}")))?
}

async fn query_deadline<T>(
    query: impl Future<Output = Result<T>>,
    cancelled: &CancellationToken,
) -> Result<T> {
    tokio::pin!(query);
    tokio::select! {
        result = &mut query => result,
        () = tokio::time::sleep(Duration::from_secs(5)) => {
            cancelled.cancel();
            let _ = query.await;
            Err(CoreError::Vm("web service query timed out".into()))
        }
    }
}

async fn drain_queries<T: Send + 'static>(
    mut queries: tokio::task::JoinSet<Result<T>>,
    cancelled: &CancellationToken,
) -> Result<Vec<T>> {
    let mut responses = Vec::new();
    let mut failure = None;
    while let Some(result) = queries.join_next().await {
        match result
            .map_err(|error| CoreError::Vm(format!("web query worker: {error}")))
            .and_then(std::convert::identity)
        {
            Ok(response) => responses.push(response),
            Err(error) => {
                failure.get_or_insert(error);
                cancelled.cancel();
            }
        }
    }
    failure.map_or(Ok(responses), Err)
}

fn machine_services(
    routes: Vec<arcbox_connect::v1::GuestWebService>,
    machine: &CapturedMachineWeb,
) -> Result<Vec<WebService>> {
    use arcbox_connect::v1::{guest_web_service::Route, machine_web_service::HttpsBackend};
    if routes.len() > 1 {
        return Err(CoreError::Vm("machine returned multiple web routes".into()));
    }
    let Some(route) = routes.into_iter().next() else {
        return Ok(Vec::new());
    };
    let Some(Route::Machine(route)) = route.route else {
        return Err(CoreError::Vm(
            "machine returned a non-machine web route".into(),
        ));
    };
    if route.machine_name != machine.open.machine_name
        || route.hostname != machine.open.hostname
        || route.ipv4_address != machine.open.ipv4_address
    {
        return Err(CoreError::Vm(
            "machine web route does not match captured identity".into(),
        ));
    }
    let (mode, port) = match route.https_backend {
        Some(HttpsBackend::NativeHttpsPort(port)) => (super::UrlHttpsMode::Passthrough, port),
        Some(HttpsBackend::TerminatedHttpPort(port)) => (super::UrlHttpsMode::Managed, port),
        None => (super::UrlHttpsMode::None, 0),
    };
    super::project_service(
        format!("machine:{}", machine.open.machine_name),
        vec![machine.open.hostname.parse().map_err(CoreError::config)?],
        route.http_port,
        mode,
        port,
    )
}
