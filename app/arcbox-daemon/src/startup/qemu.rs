//! Installs the QEMU build an emulated System VM runs on, publishing the
//! download in `SystemRuntimeStatus`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arcbox_api::SetupState;
use arcbox_connect::v1::component_download::{Component, Stage};
use arcbox_connect::v1::{ComponentDownload, system_runtime_status::State};
use arcbox_core::Config;
use arcbox_image::qemu::{QemuProgress, Retry};

use crate::context::DaemonContext;

/// Transfer updates closer together than this are folded into the next one:
/// every streaming client receives every update.
const TRANSFER_UPDATE_INTERVAL: Duration = Duration::from_millis(250);

/// Returns the installed QEMU executable, downloading the pinned build first
/// when it is missing, or `None` when this daemon pins no build for the host.
/// Retries until shutdown; the System status shows the download meanwhile.
pub(super) async fn ensure(ctx: &DaemonContext, config: &Config) -> Result<Option<PathBuf>> {
    let Some(runtime) = config
        .qemu_runtime()
        .context("Failed to select the QEMU build")?
    else {
        return Ok(None);
    };
    let version = runtime.artifact().version.clone();
    let progress = Progress::new(&ctx.setup_state, version.clone());
    let executable = runtime
        .ensure(Retry::default(), &ctx.shutdown, |event| {
            progress.publish(event);
        })
        .await
        .with_context(|| format!("Failed to install QEMU {version}"))?;
    if progress.published() {
        ctx.setup_state
            .set_system_runtime(State::Starting, "Preparing the System VM");
    }
    Ok(Some(executable))
}

/// Maps download events onto the System status.
struct Progress<'a> {
    setup_state: &'a SetupState,
    version: String,
    state: Mutex<Published>,
}

#[derive(Default)]
struct Published {
    any: bool,
    last_transfer: Option<Instant>,
    downloaded: u64,
    total: u64,
}

impl<'a> Progress<'a> {
    fn new(setup_state: &'a SetupState, version: String) -> Self {
        Self {
            setup_state,
            version,
            state: Mutex::default(),
        }
    }

    fn published(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).any
    }

    fn publish(&self, event: QemuProgress) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let version = &self.version;
        let (stage, reason) = match event {
            QemuProgress::Transferring { downloaded, total } => {
                // Later stages report these, so they track every update,
                // including the folded ones.
                state.downloaded = downloaded;
                state.total = total.unwrap_or(0);
                let now = Instant::now();
                let complete = total == Some(downloaded);
                if !complete
                    && state
                        .last_transfer
                        .is_some_and(|last| now - last < TRANSFER_UPDATE_INTERVAL)
                {
                    return;
                }
                state.last_transfer = Some(now);
                (Stage::Transferring, format!("Downloading QEMU {version}"))
            }
            QemuProgress::Verifying => (Stage::Verifying, format!("Verifying QEMU {version}")),
            QemuProgress::Installing => (Stage::Installing, format!("Installing QEMU {version}")),
            QemuProgress::WaitingToRetry {
                error,
                delay,
                downloaded,
            } => {
                state.last_transfer = None;
                state.downloaded = downloaded;
                (
                    Stage::WaitingToRetry,
                    format!(
                        "QEMU download interrupted: {error}. Retrying in {} s; without network \
                         access, install it with `abctl qemu import <archive>`",
                        delay.as_secs().max(1)
                    ),
                )
            }
        };
        state.any = true;
        self.setup_state.set_system_runtime_download(
            ComponentDownload {
                component: Component::Qemu.into(),
                version: version.clone(),
                stage: stage.into(),
                downloaded_bytes: state.downloaded,
                total_bytes: state.total,
                ..Default::default()
            },
            &reason,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_keeps_bytes_across_stages_and_folds_rapid_transfers() {
        let setup_state = SetupState::new();
        let progress = Progress::new(&setup_state, "0.1.0".into());
        let published = || {
            let status = setup_state.current();
            assert_eq!(status.system_runtime.state, State::Downloading);
            let download = status.system_runtime.download.clone();
            assert_eq!(download.component, Component::Qemu);
            assert_eq!(download.version, "0.1.0");
            (
                download.stage,
                download.downloaded_bytes,
                download.total_bytes,
            )
        };
        let transfer = |downloaded| QemuProgress::Transferring {
            downloaded,
            total: Some(40),
        };

        progress.publish(transfer(10));
        assert_eq!(published(), (Stage::Transferring.into(), 10, 40));
        progress.publish(transfer(20));
        assert_eq!(published(), (Stage::Transferring.into(), 10, 40));
        // A retry reports the bytes on disk, not the last published.
        progress.publish(QemuProgress::WaitingToRetry {
            error: "connection reset".into(),
            delay: Duration::from_secs(1),
            downloaded: 25,
        });
        assert_eq!(published(), (Stage::WaitingToRetry.into(), 25, 40));
        progress.publish(transfer(40));
        assert_eq!(published(), (Stage::Transferring.into(), 40, 40));
        progress.publish(QemuProgress::Verifying);
        assert_eq!(published(), (Stage::Verifying.into(), 40, 40));
        assert!(progress.published());
    }
}
