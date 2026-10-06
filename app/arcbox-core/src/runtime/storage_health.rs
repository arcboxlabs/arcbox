//! Generation-fenced storage observations for operation admission.

use arcbox_connect::v1::StorageHealth;

use super::Runtime;

impl Runtime {
    /// Returns the latest storage observation for the currently running guest.
    /// Missing means unknown, never writable. This call does not probe or wake a VM.
    #[must_use]
    pub fn system_storage_health(&self) -> Option<StorageHealth> {
        let vm = self.subscribe_system_vm_state();
        let ready = vm.borrow().is_ready();
        current_observation(
            &self.storage_health.borrow(),
            self.system_vm_restart_generation(),
            ready,
        )
    }

    /// Records a daemon observation from `generation`, or clears a lost watch.
    /// Late frames from a previous guest cannot replace the current observation.
    pub fn set_system_storage_health(&self, generation: u64, health: Option<StorageHealth>) {
        if generation != self.system_vm_restart_generation() {
            return;
        }
        self.storage_health.send_if_modified(|cached| {
            if cached.0 > generation {
                return false;
            }
            *cached = (generation, health);
            true
        });
    }
}

fn current_observation(
    cached: &(u64, Option<StorageHealth>),
    generation: u64,
    ready: bool,
) -> Option<StorageHealth> {
    if ready && cached.0 == generation {
        cached.1.clone()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use arcbox_connect::v1::{
        StorageVolumeHealth,
        storage_volume_health::{Role, State},
    };

    use super::*;

    #[test]
    fn previous_guest_fault_cannot_block_a_new_or_stopped_guest() {
        let health = StorageHealth {
            volumes: vec![StorageVolumeHealth {
                role: Role::Data.into(),
                state: State::ReadOnly.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let cached = (2, Some(health.clone()));
        assert_eq!(current_observation(&cached, 2, true), Some(health));
        assert!(current_observation(&cached, 3, true).is_none());
        assert!(current_observation(&cached, 2, false).is_none());
        assert!(current_observation(&(3, None), 3, true).is_none());
    }
}
