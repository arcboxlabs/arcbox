//! The `Checkpoint` capability over VZ's `saveMachineStateTo`.
//!
//! The guest is paused for the capture: VZ saves its state, the writable
//! disks are flushed and cloned next to it, and the manifest is written
//! last (`crate::checkpoint`). The session is asked first, so a locked Mac
//! refuses before the guest stops; a save that still fails while locked
//! (the lock raced the check) reports the same error.

use std::path::Path;
use std::sync::atomic::Ordering;

use arcbox_vm_driver::{
    AfterCheckpoint, Checkpoint, CheckpointFormat, CheckpointImage, CheckpointKind,
    CheckpointOptions, Error, Result, VmState,
};
use arcbox_vz::{SessionState, VZError};
use async_trait::async_trait;

use super::{Inner, VzHandle};
use crate::checkpoint::{FORMAT, Manifest, STATE_FILE, clone_disks, session_locked};
use crate::{NAME, driver_error};

#[async_trait]
impl Checkpoint for VzHandle {
    async fn checkpoint(&self, dst: &Path, opts: CheckpointOptions) -> Result<CheckpointImage> {
        if opts.kind != CheckpointKind::Full {
            return Err(Error::InvalidSpec(format!(
                "{NAME}: diff checkpoints are not supported"
            )));
        }
        let inner = &self.inner;
        let _one_at_a_time = inner.checkpointing.lock().await;
        if let Some(status) = inner.exited() {
            return Err(Error::WrongState {
                id: inner.record.id.clone(),
                state: VmState::Exited(status),
                expected: "running or quiesced",
            });
        }
        let session = SessionState::current();
        if !session.allows_checkpoint() {
            return Err(session_locked(session));
        }
        tokio::fs::create_dir_all(dst).await?;

        let was_quiesced = inner.quiesced.load(Ordering::Acquire);
        if !was_quiesced {
            inner
                .vm
                .pause()
                .await
                .map_err(|e| driver_error("pause", e))?;
            // The flag tracks the guest: a resume that fails below leaves
            // the VM reporting Quiesced, which it is.
            inner.quiesced.store(true, Ordering::Release);
        }
        // Everything between pause and resume is fallible; its result is
        // handled only after the guest is settled, or a failure would
        // leave it paused for good.
        let captured = capture(inner, dst).await;
        let hold = captured.is_ok() && opts.after == AfterCheckpoint::HoldQuiesced;
        let resumed = if hold || (captured.is_err() && was_quiesced) {
            Ok(())
        } else {
            let resumed = inner.vm.resume().await;
            if resumed.is_ok() {
                inner.quiesced.store(false, Ordering::Release);
            }
            resumed.map_err(|e| driver_error("resume", e))
        };
        // The capture is what the caller asked for, so its failure is the
        // one reported; a failed resume on top of it is logged.
        if let Err(captured) = captured {
            if let Err(resumed) = resumed {
                tracing::error!(vm = %inner.record.id, error = %resumed,
                    "the guest stays quiesced: it could not be resumed after a failed checkpoint");
            }
            return Err(captured);
        }
        resumed?;
        Ok(CheckpointImage {
            dir: dst.to_path_buf(),
            format: CheckpointFormat::new(FORMAT),
            kind: CheckpointKind::Full,
        })
    }
}

/// Saves the paused VM into `dst`: state file, disk clones, manifest.
async fn capture(inner: &Inner, dst: &Path) -> Result<()> {
    let state = dst.join(STATE_FILE);
    // VZ writes the state file afresh; an earlier image's file would
    // otherwise survive a failed save and pair with the new manifest.
    match tokio::fs::remove_file(&state).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let t = std::time::Instant::now();
    inner.vm.save_state(&state).await.map_err(|e| match e {
        VZError::SessionLocked { state, .. } => session_locked(state),
        e => driver_error("save state", e),
    })?;
    let saved = t.elapsed();
    let manifest = Manifest::new(&inner.spec, &inner.machine_identifier)?;
    let spec = inner.spec.clone();
    let dir = dst.to_path_buf();
    tokio::task::spawn_blocking(move || {
        clone_disks(&spec, &dir)?;
        manifest.write(&dir)
    })
    .await
    .map_err(|e| driver_error("checkpoint task", e))??;
    tracing::info!(vm = %inner.record.id, save_ms = saved.as_millis(),
        total_ms = t.elapsed().as_millis(), dir = %dst.display(), "VZ checkpoint written");
    Ok(())
}
