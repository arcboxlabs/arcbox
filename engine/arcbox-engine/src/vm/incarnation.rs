//! Identifies the physical datapath under the VM registry lock.

use super::*;

/// Identity of one start or reboot of a VM entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmIncarnation {
    id: VmId,
    run_sequence: u64,
}

#[cfg(feature = "test-support")]
impl VmIncarnation {
    /// Creates an identity for a listener fixture; this does not create a live VM.
    #[must_use]
    pub fn for_test() -> Self {
        Self {
            id: VmId::new(),
            run_sequence: 1,
        }
    }
}

impl VmEntry {
    pub(super) fn begin_run(&mut self) -> Result<()> {
        self.info.state = MachineState::Starting;
        self.run_sequence = self
            .run_sequence
            .checked_add(1)
            .ok_or_else(|| EngineError::invalid_state("VM run sequence exhausted"))?;
        Ok(())
    }

    pub(super) fn reboot_with(
        &mut self,
        reboot: impl FnOnce(&mut Vmm) -> Result<()>,
    ) -> Result<()> {
        if self.vmm.is_none() {
            return Err(EngineError::invalid_state("VMM not initialized"));
        }
        self.begin_run()?;
        let result = reboot(self.vmm.as_mut().expect("VMM checked before begin_run"));
        self.info.state = if result.is_ok() {
            MachineState::Running
        } else {
            MachineState::Stopping
        };
        result
    }

    fn network_incarnation(&self) -> Result<VmIncarnation> {
        if self.info.state != MachineState::Running
            || !self.vmm.as_ref().is_some_and(Vmm::is_running)
        {
            return Err(EngineError::invalid_state("VM network is not running"));
        }
        Ok(VmIncarnation {
            id: self.info.id.clone(),
            run_sequence: self.run_sequence,
        })
    }
}

impl VmManager {
    /// Captures the current live datapath identity.
    ///
    /// # Errors
    /// Returns an error if the VM is absent, stopped, or its lock is poisoned.
    pub fn network_incarnation(&self, id: &VmId) -> Result<VmIncarnation> {
        let vms = self.vms.read().map_err(|_| EngineError::LockPoisoned)?;
        vms.get(id)
            .ok_or_else(|| EngineError::not_found(id.to_string()))?
            .network_incarnation()
    }

    #[cfg(target_os = "macos")]
    pub(super) fn with_expected_vmm<T>(
        &self,
        id: &VmId,
        expected: &VmIncarnation,
        take: impl FnOnce(&mut Vmm) -> T,
    ) -> Result<T> {
        let mut vms = self.vms.write().map_err(|_| EngineError::LockPoisoned)?;
        let entry = vms
            .get_mut(id)
            .ok_or_else(|| EngineError::not_found(id.to_string()))?;
        if entry.network_incarnation()? != *expected {
            return Err(EngineError::invalid_state("VM network incarnation changed"));
        }
        Ok(take(
            entry.vmm.as_mut().expect("live incarnation has a VMM"),
        ))
    }

    /// Transfers only the expected live VM's manager. The caller owns shutdown.
    ///
    /// # Errors
    /// Returns an error for a missing, stopped, or replaced VM, or a poisoned lock.
    #[cfg(target_os = "macos")]
    pub fn take_inbound_listener_manager(
        &self,
        id: &VmId,
        expected: &VmIncarnation,
    ) -> Result<Option<arcbox_net::darwin::inbound_relay::InboundListenerManager>> {
        self.with_expected_vmm(id, expected, Vmm::take_inbound_listener_manager)
    }
}
