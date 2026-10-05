//! Excludes System VM mutations while its paired disks are preserved or checked.

use std::path::PathBuf;
use std::sync::{Arc, RwLockReadGuard};

use super::{DEFAULT_MACHINE_NAME, MachineManager};
use crate::error::{EngineError, Result};

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
}

fn is_system_machine(name: &str) -> bool {
    matches!(name, DEFAULT_MACHINE_NAME | "rosetta")
}
