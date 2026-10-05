//! Excludes System VM mutations while its paired disks are preserved or checked.

use std::path::PathBuf;
use std::sync::{Arc, RwLockReadGuard};

use super::{DEFAULT_MACHINE_NAME, MachineManager};
use crate::error::{EngineError, Result};

const STORAGE_AGENT_CAPABILITY: &[u8] = b"arcbox-storage-recovery-v1";

/// Reserves the System VM until the recovery owner releases the reservation.
#[derive(Clone)]
pub struct StorageMaintenance(Arc<MaintenanceOwner>);

struct MaintenanceOwner(Arc<MachineManager>);

impl Drop for MaintenanceOwner {
    fn drop(&mut self) {
        *self
            .0
            .storage_maintenance
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }
}

impl StorageMaintenance {
    pub(crate) fn belongs_to(&self, manager: &Arc<MachineManager>) -> bool {
        Arc::ptr_eq(&self.0.0, manager)
    }

    pub(crate) fn start(&self, name: &str) -> Result<()> {
        self.0.0.start_reserved_process(name)?;
        Ok(())
    }
}

impl MachineManager {
    /// Returns the durable boot hold retained after failed or interrupted recovery.
    #[must_use]
    pub fn storage_hold_path(&self) -> PathBuf {
        self.data_dir.join("storage-recovery/hold")
    }

    /// Reports whether System VM storage is reserved or has a durable boot hold.
    pub fn storage_is_held(&self) -> Result<bool> {
        let reserved = self
            .storage_maintenance
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;
        Ok(*reserved || self.storage_hold_path().try_exists()?)
    }

    /// Rejects lifecycle admission while the System VM is reserved for recovery.
    pub fn ensure_storage_available(&self, name: &str) -> Result<()> {
        drop(self.storage_permit(name)?);
        Ok(())
    }

    /// Reserves storage after any synchronous machine mutation has finished.
    /// The owner must stop the VM before accessing either disk.
    pub fn reserve_storage(self: &Arc<Self>) -> Result<StorageMaintenance> {
        let mut reserved = self
            .storage_maintenance
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        if *reserved {
            return Err(EngineError::invalid_state(
                "storage recovery is already running",
            ));
        }
        *reserved = true;
        Ok(StorageMaintenance(Arc::new(MaintenanceOwner(Arc::clone(
            self,
        )))))
    }

    pub(super) fn storage_permit(&self, name: &str) -> Result<RwLockReadGuard<'_, bool>> {
        let reserved = self
            .storage_maintenance
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;
        if is_system_machine(name) && (*reserved || self.storage_hold_path().try_exists()?) {
            return Err(EngineError::invalid_state(
                "System VM storage is held for recovery; use abctl disk recover",
            ));
        }
        Ok(reserved)
    }

    pub(super) fn verify_storage_pair(&self, name: &str) -> Result<()> {
        if !is_system_machine(name) {
            return Ok(());
        }
        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get(name)
            .ok_or_else(|| EngineError::not_found(name.to_owned()))?;
        let [_, data, metadata, ..] = machine.block_devices.as_slice() else {
            return Err(EngineError::invalid_state(
                "System VM requires its paired data and metadata images",
            ));
        };
        arcbox_storage::verify_pair(data.path.as_ref(), metadata.path.as_ref())
            .map_err(|error| EngineError::Machine(error.to_string()))?;
        // Older agents mount storage before Ping. Check the same binary that
        // the rootfs executes through its /arcbox VirtioFS share before boot.
        let agent_path = self.data_dir.join("bin/arcbox-agent");
        let agent = std::fs::read(&agent_path).map_err(|error| {
            EngineError::config(format!(
                "cannot inspect staged agent at {}: {error}; stage the agent from the same ArcBox build before starting the System VM",
                agent_path.display()
            ))
        })?;
        if !agent
            .windows(STORAGE_AGENT_CAPABILITY.len())
            .any(|bytes| bytes == STORAGE_AGENT_CAPABILITY)
        {
            return Err(EngineError::config(format!(
                "staged agent at {} lacks storage-protection support; update the staged agent from the same ArcBox build before starting the System VM",
                agent_path.display()
            )));
        }
        Ok(())
    }
}

fn is_system_machine(name: &str) -> bool {
    matches!(name, DEFAULT_MACHINE_NAME | "rosetta")
}
