//! Retires obsolete physical datapaths and closes publication before shutdown.

use super::*;
use arcbox_error::CommonError;

impl Runtime {
    pub(in crate::runtime) async fn prepare_container_incarnation(
        &self,
        state: &mut CommitState,
        expected: &VmIncarnation,
    ) -> Result<()> {
        if state.incarnation.as_ref() == Some(expected) {
            return Ok(());
        }
        self.clear_container_incarnation(state, Some(expected))
            .await;
        self.with_host_network_incarnation(self.default_machine_name(), expected, || ())?;
        // Retain ownership before the first port await, including a canceled first publication.
        state.incarnation = Some(expected.clone());
        Ok(())
    }

    /// Retires Docker and index state whose physical VM is no longer current and ready.
    ///
    /// # Errors
    /// Returns an error if the physical VM registry cannot be read.
    pub async fn retire_system_vm_container_networking(&self) -> Result<bool> {
        let mut state = self.host_network.commit.lock().await;
        let current = match self.container_network_incarnation() {
            Ok(current) => Some(current),
            Err(CoreError::Common(CommonError::NotFound(_) | CommonError::InvalidState(_))) => None,
            Err(error) => return Err(error),
        };
        if current.is_some() && state.incarnation == current {
            return Ok(false);
        }
        self.clear_container_incarnation(&mut state, current.as_ref())
            .await;
        Ok(true)
    }

    async fn clear_container_incarnation(
        &self,
        state: &mut CommitState,
        keep: Option<&VmIncarnation>,
    ) {
        self.invalidate_host_network_snapshot();
        #[cfg(target_os = "macos")]
        {
            let machine = self.default_machine_name();
            let mut rules = self.inbound_rules.write().await;
            let mut listeners = self.inbound_listeners.write().await;
            if let Some(slot) = listeners.get_mut(machine) {
                slot.finish_retiring().await;
                if keep != Some(&slot.incarnation) {
                    slot.stop_all().await;
                    listeners.remove(machine);
                    rules.retain(|_, (owner, _)| owner != machine);
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = keep;
            for id in self.registered_container_ids().await {
                self.stop_port_forwarding_raw(&id).await;
            }
        }
        let live = self.all_sandbox_authority_keys().await;
        self.sandbox_port_keys.write().await.retain(|_, keys| {
            keys.retain(|key| live.contains(key));
            !keys.is_empty()
        });
        let mut aliases = self.container_aliases.write().await;
        aliases.clear();
        let mut registry = self
            .dns_registry
            .write()
            .expect("DNS registry lock poisoned");
        let owners: Vec<_> = registry
            .owners()
            .filter(|owner| Self::is_container_dns_owner(owner) || *owner == "system:index")
            .map(str::to_owned)
            .collect();
        for owner in owners {
            registry.remove(&owner);
        }
        self.network_manager.replace_dns_projection(&registry);
        state.incarnation = None;
    }

    pub(in crate::runtime) fn close_host_network_admission(&self) {
        let mut activity = self
            .host_network
            .activity
            .lock()
            .expect("host activity lock poisoned");
        activity.closed = true;
        activity.advance();
    }

    pub(in crate::runtime) async fn clear_host_network_for_shutdown(&self) {
        let mut sandbox = self.sandbox_host_state.lock().await;
        let mut kubernetes = self.kubernetes_lb_ports.lock().await;
        let mut state = self.host_network.commit.lock().await;
        self.invalidate_host_network_snapshot();
        *sandbox = sandbox.wrapping_add(1);
        self.stop_port_forwarding_all_raw().await;
        self.sandbox_port_keys.write().await.clear();
        self.sandbox_dns_ids.write().await.clear();
        *kubernetes = Default::default();
        self.container_aliases.write().await.clear();
        let mut registry = self
            .dns_registry
            .write()
            .expect("DNS registry lock poisoned");
        let owners: Vec<_> = registry.owners().map(str::to_owned).collect();
        for owner in owners {
            registry.remove(&owner);
        }
        self.network_manager.replace_dns_projection(&registry);
        state.incarnation = None;
    }
}
