//! `arcbox-host-sandbox` — the host sandbox location (ADR 0011).
//!
//! Composes the Sandbox API (`arcbox-sandbox-service`) over a
//! `ComputerManager` whose computers are VMs on the Mac itself, next to the
//! System VM rather than nested inside it:
//!
//! - the VMM: [`Backend::Hv`] (the default, D4) runs each VM in its own
//!   `arcbox-hv-vmm` process; [`Backend::Vz`] runs them in the daemon and
//!   checkpoints only while the console session is unlocked;
//! - the network: `arcbox-host-net`, one userspace datapath per VM, run by
//!   the VMM process under HV and by this process under VZ;
//! - disks: `clonefile` copies of the template image (`CowBackend::Clonefile`);
//! - checkpoints bound to the macOS build ([`os_build`], D6).
//!
//! The daemon reaches the service the way it reaches the System VM's
//! agent: an [`AgentClient`] speaking the agent frame protocol, here over
//! an in-process socket pair ([`HostSandboxes::connect`]). The crate is
//! empty on other platforms.

#![cfg(target_os = "macos")]

mod hooks;

use std::os::fd::IntoRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use arcbox_computer_runtime::agent::{Readiness, VmProtoAgentFactory};
use arcbox_computer_runtime::workload::vm_agent_path;
use arcbox_computer_runtime::{
    CheckpointGate, ComputerCapabilities, ComputerManager, NodeEnvironment, Placement,
    RootfsBuilder, RootfsPaths, RuntimeConfig, VM_AGENT_PATH,
};
use arcbox_constants::dns::Domain;
use arcbox_engine::agent_client::AgentClient;
use arcbox_host_net::{DatapathPlacement, DatapathServices, HostNetwork, NoDns, Pool};
use arcbox_hv_driver::HvDriver;
use arcbox_sandbox_service::{SandboxService, dispatch};
use arcbox_snapshot::snapshot_cow::{BusyboxBlockTools, CowBackend, CowManager, CowOptions};
use arcbox_vm_driver::VmDriver;
use arcbox_vz_driver::VzDriver;

/// The data directory's subdirectory the host location owns: computer
/// records, checkpoints, clones and rootfs images.
const SANDBOX_DIR: &str = "sandbox";

/// The pool each lease's address comes from. The address names the VM on
/// the host and is what the API reports, but nothing routes it yet: every
/// guest sits on its own `10.0.2.2/24` link behind its own datapath
/// (`arcbox-host-net`). So no DNS name is published for it, and
/// `<id>.sandbox.arcbox.local` answers NODATA like any unknown local name,
/// until the pool is routed through a daemon utun (decision (c)) and the
/// name can point here.
const POOL: ([u8; 4], u8) = ([10, 200, 0, 0], 16);

/// Kernel command line of a host computer. The rootfs is the first and
/// only disk (`/dev/vda`); VZ and HV both put the console on `hvc0`. The
/// runtime appends the `ip=` the network's identity asks for.
fn boot_args() -> String {
    format!("console=hvc0 root=/dev/vda rw rootwait reboot=k panic=1 init={VM_AGENT_PATH}")
}

/// The vsock CID an [`AgentClient`] of this service reports. There is no
/// VM behind the connection; the client only uses it in diagnostics.
const SERVICE_CID: u32 = 0;

/// How long a daemon shutdown waits for the host computers to be handed
/// over before it gives up on them.
const DETACH_BUDGET: Duration = Duration::from_secs(20);

/// The hypervisor the host location runs its computers under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Hypervisor.framework, one `arcbox-hv-vmm` process per computer. The
    /// default (D4): its checkpoints work while the session is locked and
    /// the format is ArcBox's.
    #[default]
    Hv,
    /// Virtualization.framework, in the daemon's process. Checkpoints need
    /// an unlocked console session (ADR 0011, decision 6).
    Vz,
}

impl Backend {
    /// Parses a backend name, `hv` or `vz`.
    ///
    /// # Errors
    /// Names the accepted values for anything else.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "hv" => Ok(Self::Hv),
            "vz" => Ok(Self::Vz),
            other => anyhow::bail!("unknown host sandbox backend {other:?}; expected hv or vz"),
        }
    }
}

