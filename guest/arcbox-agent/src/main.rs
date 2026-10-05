//! ArcBox Guest Agent
//!
//! Runs inside the guest VM to handle host requests.
//!
//! The agent listens on vsock port 1024 and processes RPC requests from the host.
//! It manages container lifecycle and executes commands within the guest VM.

use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod agent;
// The guest's boot-completion sentinel: written by a hook `machine_init`
// installs, read by the readiness probe. Gated like `runtime_materialize` —
// compiled under `test` so the sentinel logic stays unit-testable on hosts,
// but out of a host release build, where its callers are all Linux-only and
// every path constant would read as dead code.
#[cfg(any(target_os = "linux", test))]
mod boot_done;
mod init;
// The hostname and network-manager files `machine_init` writes into a distro
// machine's root. Gated like `boot_done`, for the same reason.
#[cfg(any(target_os = "linux", test))]
mod machine_identity;
// Discovery half of the live-container NFS view. Pure parsing and
// naming, gated like `boot_done` so the tests run on a host build too.
// The containerd config the agent writes at runtime. Gated like
// `live_exports` so its tests run on a host build too.
#[cfg(any(target_os = "linux", test))]
mod containerd_config;
#[cfg(any(target_os = "linux", test))]
mod live_exports;
// A distro machine's root served to the host over NFSv3. Gated like
// `boot_done`: the filesystem logic is unit-tested on a host build, its
// only caller is the Linux RPC handler.
#[cfg(any(target_os = "linux", test))]
mod machine_export;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod runtime_materialize;
mod supervisor;

// Consumed by the Linux agent's WatchMemoryPressure handler; the library
// target compiles it everywhere so the logic stays unit-testable on hosts.
#[cfg(target_os = "linux")]
mod memory_pressure;

// Same arrangement for the WatchStats handler's /proc parsers.
#[cfg(target_os = "linux")]
mod stats;

// Same arrangement for the LoadBalancer Service listing the host forwards
// ports from.
#[cfg(target_os = "linux")]
mod kubernetes_services;

// Same arrangement for the ext4 metadata-volume migration state machine
// (pure std::fs; the mount syscalls live in agent/linux/metadata_volume.rs).
#[cfg(target_os = "linux")]
mod metadata_migrate;
#[cfg(target_os = "linux")]
mod storage_probe;

#[cfg(target_os = "linux")]
mod create_key;
#[cfg(target_os = "linux")]
mod create_registry;
#[cfg(target_os = "linux")]
mod error;

mod rpc;
mod shutdown;

// Mount module uses Linux-specific syscalls (mount/umount).
#[cfg(target_os = "linux")]
mod mount;

// NFSv3 export of the docker data mount + vsock relays (Linux-only: kernel nfsd).
#[cfg(target_os = "linux")]
mod nfs;

// SSH agent forwarding into containers (Linux-only: guest Unix + vsock relay).
#[cfg(target_os = "linux")]
mod ssh_auth;

// containerd snapshots client for container filesystem-path resolution.
#[cfg(target_os = "linux")]
mod containerd;

// Finder volume-icon files for the NFS export root.
#[cfg(target_os = "linux")]
mod volume_icon;

// VMM config loading and sandbox service are Linux-only.
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(target_os = "linux")]
mod sandbox_cleanup_watch;

// DNS: legacy /etc/hosts management (being replaced by dns_server).
mod dns;

// Guest-side DNS server and Docker event-driven container registration.
mod dns_server;
mod docker_config;
mod docker_events;
mod domains;
mod iptables;
mod publish_mirror;

/// Max bytes for `agent.log` before it rotates (matches the daemon's 10 MiB).
const AGENT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Number of rotated `agent.log.N` files to retain (matches the daemon).
const AGENT_LOG_MAX_FILES: usize = 5;

/// Startup mode selected from the process arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// One-shot system initialization (`arcbox-agent init`), run by busybox init's
    /// sysinit (rcS) before the agent is respawned. Performs `init_system` and exits.
    Init,
    /// One-shot distro machine initialization (`arcbox-agent machine-init`),
    /// run by the machine boot shim inside the overlay root before
    /// `switch_root`. Brings networking up and exits; the distro init owns
    /// everything else.
    MachineInit,
    /// Isolated storage diagnostics entered only through the recovery launcher.
    StorageRecovery,
    /// Exposes the marker that the recovery launcher verifies before execution.
    StorageRecoveryCapability,
    /// Long-running agent (default / `serve`): vsock RPC listener and background
    /// services. busybox init respawns it if it exits.
    Serve,
}

/// Selects the startup [`Mode`] from `args` (typically `std::env::args()`).
///
/// Unknown commands fail before initialization. The recovery launcher must
/// never turn an unsupported diagnostic command into a normal runtime boot.
fn parse_mode(args: &[String]) -> Result<Mode> {
    Ok(match args.get(1).map(String::as_str) {
        Some("init") => Mode::Init,
        Some("machine-init") => Mode::MachineInit,
        Some("storage-recovery") => Mode::StorageRecovery,
        Some("storage-recovery-capability") => Mode::StorageRecoveryCapability,
        None | Some("serve") => Mode::Serve,
        Some(command) => anyhow::bail!("unknown agent command: {command}"),
    })
}

