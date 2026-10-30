//! Publish host DNS and recover System VM networking in separate startup stages.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use arcbox_constants::container_network::ContainerNetwork;
use arcbox_constants::paths::HostLayout;
use arcbox_core::Runtime;

mod container_network;
mod docker_tools;

use container_network::recover_container_networking;
use docker_tools::ensure_docker_tools;

use crate::context::DaemonContext;
use crate::self_setup;
use crate::self_setup::SetupTask as _;

/// Publishes host DNS independently of System VM recovery.
pub async fn prepare_host(
    ctx: &DaemonContext,
    runtime: &Arc<Runtime>,
    dns_port: u16,
) -> Result<()> {
    #[cfg(not(target_os = "macos"))]
    let _ = (runtime, dns_port);
    // Best-effort self-setup (non-blocking).
    //
    let default_data_dir = HostLayout::resolve_for_profile_from_env(ctx.profile, None).data_dir;

    // Host resolver files are a privileged global mutation. Canonical
    // production DNS is implicit; any isolated domain requires the launch
    // contract to authorize it explicitly rather than inferring ownership
    // from caller-controlled text.
    let owns_dns = owns_dns_resolver(
        ctx.profile,
        ctx.install_dns_resolver,
        ctx.dns_domain.as_str(),
        &ctx.layout.data_dir,
        &default_data_dir,
    );
    if !owns_dns {
        tracing::debug!(
            domain = %ctx.dns_domain,
            data_dir = %ctx.layout.data_dir.display(),
            "non-canonical DNS owner; skipping /etc/resolver self-setup"
        );
    }
    #[cfg(target_os = "macos")]
    {
        crate::resolver_service::start(ctx, Arc::clone(runtime), dns_port, owns_dns)
            .await
            .context("explicit DNS resolver setup failed")?;
    }
    Ok(())
}

