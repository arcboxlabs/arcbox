//! Serializes host publication while guest mutations retain synchronous activity leases.

use super::{CoreError, Result, Runtime};
use arcbox_dns::container::ContainerMetadata;
use arcbox_engine::vm::VmIncarnation;
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

mod retirement;
mod snapshot;
#[cfg(test)]
mod tests;
pub use snapshot::{ContainerNetwork, HostNetworkSnapshot};

#[derive(Default)]
pub(super) struct HostNetwork {
    pub(super) commit: AsyncMutex<CommitState>,
    activity: Mutex<Activity>,
    #[cfg(any(test, feature = "test-support"))]
    fixture: Option<NetworkFixture>,
}

#[derive(Default)]
pub(super) struct CommitState {
    incarnation: Option<VmIncarnation>,
}

#[derive(Default)]
struct Activity {
    generation: u64,
    active: usize,
    closed: bool,
}

impl Activity {
    fn check_open(&self) -> Result<()> {
        if self.closed {
            return Err(CoreError::invalid_state(
                "host networking admission is closed",
            ));
        }
        Ok(())
    }

    fn advance(&mut self) {
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.closed = true;
        }
    }
}

/// Retains mutation activity until host publication or cancellation completes.
pub struct HostMutationLease<'a> {
    runtime: &'a Runtime,
    vm: VmIncarnation,
}

/// Desired host state from one validated Docker mutation.
pub enum ContainerChange<'a> {
    /// Refreshes DNS, aliases, and the parsed host port bindings.
    Publish(&'a ContainerMetadata, &'a [(String, u16, u16, String)]),
    /// Withdraws every host registration for the canonical container ID.
    Remove(&'a str),
}

impl HostMutationLease<'_> {
    /// Publishes host state without holding the commit lock during guest requests.
    ///
    /// # Errors
    /// Returns an error if admission closes, the VM changes, or port binding fails.
    pub async fn commit(&self, change: ContainerChange<'_>) -> Result<()> {
        let runtime = self.runtime;
        let mut commit = runtime.host_network.commit.lock().await;
        self.with_live_vm(|| ())?;
        runtime
            .prepare_container_incarnation(&mut commit, &self.vm)
            .await?;
        let id = match change {
            ContainerChange::Publish(container, _) => container.id.as_str(),
            ContainerChange::Remove(id) => id,
        };
        let result = async {
            match change {
                ContainerChange::Publish(_, bindings) if !bindings.is_empty() => {
                    runtime
                        .update_container_ports(&self.vm, id, bindings)
                        .await?;
                }
                _ => runtime.clear_container_ports(&self.vm, id).await?,
            }
            let mut aliases = runtime.container_aliases.write().await;
            self.with_live_vm(|| {
                aliases.retain(|_, owner| owner != id);
                match change {
                    ContainerChange::Publish(container, _) => {
                        aliases.insert(container.name.clone(), container.id.clone());
                        runtime.replace_container_dns(container);
                    }
                    ContainerChange::Remove(_) => runtime.remove_dns_registration(id),
                }
            })
        }
        .await;
        if let Err(error) = result {
            runtime
                .clear_container_ports(&self.vm, id)
                .await
                .map_err(|cleanup| {
                    CoreError::invalid_state(format!("{error}; port cleanup failed: {cleanup}"))
                })?;
            return Err(error);
        }
        Ok(())
    }

    fn with_live_vm<T>(&self, publish: impl FnOnce() -> T) -> Result<T> {
        self.runtime.with_host_network_incarnation(
            self.runtime.default_machine_name(),
            &self.vm,
            publish,
        )
    }
}

impl Runtime {
    pub(super) fn begin_host_network_write(&self) -> Result<()> {
        let mut activity = self
            .host_network
            .activity
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?;
        activity.check_open()?;
        activity.advance();
        activity.check_open()
    }

    pub(super) fn invalidate_host_network_snapshot(&self) {
        self.host_network
            .activity
            .lock()
            .expect("host activity lock poisoned")
            .advance();
    }

    pub(super) fn ensure_host_network_open(&self) -> Result<()> {
        self.host_network
            .activity
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?
            .check_open()
    }

    pub(super) async fn update_container_ports(
        &self,
        vm: &VmIncarnation,
        id: &str,
        bindings: &[(String, u16, u16, String)],
    ) -> Result<()> {
        let machine = self.default_machine_name();
        #[cfg(target_os = "macos")]
        return self
            .start_port_forwarding_with_incarnation(machine, id, bindings, vm)
            .await;
        #[cfg(not(target_os = "macos"))]
        self.start_port_forwarding_fallback(machine, id, bindings, vm)
            .await
    }

