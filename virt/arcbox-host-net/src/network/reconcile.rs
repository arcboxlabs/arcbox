//! [`HostNetwork`] as the port's [`NetworkReconcile`].

use arcbox_vm_driver::net::{NetworkLease, NetworkReconcile};
use arcbox_vm_driver::{Error, Result, VmId};
use async_trait::async_trait;

use super::HostNetwork;

#[async_trait]
impl NetworkReconcile for HostNetwork {
    async fn pending_cleanups(&self) -> Result<Vec<(VmId, String)>> {
        Ok(self
            .state()
            .quarantined
            .iter()
            .map(|(vm, lease)| (vm.clone(), lease.cleanup_token.clone()))
            .collect())
    }

    async fn validate_cleanup(&self, vm: &VmId, token: &str) -> Result<NetworkLease> {
        self.state().validate_cleanup(vm, token).cloned()
    }

    async fn finalize_cleanup(&self, vm: &VmId, token: &str) -> Result<()> {
        let pool = self.pool();
        let mut state = self.state();
        state.validate_cleanup(vm, token)?;
        if let Some(lease) = state.quarantined.remove(vm)
            && let Some(host) = pool.host(lease.ip)
        {
            state.used.remove(&host);
        }
        Ok(())
    }

    /// Always `None`: nothing this network installs outlives its process.
    async fn startup_cleanup_token(&self) -> Option<String> {
        None
    }

    async fn validate_startup_cleanup(&self, _token: &str) -> Result<()> {
        Err(Error::PreconditionFailed(
            "no startup cleanup is pending".into(),
        ))
    }

    async fn finalize_startup_cleanup(&self, _token: &str) -> Result<()> {
        Err(Error::PreconditionFailed(
            "no startup cleanup is pending".into(),
        ))
    }

    async fn wait_startup_cleanup_complete(&self) {}

    fn replay_complete(&self) {
        self.state().replayed = true;
    }
}
