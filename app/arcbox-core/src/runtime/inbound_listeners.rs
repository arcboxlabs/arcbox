//! Owns listener managers across VM replacement and canceled cleanup.

use super::{CoreError, Result, Runtime};
use arcbox_engine::vm::VmIncarnation;
use arcbox_net::darwin::inbound_relay::InboundListenerManager;
use std::collections::hash_map::Entry;

#[cfg(test)]
mod tests;

pub(super) struct Slot {
    pub(super) current: InboundListenerManager,
    pub(super) incarnation: VmIncarnation,
    retiring: Option<InboundListenerManager>,
}

#[cfg(test)]
impl From<InboundListenerManager> for Slot {
    fn from(current: InboundListenerManager) -> Self {
        Self {
            current,
            incarnation: VmIncarnation::for_test(),
            retiring: None,
        }
    }
}

impl Slot {
    pub(super) async fn finish_retiring(&mut self) {
        if let Some(manager) = self.retiring.as_mut() {
            manager.stop_all().await;
            self.retiring = None;
        }
    }

    pub(super) async fn stop_all(&mut self) {
        self.finish_retiring().await;
        self.current.stop_all().await;
    }
}

impl Runtime {
    /// Keeps the source manager in the VMM until Runtime can retain both owners.
    pub(super) async fn adopt_inbound_listener_manager(
        &self,
        machine_name: &str,
        take_manager: impl FnOnce() -> Result<Option<(VmIncarnation, InboundListenerManager)>>,
    ) -> Result<()> {
        let mut rules = self.inbound_rules.write().await;
        let mut listeners = self.inbound_listeners.write().await;
        if let Some(slot) = listeners.get_mut(machine_name) {
            slot.finish_retiring().await;
        }
        let Some((incarnation, manager)) = take_manager()? else {
            return Ok(());
        };
        // No await may separate the source transfer and storage of both managers.
        match listeners.entry(machine_name.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(Slot {
                    current: manager,
                    incarnation,
                    retiring: None,
                });
            }
            Entry::Occupied(mut entry) => {
                let slot = entry.get_mut();
                slot.incarnation = incarnation;
                slot.retiring = Some(std::mem::replace(&mut slot.current, manager));
                rules.retain(|_, (machine, _)| machine != machine_name);
                tracing::info!(
                    machine = machine_name,
                    "retiring the previous VM incarnation's inbound listeners"
                );
                slot.finish_retiring().await;
            }
        }
        Ok(())
    }

    pub(super) async fn stop_inbound_rules(
        &self,
        container_id: &str,
        expected: Option<(&str, &VmIncarnation)>,
    ) -> Result<()> {
        let mut rules = self.inbound_rules.write().await;
        let mut slots = self.inbound_listeners.write().await;
        if let Some((machine, incarnation)) = expected {
            if !slots
                .get(machine)
                .is_some_and(|slot| slot.incarnation == *incarnation)
            {
                return Err(CoreError::Machine(
                    "inbound listener incarnation changed".into(),
                ));
            }
        }
        if let Some((machine, installed)) = rules.get(container_id) {
            if let Some(slot) = slots.get_mut(machine) {
                for (ip, port, _, protocol) in installed {
                    slot.current.remove_rule(*ip, *port, *protocol).await;
                }
            }
            rules.remove(container_id);
        }
        Ok(())
    }
}