fn validate_guest_mode(mode: Mode, guest: agent::Guest) -> Result<()> {
    if (mode == Mode::StorageRecovery) != (guest == agent::Guest::StorageRecovery) {
        anyhow::bail!(
            "storage recovery requires the storage-recovery command and arcbox.storage_recovery=1"
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = parse_mode(&std::env::args().collect::<Vec<_>>())?;
    if mode == Mode::StorageRecoveryCapability {
        // The trusted rootfs launcher checks this marker in the binary before
        // execution. Older agents accept unknown commands as normal serving.
        println!("arcbox-storage-recovery-v1");
        return Ok(());
    }
    let guest = agent::Guest::detect()?;
    validate_guest_mode(mode, guest)?;
    let is_pid1 = std::process::id() == 1;

    // Initialize logging early so init_system() has tracing output.
    // Write to /arcbox/log/agent.log (VirtioFS, visible from host as
    // ~/.arcbox/log/agent.log) and to the VM console (hvc1 if available,
    // falling back to stderr which goes to hvc0).
    let log_dir = format!("/arcbox/{}", arcbox_constants::paths::guest::LOG);
    let console_writer: Box<dyn std::io::Write + Send> =
        match std::fs::OpenOptions::new().write(true).open("/dev/hvc1") {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stderr()),
        };
    let _log_guard = if std::path::Path::new("/arcbox").exists() {
        match std::fs::create_dir_all(&log_dir) {
            Ok(()) => {
                // Size-based rotation (10 MiB x 5) reusing the daemon's writer:
                // keeps the active file named `agent.log` (so `abctl logs
                // --component agent` still finds it) while bounding growth.
                let log_path = std::path::Path::new(&log_dir).join("agent.log");
                let file_appender = arcbox_logging::SizeRotatingWriter::new(
                    log_path,
                    AGENT_LOG_MAX_BYTES,
                    AGENT_LOG_MAX_FILES,
                );
                let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
                tracing_subscriber::registry()
                    .with(
                        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
                            |_| "arcbox_agent=info,arcbox_computer_runtime=info".into(),
                        ),
                    )
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(std::sync::Mutex::new(console_writer)),
                    )
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_target(true)
                            .with_writer(non_blocking),
                    )
                    .init();
                Some(guard)
            }
            Err(e) => {
                // VirtioFS log dir not writable — fall back to console only.
                eprintln!("arcbox-agent: failed to create {log_dir}: {e}, falling back to console");
                tracing_subscriber::registry()
                    .with(
                        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
                            |_| "arcbox_agent=info,arcbox_computer_runtime=info".into(),
                        ),
                    )
                    .with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(std::sync::Mutex::new(console_writer)),
                    )
                    .init();
                None
            }
        }
    } else {
        // No VirtioFS mount — console only (development / testing).
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "arcbox_agent=info,arcbox_computer_runtime=info".into()),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(console_writer)),
            )
            .init();
        None
    };

    if mode == Mode::StorageRecovery {
        init::init_recovery_system();
        init::verify_critical_mounts().map_err(anyhow::Error::msg)?;
        // Tokio owns every recovery child. A waitpid(-1) reaper can consume
        // a checker's exit status before its Child::wait observes the result.
        return agent::run(guest).await;
    }

    // `arcbox-agent init` is the one-shot system-init entry that busybox init's
    // sysinit (rcS) runs before respawning the long-running agent: it performs the
    // system initialization and exits without starting the serving stack.
    if mode == Mode::MachineInit {
        tracing::info!("Running one-shot machine initialization");
        // No critical-mount verification: the machine root is the distro's
        // own writable overlay, not the tmpfs-staged EROFS layout.
        init::machine_init();
        return Ok(());
    }

    if mode == Mode::Init {
        tracing::info!("Running one-shot system initialization");
        init::init_system();
        // Fail fast (non-zero exit) if a writable layer the agent depends on did
        // not mount, so rcS can halt/retry instead of respawning an agent that
        // would run on the read-only EROFS rootfs and fail in obscure ways.
        if let Err(e) = init::verify_critical_mounts() {
            tracing::error!("system initialization incomplete: {e}");
            return Err(anyhow::anyhow!("system initialization incomplete: {e}"));
        }
        return Ok(());
    }

    // When the agent is run directly as PID 1 (legacy standalone boot, e.g. the
    // e2e harness) it owns system init itself. Under busybox init the agent is not
    // PID 1 — rcS already ran `arcbox-agent init` — so this block is skipped and
    // PID 1 (busybox init) reaps orphaned grandchildren natively.
    if is_pid1 {
        tracing::info!("Running as PID 1, initializing system");
        init::init_system();

        // Install SIGCHLD handler so orphaned grandchildren (containerd shims,
        // etc.) don't accumulate as zombies.
        supervisor::spawn_reaper();
    }

    tracing::info!(?guest, "ArcBox agent starting...");
    if guest != agent::Guest::SystemVm {
        return agent::run(guest).await;
    }

    let cancel = tokio_util::sync::CancellationToken::new();

    // Start the guest DNS server (0.0.0.0:53).
    let dns = std::sync::Arc::new(dns_server::GuestDnsServer::new(cancel.clone()));
    let dns_handle = {
        let dns = Arc::clone(&dns);
        tokio::spawn(async move {
            if let Err(e) = dns.run().await {
                tracing::error!(error = %e, "guest DNS server exited with error");
            }
        })
    };

    // Container domains: routes each container's ports 80 and 443 to what
    // it serves, sweeping a previous agent's rules before it follows anyone.
    let (domains, domains_handle) = domains::DomainRoutes::spawn(cancel.clone());

    // Start Docker event listener for auto-registering container DNS,
    // mirroring host-address-pinned publishes, and following containers for
    // their domains. Rules a previous agent left in the kernel go first;
    // reconciliation reinstalls what still applies.
    let docker_handle = {
        let dns = Arc::clone(&dns);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = publish_mirror::remove_all_orphans().await {
                tracing::warn!(error = %e, "failed to sweep stale publish mirror rules");
            }
            let uplink = uplink_interface();
            let sync = docker_events::ContainerSync {
                dns: &dns,
                mirror: publish_mirror::PublishMirror::new(uplink),
                domains,
            };
            docker_events::reconcile_and_watch(sync, cancel).await;
        })
    };

    // Bridge the guest NFSv4 server (nfsd on 2049) to the host over vsock. The
    // export itself is set up lazily once dockerd's data mount exists (see
    // agent::linux::runtime); the relay just waits for it. NFSv4 needs only
    // this one port — no MOUNT protocol.
    #[cfg(target_os = "linux")]
    let nfs_handle = tokio::spawn(nfs::run_nfs_relay(
        cancel.clone(),
        arcbox_constants::ports::NFS_NFSD_RELAY_PORT,
        nfs::NFSD_PORT,
    ));

    // Forward container SSH-agent connections to the daemon over vsock. The
    // daemon parks a pool of connections here and pairs each with a container
    // that opens the forwarded socket; see the `ssh_auth` module.
    #[cfg(target_os = "linux")]
    let ssh_auth_handle = tokio::spawn(ssh_auth::run_ssh_auth_relay(cancel.clone()));

    // Run the agent (vsock listener + RPC handler).
    let result = agent::run(guest).await;

    // Shut down background tasks.
    cancel.cancel();
    let _ = tokio::join!(dns_handle, docker_handle, domains_handle);
    #[cfg(target_os = "linux")]
    let _ = nfs_handle.await;
    #[cfg(target_os = "linux")]
    let _ = ssh_auth_handle.await;

    result
}

