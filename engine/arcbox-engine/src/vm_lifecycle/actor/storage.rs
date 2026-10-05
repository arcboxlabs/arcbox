//! Storage maintenance commands and the reserved stop task.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::error::{EngineError, Result};
use crate::machine::StorageMaintenance;

use super::{Completion, InternalEvent, LifecycleActor, Machine, VmEvent, VmLifecycleState};

impl LifecycleActor {
    pub(super) fn on_storage_check(
        &self,
        reservation: StorageMaintenance,
        reply: oneshot::Sender<Result<()>>,
    ) {
        let result = if !reservation.belongs_to(&self.shared.machine_manager)
            || matches!(
                self.public(),
                VmLifecycleState::Creating
                    | VmLifecycleState::Starting
                    | VmLifecycleState::Stopping
            ) {
            Err(EngineError::invalid_state(
                "storage recovery requires a stable System VM lifecycle",
            ))
        } else {
            Ok(())
        };
        let _ = reply.send(result);
    }

    pub(super) fn on_storage_stop(
        &mut self,
        machine: &mut Machine,
        reservation: StorageMaintenance,
        reply: oneshot::Sender<Result<()>>,
    ) {
        if !reservation.belongs_to(&self.shared.machine_manager) {
            let _ = reply.send(Err(EngineError::invalid_state(
                "foreign storage maintenance reservation",
            )));
            return;
        }
        self.stop_waiters.push(reply);
        if self.storage_stopping {
            return;
        }
        self.storage_stopping = true;
        self.storage_stop = Some(reservation);
        self.shared.health_monitor.stop();
        self.pending_timeout = None;
        self.pending_stop = false;
        self.dispatch(machine, VmEvent::StopStorage);
    }

    pub(super) fn spawn_storage_stop(&mut self) {
        if let Some(cancelled) = self.storage_cancel.take() {
            cancelled.cancel();
        }
        let previous = self
            .inflight
            .take()
            .or_else(|| self.storage_boot_done.take());
        self.epoch += 1;
        let epoch = self.epoch;
        let shared = Arc::clone(&self.shared);
        let events = self.events_tx.clone();
        let reservation = self.storage_stop.take().unwrap();
        self.inflight = Some(tokio::spawn(async move {
            let joined = if let Some(previous) = previous {
                previous
                    .await
                    .map_err(|error| EngineError::Vm(error.to_string()))
            } else {
                Ok(())
            };
            let stopped = tokio::task::spawn_blocking(move || {
                let _reservation = reservation;
                if shared
                    .machine_manager
                    .get(&shared.machine_name)
                    .is_some_and(|machine| {
                        !matches!(
                            machine.state,
                            crate::machine::MachineState::Created
                                | crate::machine::MachineState::Stopped
                        )
                    })
                {
                    shared.machine_manager.stop(&shared.machine_name)?;
                }
                Ok::<_, EngineError>(())
            })
            .await
            .map_err(|error| EngineError::Vm(error.to_string()))
            .and_then(|result| result);
            let outcome = match (joined, stopped) {
                (Ok(()), Ok(())) => InternalEvent::Stopped,
                (joined, stopped) => InternalEvent::StopFailed(format!(
                    "recovery boot join: {joined:?}; reserved VM stop: {stopped:?}"
                )),
            };
            let _ = events.send(Completion { epoch, outcome });
        }));
    }
}