/// Recovers System VM networking and Docker host integrations.
///
/// - Recovers DNS and port forwarding for containers that survived a daemon restart
/// - Re-installs the container subnet route (cold-start reconcile)
/// - Installs the Docker socket via helper
///
/// Host integrations remain best-effort.
pub async fn run_system(ctx: &DaemonContext, runtime: &Arc<Runtime>) -> Result<()> {
    let linux_vm = runtime.config().vm.autostart;
    if linux_vm {
        if let Err(error) = recover_container_networking(runtime).await {
            tracing::warn!(%error, "container networking recovery failed");
        }
    }

    // Cold-start route reconcile (non-blocking). This is load-bearing after
    // app updates or daemon restarts where the VM survives but the host route
    // may have been removed or stolen by another network service.
    #[cfg(target_os = "macos")]
    if linux_vm {
        #[cfg(feature = "vmnet")]
        use arcbox_core::bridge_discovery::MachineBridgeExt as _;
        use arcbox_engine::vm_lifecycle::DEFAULT_MACHINE_NAME;
        #[cfg(feature = "vmnet")]
        let vmnet_bridge = runtime
            .machine_manager()
            .vmnet_bridge_name(DEFAULT_MACHINE_NAME);
        #[cfg(not(feature = "vmnet"))]
        let vmnet_bridge = None;
        let bridge_mac = runtime.machine_manager().bridge_mac(DEFAULT_MACHINE_NAME);
        match cold_start_route_plan(vmnet_bridge, bridge_mac) {
            #[cfg(feature = "vmnet")]
            Some(ColdStartRoutePlan::VmnetBridge(bridge)) => spawn_vmnet_route_reconcile(
                Arc::clone(&ctx.setup_state),
                bridge,
                runtime.config().container.cidr,
            ),
            Some(ColdStartRoutePlan::BridgeMac(mac)) => spawn_bridge_mac_route_reconcile(
                Arc::clone(&ctx.setup_state),
                mac,
                runtime.config().container.cidr,
            ),
            None => {}
        }
    }

    let default_data_dir = HostLayout::resolve_for_profile_from_env(ctx.profile, None).data_dir;

    // The `/var/run/docker.sock` symlink belongs to the production daemon
    // running its canonical data dir: the link target embeds the
    // data dir, so a daemon on an overridden one (an e2e harness daemon on
    // a temp dir, an ad-hoc dev run) would re-point the host-global Docker
    // socket at a path that dies with it. `ARCBOX_DATA_DIR` counts as
    // canonical — it relocates the machine-wide installation, not one
    // daemon instance; the installer persists it into the launchd plist's
    // EnvironmentVariables so a relocated install still satisfies this
    // check (its plist also passes the same path as `--data-dir`).
    let owns_global_host_setup =
        owns_implicit_host_setup(ctx.profile, &ctx.layout.data_dir, &default_data_dir);
    let socket_task = (linux_vm && owns_global_host_setup).then(|| self_setup::DockerSocket {
        target: ctx.layout.docker_socket.clone(),
    });
    if socket_task.is_none() {
        tracing::debug!(
            data_dir = %ctx.layout.data_dir.display(),
            "non-default data dir; skipping /var/run/docker.sock self-setup"
        );
    }

    // The `/etc/hosts` ArcBox alias is host-global like the docker socket:
    // only the production daemon on its canonical data dir installs it. It
    // exists so the ~/ArcBox NFS mount can use `ArcBox:/` as its source
    // (Finder shows the source host name); daemons without it simply mount
    // from `127.0.0.1:/`.
    let hosts_task =
        (linux_vm && ctx.mount_nfs && owns_global_host_setup).then_some(self_setup::HostsAlias);

    // Create /usr/local/bin/ symlinks for Docker CLI tools if running from
    // bundle with docker integration enabled.
    let cli_tasks: Vec<Box<dyn self_setup::SetupTask>> = if linux_vm && ctx.docker_integration {
        if let Some(contents) = crate::startup::find_bundle_contents() {
            let xbin = contents.join("MacOS/xbin");
            if xbin.is_dir() {
                vec![Box::new(self_setup::CliTools { xbin_dir: xbin })]
            } else {
                vec![]
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    let setup_state_self = Arc::clone(&ctx.setup_state);
    tokio::spawn(async move {
        let mut tasks: Vec<&dyn self_setup::SetupTask> = Vec::new();
        if let Some(socket) = &socket_task {
            tasks.push(socket);
        }
        if let Some(hosts) = &hosts_task {
            tasks.push(hosts);
        }
        for t in &cli_tasks {
            tasks.push(t.as_ref());
        }
        if tasks.is_empty() {
            // Nothing to configure — don't launchd-activate the helper
            // just to hang up (the common case for harness daemons).
            return;
        }
        self_setup::run(&tasks).await;
        if let Some(socket) = &socket_task {
            setup_state_self.set_docker_socket_linked(socket.is_satisfied());
        }
    });

    // Docker CLI tools installation (non-blocking, best-effort).
    if linux_vm && ctx.docker_integration {
        let data_dir = ctx.layout.data_dir.clone();
        let setup_state = Arc::clone(&ctx.setup_state);
        tokio::spawn(async move {
            match ensure_docker_tools(&data_dir).await {
                Ok(()) => setup_state.set_docker_tools_installed(true),
                Err(e) => {
                    tracing::warn!(error = %e, "Docker tools setup failed");
                }
            }
        });
    }
    Ok(())
}

fn owns_dns_resolver(
    profile: arcbox_constants::paths::ArcboxProfile,
    explicitly_enabled: bool,
    domain: &str,
    data_dir: &Path,
    canonical_data_dir: &Path,
) -> bool {
    explicitly_enabled
        || (profile == arcbox_constants::paths::ArcboxProfile::Production
            && domain == crate::startup::DEFAULT_DNS_DOMAIN
            && data_dir == canonical_data_dir)
}

pub fn owns_implicit_host_setup(
    profile: arcbox_constants::paths::ArcboxProfile,
    data_dir: &Path,
    canonical_data_dir: &Path,
) -> bool {
    profile == arcbox_constants::paths::ArcboxProfile::Production && data_dir == canonical_data_dir
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum ColdStartRoutePlan {
    #[cfg(feature = "vmnet")]
    VmnetBridge(String),
    BridgeMac(String),
}

#[cfg(target_os = "macos")]
fn cold_start_route_plan(
    vmnet_bridge: Option<String>,
    bridge_mac: Option<String>,
) -> Option<ColdStartRoutePlan> {
    #[cfg(feature = "vmnet")]
    if let Some(bridge) = vmnet_bridge {
        return Some(ColdStartRoutePlan::VmnetBridge(bridge));
    }
    #[cfg(not(feature = "vmnet"))]
    let _ = vmnet_bridge;
    bridge_mac.map(ColdStartRoutePlan::BridgeMac)
}

#[cfg(all(target_os = "macos", feature = "vmnet"))]
fn spawn_vmnet_route_reconcile(
    setup_state: Arc<arcbox_api::SetupState>,
    bridge: String,
    container_network: ContainerNetwork,
) {
    drop(tokio::spawn(async move {
        match arcbox_core::route_reconciler::ensure_route_for_bridge_with_network(
            &bridge,
            container_network,
        )
        .await
        {
            Ok(()) => setup_state.set_route_installed(true),
            Err(e) => {
                tracing::warn!(error = %e, "failed to install container route on cold start (vmnet)");
            }
        }
    }));
}

#[cfg(target_os = "macos")]
fn spawn_bridge_mac_route_reconcile(
    setup_state: Arc<arcbox_api::SetupState>,
    mac: String,
    container_network: ContainerNetwork,
) {
    drop(tokio::spawn(async move {
        match arcbox_core::route_reconciler::ensure_route_with_retry_for_network(
            &mac,
            container_network,
        )
        .await
        {
            Ok(()) => setup_state.set_route_installed(true),
            Err(e) => {
                tracing::warn!(error = %e, "failed to install container route on cold start");
            }
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_resolver_owner_isolated_by_domain_or_canonical_data_dir() {
        let canonical = Path::new("/canonical");

        assert!(owns_dns_resolver(
            arcbox_constants::paths::ArcboxProfile::Production,
            false,
            crate::startup::DEFAULT_DNS_DOMAIN,
            canonical,
            canonical
        ));
        assert!(!owns_dns_resolver(
            arcbox_constants::paths::ArcboxProfile::Production,
            false,
            crate::startup::DEFAULT_DNS_DOMAIN,
            Path::new("/isolated-test"),
            canonical
        ));
        assert!(!owns_dns_resolver(
            arcbox_constants::paths::ArcboxProfile::Production,
            false,
            "ad-hoc.dev.arcbox.local",
            Path::new("/isolated-test"),
            canonical
        ));
        assert!(owns_dns_resolver(
            arcbox_constants::paths::ArcboxProfile::Development,
            true,
            "dev-id.dev.arcbox.local",
            Path::new("/isolated-dev"),
            canonical
        ));
        assert!(!owns_dns_resolver(
            arcbox_constants::paths::ArcboxProfile::Development,
            false,
            crate::startup::DEFAULT_DNS_DOMAIN,
            canonical,
            canonical
        ));
    }

    #[test]
    fn implicit_host_setup_belongs_only_to_canonical_production() {
        let canonical = Path::new("/canonical");

        assert!(owns_implicit_host_setup(
            arcbox_constants::paths::ArcboxProfile::Production,
            canonical,
            canonical,
        ));
        assert!(!owns_implicit_host_setup(
            arcbox_constants::paths::ArcboxProfile::Production,
            Path::new("/isolated"),
            canonical,
        ));
        assert!(!owns_implicit_host_setup(
            arcbox_constants::paths::ArcboxProfile::Development,
            canonical,
            canonical,
        ));
    }

    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    #[test]
    fn cold_start_route_plan_uses_vmnet_bridge() {
        let plan = cold_start_route_plan(Some("bridge100".to_string()), None);

        assert_eq!(
            plan,
            Some(ColdStartRoutePlan::VmnetBridge("bridge100".to_string()))
        );
    }

    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    #[test]
    fn cold_start_route_plan_falls_back_to_bridge_mac() {
        let plan = cold_start_route_plan(None, Some("fe:b2:14:4d:a4:64".to_string()));

        assert_eq!(
            plan,
            Some(ColdStartRoutePlan::BridgeMac(
                "fe:b2:14:4d:a4:64".to_string()
            ))
        );
    }

    #[cfg(all(target_os = "macos", not(feature = "vmnet")))]
    #[test]
    fn cold_start_route_plan_uses_bridge_mac_without_vmnet() {
        let plan = cold_start_route_plan(Some("bridge100".to_string()), Some("mac".to_string()));

        assert_eq!(plan, Some(ColdStartRoutePlan::BridgeMac("mac".to_string())));
    }
}
