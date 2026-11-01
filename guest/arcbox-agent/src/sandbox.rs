//! The nested sandbox location: the Sandbox API over a [`ComputerManager`]
//! of Firecracker microVMs inside the System VM.
//!
//! The API itself is `arcbox-sandbox-service`; this module composes it for
//! the System VM — the Firecracker driver, the TAP network, the busybox
//! block tooling — and supplies its [`SandboxHooks`]: names go to the
//! guest's own DNS server, `docker:<ref>` images come from the guest's own
//! dockerd, and ports are forwarded with guest DNAT.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use arcbox_computer_runtime::agent::VmProtoAgentFactory;
use arcbox_computer_runtime::{ComputerManager, NodeEnvironment, RootfsBuilder, RootfsPaths};
use arcbox_connect::v1::{SandboxCleanupTicket, SandboxPortProtocol};
use arcbox_constants::dns::Domain;
use arcbox_fc_driver::{FcDriver, FcDriverConfig};
use arcbox_sandbox_service::docker::DockerEngine;
use arcbox_sandbox_service::{ImageOrigin, SandboxError, SandboxHooks};
use arcbox_snapshot::snapshot_cow::{BlockTools, BusyboxBlockTools, CowManager, CowOptions};
use arcbox_tap_net::{IptablesLegacy, TapNetwork};
use async_trait::async_trait;
use tokio::sync::Mutex;

pub use arcbox_sandbox_service::SandboxService;

use crate::config::GuestConfig;
use port_forward::{PortForwardManager, Protocol};

mod port_forward;

/// Verify the nested-virtualization prerequisite for sandboxes.
///
/// Firecracker needs `/dev/kvm`, which exists in this guest only when the
/// host enabled nested virtualization (VZ backend on Apple Silicon M3+ with
/// macOS 15+). Returns an actionable reason when the prerequisite is missing
/// so `Create` can fail fast with `FAILED_PRECONDITION` instead of an opaque
/// asynchronous boot failure.
pub fn probe_kvm() -> Result<(), String> {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
            "sandboxes require nested virtualization (/dev/kvm is missing in the guest): \
             use the VZ backend on Apple Silicon M3 or newer with macOS 15+; \
             the HV backend and Intel/M1/M2 hosts cannot run sandboxes"
                .into(),
        ),
        Err(e) => Err(format!(
            "/dev/kvm exists but cannot be opened ({e}); sandboxes are unavailable"
        )),
    }
}

/// The `vm-agent` binary as the host daemon stages it (host `~/.arcbox/bin`
/// via VirtioFS).
const VM_AGENT_BIN: &str = "/arcbox/bin/vm-agent";

/// Static busybox shipped in the guest EROFS rootfs.
const GUEST_BUSYBOX: &str = "/bin/busybox";

/// Directory for generated rootfs images (btrfs data volume, writable).
const ROOTFS_CACHE_DIR: &str = "/var/lib/arcbox/sandbox";

/// Staging root for image layouts exported out of the guest's dockerd.
const LAYOUT_STAGING_DIR: &str = "/var/lib/arcbox/sandbox/templates";

/// The System VM's loop-device and block-size tooling: the busybox its
/// EROFS rootfs ships, at [`BusyboxBlockTools::DEFAULT_PATH`].
///
/// One value per composition, shared by the copy-on-write rootfs manager in
/// [`node_environment`] and by [`rootfs_builder`], so both mount through the
/// same busybox.
pub fn block_tools() -> Arc<dyn BlockTools> {
    Arc::new(BusyboxBlockTools::default())
}

/// The rootfs builder over the System VM's layout. Standalone so the
/// startup sweep of half-written images can run whether or not the sandbox
/// service comes up.
pub fn rootfs_builder(block_tools: Arc<dyn BlockTools>) -> RootfsBuilder {
    RootfsBuilder::new(
        RootfsPaths {
            vm_agent: VM_AGENT_BIN.into(),
            cache_dir: ROOTFS_CACHE_DIR.into(),
            busybox: GUEST_BUSYBOX.into(),
        },
        block_tools,
    )
}

/// The environment-specific components the sandbox stack runs on inside
/// the System VM. This is where they are built: `ComputerManager::new`
/// builds none of them, so the choice of VMM is made here.
///
/// Four components, out of the two halves of `config`:
///
/// - the Firecracker driver over the adapter half's binaries and
///   process-level flags ([`FcDriverConfig`]);
/// - the Linux TAP network over the runtime half's `[network]` pool, with
///   its quarantine ledger under the data dir, the adapter half's
///   datapath, and iptables-legacy for the netfilter rendering of the
///   invariant translation — which is the `Filter` datapath itself and
///   what the `Ebpf` one falls back to;
/// - the `arcbox-vm-proto` guest-agent client, which every Firecracker
///   sandbox speaks;
/// - the copy-on-write rootfs manager over the data dir, `block_tools`, and
///   the runtime half's `dmsetup` search list.
pub fn node_environment(
    config: &GuestConfig,
    block_tools: Arc<dyn BlockTools>,
) -> anyhow::Result<NodeEnvironment> {
    let runtime = &config.runtime;
    let data_dir = std::path::Path::new(&runtime.firecracker.data_dir);
    let network = TapNetwork::with_quarantine_dir(
        &runtime.network.cidr,
        &runtime.network.gateway,
        runtime.network.dns.clone(),
        data_dir.join("sandbox-network-quarantine"),
        config.adapters.sandbox_datapath,
        Arc::new(IptablesLegacy::default()),
    )?;
    let mut cow_options = CowOptions::new(data_dir);
    cow_options.block_tools = block_tools;
    if let Some(candidates) = &runtime.firecracker.dmsetup_candidates {
        cow_options.dmsetup_candidates = candidates.iter().map(std::path::PathBuf::from).collect();
    }
    Ok(NodeEnvironment {
        driver: Arc::new(FcDriver::new(FcDriverConfig::from(&config.adapters))),
        network: Arc::new(network),
        agent: Arc::new(VmProtoAgentFactory::default()),
        cow_manager: Arc::new(CowManager::new(cow_options)?),
    })
}

