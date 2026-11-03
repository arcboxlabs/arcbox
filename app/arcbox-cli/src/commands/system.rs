//! System VM control commands.

use anyhow::{Context, Result};
use arcbox_connect::v1 as pb;
use arcbox_connect::v1::SystemServiceClient;
use arcbox_connect::v1::SystemVmBackend;
use clap::{Subcommand, ValueEnum};

use crate::connect;

#[derive(Debug, Subcommand)]
pub enum SystemCommands {
    /// Show or switch the System VM hypervisor backend (HV / VZ / QEMU).
    ///
    /// With no argument, prints the backend the System VM runs on, and the
    /// configured one too when this host overrides it. With a backend,
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
    /// QEMU TCG emulation — slow; what an Apple silicon Mac without
    /// hardware virtualization boots whatever is chosen.
    Qemu,
}

impl From<BackendArg> for SystemVmBackend {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Hv => Self::Hv,
            BackendArg::Vz => Self::Vz,
            BackendArg::Qemu => Self::Qemu,
        }
    }
}

/// Human label for a wire backend value.
pub(super) fn label(backend: SystemVmBackend) -> &'static str {
    match backend {
        SystemVmBackend::Hv => "hv (Hypervisor.framework + FEX)",
        SystemVmBackend::Vz => "vz (Virtualization.framework)",
        SystemVmBackend::Qemu => "qemu (TCG emulation)",
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
            let configured = info.backend.as_known().unwrap_or_default();
            // The effective backend lives in the setup status; without it
            // the configured one is all there is to show.
            let status = client
                .get_setup_status(pb::Empty::default())
                .await
                .ok()
                .map(connectrpc::UnaryResponse::into_owned);
            println!("{}", render_backend(configured, status.as_ref()));
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

/// The backend the System VM runs on, and the configured one when they
/// differ: an Apple silicon Mac without hardware virtualization boots QEMU
/// whatever is configured, and never records that.
fn render_backend(configured: SystemVmBackend, status: Option<&pb::SetupStatus>) -> String {
    let effective = status
        .and_then(|status| status.effective_backend.as_known())
        .filter(|backend| *backend != SystemVmBackend::Unspecified);
    let no_hardware_virtualization =
        status.and_then(|status| status.hardware_virtualization_available) == Some(false);
    match effective {
        Some(effective) if effective != configured => {
            let mut rendered = format!(
                "System VM backend: {}\nConfigured backend: {}",
                label(effective),
                label(configured)
            );
            if no_hardware_virtualization {
                rendered.push_str(" (this host has no hardware virtualization)");
            }
            rendered
        }
        // Unspecified once resolved means no backend can run here; a daemon
        // that predates the field leaves it unspecified too.
        None if no_hardware_virtualization => format!(
            "System VM backend: none (this host has no hardware virtualization)\n\
             Configured backend: {}",
            label(configured)
        ),
        _ => format!("System VM backend: {}", label(configured)),
    }
}

/// Human rendering of the System VM's limits against the host.
fn render_resources(r: &pb::SystemVmResources) -> String {
    format!(
        "System VM resources:\n  CPUs:   {} of {} on the host\n  Memory: {} MiB of {} MiB on the host",
        r.cpus, r.host_cpus, r.memory_mb, r.host_memory_mb
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(effective: SystemVmBackend, hardware: Option<bool>) -> pb::SetupStatus {
        pb::SetupStatus {
            effective_backend: effective.into(),
            hardware_virtualization_available: hardware,
            ..Default::default()
        }
    }

    /// The regression: a Mac without hardware virtualization runs QEMU
    /// while VZ stays configured, and the command printed only VZ.
    #[test]
    fn fallback_shows_the_running_backend_and_the_configured_one() {
        let rendered = render_backend(
            SystemVmBackend::Vz,
            Some(&status(SystemVmBackend::Qemu, Some(false))),
        );
        assert_eq!(
            rendered,
            "System VM backend: qemu (TCG emulation)\n\
             Configured backend: vz (Virtualization.framework) \
             (this host has no hardware virtualization)"
        );
    }

    #[test]
    fn matching_backends_print_one_line() {
        let rendered = render_backend(
            SystemVmBackend::Hv,
            Some(&status(SystemVmBackend::Hv, Some(true))),
        );
        assert_eq!(
            rendered,
            "System VM backend: hv (Hypervisor.framework + FEX)"
        );
    }

    /// An unreachable status or a daemon without the field must not invent
    /// a difference.
    #[test]
    fn unknown_effective_backend_falls_back_to_the_configured_one() {
        let expected = "System VM backend: vz (Virtualization.framework)";
        assert_eq!(render_backend(SystemVmBackend::Vz, None), expected);
        assert_eq!(
            render_backend(
                SystemVmBackend::Vz,
                Some(&status(SystemVmBackend::Unspecified, None))
            ),
            expected
        );
    }

    #[test]
    fn a_host_that_runs_no_backend_says_so() {
        let rendered = render_backend(
            SystemVmBackend::Qemu,
            Some(&status(SystemVmBackend::Unspecified, Some(false))),
        );
        assert!(
            rendered.starts_with("System VM backend: none"),
            "{rendered}"
        );
        assert!(
            rendered.ends_with("Configured backend: qemu (TCG emulation)"),
            "{rendered}"
        );
    }
}
