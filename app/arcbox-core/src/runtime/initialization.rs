//! Prepare shared host state before optional System VM initialization.

use super::{InitProgress, Runtime, assets::ensure_guest_binaries, engine_config};
#[cfg(target_os = "macos")]
use crate::error::CoreError;
use crate::error::Result;

impl Runtime {
    /// Initializes shared host state and starts the System VM when autostart is enabled.
    ///
    /// On macOS, remove the legacy shared CA key before either guest mode becomes usable.
    /// Validate guest binaries before starting the System VM.
    /// `progress` reports System VM milestones; VM-host-only mode reports none.
    ///
    /// # Errors
    ///
    /// Returns an error if host preparation, guest binaries, or System VM startup fails.
    pub async fn init(&self, progress: impl Fn(InitProgress) + Send) -> Result<()> {
        tokio::fs::create_dir_all(&self.config.data_dir).await?;
        tokio::fs::create_dir_all(self.config.data_dir.join("vms")).await?;
        tokio::fs::create_dir_all(self.config.data_dir.join("machines")).await?;

        #[cfg(target_os = "macos")]
        {
            let tls_dir = self
                .config
                .data_dir
                .join(arcbox_constants::paths::guest::TLS);
            // Distro machines also share this directory when System VM autostart is disabled or storage is held.
            tokio::task::spawn_blocking(move || {
                std::fs::create_dir_all(&tls_dir)?;
                arcbox_local_ca::remove_shared_private_key(&tls_dir)?;
                Ok::<_, CoreError>(())
            })
            .await
            .map_err(|error| {
                CoreError::Vm(format!("shared CA key removal task failed: {error}"))
            })??;
        }

        // VM-host-only mode skips the System VM and its Docker/Kubernetes prerequisites.
        if !self.config.vm.autostart {
            tracing::info!(
                "Linux VM autostart disabled; running as a VM host only (Docker/Kubernetes unavailable)"
            );
            return Ok(());
        }

        // Runtime binaries are generation-scoped so a new boot bundle cannot replace a running guest's tools.
        let generation = self.vm_lifecycle.boot_assets().config().version.clone();
        let runtime_bin_dir = self
            .config
            .data_dir
            .join("runtime")
            .join(&generation)
            .join("bin");
        tokio::fs::create_dir_all(&runtime_bin_dir).await?;
        self.vm_lifecycle
            .boot_assets()
            .prepare_binaries(&runtime_bin_dir, None)
            .await?;
        ensure_guest_binaries(&self.config.data_dir, &generation)?;

        // The guest merges the operator's staged overrides into daemon.json.
        engine_config::stage_engine_config(&self.config.data_dir, &self.config.docker)?;

        if self.machine_manager.storage_is_held()? {
            tracing::warn!(
                "System VM storage is held for recovery; keeping the control plane available"
            );
            return Ok(());
        }

        // Keep the agent-ready milestone separate from the later dockerd readiness probe.
        progress(InitProgress::SystemVmStarting);
        if let Err(error) = self.vm_lifecycle.ensure_ready().await {
            if self.protect_failed_storage_boot(&error)? {
                tracing::warn!(%error, "System VM storage is protected; keeping the control plane available");
                return Ok(());
            }
            return Err(error.into());
        }
        progress(InitProgress::SystemVmReady);

        if let Err(error) = self.ensure_vm_ready().await {
            if self.protect_failed_runtime_start(&error).await? {
                tracing::warn!(%error, "Required storage is unavailable; keeping the stopped System VM protected");
                return Ok(());
            }
            return Err(error);
        }

        tracing::info!(
            backend = self.container_backend.name(),
            "ArcBox runtime initialized"
        );
        Ok(())
    }
}
