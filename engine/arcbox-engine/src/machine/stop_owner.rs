//! Owns one stop attempt and at most one forced takeover of that attempt.

use super::*;

pub(super) struct StopRun {
    pub vm_id: VmId,
    pub expected: VmIncarnation,
}

pub(super) struct StopOperation {
    run: Arc<StopRun>,
    phase: Phase,
}

enum Phase {
    Graceful,
    Forced(Option<Predecessor>),
}

enum Predecessor {
    Pending,
    Complete(bool),
}

pub(super) enum Completion {
    Graceful(Result<bool>),
    Forced(Result<()>),
}

impl MachineManager {
    pub(super) fn admit_stop(
        &self,
        name: &str,
        expected: &VmIncarnation,
        graceful: bool,
    ) -> Result<Arc<StopRun>> {
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get_mut(name)
            .ok_or_else(|| EngineError::not_found(name))?;
        if machine.state != MachineState::Running
            && (graceful || machine.state != MachineState::Stopping)
        {
            return Err(EngineError::invalid_state(format!(
                "machine '{name}' cannot begin this stop"
            )));
        }
        let mut stops = self
            .stop_operations
            .lock()
            .map_err(|_| EngineError::LockPoisoned)?;
        self.vm_manager
            .with_run_incarnation(&machine.vm_id, expected, || {
                let run = match stops
                    .get_mut(name)
                    .filter(|operation| operation.run.expected == *expected)
                {
                    Some(operation) => {
                        if graceful || !matches!(operation.phase, Phase::Graceful) {
                            return Err(EngineError::invalid_state(
                                "another stop owns this machine run",
                            ));
                        }
                        operation.phase = Phase::Forced(Some(Predecessor::Pending));
                        Arc::clone(&operation.run)
                    }
                    None => {
                        let run = Arc::new(StopRun {
                            vm_id: machine.vm_id.clone(),
                            expected: expected.clone(),
                        });
                        stops.insert(
                            name.to_owned(),
                            StopOperation {
                                run: Arc::clone(&run),
                                phase: if graceful {
                                    Phase::Graceful
                                } else {
                                    Phase::Forced(None)
                                },
                            },
                        );
                        run
                    }
                };
                machine.state = MachineState::Stopping;
                self.publish_event(
                    name,
                    crate::event::Event::MachineStopping {
                        name: name.to_owned(),
                    },
                );
                Ok(run)
            })?
    }

    pub(super) fn require_force_owner(&self, name: &str, run: &Arc<StopRun>) -> Result<()> {
        self.stop_operations
            .lock()
            .map_err(|_| EngineError::LockPoisoned)?
            .get(name)
            .filter(|operation| {
                Arc::ptr_eq(&operation.run, run) && matches!(operation.phase, Phase::Forced(_))
            })
            .map(|_| ())
            .ok_or_else(|| EngineError::invalid_state("stop operation was superseded"))
    }

    pub(super) fn finish_stop(
        &self,
        name: &str,
        run: &Arc<StopRun>,
        completion: Completion,
    ) -> Result<bool> {
        let (mut result, forced) = match completion {
            Completion::Graceful(result) => (result, false),
            Completion::Forced(result) => (result.map(|()| true), true),
        };
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get_mut(name)
            .ok_or_else(|| EngineError::not_found(name))?;
        let mut stops = self
            .stop_operations
            .lock()
            .map_err(|_| EngineError::LockPoisoned)?;
        let operation = stops
            .get_mut(name)
            .filter(|operation| Arc::ptr_eq(&operation.run, run))
            .ok_or_else(|| EngineError::invalid_state("stop operation was superseded"))?;
        if !forced {
            if let Phase::Forced(predecessor) = &mut operation.phase {
                *predecessor = Some(Predecessor::Complete(matches!(result, Ok(true))));
                return result.and_then(|stopped| {
                    stopped
                        .then_some(true)
                        .ok_or_else(|| EngineError::invalid_state("forced stop owns completion"))
                });
            }
            if matches!(result, Ok(false)) {
                operation.phase = Phase::Forced(None);
                return Ok(false);
            }
        } else if result.is_err() {
            match operation.phase {
                Phase::Forced(Some(Predecessor::Pending)) => {
                    operation.phase = Phase::Graceful;
                    return result;
                }
                Phase::Forced(Some(Predecessor::Complete(true))) => result = Ok(true),
                _ => {}
            }
        }
        stops.remove(name);
        match result {
            Ok(stopped) => self
                .publish_stopped(name, machine, &run.expected)
                .map(|()| stopped),
            Err(error) => {
                self.restore_running(name, machine, &run.expected);
                Err(error)
            }
        }
    }

    fn restore_running(&self, name: &str, machine: &mut MachineInfo, expected: &VmIncarnation) {
        let id = machine.vm_id.clone();
        if let Err(error) = self.vm_manager.with_network_incarnation(&id, expected, || {
            machine.state = MachineState::Running;
            self.publish_event(
                name,
                crate::event::Event::MachineStopCancelled {
                    name: name.to_owned(),
                },
            );
        }) {
            tracing::warn!(machine = name, %error, "Stop failed without a confirmed live run to restore");
        }
    }
}
