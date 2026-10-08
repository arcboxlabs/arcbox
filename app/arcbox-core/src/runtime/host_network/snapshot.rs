//! Commits a complete guest observation only while its host generation remains current.

use super::*;
use std::collections::HashSet;

/// Validated inspect metadata and parsed host bindings from the same guest response.
#[derive(Clone)]
pub struct ContainerNetwork {
    /// Canonical container identity, names, and addresses.
    pub metadata: ContainerMetadata,
    /// Host address, host port, container port, and protocol.
    pub bindings: Vec<(String, u16, u16, String)>,
}

/// Pins a complete guest query to an idle host generation and physical VM.
pub struct HostNetworkSnapshot<'a> {
    runtime: &'a Runtime,
    vm: VmIncarnation,
    generation: u64,
}

impl Runtime {
    /// Captures a query boundary, or skips while a guest mutation remains in flight.
    ///
    /// # Errors
    /// Returns an error if host admission closed or the physical VM is unavailable.
    pub async fn capture_container_snapshot(&self) -> Result<Option<HostNetworkSnapshot<'_>>> {
        let _commit = self.host_network.commit.lock().await;
        let vm = self.container_network_incarnation()?;
        let activity = self
            .host_network
            .activity
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?;
        activity.check_open()?;
        Ok((activity.active == 0).then_some(HostNetworkSnapshot {
            runtime: self,
            vm,
            generation: activity.generation,
        }))
    }
}

impl HostNetworkSnapshot<'_> {
    /// Replaces Docker host state after a complete validated query.
    /// Returns false if a local mutation invalidated the query.
    ///
    /// # Errors
    /// Returns an error if admission closes, the physical VM changes, or a port operation fails.
    pub async fn commit(self, containers: &[ContainerNetwork]) -> Result<bool> {
        let runtime = self.runtime;
        let mut commit = runtime.host_network.commit.lock().await;
        {
            let mut activity = runtime
                .host_network
                .activity
                .lock()
                .map_err(|_| CoreError::LockPoisoned)?;
            activity.check_open()?;
            if activity.active != 0 || activity.generation != self.generation {
                return Ok(false);
            }
            activity.advance();
            activity.check_open()?;
        }
        self.with_live_vm(|| ())?;
        runtime
            .prepare_container_incarnation(&mut commit, &self.vm)
            .await?;
        let desired: HashSet<_> = containers.iter().map(|c| c.metadata.id.as_str()).collect();
        let registered = runtime.registered_container_ids().await;
        let result = async {
            for id in registered
                .iter()
                .filter(|id| !desired.contains(id.as_str()))
            {
                self.with_live_vm(|| ())?;
                runtime.clear_container_ports(&self.vm, id).await?;
                tracing::info!(
                    container_id = %id,
                    "reconciler tearing down host networking for a container no longer running"
                );
            }
            for container in containers {
                self.with_live_vm(|| ())?;
                if container.bindings.is_empty() {
                    runtime
                        .clear_container_ports(&self.vm, &container.metadata.id)
                        .await?;
                } else {
                    runtime
                        .update_container_ports(
                            &self.vm,
                            &container.metadata.id,
                            &container.bindings,
                        )
                        .await?;
                }
            }
            let mut aliases = runtime.container_aliases.write().await;
            self.with_live_vm(|| {
                let mut registry = runtime
                    .dns_registry
                    .write()
                    .expect("DNS registry lock poisoned");
                let obsolete: Vec<_> = registry
                    .owners()
                    .filter(|id| Runtime::is_container_dns_owner(id) && !desired.contains(id))
                    .map(str::to_owned)
                    .collect();
                for id in obsolete {
                    registry.remove(&id);
                }
                aliases.clear();
                for container in containers {
                    let container = &container.metadata;
                    registry
                        .replace(
                            container.id.clone(),
                            runtime.container_dns_claims(container),
                            container.primary_ipv4().map(std::net::IpAddr::V4),
                        )
                        .expect("runtime claims retain their registration owner");
                    aliases.insert(container.name.clone(), container.id.clone());
                }
                runtime.network_manager.replace_dns_projection(&registry);
            })
        }
        .await;
        if let Err(mut error) = result {
            if self.with_live_vm(|| ()).is_err() {
                for id in desired {
                    if let Err(cleanup) = runtime.clear_container_ports(&self.vm, id).await {
                        error = CoreError::invalid_state(format!(
                            "{error}; port cleanup failed for {id}: {cleanup}"
                        ));
                    }
                }
            }
            return Err(error);
        }
        Ok(true)
    }

    fn with_live_vm<T>(&self, publish: impl FnOnce() -> T) -> Result<T> {
        self.runtime.with_host_network_incarnation(
            self.runtime.default_machine_name(),
            &self.vm,
            publish,
        )
    }
}