/// What the daemon supplies to compose the host location.
pub struct HostSandboxOptions {
    /// The daemon's data directory. The location keeps its state under
    /// `sandbox/` there, and finds `bin/vm-agent` and `bin/busybox` next to
    /// the guest agent.
    pub data_dir: PathBuf,
    /// The hypervisor.
    pub backend: Backend,
    /// The kernel computers boot: the boot assets' kernel, which runs under
    /// both VZ and HV.
    pub kernel: PathBuf,
    /// The signed `arcbox-hv-vmm` binary [`Backend::Hv`] spawns.
    pub hv_vmm: PathBuf,
    /// The DNS domain sandbox names are formed under.
    pub domain: Domain,
    /// The Docker Engine socket that reaches the System VM's dockerd, for
    /// `docker:<ref>` images no registry has and for Dockerfile templates
    /// (D5).
    pub docker_socket: PathBuf,
    /// Whether the System VM's dockerd can answer right now.
    pub system_docker_ready: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// The host location, composed.
pub struct HostSandboxes {
    service: Arc<SandboxService>,
}

impl HostSandboxes {
    /// Composes the location and starts its manager's background work (the
    /// startup sweep that adopts HV computers a previous daemon left
    /// running, the idle and TTL timers). Needs a tokio runtime.
    ///
    /// # Errors
    /// Fails when the macOS build cannot be read, the data directory
    /// cannot be created, or the manager refuses the composition.
    pub fn start(options: HostSandboxOptions) -> anyhow::Result<Self> {
        let os_build = os_build()?;
        let root = options.data_dir.join(SANDBOX_DIR);
        let images = root.join("images");
        std::fs::create_dir_all(&images).with_context(|| format!("create {}", images.display()))?;

        let pool = Pool::new(POOL.0.into(), POOL.1)?;
        let mut placement = Placement::host(os_build);
        let (driver, datapath): (Arc<dyn VmDriver>, DatapathPlacement) = match options.backend {
            Backend::Hv => (
                Arc::new(HvDriver::new(options.hv_vmm.clone(), true)),
                DatapathPlacement::Driver,
            ),
            Backend::Vz => {
                let vz = Arc::new(VzDriver::new(None, None));
                placement = placement
                    .with_checkpoint_gate(Arc::new(SessionGate(Arc::clone(&vz))))
                    .with_network_port_forward();
                (vz, DatapathPlacement::Network)
            }
        };
        let network =
            HostNetwork::new(pool, datapath, DatapathServices::default(), Arc::new(NoDns));

        let mut cow = CowOptions::new(&root);
        cow.backend = CowBackend::Clonefile;
        let environment = NodeEnvironment {
            driver,
            network: Arc::new(network),
            // Neither host driver listens for guest vsock dial-outs, and an
            // HV dial to a port with no listener reads EOF rather than
            // refusing, so readiness is an agent round trip, retried.
            agent: Arc::new(VmProtoAgentFactory::with_readiness(Readiness::Probe)),
            cow_manager: Arc::new(CowManager::new(cow)?),
        };
        let config = runtime_config(&root, &options.kernel);
        let default_rootfs = config.defaults.rootfs.clone();
        let manager = ComputerManager::new_at(config, environment, placement)
            .map_err(|error| anyhow::anyhow!("host sandbox manager: {error}"))?
            .into_shared();

        let rootfs = RootfsBuilder::new(
            RootfsPaths {
                vm_agent: vm_agent_path(&options.data_dir),
                cache_dir: images,
                busybox: options.data_dir.join("bin/busybox"),
            },
            // Unused on macOS: the agent is written into the image, never
            // loop-mounted.
            Arc::new(BusyboxBlockTools::default()),
        );
        let hooks = hooks::HostHooks::new(&options, root.join("layouts"));
        let service = SandboxService::new(manager, rootfs, default_rootfs, Arc::new(hooks));
        tracing::info!(backend = ?options.backend, root = %root.display(), "host sandboxes ready");
        Ok(Self { service })
    }

