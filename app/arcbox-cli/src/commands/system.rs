//! System VM control commands.

use anyhow::{Context, Result};
use arcbox_connect::v1 as pb;
use arcbox_connect::v1::SystemServiceClient;
use arcbox_connect::v1::SystemVmBackend;
use clap::{Subcommand, ValueEnum};

use crate::connect;

#[derive(Debug, Subcommand)]
pub enum SystemCommands {
    /// Show or switch the System VM hypervisor backend (HV / VZ).
    ///
    /// With no argument, prints the current backend. With `hv` or `vz`,
    /// persists the choice and restarts the System VM so it takes effect —
    /// this stops running containers.
    Backend {
        /// Backend to switch to. Omit to print the current backend.
        #[arg(value_enum)]
        set: Option<BackendArg>,
    },
    /// Show or set the System VM's CPU and memory limits.
    ///
    /// With no flags, prints the current limits and the host's capacity.
    /// With `--cpus` and/or `--memory`, writes the new limits to
    /// `~/.config/arcbox/config.toml` and recreates the System VM so they
    /// take effect — this stops running containers.
    Resources {
        /// vCPUs, 1 through the host's logical CPU count.
        #[arg(long)]
        cpus: Option<u32>,
        /// Memory in MiB, 512 through the host's physical memory.
        #[arg(long, value_name = "MIB")]
        memory: Option<u64>,
    },
}

/// CLI spelling of [`SystemVmBackend`].
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BackendArg {
    /// Hypervisor.framework — ArcBox's custom VMM (amd64 via FEX).
    Hv,
    /// Virtualization.framework — Apple-managed execution (default).
    Vz,
}

impl From<BackendArg> for SystemVmBackend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Hv => Self::Hv,
            BackendArg::Vz => Self::Vz,
        }
    }
}

/// Human label for a wire backend value.
fn label(backend: SystemVmBackend) -> &'static str {
    match backend {
        SystemVmBackend::Hv => "hv (Hypervisor.framework + FEX)",
        SystemVmBackend::Vz => "vz (Virtualization.framework)",
        SystemVmBackend::Unspecified => "unspecified",
    }
}

fn system_client() -> SystemServiceClient<connectrpc::client::SharedHttp2Connection> {
    let (transport, config) = connect::daemon(&super::resolve_grpc_socket_path());
    SystemServiceClient::new(transport, config)
}

pub async fn execute(cmd: SystemCommands) -> Result<()> {
    match cmd {
        SystemCommands::Backend { set } => {
            let client = system_client();
            let info: pb::SystemVmBackendInfo = if let Some(arg) = set {
                let backend = SystemVmBackend::from(arg);
                println!(
                    "Switching System VM backend to {} — restarting the System VM...",
                    label(backend)
                );
                client
                    .set_system_vm_backend(pb::SetSystemVmBackendRequest {
                        backend: backend.into(),
                        ..Default::default()
                    })
                    .await
                    .context("failed to switch System VM backend")?
                    .into_owned()
            } else {
                client
                    .get_system_vm_backend(pb::Empty::default())
                    .await
                    .context("failed to query System VM backend")?
                    .into_owned()
            };
            let current = info.backend.as_known().unwrap_or_default();
            println!("System VM backend: {}", label(current));
            Ok(())
        }
        SystemCommands::Resources { cpus, memory } => {
            let client = system_client();
            let resources: pb::SystemVmResources = if cpus.is_some() || memory.is_some() {
                println!("Applying System VM resources — restarting the System VM...");
                client
                    .set_system_vm_resources(pb::SetSystemVmResourcesRequest {
                        cpus: cpus.unwrap_or(0),
                        memory_mb: memory.unwrap_or(0),
                        ..Default::default()
                    })
                    .await
                    .context("failed to set System VM resources")?
                    .into_owned()
            } else {
                client
                    .get_system_vm_resources(pb::Empty::default())
                    .await
                    .context("failed to query System VM resources")?
                    .into_owned()
            };
            println!("{}", render_resources(&resources));
            Ok(())
        }
    }
}

/// Human rendering of the System VM's limits against the host.
fn render_resources(r: &pb::SystemVmResources) -> String {
    format!(
        "System VM resources:\n  CPUs:   {} of {} on the host\n  Memory: {} MiB of {} MiB on the host",
        r.cpus, r.host_cpus, r.memory_mb, r.host_memory_mb
    )
}
