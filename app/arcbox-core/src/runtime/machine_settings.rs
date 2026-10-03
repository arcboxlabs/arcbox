//! Per-machine settings the daemon keeps for the user: the size a machine
//! boots with, and which machine `abctl` acts on when given no name.

use std::path::Path;
use std::sync::PoisonError;

use super::{DEFAULT_MACHINE_NAME, HostCapacity, Runtime};
use crate::error::{CoreError, Result};

/// A machine's CPU and memory limits after [`Runtime::set_machine_resources`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineResources {
    /// vCPUs the machine boots with from now on.
    pub cpus: u32,
    /// Memory the machine boots with from now on, in MiB.
    pub memory_mb: u64,
    /// The machine is running with its previous size; the new one applies
    /// when it is next started.
    pub restart_required: bool,
    /// The host's capacity, the ceiling for both limits.
    pub host: HostCapacity,
}

impl Runtime {
    /// Changes a user machine's CPU and memory limits; a zero keeps the
    /// current value. The limits are checked against the host, then
    /// recorded for the machine's next start — see
    /// `MachineManager::set_resources` for why a running machine keeps its
    /// size until restarted.
    ///
    /// # Errors
    ///
    /// Returns an error for the System VM (its limits are
    /// `resize_system_vm`'s, which restarts it), an unknown machine, a
    /// limit the host cannot back, or a machine mid-transition.
    pub fn set_machine_resources(
        &self,
        name: &str,
        cpus: u32,
        memory_mb: u64,
    ) -> Result<MachineResources> {
        if name == DEFAULT_MACHINE_NAME {
            return Err(CoreError::config(
                "the System VM's limits are set with `abctl system resources`, which restarts it",
            ));
        }
        let machine = self
            .machine_manager
            .get(name)
            .ok_or_else(|| CoreError::not_found(name))?;
        let cpus = if cpus == 0 { machine.cpus } else { cpus };
        let memory_mb = if memory_mb == 0 {
            machine.memory_mb
        } else {
            memory_mb
        };
        let host = HostCapacity::probe();
        host.check(cpus, memory_mb)?;
        let resized = self
            .machine_manager
            .set_resources(name, Some(cpus), Some(memory_mb))?;
        Ok(MachineResources {
            cpus: resized.cpus,
            memory_mb: resized.memory_mb,
            restart_required: resized.restart_required,
            host,
        })
    }

    /// The machine `abctl machine exec` and `ssh` act on when given no
    /// name, if one is set. The name is not checked here: the machine may
    /// have been removed since, which the command using it reports.
    #[must_use]
    pub fn default_machine(&self) -> Option<String> {
        self.default_machine
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sets (or, for `None`, clears) the default machine, persisting it to
    /// the user's `config.toml` so it survives a daemon restart.
    ///
    /// # Errors
    ///
    /// Returns an error if the named machine does not exist or the config
    /// file cannot be written.
    pub fn set_default_machine(&self, name: Option<&str>) -> Result<()> {
        self.store_default_machine(&crate::config::writable_user_config_path(), name)
    }

    pub(super) fn store_default_machine(
        &self,
        config_path: &Path,
        name: Option<&str>,
    ) -> Result<()> {
        if let Some(name) = name
            && !self.machine_manager.exists(name)
        {
            return Err(CoreError::not_found(name));
        }
        crate::config::persist::set_default_machine(config_path, name)?;
        *self
            .default_machine
            .write()
            .unwrap_or_else(PoisonError::into_inner) = name.map(str::to_owned);
        tracing::info!(machine = name.unwrap_or("none"), "default machine set");
        Ok(())
    }
}