/// The NIC the host relay's traffic reaches the guest on; `eth0` when the
/// probe finds nothing, which is what every ArcBox guest image ships.
fn uplink_interface() -> String {
    #[cfg(target_os = "linux")]
    if let Some(name) = init::detect_primary_interface() {
        return name;
    }
    "eth0".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_mode, validate_guest_mode};
    use crate::agent::Guest;

    fn argv(extra: &[&str]) -> Vec<String> {
        std::iter::once("arcbox-agent")
            .chain(extra.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn init_subcommand_selects_init_mode() {
        assert_eq!(parse_mode(&argv(&["init"])).unwrap(), Mode::Init);
    }

    #[test]
    fn machine_init_subcommand_selects_machine_init_mode() {
        assert_eq!(
            parse_mode(&argv(&["machine-init"])).unwrap(),
            Mode::MachineInit
        );
    }

    #[test]
    fn no_subcommand_defaults_to_serve() {
        assert_eq!(parse_mode(&argv(&[])).unwrap(), Mode::Serve);
    }

    #[test]
    fn explicit_serve_subcommand_selects_serve() {
        assert_eq!(parse_mode(&argv(&["serve"])).unwrap(), Mode::Serve);
    }

    #[test]
    fn unknown_subcommand_cannot_start_the_runtime() {
        assert!(parse_mode(&argv(&["wat"])).is_err());
    }

    #[test]
    fn recovery_requires_both_the_command_and_kernel_flag() {
        assert_eq!(
            parse_mode(&argv(&["storage-recovery"])).unwrap(),
            Mode::StorageRecovery
        );
        assert_eq!(
            parse_mode(&argv(&["storage-recovery-capability"])).unwrap(),
            Mode::StorageRecoveryCapability
        );
        assert!(validate_guest_mode(Mode::StorageRecovery, Guest::StorageRecovery).is_ok());
        assert!(validate_guest_mode(Mode::StorageRecovery, Guest::SystemVm).is_err());
        assert!(validate_guest_mode(Mode::Serve, Guest::StorageRecovery).is_err());
        assert!(validate_guest_mode(Mode::Init, Guest::StorageRecovery).is_err());
    }
}
