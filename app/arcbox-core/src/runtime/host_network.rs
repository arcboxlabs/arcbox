//! Serializes host publication while guest mutations retain synchronous activity leases.

use super::{CoreError, Result, Runtime};
use arcbox_engine::vm::VmIncarnation;
use std::sync::Mutex;
use tokio::sync::Mutex as AsyncMutex;

#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct HostNetwork {
    commit: AsyncMutex<()>,
    activity: Mutex<Activity>,
    #[cfg(any(test, feature = "test-support"))]
    fixture: Option<NetworkFixture>,
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
        Ok(HostMutationLease { runtime: self })
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