/// The nested location's Sandbox API over `config`, publishing names in
/// `dns`.
pub fn new_service(
    config: GuestConfig,
    dns: Arc<crate::dns_server::GuestDnsServer>,
) -> anyhow::Result<Arc<SandboxService>> {
    let default_rootfs = config.runtime.defaults.rootfs.clone();
    // The rootfs builder shares the environment's block tooling so both
    // mount through the same busybox.
    let block_tools = block_tools();
    let rootfs = rootfs_builder(Arc::clone(&block_tools));
    let environment = node_environment(&config, block_tools)?;
    // `into_shared` starts the lifecycle monitor driving the idle/TTL
    // expiry timers (CORE-21/60).
    let manager = ComputerManager::new(config.runtime, environment)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into_shared();
    Ok(SandboxService::new(
        manager,
        rootfs,
        default_rootfs,
        Arc::new(GuestHooks {
            dns,
            docker: DockerEngine::new(arcbox_constants::paths::DOCKER_API_UNIX_SOCKET),
        }),
    ))
}

/// The System VM's half of the Sandbox API.
struct GuestHooks {
    dns: Arc<crate::dns_server::GuestDnsServer>,
    docker: DockerEngine,
}

/// Returns the global [`PortForwardManager`] singleton.
fn port_forwards() -> &'static Mutex<PortForwardManager> {
    static MANAGER: OnceLock<Mutex<PortForwardManager>> = OnceLock::new();
    MANAGER.get_or_init(|| Mutex::new(PortForwardManager::default()))
}

/// Map the host↔guest wire protocol enum onto the forwarder's protocol
/// (`UNSPECIFIED` defaults to TCP).
///
/// This is the vsock payload's own enum, not the published sandbox
/// contract's — the two are deliberately separate (CORE-57).
fn wire_protocol(protocol: SandboxPortProtocol) -> Protocol {
    match protocol {
        SandboxPortProtocol::Udp => Protocol::Udp,
        _ => Protocol::Tcp,
    }
}

#[async_trait]
impl SandboxHooks for GuestHooks {
    fn dns_domain(&self) -> &Domain {
        self.dns.domain()
    }

    fn register_dns(&self, id: &str, ip: &str) -> anyhow::Result<()> {
        self.dns.register_sandbox(id, ip)
    }

    fn deregister_dns(&self, id: &str) {
        self.dns.deregister_sandbox(id);
    }

    /// Exported from the guest's own dockerd and converted inside the VM:
    /// the System VM pulls images itself, so a reference that is not in
    /// dockerd is an error here rather than a registry pull.
    async fn image_rootfs(
        &self,
        image: &str,
        _origin: ImageOrigin,
        rootfs: &RootfsBuilder,
        pinned: &BTreeSet<PathBuf>,
    ) -> anyhow::Result<String> {
        let layout = self
            .docker
            .export_layout(image, std::path::Path::new(LAYOUT_STAGING_DIR))
            .await?;
        Ok(rootfs.convert_layer_to_rootfs(&layout, pinned).await?)
    }

    async fn build_image(&self, dockerfile: &[u8], tag: &str) -> anyhow::Result<()> {
        if self.docker.inspect_image(tag).await.is_ok() {
            tracing::info!(%tag, "reusing previously built dockerfile image");
            return Ok(());
        }
        self.docker.build_image(dockerfile, tag).await
    }

    async fn forward_port(
        &self,
        service: &SandboxService,
        id: &str,
        sandbox_port: u16,
        protocol: SandboxPortProtocol,
    ) -> Result<u16, SandboxError> {
        let identity = service.sandbox_network_identity(id)?;
        port_forwards()
            .lock()
            .await
            .forward(id, &identity, sandbox_port, wire_protocol(protocol))
            .await
            .map_err(|e| SandboxError::Internal(e.to_string()))
    }

    async fn remove_port(
        &self,
        service: &SandboxService,
        id: &str,
        sandbox_port: u16,
        protocol: SandboxPortProtocol,
    ) -> Result<(), SandboxError> {
        let identity = service.sandbox_network_identity(id)?;
        port_forwards()
            .lock()
            .await
            .remove(
                id,
                &identity.cleanup_token,
                sandbox_port,
                wire_protocol(protocol),
            )
            .await
            .map_err(|e| SandboxError::Internal(e.to_string()))
    }

    async fn release_forwards(
        &self,
        ticket: &SandboxCleanupTicket,
        ip: std::net::Ipv4Addr,
    ) -> Result<(), SandboxError> {
        let internal = |error: anyhow::Error| SandboxError::Internal(error.to_string());
        if ticket.startup {
            return port_forward::remove_legacy_orphan_rules()
                .await
                .map_err(internal);
        }
        port_forwards()
            .lock()
            .await
            .remove_all_for(&ticket.id, &ticket.token)
            .await
            .map_err(internal)?;
        port_forward::remove_orphan_rules_for(ip, &ticket.token)
            .await
            .map_err(internal)
    }
}
