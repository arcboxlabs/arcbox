//! Owns storage admission until verified completion or runtime drop.

use std::sync::{Arc, Mutex as StdMutex};

use arcbox_engine::machine::{MachineManager, StorageMaintenance};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};

#[derive(Default)]
pub(super) struct Owner {
    pub(super) cancelled: CancellationToken,
    pub(super) tasks: Mutex<Tasks>,
    completion: StdMutex<Option<StorageMaintenance>>,
}

#[derive(Default)]
pub(super) struct Tasks {
    pub(super) worker: Option<JoinHandle<Result<()>>>,
    pub(super) join_error: Option<String>,
    pub(super) cleanup_error: Option<String>,
}

impl Owner {
    pub(super) fn reserve(&self, manager: &Arc<MachineManager>) -> Result<StorageMaintenance> {
        // A completion panic does not invalidate ownership of the reservation.
        let reservation = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reservation.as_ref() {
            Some(reservation) => Ok(reservation.clone()),
            None => manager.reserve_storage().map_err(Into::into),
        }
    }

    pub(super) fn retain(&self, reservation: StorageMaintenance) {
        *self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reservation);
    }

    pub(super) fn close(&self) {
        let _completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cancelled.cancel();
    }

    pub(super) fn check_open(&self) -> Result<()> {
        if self.cancelled.is_cancelled() {
            return Err(CoreError::invalid_state(
                "storage recovery cancelled during daemon shutdown",
            ));
        }
        Ok(())
    }

    pub(super) fn complete(&self, commit: impl FnOnce() -> Result<()>) -> Result<()> {
        // The hold removal and shutdown cancellation have one linearization point.
        let mut reservation = self
            .completion
            .lock()
            .map_err(|error| CoreError::Machine(error.to_string()))?;
        self.check_open()?;
        commit()?;
        *reservation = None;
        Ok(())
    }
}

impl Tasks {
    pub(super) async fn join(&mut self) {
        if let Some(worker) = self.worker.as_mut() {
            match worker.await {
                Ok(cleanup) => self.cleanup_error = cleanup.err().map(|error| error.to_string()),
                Err(error) => self.join_error = Some(error.to_string()),
            }
            self.worker = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn cancelling_the_first_closer_keeps_the_worker_for_the_next_closer() {
        let owner = Arc::new(Owner::default());
        let (release, finish) = tokio::sync::oneshot::channel();
        owner.tasks.lock().await.worker = Some(tokio::spawn(async move {
            finish.await.unwrap();
            Ok(())
        }));
        let first_owner = Arc::clone(&owner);
        let (joining, joined) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(async move {
            first_owner.close();
            let mut tasks = first_owner.tasks.lock().await;
            joining.send(()).unwrap();
            tasks.join().await;
        });
        joined.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(owner.tasks.lock().await.worker.is_some());
        assert!(owner.check_open().is_err());
        release.send(()).unwrap();
        let mut tasks = owner.tasks.lock().await;
        tasks.join().await;
        assert!(tasks.worker.is_none());
        assert!(tasks.join_error.is_none());
    }

    #[test]
    fn cancellation_does_not_commit_an_unprotected_outcome() {
        let owner = Owner::default();
        owner.close();
        assert!(
            owner
                .complete(|| panic!("cancelled recovery must retain its hold"))
                .is_err()
        );
    }

    #[test]
    fn a_panicked_completion_does_not_prevent_shutdown_cancellation() {
        let owner = Arc::new(Owner::default());
        let writer = Arc::clone(&owner);
        assert!(
            std::thread::spawn(move || {
                let _guard = writer.completion.lock().unwrap();
                panic!("interrupted completion");
            })
            .join()
            .is_err()
        );
        owner.close();
        assert!(owner.cancelled.is_cancelled());
    }
}
