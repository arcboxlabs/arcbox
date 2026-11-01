//! The VMM a computer runs on, and the staging area its files go into
//! whether or not the driver has one.
//!
//! Every flow — boot, restore, resume, the warm pool — is written against
//! the prepared shape: spawn the VMM, stage the files it boots from, then
//! boot or restore on it. That is Firecracker's shape, and the port lets a
//! driver lack both halves: a VMM that lives in the daemon (VZ) has nothing
//! to spawn ahead of a guest, and one that reads host paths (VZ, HV) has no
//! area to stage into.
//!
//! The port already says what a driver without `Staging` means: its VMs
//! read host paths, so its staging is the identity on what it reads
//! ([`HostPathStaging`]). The flows keep their one shape; what the driver
//! lacks is filled in here, once.

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;

use arcbox_vm_driver::{IsolationSpec, Prepare, PreparedVm, Staging, VmDriver, VmId, VmRecord};

use crate::error::Result;

mod host_staging;

pub use host_staging::HostPathStaging;

/// The VMM `id` will run on, spawned now through the driver's `Prepare`,
/// which [`ComputerManager::new`](crate::ComputerManager::new) requires.
pub async fn prepare_vm(
    driver: &Arc<dyn VmDriver>,
    id: &VmId,
    isolation: &IsolationSpec,
    runtime_dir: &Path,
) -> Result<Box<dyn PreparedVm>> {
    Ok(prepare_capability(driver.as_ref())
        .prepare(id, isolation, runtime_dir)
        .await?)
}

/// The driver's `Prepare`, which [`ComputerManager::new`](crate::ComputerManager::new)
/// requires.
pub fn prepare_capability(driver: &dyn VmDriver) -> &dyn Prepare {
    driver
        .prepare()
        .expect("ComputerManager::new requires the driver's Prepare capability")
}

/// The staging area of the VM `record` names: the driver's own when the
/// grip it came from offers one, otherwise the host-path staging rooted at
/// the VM's runtime directory.
///
/// Takes the accessor's answer rather than the grip: both grips on a VM —
/// the [`PreparedVm`] that spawned it and the
/// [`VmHandle`](arcbox_vm_driver::VmHandle) to the VM running on it — name
/// the same area, and so do the two host-path stagings built from their
/// records, since both are derived from the same runtime directory.
pub fn staging_of<'a>(staging: Option<&'a dyn Staging>, record: &VmRecord) -> StagingRef<'a> {
    staging.map_or_else(
        || StagingRef::HostPath(HostPathStaging::new(&record.runtime_dir)),
        StagingRef::Driver,
    )
}

/// A staging area, borrowed from the driver or built here.
pub enum StagingRef<'a> {
    /// The driver's own.
    Driver(&'a dyn Staging),
    /// The driver has none; its VMs read host paths.
    HostPath(HostPathStaging),
}

impl<'a> Deref for StagingRef<'a> {
    type Target = dyn Staging + 'a;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Driver(staging) => *staging,
            Self::HostPath(staging) => staging,
        }
    }
}