    pub(super) async fn clear_container_ports(&self, vm: &VmIncarnation, id: &str) -> Result<()> {
        #[cfg(target_os = "macos")]
        return self
            .stop_inbound_rules(id, Some((self.default_machine_name(), vm)))
            .await;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = vm;
            self.stop_port_forwarding_raw(id).await;
            Ok(())
        }
    }
    pub(super) fn with_host_network_incarnation<T>(
        &self,
        machine: &str,
        expected: &VmIncarnation,
        publish: impl FnOnce() -> T,
    ) -> Result<T> {
        self.ensure_host_network_open()?;
        #[cfg(any(test, feature = "test-support"))]
        if let Some(fixture) = &self.host_network.fixture {
            let current = fixture.0.read().map_err(|_| CoreError::LockPoisoned)?;
            if current.as_ref() != Some(expected) {
                return Err(CoreError::invalid_state("fixture VM incarnation changed"));
            }
            return Ok(publish());
        }
        let state = self.vm_lifecycle.subscribe_state();
        let state = state.borrow();
        if machine == self.default_machine_name() && !state.is_ready() {
            return Err(CoreError::invalid_state("System VM is not ready"));
        }
        Ok(self
            .machine_manager
            .with_network_incarnation(machine, expected, |_| publish())?)
    }

    #[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
    pub(super) fn has_network_fixture(&self) -> bool {
        self.host_network.fixture.is_some()
    }

    #[cfg(all(target_os = "macos", test))]
    pub(super) fn restart_network_fixture(&self) -> VmIncarnation {
        self.host_network
            .fixture
            .as_ref()
            .expect("explicit network fixture")
            .restart();
        self.container_network_incarnation().unwrap()
    }
}

impl Drop for HostMutationLease<'_> {
    fn drop(&mut self) {
        let mut activity = self
            .runtime
            .host_network
            .activity
            .lock()
            .expect("host activity lock poisoned");
        activity.active = activity
            .active
            .checked_sub(1)
            .expect("a lease owns one active mutation");
        activity.advance();
    }
}

impl Runtime {
    /// Captures the ready System VM's physical identity before a guest query.
    ///
    /// # Errors
    /// Returns an error if readiness or physical VM liveness is unavailable.
    pub fn container_network_incarnation(&self) -> Result<VmIncarnation> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(fixture) = &self.host_network.fixture {
            return fixture
                .0
                .read()
                .map_err(|_| CoreError::LockPoisoned)?
                .clone()
                .ok_or_else(|| CoreError::invalid_state("fixture VM is stopped"));
        }
        let state = self.vm_lifecycle.subscribe_state();
        let state = state.borrow();
        if !state.is_ready() {
            return Err(CoreError::invalid_state("System VM is not ready"));
        }
        Ok(self
            .machine_manager
            .network_incarnation(self.default_machine_name())?)
    }

    /// Admits a guest mutation after acquiring its canonical container lock.
    ///
    /// # Errors
    /// Returns an error if admission closed or the captured VM changed.
    pub async fn begin_container_mutation(
        &self,
        vm: VmIncarnation,
    ) -> Result<HostMutationLease<'_>> {
        let _commit = self.host_network.commit.lock().await;
        if self.container_network_incarnation()? != vm {
            return Err(CoreError::invalid_state("System VM incarnation changed"));
        }
        let mut activity = self
            .host_network
            .activity
            .lock()
            .map_err(|_| CoreError::LockPoisoned)?;
        activity.check_open()?;
        let active = activity.active.checked_add(1).ok_or_else(|| {
            activity.closed = true;
            CoreError::invalid_state("host mutation count exhausted")
        })?;
        activity.advance();
        activity.check_open()?;
        activity.active = active;
        Ok(HostMutationLease { runtime: self, vm })
    }
}

/// Explicit ready VM identity source for offline cross-crate tests.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
pub struct NetworkFixture(std::sync::Arc<std::sync::RwLock<Option<VmIncarnation>>>);

#[cfg(any(test, feature = "test-support"))]
impl Default for NetworkFixture {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(Some(
            VmIncarnation::for_test(),
        ))))
    }
}

#[cfg(any(test, feature = "test-support"))]
impl NetworkFixture {
    /// Replaces the current physical identity without starting a VM.
    pub fn restart(&self) {
        *self.0.write().expect("fixture lock poisoned") = Some(VmIncarnation::for_test());
    }

    /// Rejects capture and publication until the fixture restarts.
    pub fn stop(&self) {
        *self.0.write().expect("fixture lock poisoned") = None;
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Runtime {
    /// Installs an explicit identity fixture; ordinary runtimes still require a live VM.
    #[must_use]
    pub fn with_network_fixture(mut self, fixture: NetworkFixture) -> Self {
        self.host_network.fixture = Some(fixture);
        self
    }
}