    /// A client of this location's Sandbox API: the agent frame protocol
    /// over an in-process socket pair, served until the client hangs up.
    ///
    /// # Errors
    /// Fails when the socket pair cannot be made.
    pub fn connect(&self) -> arcbox_engine::Result<AgentClient> {
        let io = |error: std::io::Error| {
            arcbox_engine::EngineError::Machine(format!("host sandbox connection: {error}"))
        };
        let (client, server) = std::os::unix::net::UnixStream::pair().map_err(io)?;
        server.set_nonblocking(true).map_err(io)?;
        let server = tokio::net::UnixStream::from_std(server).map_err(io)?;
        let service = Arc::clone(&self.service);
        // The connection lives as long as its client: `serve` returns when
        // the client end closes, which every request path does on drop.
        tokio::spawn(async move {
            if let Err(error) = dispatch::serve(&service, server).await {
                tracing::debug!(%error, "host sandbox connection ended");
            }
        });
        AgentClient::from_fd_async(SERVICE_CID, client.into_raw_fd())
    }

    /// Whether `id` names a computer of this location.
    #[must_use]
    pub fn owns_sandbox(&self, id: &str) -> bool {
        !id.is_empty()
            && self
                .service
                .manager()
                .inspect_computer(&id.to_owned())
                .is_ok()
    }

    /// Whether `id` names a checkpoint of this location.
    #[must_use]
    pub fn owns_snapshot(&self, id: &str) -> bool {
        self.service
            .manager()
            .list_checkpoints(None)
            .is_ok_and(|all| all.iter().any(|checkpoint| checkpoint.id == id))
    }

    /// What this location can do right now.
    #[must_use]
    pub fn capabilities(&self) -> ComputerCapabilities {
        self.service.manager().capabilities()
    }

    /// Hands the running computers over before the daemon exits: HV VMM
    /// processes keep running for the next daemon to adopt; VZ VMs end with
    /// this process.
    pub async fn shutdown(&self) {
        match tokio::time::timeout(DETACH_BUDGET, self.service.manager().detach_all()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "host sandboxes: detach failed"),
            Err(_) => tracing::warn!("host sandboxes: detach did not finish in time"),
        }
    }
}

/// The runtime configuration of the host location under `root`.
fn runtime_config(root: &Path, kernel: &Path) -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.firecracker.data_dir = root.to_string_lossy().into_owned();
    config.defaults.kernel = kernel.to_string_lossy().into_owned();
    config.defaults.rootfs = root
        .join("images/default.ext4")
        .to_string_lossy()
        .into_owned();
    config.defaults.boot_args = boot_args();
    config
}

/// A VZ save needs an unlocked console session; the gate the manager asks
/// before every capture, idle auto-pause included.
struct SessionGate(Arc<VzDriver>);

impl CheckpointGate for SessionGate {
    fn checkpoint_available(&self) -> Result<(), String> {
        match self.0.checkpoint_blocker() {
            None => Ok(()),
            Some(session) => Err(format!("VZ cannot save a checkpoint now: {session}")),
        }
    }
}

/// This Mac's macOS build (`kern.osversion`), e.g. `25F71`: what a host
/// checkpoint is bound to (D6).
///
/// # Errors
/// Fails when the sysctl cannot be read.
pub fn os_build() -> anyhow::Result<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `buf` is valid for `len` bytes, and `len` is updated in place
    // with the bytes written; the name is a NUL-terminated C string.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osversion".as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("sysctl kern.osversion");
    }
    let build = std::ffi::CStr::from_bytes_until_nul(&buf[..len])
        .context("kern.osversion is not a C string")?
        .to_str()
        .context("kern.osversion is not UTF-8")?;
    Ok(build.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_parse_and_others_are_refused() {
        assert_eq!(Backend::parse("hv").unwrap(), Backend::Hv);
        assert_eq!(Backend::parse(" VZ ").unwrap(), Backend::Vz);
        assert!(Backend::parse("fc").is_err());
        assert_eq!(Backend::default(), Backend::Hv);
    }

    #[test]
    fn the_os_build_is_this_macs() {
        let build = os_build().unwrap();
        assert!(!build.is_empty() && build.is_ascii(), "{build:?}");
    }

    #[test]
    fn computers_boot_the_agent_from_the_root_disk() {
        let config = runtime_config(Path::new("/d/sandbox"), Path::new("/boot/kernel"));
        assert_eq!(config.defaults.kernel, "/boot/kernel");
        assert_eq!(config.defaults.rootfs, "/d/sandbox/images/default.ext4");
        assert!(config.defaults.boot_args.contains("root=/dev/vda"));
        assert!(config.defaults.boot_args.contains("init=/sbin/vm-agent"));
        assert!(config.firecracker.jailer.is_none());
    }
}
