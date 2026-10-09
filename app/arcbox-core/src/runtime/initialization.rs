//! Prepare shared host state before optional System VM initialization.

use super::{InitProgress, Runtime, assets::ensure_guest_binaries, engine_config};
#[cfg(target_os = "macos")]
use crate::error::CoreError;
use crate::error::Result;

impl Runtime {
    /// Initializes shared host state and starts the System VM when autostart is enabled.
    ///
    /// `progress` reports System VM milestones; VM-host-only mode reports none.
    ///
    /// # Errors
    ///
    /// Returns an error if host preparation, guest binaries, or System VM startup fails.
    pub async fn init(&self, progress: impl Fn(InitProgress) + Send) -> Result<()> {
        self.prepare_host().await?;
        let _startup = self.reserve_system_startup().await;
        self.init_system(progress).await
    }

    /// Waits for System VM admission, then excludes recovery and reconfiguration during startup.
    ///
    /// Retain the guard through System service startup and final readiness publication.
    /// Release the guard before waiting for a storage recovery result.
    ///
    /// Callers may cancel the wait before the guard is acquired.
    pub async fn reserve_system_startup(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.storage_recovery.operation.lock().await
    }

    /// Prepares shared directories without downloading assets or starting the System VM.
    ///
    /// Await completion before exposing machine operations or sharing the data directory with guests.
    /// On macOS, this removes the legacy shared CA key before either guest mode becomes usable.
    ///
    /// # Errors
    /// Returns an error if directory preparation or legacy private-key removal fails.
    pub async fn prepare_host(&self) -> Result<()> {
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
        Ok(())
    }

    /// Starts the System VM when autostart is enabled, after [`Self::prepare_host`] completes.
    ///
    /// Hold [`Self::reserve_system_startup`] through final System service readiness publication.
    /// Validate guest binaries before starting the System VM.
    /// `progress` reports System VM milestones; VM-host-only mode reports none.
    ///
    /// # Errors
    /// Returns an error if guest binary preparation or System VM startup fails.
    pub async fn init_system(&self, progress: impl Fn(InitProgress) + Send) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::Runtime;
    use crate::Config;

    #[tokio::test]
    async fn initialization_failure_releases_system_startup_admission() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        std::fs::write(directory.path().join("runtime"), b"blocked assets").unwrap();
        assert!(runtime.init(|_| {}).await.is_err());
        let _next_startup = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runtime.reserve_system_startup(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn host_preparation_succeeds_without_system_vm_prerequisites() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        // A blocked asset directory fails System VM preparation before any network or VM work.
        let assets = directory.path().join("runtime");
        std::fs::write(&assets, b"blocked System VM assets").unwrap();

        runtime.prepare_host().await.unwrap();

        assert!(directory.path().join("vms").is_dir());
        assert!(directory.path().join("machines").is_dir());
        assert!(runtime.machine_manager().list().is_empty());
        let error = runtime
            .init_system(|_| panic!("System VM cannot start without assets"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                crate::CoreError::Common(arcbox_error::CommonError::Io(_))
            ),
            "{error}"
        );
        assert_eq!(std::fs::read(assets).unwrap(), b"blocked System VM assets");
        assert!(runtime.machine_manager().list().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn host_preparation_requires_private_key_cleanup_before_system_initialization() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(Config {
            data_dir: directory.path().to_owned(),
            ..Default::default()
        })
        .unwrap();
        let tls = directory.path().join(arcbox_constants::paths::guest::TLS);
        let key = tls.join(arcbox_constants::paths::guest::TLS_CA_KEY);
        let certificate = tls.join(arcbox_constants::paths::guest::TLS_CA_CERT);
        std::fs::create_dir_all(&key).unwrap();
        std::fs::write(&certificate, b"published public root").unwrap();

        let error = runtime.prepare_host().await.unwrap_err();
        assert!(error.to_string().contains("ca-key.pem"), "{error}");
        assert!(key.is_dir());

        std::fs::remove_dir(&key).unwrap();
        std::fs::write(&key, b"legacy private key").unwrap();
        runtime.prepare_host().await.unwrap();

        assert!(!key.try_exists().unwrap());
        assert_eq!(
            std::fs::read(certificate).unwrap(),
            b"published public root"
        );
        assert!(!directory.path().join("runtime").try_exists().unwrap());
        assert!(runtime.machine_manager().list().is_empty());
    }
}
