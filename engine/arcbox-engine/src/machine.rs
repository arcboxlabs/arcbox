//! Linux machine management.
//!
//! A "machine" is a high-level abstraction over a VM that provides
//! a Linux environment for running containers.

use crate::error::{EngineError, Result};
use crate::persistence::MachinePersistence;
use crate::vm::{HostNetwork, SharedDirConfig, VmConfig, VmId, VmManager};
use arcbox_connect::v1::{EnsureMachineExportRequest, EnsureMachineExportResponse};
// Only the macOS `connect_agent` dials the agent port — the vsock helper it
// rides is macOS-only.
#[cfg(target_os = "macos")]
use arcbox_constants::ports::AGENT_PORT;
use arcbox_constants::virtiofs::{MOUNT_PRIVATE, MOUNT_USERS, TAG_ARCBOX, TAG_PRIVATE, TAG_USERS};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
#[cfg(target_os = "macos")]
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Default machine name used for container operations.
pub const DEFAULT_MACHINE_NAME: &str = "default";

/// Machine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineState {
    /// Machine created but not started.
    Created,
    /// Machine is starting.
    Starting,
    /// Machine is running.
    Running,
    /// Machine is stopping.
    Stopping,
    /// Machine is stopped.
    Stopped,
}

pub mod archive;
mod clone;
mod host_hold;
#[cfg(target_os = "macos")]
mod serial;
#[cfg(test)]
mod tests;
mod transfer;

pub use clone::clone_file;
pub use host_hold::HostHold;
use transfer::DataDisk;

/// How long a stop waits for the host to release what it holds of the
/// machine before touching the VM. The release takes well under a second
/// while the machine still answers; the bound covers a holder that is
/// busy with another machine.
const HOST_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Machine information.
#[derive(Debug, Clone)]
pub struct MachineInfo {
    /// Machine name.
    pub name: String,
    /// Machine state.
    pub state: MachineState,
    /// Underlying VM ID.
    pub vm_id: VmId,
    /// vsock CID for agent communication (assigned when VM starts).
    pub cid: Option<u32>,
    /// Number of CPUs.
    pub cpus: u32,
    /// Memory in MB.
    pub memory_mb: u64,
    /// Disk size in GB.
    pub disk_gb: u64,
    /// Kernel path.
    pub kernel: Option<String>,
    /// Kernel command line.
    pub cmdline: Option<String>,
    /// Block devices (e.g., EROFS rootfs image).
    pub block_devices: Vec<crate::vm::BlockDeviceConfig>,
    /// Distribution name (e.g., "alpine", "ubuntu").
    pub distro: Option<String>,
    /// Distribution version (e.g., "3.21", "24.04").
    pub distro_version: Option<String>,
    /// Path to the disk image.
    pub disk_path: Option<PathBuf>,
    /// Path to the SSH private key.
    pub ssh_key_path: Option<PathBuf>,
    /// Guest IP address (reported by agent via vsock).
    pub ip_address: Option<String>,
    /// Address of the guest's bridge NIC, the one the Mac reaches directly
    /// and `<name>.arcbox.local` resolves to. Reported by the agent at
    /// readiness; `None` while stopped or when the guest has no bridge NIC.
    pub bridge_ip_address: Option<String>,
    /// macOS hypervisor backend this machine boots on.
    pub backend: arcbox_vmm::VmBackend,
    /// Whether the guest may run its own hypervisor.
    pub nested_virt: bool,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last successful start time.
    pub started_at: Option<DateTime<Utc>>,
    /// Host directories shared into the machine (shim machines).
    pub mounts: Vec<MachineMount>,
}

/// The outcome of [`MachineManager::set_resources`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineResize {
    /// vCPUs the machine boots with from now on.
    pub cpus: u32,
    /// Memory the machine boots with from now on, in MiB.
    pub memory_mb: u64,
    /// The machine is running with its previous size; the new one applies
    /// when it is next started.
    pub restart_required: bool,
}

/// A pulled distro rootfs image a machine boots from.
#[derive(Debug, Clone)]
pub struct MachineRootfs {
    /// Host path of the rootfs image (from the machine image registry).
    pub path: PathBuf,
    /// Image format (`squashfs`), used for the kernel `rootfstype=`.
    pub format: String,
    /// Boot shim staging the rootfs (see
    /// `../company/engineering/arcbox/plans/machine-boot-shim.md`). When set, devices are
    /// vda=shim EROFS / vdb=rootfs / vdc=data and the kernel command line
    /// follows the machine-init contract; when `None`, the rootfs itself
    /// boots as vda (custom-kernel testing).
    pub shim: Option<BootShim>,
}

/// The boot-assets artifacts that stage a distro machine's boot.
#[derive(Debug, Clone)]
pub struct BootShim {
    /// Boot-assets kernel image path.
    pub kernel: PathBuf,
    /// Boot-assets EROFS rootfs path (ships `/sbin/arcbox-machine-init`).
    pub rootfs: PathBuf,
}

/// A host directory mounted into a machine (per-machine VirtioFS share).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MachineMount {
    /// Host directory to share.
    pub host_path: String,
    /// Guest mount point (absolute; must not contain `,` or `=`, which the
    /// cmdline mount table uses as separators).
    pub guest_path: String,
    /// Whether the share is read-only.
    pub read_only: bool,
}

/// Machine configuration.
#[derive(Debug, Clone)]
pub struct MachineConfig {
    /// Machine name.
    pub name: String,
    /// Number of CPUs.
    pub cpus: u32,
    /// Memory in MB.
    pub memory_mb: u64,
    /// Disk size in GB.
    pub disk_gb: u64,
    /// Kernel path.
    pub kernel: Option<String>,
    /// Kernel command line.
    pub cmdline: Option<String>,
    /// Block devices (e.g., EROFS rootfs image).
    pub block_devices: Vec<crate::vm::BlockDeviceConfig>,
    /// Pulled distro rootfs image to boot from. When set, `create` attaches
    /// it read-only as the first block device (vda), provisions a sparse
    /// per-machine data disk after it, and defaults the kernel command line
    /// to mount it as root.
    pub rootfs: Option<MachineRootfs>,
    /// Host directories mounted into the machine as per-machine VirtioFS
    /// shares. Honored on shim machines (the shim mounts them into the new
    /// root); ignored elsewhere.
    pub mounts: Vec<MachineMount>,
    /// Distribution name (e.g., "alpine", "ubuntu").
    pub distro: Option<String>,
    /// Distribution version (e.g., "3.21", "24.04").
    pub distro_version: Option<String>,
    /// macOS hypervisor backend for this machine.
    ///
    /// `Vz` (default) runs Apple's Virtualization.framework managed execution
    /// (required for Rosetta); `Hv` runs ArcBox's custom HV-framework VMM.
    pub backend: arcbox_vmm::VmBackend,
    /// Whether to expose Apple Rosetta inside the guest for `linux/amd64`
    /// translation.
    ///
    /// Only honored when [`Self::backend`] is `Vz`; the HV path silently
    /// drops it because Hypervisor.framework does not host the Rosetta
    /// share. Defaults to `false`.
    pub enable_rosetta: bool,
    /// Whether the guest may run its own hypervisor (sandboxes).
    ///
    /// Only the System VM sets this. Hypervisor.framework has about a dozen
    /// nested-capable address spaces per host, and a machine that pins one
    /// for nothing eventually makes every later VM start fail.
    pub nested_virt: bool,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            cpus: arcbox_hypervisor::default_vm_cpu_count(),
            memory_mb: 4096,
            disk_gb: 50,
            kernel: None,
            cmdline: None,
            block_devices: Vec::new(),
            rootfs: None,
            mounts: Vec::new(),
            distro: None,
            distro_version: None,
            backend: arcbox_vmm::VmBackend::default(),
            enable_rosetta: false,
            nested_virt: false,
        }
    }
}

/// Console device for the host architecture, following the boot-assets
/// convention.
const fn boot_console() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "ttyS0"
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "hvc0"
    }
}

/// Keeps the kernel's `eth0` name for the machine's NIC.
///
/// The mirrored images are the linuxcontainers `default` variant, whose
/// network config — networkd's `eth0.network`, netplan, `ifcfg-eth0`,
/// ifupdown — is written for the name a container's NIC has. In a VM, udev's
/// predictable naming renames the virtio NIC to `enp0s1`, that config then
/// matches nothing, and the distro never takes over DHCP, the default route
/// or its resolver's upstream from what the boot shim set up before init.
const KEEP_KERNEL_NIC_NAMES: &str = "net.ifnames=0";

/// Caps the kernel console at `err` and above, matching the
/// `console_loglevel` Ubuntu's image already sets in `sysctl.d`.
///
/// `console=hvc0` (above) routes every kernel `printk` to the host pipe. A
/// distro whose kernel default is the noisier `7` — Debian is the notable
/// one — then streams `info`-level records there forever: an idle machine's
/// systemd unit churn alone emits an `audit:` line on `hvc0` every unit
/// start/stop (~285 KiB/day, measured 2026-09-29), while Ubuntu at `4` stays
/// silent after boot. The host now drains that pipe per machine
/// (`machine::serial`), so a full pipe no longer wedges the VM, but a
/// machine that trickles for nothing still costs the drain its idle backoff.
/// Pinning the level on the command line brings every distro down to
/// Ubuntu's near-silent console regardless of its own default. `err` and
/// above still reach the console, so a genuine boot failure is not hidden.
const QUIET_KERNEL_CONSOLE: &str = "loglevel=4";

/// Kernel command line for a shim-less distro machine: root on the read-only
/// rootfs image at vda (custom-kernel testing).
fn default_distro_cmdline(rootfs_format: &str) -> String {
    let console = boot_console();
    format!(
        "console={console} root=/dev/vda ro rootfstype={rootfs_format} earlycon \
         {KEEP_KERNEL_NIC_NAMES} {QUIET_KERNEL_CONSOLE}"
    )
}

/// Kernel command line for the machine boot shim: the shim EROFS boots as
/// root and stages the distro rootfs + data disk named by the `arcbox.*`
/// keys (see `../company/engineering/arcbox/plans/machine-boot-shim.md`). User mounts ride
/// along as a `tag=guest_path[:ro]` table the shim replays, and the
/// machine's hostname ([`machine_hostname`]) rides along for the shim to
/// give the guest.
fn machine_shim_cmdline(hostname: &str, rootfs_format: &str, mounts: &[MachineMount]) -> String {
    use arcbox_constants::cmdline::{
        MACHINE_DATA_KEY, MACHINE_INIT_PATH, MACHINE_MOUNTS_KEY, MACHINE_NAME_KEY,
        MACHINE_ROOTFS_KEY, MACHINE_ROOTFS_TYPE_KEY,
    };
    let console = boot_console();
    let mut cmdline = format!(
        "console={console} root=/dev/vda ro rootfstype=erofs earlycon \
         {KEEP_KERNEL_NIC_NAMES} {QUIET_KERNEL_CONSOLE} init={MACHINE_INIT_PATH} \
         {MACHINE_ROOTFS_KEY}/dev/vdb {MACHINE_ROOTFS_TYPE_KEY}{rootfs_format} \
         {MACHINE_DATA_KEY}/dev/vdc {MACHINE_NAME_KEY}{hostname}"
    );
    if !mounts.is_empty() {
        let table = mounts
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let ro = if m.read_only { ":ro" } else { "" };
                format!("{}={}{ro}", mount_tag(i), m.guest_path)
            })
            .collect::<Vec<_>>()
            .join(",");
        cmdline.push(' ');
        cmdline.push_str(MACHINE_MOUNTS_KEY);
        cmdline.push_str(&table);
    }
    cmdline
}

/// VirtioFS tag for the machine's `i`-th user mount.
fn mount_tag(index: usize) -> String {
    format!("m{index}")
}

/// The VirtioFS shares every machine gets, plus one per user mount.
///
/// The `arcbox` tag shares the data directory (boot assets, logs, runtime);
/// `users` shares `/Users` so macOS paths work transparently in the guest
/// (`docker run -v /Users/foo/project:/app` just works), and `private`
/// shares `/private` for the symlink targets under it. User mounts follow
/// as `m0`, `m1`, …, the tags the cmdline mount table names.
fn vm_shared_dirs(data_dir: &std::path::Path, mounts: &[MachineMount]) -> Vec<SharedDirConfig> {
    let mut shared_dirs = vec![SharedDirConfig::new(
        data_dir.to_string_lossy().to_string(),
        TAG_ARCBOX,
    )];
    if std::path::Path::new(MOUNT_USERS).is_dir() {
        shared_dirs.push(SharedDirConfig::new(MOUNT_USERS, TAG_USERS));
    }
    if std::path::Path::new(MOUNT_PRIVATE).is_dir() {
        shared_dirs.push(SharedDirConfig::new(MOUNT_PRIVATE, TAG_PRIVATE));
    }
    for (i, mount) in mounts.iter().enumerate() {
        let mut share = SharedDirConfig::new(mount.host_path.clone(), mount_tag(i));
        share.read_only = mount.read_only;
        shared_dirs.push(share);
    }
    shared_dirs
}

/// Checks that `name` is free to register and returns the hostname it would
/// get.
///
/// The hostname is a key too: `my_box` and `my-box` would answer to one
/// `my-box.arcbox.local`, and the DNS table keeps whichever started last,
/// so a user's `ssh` lands on the wrong machine with nothing to say why.
/// The second name is refused instead.
fn reserve_hostname(machines: &HashMap<String, MachineInfo>, name: &str) -> Result<String> {
    let hostname = machine_hostname(name)?;
    if machines.contains_key(name) {
        return Err(EngineError::already_exists(name.to_owned()));
    }
    if let Some(other) = machines
        .keys()
        .find(|existing| machine_hostname(existing).ok().as_deref() == Some(hostname.as_str()))
    {
        return Err(EngineError::already_exists(format!(
            "machine name '{name}' would take hostname '{hostname}', which machine '{other}' \
             already has"
        )));
    }
    Ok(hostname)
}

/// The hostname a machine named `name` gets, which is also the label of
/// its `<hostname>.arcbox.local` record and what the shim is handed on the
/// kernel command line.
///
/// A machine name may carry `_` and `.`, which a DNS label may not; both
/// become `-`, so `my_box.v2` answers as `my-box-v2`. What remains must be
/// a label (RFC 1123): 1 to 63 ASCII letters, digits and hyphens, not
/// starting or ending with a hyphen. Two names that map to one hostname
/// share the DNS record; the ownership table keeps the latest.
///
/// # Errors
///
/// Returns an error when the name cannot become a hostname.
pub fn machine_hostname(name: &str) -> Result<String> {
    let hostname: String = name
        .chars()
        .map(|c| if c == '_' || c == '.' { '-' } else { c })
        .collect();
    let is_label = !hostname.is_empty()
        && hostname.len() <= 63
        && hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !hostname.starts_with('-')
        && !hostname.ends_with('-');
    if is_label {
        Ok(hostname)
    } else {
        Err(EngineError::config(format!(
            "machine name '{name}' cannot be a hostname: use 1-63 letters, digits, \
             hyphens, underscores and dots, not starting or ending with a separator"
        )))
    }
}

/// Validates a user mount: the host path must exist and the guest path must
/// be absolute and free of the cmdline table separators.
fn validate_mount(mount: &MachineMount) -> Result<()> {
    if !std::path::Path::new(&mount.host_path).is_dir() {
        return Err(EngineError::config(format!(
            "mount host path '{}' is not a directory",
            mount.host_path
        )));
    }
    if !mount.guest_path.starts_with('/') {
        return Err(EngineError::config(format!(
            "mount guest path '{}' must be absolute",
            mount.guest_path
        )));
    }
    if mount.guest_path.contains(',') || mount.guest_path.contains('=') {
        return Err(EngineError::config(format!(
            "mount guest path '{}' must not contain ',' or '='",
            mount.guest_path
        )));
    }
    Ok(())
}

/// Machine manager.
pub struct MachineManager {
    machines: RwLock<HashMap<String, MachineInfo>>,
    vm_manager: Arc<VmManager>,
    persistence: MachinePersistence,
    /// Data directory for `VirtioFS` sharing.
    data_dir: PathBuf,
    /// Machine-specific directory (`data_dir/machines`/).
    machines_dir: PathBuf,
    /// Host networking every machine's datapath is wired to on start.
    host_network: HostNetwork,
    /// System-wide event bus. User-machine lifecycle events are published here
    /// so watchers (`MachineService.Events`) see them; the default System VM's
    /// events are published by its own lifecycle actor instead.
    event_bus: crate::event::EventBus,
    /// What the host holds of each running machine; every stop waits for
    /// these to clear before it touches the VM (see [`HostHold`]).
    host_holds: host_hold::HostHolds,
    /// The serial drain of every running machine, by name (see [`serial`]).
    /// Dropping an entry stops its drain.
    #[cfg(target_os = "macos")]
    serial_drains: Mutex<HashMap<String, serial::DrainHandle>>,
}

impl MachineManager {
    /// Creates a new machine manager.
    #[must_use]
    pub fn new(
        vm_manager: Arc<VmManager>,
        data_dir: PathBuf,
        host_network: HostNetwork,
        event_bus: crate::event::EventBus,
    ) -> Self {
        let machines_dir = data_dir.join("machines");
        let persistence = MachinePersistence::new(&machines_dir);
        Self::sweep_staging(&machines_dir);

        // Load persisted machines
        let mut machines = HashMap::new();
        for persisted in persistence.load_all() {
            let needs_recovery = persisted.state.needs_recovery();
            if needs_recovery {
                tracing::warn!(
                    "Machine '{}' was running when daemon stopped — marking as stopped",
                    persisted.name
                );
            }

            // Reconstruct VmConfig from persisted data, including the
            // machine's own VirtioFS shares (tags must match what the
            // persisted cmdline mount table references).
            let vm_config = VmConfig {
                cpus: persisted.cpus,
                memory_mb: persisted.memory_mb,
                kernel: persisted.kernel.clone(),
                cmdline: persisted.cmdline.clone(),
                shared_dirs: vm_shared_dirs(&data_dir, &persisted.mounts),
                block_devices: persisted.block_devices.clone(),
                backend: persisted.backend,
                nested_virt: persisted.nested_virt,
                ..Default::default()
            };

            // Try to create the underlying VM
            if let Ok(vm_id) = vm_manager.create(vm_config) {
                let info = MachineInfo {
                    name: persisted.name.clone(),
                    state: persisted.state.into(),
                    vm_id,
                    cid: None, // Will be assigned when VM starts
                    cpus: persisted.cpus,
                    memory_mb: persisted.memory_mb,
                    disk_gb: persisted.disk_gb,
                    kernel: persisted.kernel.clone(),
                    cmdline: persisted.cmdline,
                    block_devices: persisted.block_devices.clone(),
                    distro: persisted.distro.clone(),
                    distro_version: persisted.distro_version.clone(),
                    disk_path: persisted.disk_path.clone().map(PathBuf::from),
                    ssh_key_path: persisted.ssh_key_path.clone().map(PathBuf::from),
                    ip_address: persisted.ip_address.clone(),
                    bridge_ip_address: persisted.bridge_ip_address.clone(),
                    backend: persisted.backend,
                    nested_virt: persisted.nested_virt,
                    created_at: persisted.created_at,
                    started_at: persisted.started_at,
                    mounts: persisted.mounts.clone(),
                };
                machines.insert(persisted.name.clone(), info);
            }

            // Persist the corrected state regardless of whether VM recreation
            // succeeded — a stale Running on disk must not survive reload.
            if needs_recovery {
                if let Err(e) = persistence.update_state(&persisted.name, MachineState::Stopped) {
                    tracing::warn!(
                        "Failed to persist corrected state for '{}': {}",
                        persisted.name,
                        e
                    );
                }
            }
        }

        tracing::info!("Loaded {} persisted machines", machines.len());

        Self {
            machines: RwLock::new(machines),
            vm_manager,
            persistence,
            data_dir,
            machines_dir,
            host_network,
            event_bus,
            host_holds: host_hold::HostHolds::default(),
            #[cfg(target_os = "macos")]
            serial_drains: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a host-side dependency on machine `name` — a mount served
    /// by the machine, say — that every stop waits for before it touches
    /// the VM. Release it, by dropping the hold, on `MachineStopping`.
    #[must_use]
    pub fn host_hold(&self, name: &str) -> HostHold {
        self.host_holds.hold(name)
    }

    /// Waits for the host to release what it holds of `name`, within
    /// [`HOST_RELEASE_TIMEOUT`]; a hold that outlives the wait is logged
    /// and the stop goes ahead.
    fn await_host_release(&self, name: &str) {
        if !self.host_holds.wait_released(name, HOST_RELEASE_TIMEOUT) {
            tracing::warn!(
                machine = name,
                "the host did not release the machine within {HOST_RELEASE_TIMEOUT:?}; stopping it anyway"
            );
        }
    }

    /// Publish a lifecycle event for user machine `name`. The default System VM
    /// is skipped: its lifecycle actor publishes the same events, so emitting
    /// here too would double them.
    fn publish_event(&self, name: &str, event: crate::event::Event) {
        if name != DEFAULT_MACHINE_NAME {
            self.event_bus.publish(event);
        }
    }

    /// Creates a new machine.
    ///
    /// Sets up EROFS rootfs (read-only, /dev/vda) and a Btrfs data disk
    /// (/dev/vdb) with block device and `VirtioFS` sharing configured.
    ///
    /// When `config.distro` is set, also resolves and downloads a distro
    /// rootfs tarball and generates an SSH key pair.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine cannot be created.
    pub async fn create(&self, config: MachineConfig) -> Result<String> {
        self.create_machine(config, DataDisk::Sparse)
    }

    /// Registers a machine from `config`, provisioning its data disk as
    /// `data_disk` says: fresh and sparse for `create`, moved in from a
    /// staging directory for `import`.
    fn create_machine(&self, config: MachineConfig, data_disk: DataDisk) -> Result<String> {
        // Hold the write lock for the entire create operation to prevent TOCTOU
        // races: without this, two concurrent creates with the same name could
        // both pass the existence check before either inserts. `create` is rare
        // and user-driven, so the alternative (insert a `Creating` sentinel,
        // drop the lock for I/O, then finalize/rollback) is not worth its
        // orphan-state failure mode.
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let hostname = reserve_hostname(&machines, &config.name)?;

        let machine_dir = self.machines_dir.join(&config.name);
        std::fs::create_dir_all(&machine_dir)?;

        // User mounts become per-machine VirtioFS shares (tags m0, m1, …);
        // the shim replays the cmdline mount table into the new root. Only
        // meaningful behind the shim — reject elsewhere instead of silently
        // dropping them.
        if !config.mounts.is_empty() {
            if config.rootfs.as_ref().is_none_or(|r| r.shim.is_none()) {
                return Err(EngineError::config(
                    "mounts require a shim-booted distro machine",
                ));
            }
            for mount in &config.mounts {
                validate_mount(mount)?;
            }
        }
        let shared_dirs = vm_shared_dirs(&self.data_dir, &config.mounts);

        // Distro machines boot the pulled rootfs image (behind the boot shim
        // when configured) with a sparse per-machine data disk; plain VMs
        // keep the caller-provided kernel and block devices untouched.
        // Device contract: [shim?, rootfs, data, ..extras] so the shim's
        // vda/vdb/vdc expectations hold regardless of extra devices.
        let (kernel, block_devices, cmdline, disk_path) = match &config.rootfs {
            Some(rootfs) => {
                // A distro rootfs carries no kernel: the shim supplies the
                // boot-assets kernel; without a shim an explicit kernel is
                // required or the VM would boot with an empty kernel path.
                if rootfs.shim.is_none() && config.kernel.is_none() {
                    return Err(EngineError::config(
                        "a distro rootfs without a boot shim requires an explicit \
                         kernel (pass --kernel)",
                    ));
                }
                let data_disk_path = machine_dir.join(clone::DATA_DISK);
                if let DataDisk::Staged(staged) = &data_disk {
                    std::fs::rename(staged, &data_disk_path)?;
                }
                // Never shrinks an existing image, so a restored disk keeps
                // its size and only a larger `disk_gb` grows it.
                crate::vm::ensure_sparse_block_image(
                    &data_disk_path,
                    config.disk_gb.saturating_mul(1024 * 1024 * 1024),
                )?;
                let data_disk = data_disk_path;
                let mut devices = Vec::new();
                if let Some(shim) = &rootfs.shim {
                    devices.push(crate::vm::BlockDeviceConfig {
                        path: shim.rootfs.to_string_lossy().into_owned(),
                        read_only: true,
                    });
                }
                devices.push(crate::vm::BlockDeviceConfig {
                    path: rootfs.path.to_string_lossy().into_owned(),
                    read_only: true,
                });
                devices.push(crate::vm::BlockDeviceConfig {
                    path: data_disk.to_string_lossy().into_owned(),
                    read_only: false,
                });
                devices.extend(config.block_devices.clone());

                let kernel = config.kernel.clone().or_else(|| {
                    rootfs
                        .shim
                        .as_ref()
                        .map(|s| s.kernel.to_string_lossy().into_owned())
                });
                let cmdline = config.cmdline.clone().or_else(|| {
                    Some(match &rootfs.shim {
                        Some(_) => machine_shim_cmdline(&hostname, &rootfs.format, &config.mounts),
                        None => default_distro_cmdline(&rootfs.format),
                    })
                });
                (kernel, devices, cmdline, Some(data_disk))
            }
            None => {
                if matches!(data_disk, DataDisk::Staged(_)) {
                    return Err(EngineError::config(
                        "a restored data disk needs a distro rootfs to boot under",
                    ));
                }
                (
                    config.kernel.clone(),
                    config.block_devices.clone(),
                    config.cmdline.clone(),
                    None,
                )
            }
        };

        // Create underlying VM
        let vm_config = VmConfig {
            cpus: config.cpus,
            memory_mb: config.memory_mb,
            kernel: kernel.clone(),
            cmdline: cmdline.clone(),
            shared_dirs,
            block_devices: block_devices.clone(),
            rosetta: config.enable_rosetta,
            nested_virt: config.nested_virt,
            backend: config.backend,
            ..Default::default()
        };
        let vm_id = self.vm_manager.create(vm_config)?;

        let info = MachineInfo {
            name: config.name.clone(),
            state: MachineState::Created,
            vm_id,
            cid: None,
            cpus: config.cpus,
            memory_mb: config.memory_mb,
            disk_gb: config.disk_gb,
            kernel,
            cmdline,
            block_devices,
            distro: config.distro,
            distro_version: config.distro_version,
            disk_path,
            ssh_key_path: None,
            ip_address: None,
            bridge_ip_address: None,
            backend: config.backend,
            nested_virt: config.nested_virt,
            created_at: Utc::now(),
            started_at: None,
            mounts: config.mounts,
        };
        self.register(&mut machines, info)
    }

    /// Records a newly built machine: persists it, adds it to the registry
    /// and publishes `MachineCreated`. The caller holds the registry's write
    /// lock from its name check through this call, so no other create can
    /// take the name in between.
    fn register(
        &self,
        machines: &mut HashMap<String, MachineInfo>,
        info: MachineInfo,
    ) -> Result<String> {
        let name = info.name.clone();
        self.persistence.save(&info)?;
        machines.insert(name.clone(), info);
        self.publish_event(
            &name,
            crate::event::Event::MachineCreated { name: name.clone() },
        );
        Ok(name)
    }

    /// Starts a machine.
    ///
    /// For machine VMs with a distro, this also waits for the guest agent to
    /// become ready and discovers the guest IP address via vsock.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine cannot be started.
    ///
    /// On macOS this also starts the machine's serial drain, which keeps the
    /// guest's console pipes empty for as long as it runs; see [`serial`] for
    /// why a VM cannot go without one.
    pub async fn start(self: &Arc<Self>, name: &str) -> Result<()> {
        let (vm_id, cid) = self.assign_cid_for_start(name)?;

        // Check if this is a distro-based machine VM.
        let is_machine_vm = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?
            .get(name)
            .and_then(|m| m.distro.as_ref())
            .is_some();

        // Start underlying VM
        self.vm_manager.start(&vm_id, self.host_network.clone())?;

        // Update machine state
        let started_at = Utc::now();
        {
            let mut machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;

            if let Some(machine) = machines.get_mut(name) {
                machine.state = MachineState::Running;
                machine.cid = Some(cid);
                machine.ip_address = None;
                machine.bridge_ip_address = None;
                machine.started_at = Some(started_at);

                tracing::info!("Machine '{}' started with CID {}", name, cid);
            }
        }

        #[cfg(target_os = "macos")]
        self.start_serial_drain(name, &vm_id);

        // Update persisted state (single read-modify-write)
        if let Err(e) = self.persistence.update(name, |m| {
            m.state = MachineState::Running.into();
            m.ip_address = None;
            m.bridge_ip_address = None;
            m.started_at = Some(started_at);
        }) {
            tracing::warn!("Failed to persist state for machine '{}': {}", name, e);
        }

        // For machine VMs, wait for agent readiness and discover IP.
        if is_machine_vm {
            self.wait_for_machine_ready(name).await.map_err(|e| {
                EngineError::Machine(format!(
                    "Machine '{name}' started but readiness check failed: {e}"
                ))
            })?;
        }

        self.publish_event(
            name,
            crate::event::Event::MachineStarted {
                name: name.to_string(),
            },
        );
        Ok(())
    }

    /// Waits for the guest agent to become ready and discovers the IP address.
    ///
    /// Polls the agent via vsock with exponential backoff. Once the agent
    /// responds, queries `SystemInfo` to get the guest IP. The probe follows
    /// the transport the backend hands out: async (VZ) attempts run on the
    /// runtime; blocking (HV) attempts run inside `block_in_place`, because
    /// the HV socketpair's rapid fd teardown stalls the tokio reactor (same
    /// rationale as `wait_for_agent` in `vm_lifecycle`) — a per-attempt
    /// blocking region, bounded by one RPC round-trip.
    ///
    /// Probe before the first sleep: failed probes are ~1ms (vsock RST via
    /// the event-driven RX path), while sleeping first puts a full backoff
    /// interval on every start even when the agent is already up.
    async fn wait_for_machine_ready(&self, name: &str) -> Result<()> {
        const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        const INITIAL_DELAY_MS: u64 = 50;
        const MAX_DELAY_MS: u64 = 500;

        tracing::info!("Waiting for machine '{}' agent to become ready...", name);

        let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
        let mut delay_ms = INITIAL_DELAY_MS;
        let mut attempt: u32 = 0;

        let addresses = loop {
            attempt += 1;

            let probed = match self.connect_agent(name) {
                Ok(agent) if agent.is_blocking() => {
                    tokio::task::block_in_place(|| probe_ip_blocking(agent, name, attempt))?
                }
                Ok(agent) => probe_ip_async(agent, name, attempt).await?,
                Err(e) => {
                    tracing::trace!("Machine '{}' connect failed (attempt {attempt}): {e}", name);
                    None
                }
            };
            if let Some(addresses) = probed {
                break addresses;
            }

            if std::time::Instant::now() >= deadline {
                self.log_console_tail(name);
                return Err(EngineError::Machine(format!(
                    "Machine '{name}' agent did not report a routable IP within timeout"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            delay_ms = (delay_ms * 3 / 2).min(MAX_DELAY_MS);
        };

        // Back on async context — update state.
        {
            let mut machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;
            if let Some(machine) = machines.get_mut(name) {
                machine.ip_address = Some(addresses.ip.clone());
                machine.bridge_ip_address.clone_from(&addresses.bridge_ip);
            }
        }
        if let Err(e) = self.persistence.update(name, |m| {
            m.ip_address = Some(addresses.ip.clone());
            m.bridge_ip_address.clone_from(&addresses.bridge_ip);
        }) {
            tracing::warn!("Failed to persist IP for machine '{}': {}", name, e);
        }
        tracing::info!(
            machine = name,
            ip = %addresses.ip,
            bridge_ip = addresses.bridge_ip.as_deref().unwrap_or("none"),
            "machine ready"
        );
        Ok(())
    }

    fn assign_cid_for_start(&self, name: &str) -> Result<(VmId, u32)> {
        let (vm_id, cid) = {
            let machines = self
                .machines
                .read()
                .map_err(|_| EngineError::LockPoisoned)?;

            let machine = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;

            if machine.state == MachineState::Running {
                return Err(EngineError::invalid_state(format!(
                    "machine '{name}' is already running"
                )));
            }

            if machine.state == MachineState::Starting || machine.state == MachineState::Stopping {
                return Err(EngineError::invalid_state(format!(
                    "machine '{name}' is in transition state"
                )));
            }

            // Lowest CID not held by another machine. CIDs 0/1 are reserved
            // and 2 is the host, so guests start at 3. Uniqueness across
            // machines is cosmetic — vsock routing is per-VM device
            // (`VmManager::connect_vsock` addresses by VM id, and the guest
            // only sees its own CID) — but distinct CIDs keep guest-side
            // identity unambiguous in logs and diagnostics.
            let used: std::collections::HashSet<u32> =
                machines.values().filter_map(|m| m.cid).collect();
            let cid = (3..=u32::MAX)
                .find(|c| !used.contains(c))
                .expect("fewer than u32::MAX machines");

            (machine.vm_id.clone(), cid)
        };

        self.vm_manager.set_guest_cid(&vm_id, cid)?;

        Ok((vm_id, cid))
    }

    /// Returns a reference to the underlying VM manager.
    #[must_use]
    pub fn vm_manager(&self) -> &VmManager {
        &self.vm_manager
    }

    /// Returns the vmnet bridge interface name for a machine's VM.
    ///
    /// Only available when the `vmnet` feature is enabled and the VM is running.
    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    pub fn vmnet_interface_mac(&self, name: &str) -> Option<String> {
        let machines = self.machines.read().ok()?;
        let machine = machines.get(name)?;
        self.vm_manager.vmnet_interface_mac(&machine.vm_id)
    }

    /// Returns the bridge NIC MAC address for a machine's VM.
    pub fn bridge_mac(&self, name: &str) -> Option<String> {
        let machines = self.machines.read().ok()?;
        let machine = machines.get(name)?;
        Some(crate::vm::bridge_nic_mac_for_vm_id(&machine.vm_id))
    }

    /// Gets the vsock CID for a running machine.
    #[must_use]
    pub fn get_cid(&self, name: &str) -> Option<u32> {
        self.machines.read().ok()?.get(name)?.cid
    }

    /// Connects to the agent on a running machine.
    ///
    /// Returns an `AgentClient` that can be used to communicate with the
    /// guest agent for container operations.
    ///
    /// # Errors
    /// Returns an error if the machine is not found, not running, or connection fails.
    #[cfg(target_os = "macos")]
    pub fn connect_agent(&self, name: &str) -> Result<crate::agent_client::AgentClient> {
        use crate::agent_client::AgentClient;
        let (cid, vm_id) = {
            let machines = self
                .machines
                .read()
                .map_err(|_| EngineError::LockPoisoned)?;
            let machine = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;
            if machine.state != MachineState::Running {
                return Err(EngineError::invalid_state(format!(
                    "machine '{name}' is not running"
                )));
            }
            let cid = machine
                .cid
                .ok_or_else(|| EngineError::invalid_state("CID not assigned"))?;
            (cid, machine.vm_id.clone())
        };
        let backend = self.vm_manager.backend(&vm_id)?;
        let fd = self.connect_vsock_port(name, AGENT_PORT)?;
        // The transport must follow the backend, not the fd's socket domain:
        // both backends hand over unnamed AF_UNIX fds, but only the HV
        // socketpair needs the blocking transport (tokio/kqueue stalls on
        // rapid connect/teardown cycles), and only the async transport
        // supports the streaming sandbox RPCs VZ clients rely on.
        match backend {
            arcbox_vmm::VmBackend::Hv => AgentClient::from_fd_blocking(cid, fd),
            arcbox_vmm::VmBackend::Vz => AgentClient::from_fd_async(cid, fd),
        }
    }

    /// Connects to a vsock port on a running machine (macOS).
    ///
    /// This is a generic helper used by agent and guest runtime proxy paths.
    #[cfg(target_os = "macos")]
    pub fn connect_vsock_port(&self, name: &str, port: u32) -> Result<std::os::unix::io::RawFd> {
        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;

        let machine = machines
            .get(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;

        if machine.state != MachineState::Running {
            return Err(EngineError::invalid_state(format!(
                "machine '{name}' is not running"
            )));
        }

        self.vm_manager.connect_vsock(&machine.vm_id, port)
    }

    /// Connects to the agent on a running machine (Linux).
    #[cfg(target_os = "linux")]
    pub fn connect_agent(&self, name: &str) -> Result<crate::agent_client::AgentClient> {
        use crate::agent_client::AgentClient;

        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;

        let machine = machines
            .get(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;

        if machine.state != MachineState::Running {
            return Err(EngineError::invalid_state(format!(
                "machine '{}' is not running",
                name
            )));
        }

        let cid = machine
            .cid
            .ok_or_else(|| EngineError::invalid_state("CID not assigned"))?;

        // On Linux, AgentClient connects directly via AF_VSOCK
        Ok(AgentClient::new(cid))
    }

    /// Connects to a vsock port on a running machine (Linux).
    #[cfg(target_os = "linux")]
    pub fn connect_vsock_port(&self, name: &str, port: u32) -> Result<std::os::unix::io::RawFd> {
        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;

        let machine = machines
            .get(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;

        if machine.state != MachineState::Running {
            return Err(EngineError::invalid_state(format!(
                "machine '{}' is not running",
                name
            )));
        }

        self.vm_manager.connect_vsock(&machine.vm_id, port)
    }

    /// Connects to the machine's agent and pings it once.
    ///
    /// A ping is also a wall-clock sync: the request carries the host's
    /// `timestamp_secs`, which the agent applies via `clock_settime`.
    /// `connect_agent` is a blocking hypervisor call, so it runs off the
    /// async executor; the transport it yields is blocking on the HV
    /// socketpair and async on VZ/Linux vsock, so the ping dispatches on
    /// `is_blocking()`.
    ///
    /// # Errors
    /// Returns an error if the machine is not running, the agent is
    /// unreachable, or the ping fails.
    pub async fn ping_agent(self: Arc<Self>, machine_name: String) -> Result<()> {
        let manager = Arc::clone(&self);
        let name = machine_name.clone();
        let connected = tokio::task::spawn_blocking(move || manager.connect_agent(&name)).await;
        match connected {
            Ok(Ok(mut agent)) => {
                if agent.is_blocking() {
                    tokio::task::spawn_blocking(move || agent.ping_blocking().map(|_| ()))
                        .await
                        .unwrap_or_else(|e| {
                            Err(EngineError::Vm(format!("agent ping task panicked: {e}")))
                        })
                } else {
                    agent.ping().await.map(|_| ())
                }
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(EngineError::Vm(format!("agent connect task panicked: {e}"))),
        }
    }

    /// Asks a running machine's agent for its addresses and records the
    /// bridge NIC's on the machine, returning it.
    ///
    /// Distro machines record it in their own readiness probe; the System
    /// VM's readiness lives in the lifecycle actor, which calls this once
    /// the agent answers so `default.arcbox.local` can be published from
    /// the same record as every other machine. Same transport dispatch as
    /// [`Self::ping_agent`].
    ///
    /// # Errors
    /// Returns an error if the machine is not running or the agent is
    /// unreachable.
    pub async fn record_bridge_address(
        self: Arc<Self>,
        machine_name: String,
    ) -> Result<Option<String>> {
        let manager = Arc::clone(&self);
        let name = machine_name.clone();
        let connected = tokio::task::spawn_blocking(move || manager.connect_agent(&name)).await;
        let info = match connected {
            Ok(Ok(mut agent)) => {
                if agent.is_blocking() {
                    tokio::task::spawn_blocking(move || agent.get_system_info_blocking())
                        .await
                        .unwrap_or_else(|e| {
                            Err(EngineError::Vm(format!("system info task panicked: {e}")))
                        })?
                } else {
                    agent.get_system_info().await?
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(e) => {
                return Err(EngineError::Vm(format!("agent connect task panicked: {e}")));
            }
        };
        let bridge_ip = Some(info.bridge_ip_address).filter(|ip| !ip.is_empty());
        {
            let mut machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;
            if let Some(machine) = machines.get_mut(&machine_name) {
                machine.bridge_ip_address.clone_from(&bridge_ip);
            }
        }
        if let Err(e) = self.persistence.update(&machine_name, |m| {
            m.bridge_ip_address.clone_from(&bridge_ip);
        }) {
            tracing::warn!(
                "Failed to persist bridge address for machine '{}': {}",
                machine_name,
                e
            );
        }
        Ok(bridge_ip)
    }

    /// Asks a running distro machine's agent to serve the machine's root
    /// filesystem to the host and returns the endpoint to mount; see
    /// `EnsureMachineExportRequest` for what the request carries. Same
    /// transport dispatch as [`Self::ping_agent`].
    ///
    /// # Errors
    /// Returns an error if the machine is not running, the agent is
    /// unreachable, or the agent refused the export.
    pub async fn ensure_export(
        self: Arc<Self>,
        machine_name: String,
        request: EnsureMachineExportRequest,
    ) -> Result<EnsureMachineExportResponse> {
        let manager = Arc::clone(&self);
        let name = machine_name.clone();
        let connected = tokio::task::spawn_blocking(move || manager.connect_agent(&name)).await;
        match connected {
            Ok(Ok(mut agent)) => {
                if agent.is_blocking() {
                    tokio::task::spawn_blocking(move || {
                        agent.ensure_machine_export_blocking(&request)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        Err(EngineError::Vm(format!(
                            "machine export task panicked: {e}"
                        )))
                    })
                } else {
                    agent.ensure_machine_export(&request).await
                }
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(EngineError::Vm(format!("agent connect task panicked: {e}"))),
        }
    }

    /// Connects to the machine's agent and trims its data filesystems,
    /// returning the bytes the guest reported trimmed. Same transport
    /// dispatch as [`Self::ping_agent`]: the HV socketpair is blocking, VZ
    /// and Linux vsock are async.
    ///
    /// # Errors
    /// Returns an error if the machine is not running, the agent is
    /// unreachable, or a filesystem refused the trim.
    pub async fn trim_disk(self: Arc<Self>, machine_name: String) -> Result<u64> {
        let manager = Arc::clone(&self);
        let name = machine_name.clone();
        let connected = tokio::task::spawn_blocking(move || manager.connect_agent(&name)).await;
        let response = match connected {
            Ok(Ok(mut agent)) => {
                if agent.is_blocking() {
                    tokio::task::spawn_blocking(move || agent.disk_trim_blocking())
                        .await
                        .unwrap_or_else(|e| {
                            Err(EngineError::Vm(format!("disk trim task panicked: {e}")))
                        })?
                } else {
                    agent.disk_trim().await?
                }
            }
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(EngineError::Vm(format!("agent connect task panicked: {e}"))),
        };
        tracing::debug!(machine = %machine_name, result = %response.result, "disk trim done");
        Ok(response.bytes_trimmed)
    }

    /// Starts the serial drain for a machine that just entered `Running`,
    /// replacing (and thereby stopping) any earlier one under the same name.
    #[cfg(target_os = "macos")]
    fn start_serial_drain(&self, name: &str, vm_id: &VmId) {
        let handle = match self.vm_manager.dup_serial_readers(vm_id) {
            Ok(Some(readers)) => serial::spawn(name, readers),
            Ok(None) => {
                tracing::debug!(machine = name, "no host console pipes to drain");
                return;
            }
            Err(e) => {
                tracing::warn!(
                    machine = name,
                    "console pipes unavailable, not draining: {e}"
                );
                return;
            }
        };
        self.serial_drains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), handle);
    }

    #[cfg(target_os = "macos")]
    fn stop_serial_drain(&self, name: &str) {
        self.serial_drains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
    }

    #[cfg(not(target_os = "macos"))]
    fn stop_serial_drain(&self, _name: &str) {}

    /// Logs the tail of a machine's console and agent-log output at WARN, for
    /// diagnosing a boot that never reached agent readiness.
    ///
    /// The serial drain owns the pipes from the moment `start` marks the
    /// machine `Running`, so the tail is what it kept; a machine without a
    /// drain (no host pipes on its backend) falls back to whatever is still
    /// unread in the pipes, which survives even an early guest death.
    #[cfg(target_os = "macos")]
    fn log_console_tail(&self, name: &str) {
        let kept = self
            .serial_drains
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .map(serial::DrainHandle::tail);
        let (console, agent_log) = match kept {
            Some(tail) => tail,
            None => {
                // Read the vm_id directly: the machine may already be
                // non-Running (an early guest death).
                let vm_id = match self.machines.read() {
                    Ok(machines) => machines.get(name).map(|m| m.vm_id.clone()),
                    Err(_) => None,
                };
                let Some(vm_id) = vm_id else { return };
                let last_lines = |output: Result<String>| -> Vec<String> {
                    let Ok(text) = output else { return Vec::new() };
                    let mut lines: Vec<String> =
                        text.lines().rev().take(40).map(str::to_owned).collect();
                    lines.reverse();
                    lines
                };
                (
                    last_lines(self.vm_manager.read_console_output(&vm_id)),
                    last_lines(self.vm_manager.read_agent_log_output(&vm_id)),
                )
            }
        };
        for (label, lines) in [("console", console), ("agent-log", agent_log)] {
            for line in lines {
                tracing::warn!(machine = %name, "machine {label}: {line}");
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn log_console_tail(&self, _name: &str) {}

    /// Captures a debug snapshot (virtio queues + vCPU exit counters)
    /// for a machine.
    ///
    /// Deliberately not gated on `MachineState::Running`: a machine
    /// stuck booting (state still Starting) is this snapshot's main
    /// diagnostic target. The VM manager errors if no VMM exists yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine is not found or its VMM has not
    /// been created.
    pub fn debug_snapshot(&self, name: &str) -> Result<arcbox_vmm::VmDebugSnapshot> {
        let machines = self
            .machines
            .read()
            .map_err(|_| EngineError::LockPoisoned)?;

        let machine = machines
            .get(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;

        self.vm_manager.debug_snapshot(&machine.vm_id)
    }

    /// Stops a machine (force).
    ///
    /// Accepts `Stopping` as well as `Running` so a force stop can preempt
    /// an in-flight graceful stop instead of erroring for up to the whole
    /// graceful-shutdown window. The VM dies the moment it is stopped, so
    /// `MachineStopping` goes out first and the host gets
    /// [`HOST_RELEASE_TIMEOUT`] to release what it holds of the machine.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine cannot be stopped.
    pub fn stop(&self, name: &str) -> Result<()> {
        let stoppable =
            |state: MachineState| matches!(state, MachineState::Running | MachineState::Stopping);
        {
            let machines = self
                .machines
                .read()
                .map_err(|_| EngineError::LockPoisoned)?;
            let machine = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;
            if !stoppable(machine.state) {
                return Err(EngineError::invalid_state(format!(
                    "machine '{name}' is not running"
                )));
            }
        }

        self.publish_event(
            name,
            crate::event::Event::MachineStopping {
                name: name.to_string(),
            },
        );
        // Not under the registry lock: the holders read the registry too.
        self.await_host_release(name);

        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get_mut(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;
        // A concurrent stop may have finished during the wait.
        if !stoppable(machine.state) {
            return Err(EngineError::invalid_state(format!(
                "machine '{name}' is not running"
            )));
        }

        // Stop underlying VM
        #[cfg(target_os = "macos")]
        self.vm_manager
            .force_stop_without_hypervisor(&machine.vm_id)?;
        #[cfg(not(target_os = "macos"))]
        self.vm_manager.stop(&machine.vm_id)?;

        machine.state = MachineState::Stopped;
        machine.cid = None;
        machine.bridge_ip_address = None;
        self.stop_serial_drain(name);

        if let Err(e) = self.persist_stopped(name) {
            tracing::warn!(
                "Failed to persist stopped state for machine '{}': {}",
                name,
                e
            );
        }

        self.publish_event(
            name,
            crate::event::Event::MachineStopped {
                name: name.to_string(),
            },
        );
        Ok(())
    }

    /// Reports whether a machine's VM stopped on its own (guest-driven), and if
    /// so whether it was a reboot request. See [`VmManager::vm_self_stopped`].
    #[must_use]
    pub fn vm_self_stopped(&self, name: &str) -> Option<bool> {
        let machines = self.machines.read().ok()?;
        let vm_id = machines.get(name)?.vm_id.clone();
        drop(machines);
        self.vm_manager.vm_self_stopped(&vm_id)
    }

    /// Reboots a machine's VM in place (guest PSCI SYSTEM_RESET): a full
    /// teardown then a fresh boot, leaving the machine record Running. The
    /// caller re-establishes agent readiness afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine is unknown or the reboot fails.
    pub fn reboot(&self, name: &str) -> Result<()> {
        let vm_id = {
            let machines = self
                .machines
                .read()
                .map_err(|_| EngineError::LockPoisoned)?;
            machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?
                .vm_id
                .clone()
        };
        // Reboot is a slow teardown+reboot; do it without holding the registry
        // lock so concurrent reads stay responsive.
        self.vm_manager.reboot(&vm_id)?;
        tracing::info!("Rebooted machine '{}'", name);
        Ok(())
    }

    /// Sets the CPU and memory a machine boots with; `None` keeps the
    /// current value.
    ///
    /// The new size is written to the VM config and the persisted record
    /// right away. A stopped machine boots with it next; a running machine
    /// keeps the size it booted with, and the result says a restart is
    /// needed. Persisting before the restart is deliberate: a size the user
    /// set and the daemon forgot on its next start would be worse than one
    /// it refused.
    ///
    /// # Errors
    ///
    /// Returns an error if the machine is not found, is starting or
    /// stopping, a value is zero, or the record cannot be persisted.
    pub fn set_resources(
        &self,
        name: &str,
        cpus: Option<u32>,
        memory_mb: Option<u64>,
    ) -> Result<MachineResize> {
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;
        let machine = machines
            .get_mut(name)
            .ok_or_else(|| EngineError::not_found(name.to_string()))?;
        if matches!(
            machine.state,
            MachineState::Starting | MachineState::Stopping
        ) {
            return Err(EngineError::invalid_state(format!(
                "cannot resize machine '{name}' while it is {:?}",
                machine.state
            )));
        }
        let cpus = cpus.unwrap_or(machine.cpus);
        let memory_mb = memory_mb.unwrap_or(machine.memory_mb);
        if cpus == 0 || memory_mb == 0 {
            return Err(EngineError::config(
                "a machine needs at least one CPU and some memory",
            ));
        }
        let restart_required = machine.state == MachineState::Running;
        if (cpus, memory_mb) != (machine.cpus, machine.memory_mb) {
            self.vm_manager
                .set_resources(&machine.vm_id, cpus, memory_mb)?;
            machine.cpus = cpus;
            machine.memory_mb = memory_mb;
            self.persistence.update(name, |m| {
                m.cpus = cpus;
                m.memory_mb = memory_mb;
            })?;
            tracing::info!(
                machine = name,
                cpus,
                memory_mb,
                restart_required,
                "machine resized"
            );
        }
        Ok(MachineResize {
            cpus,
            memory_mb,
            restart_required,
        })
    }

    /// Switches a stopped machine's hypervisor backend.
    ///
    /// Updates the lazily-built VM config and the in-memory and persisted
    /// records; `set_backend` itself does not touch the machine's disks. The
    /// machine must be stopped; the new backend takes effect on the next start,
    /// when the `Vmm` is rebuilt from `VmConfig`. Note that for the System VM
    /// the kernel command line differs between backends, so that next start
    /// detects config drift and recreates the machine record (the persistent
    /// data image survives; SSH host keys are regenerated).
    ///
    /// # Errors
    ///
    /// Returns an error if the machine is not found, is running/starting, or
    /// the persisted config cannot be updated.
    pub fn set_backend(&self, name: &str, backend: arcbox_vmm::VmBackend) -> Result<()> {
        // Validate and capture the VM id without mutating anything yet.
        let vm_id = {
            let machines = self
                .machines
                .read()
                .map_err(|_| EngineError::LockPoisoned)?;
            let machine = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;
            if matches!(
                machine.state,
                MachineState::Running | MachineState::Starting
            ) {
                return Err(EngineError::invalid_state(format!(
                    "cannot switch backend while machine '{name}' is {:?}",
                    machine.state
                )));
            }
            machine.vm_id.clone()
        };

        // Update the runtime views first, then commit the durable record last.
        // The persisted backend is what seeds the lifecycle after a daemon
        // restart, so writing it only once the in-memory updates have succeeded
        // means a failed switch never leaves a backend on disk that the running
        // system did not actually apply (which a later restart would then boot).
        self.vm_manager.set_backend(&vm_id, backend)?;
        if let Some(machine) = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?
            .get_mut(name)
        {
            machine.backend = backend;
        }
        self.persistence.update(name, |m| m.backend = backend)?;
        Ok(())
    }

    /// Attempts graceful machine shutdown via guest ACPI stop request.
    ///
    /// Returns `Ok(true)` if the machine stopped, `Ok(false)` if graceful
    /// shutdown timed out or is unavailable.
    pub fn graceful_stop(&self, name: &str, timeout: Duration) -> Result<bool> {
        let vm_id = {
            let mut machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;

            let machine = machines
                .get_mut(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;

            if machine.state != MachineState::Running {
                return Err(EngineError::invalid_state(format!(
                    "machine '{name}' is not running"
                )));
            }

            machine.state = MachineState::Stopping;
            machine.vm_id.clone()
        };
        self.publish_event(
            name,
            crate::event::Event::MachineStopping {
                name: name.to_string(),
            },
        );
        // The guest's shutdown kills the machine's export within moments
        // of the RPC below — before the unmount it races has finished
        // (alpine, 2026-10-04) — so the host's release goes first here too.
        self.await_host_release(name);

        match self.vm_manager.graceful_stop(&vm_id, timeout) {
            Ok(true) => {
                let mut machines = self
                    .machines
                    .write()
                    .map_err(|_| EngineError::LockPoisoned)?;

                let machine = machines
                    .get_mut(name)
                    .ok_or_else(|| EngineError::not_found(name.to_string()))?;
                machine.state = MachineState::Stopped;
                machine.cid = None;
                machine.bridge_ip_address = None;
                self.stop_serial_drain(name);

                if let Err(e) = self.persist_stopped(name) {
                    tracing::warn!(
                        "Failed to persist stopped state for machine '{}': {}",
                        name,
                        e
                    );
                }
                drop(machines);
                self.publish_event(
                    name,
                    crate::event::Event::MachineStopped {
                        name: name.to_string(),
                    },
                );
                Ok(true)
            }
            Ok(false) => {
                self.rollback_stopping(name);
                Ok(false)
            }
            Err(e) => {
                self.rollback_stopping(name);
                Err(e)
            }
        }
    }

    /// Records a stop: the state, and the bridge address, which is only
    /// meaningful while the machine runs (the uplink address is kept, as
    /// it always was, for `inspect` on a stopped machine).
    fn persist_stopped(&self, name: &str) -> Result<()> {
        self.persistence.update(name, |m| {
            m.state = MachineState::Stopped.into();
            m.bridge_ip_address = None;
        })
    }

    /// Rolls a failed graceful stop back to `Running` — but only if the
    /// machine is still `Stopping`: a concurrent force [`Self::stop`] may
    /// have already stopped it, and its `Stopped` must not be overwritten.
    fn rollback_stopping(&self, name: &str) {
        if let Ok(mut machines) = self.machines.write() {
            if let Some(machine) = machines.get_mut(name) {
                if machine.state == MachineState::Stopping {
                    machine.state = MachineState::Running;
                }
            }
        }
    }

    /// Gets machine information.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<MachineInfo> {
        self.machines.read().ok()?.get(name).cloned()
    }

    /// Returns whether a machine with `name` is registered.
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        self.machines
            .read()
            .is_ok_and(|machines| machines.contains_key(name))
    }

    /// Lists all machines.
    #[must_use]
    pub fn list(&self) -> Vec<MachineInfo> {
        self.machines
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Removes a machine and all associated artifacts (disk, SSH keys, config).
    ///
    /// # Errors
    ///
    /// Returns an error if the machine cannot be removed.
    pub fn remove(&self, name: &str, force: bool) -> Result<()> {
        self.remove_if_present(name, force)?
            .then_some(())
            .ok_or_else(|| EngineError::not_found(name.to_string()))
    }

    /// Removes a registered machine; returns false if no record exists.
    ///
    /// Check absence under the registry lock. Errors from removing an existing
    /// machine must propagate, including a missing VM or a persistence failure.
    pub(crate) fn remove_if_present(&self, name: &str, force: bool) -> Result<bool> {
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;

        let Some(machine) = machines.get(name) else {
            return Ok(false);
        };

        // Check if machine is running
        if machine.state == MachineState::Running && !force {
            return Err(EngineError::invalid_state(
                "cannot remove running machine (use --force)".to_string(),
            ));
        }

        // Stop if running and force is set
        if machine.state == MachineState::Running {
            let vm_id = machine.vm_id.clone();
            drop(machines); // Release lock before stopping
            self.publish_event(
                name,
                crate::event::Event::MachineStopping {
                    name: name.to_string(),
                },
            );
            // The VM dies below; what the host holds of the machine (its
            // root mount) must be released while the machine still answers.
            self.await_host_release(name);
            self.vm_manager.stop(&vm_id)?;
            self.stop_serial_drain(name);
            machines = self
                .machines
                .write()
                .map_err(|_| EngineError::LockPoisoned)?;
        }

        // Get VM ID before removing from map.
        let vm_id = {
            let m = machines
                .get(name)
                .ok_or_else(|| EngineError::not_found(name.to_string()))?;
            m.vm_id.clone()
        };

        // Remove from VM manager
        self.vm_manager.remove(&vm_id)?;

        // Remove from machines map
        machines.remove(name);

        // Remove persisted config (removes entire machine directory including SSH keys).
        // This must succeed — if it doesn't, the machine will reappear on daemon
        // restart even though VM and in-memory state are already gone.
        self.persistence.remove(name)?;

        drop(machines);
        self.publish_event(
            name,
            crate::event::Event::MachineRemoved {
                name: name.to_string(),
            },
        );
        tracing::info!("Removed machine '{}'", name);
        Ok(true)
    }

    /// Takes the inbound listener manager from a running machine's VM (Darwin only).
    ///
    /// Returns `None` if the machine is not found, not running, or the manager
    /// has already been taken.
    #[cfg(target_os = "macos")]
    pub fn take_inbound_listener_manager(
        &self,
        name: &str,
    ) -> Option<arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        let vm_id = {
            let machines = self.machines.read().ok()?;
            let machine = machines.get(name)?;
            if machine.state != MachineState::Running {
                return None;
            }
            machine.vm_id.clone()
        };
        self.vm_manager.take_inbound_listener_manager(&vm_id)
    }

    /// Registers a mock machine for testing purposes.
    ///
    /// This method creates a machine entry without creating an actual VM.
    /// The machine will be in Running state with a mock CID.
    ///
    /// # Note
    /// This is intended for unit testing only and should not be used in production.
    pub fn register_mock_machine(&self, name: &str, cid: u32) -> Result<()> {
        let mut machines = self
            .machines
            .write()
            .map_err(|_| EngineError::LockPoisoned)?;

        if machines.contains_key(name) {
            return Ok(()); // Already registered
        }

        let info = MachineInfo {
            name: name.to_string(),
            state: MachineState::Running,
            vm_id: VmId::new(), // Fake VM ID
            cid: Some(cid),
            cpus: arcbox_hypervisor::default_vm_cpu_count(),
            memory_mb: 4096,
            disk_gb: 50,
            kernel: None,
            cmdline: None,
            block_devices: Vec::new(),
            distro: None,
            distro_version: None,
            disk_path: None,
            ssh_key_path: None,
            ip_address: None,
            bridge_ip_address: None,
            backend: arcbox_vmm::VmBackend::default(),
            nested_virt: false,
            created_at: Utc::now(),
            started_at: None,
            mounts: Vec::new(),
        };

        machines.insert(name.to_string(), info);
        tracing::debug!("Registered mock machine '{}' with CID {}", name, cid);
        Ok(())
    }
}

/// The addresses a ready machine reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestAddresses {
    /// The routable address on the machine's uplink (`10.0.2.x`).
    ip: String,
    /// The bridge NIC's address, when the guest has one; see
    /// [`MachineInfo::bridge_ip_address`].
    bridge_ip: Option<String>,
}

/// The readiness verdict for one `SystemInfo` snapshot: `None` means "not
/// ready yet, retry".
///
/// An agent that answers is not yet a usable machine. The distro's own init
/// starts *after* the agent (the boot shim backgrounds the agent, then
/// `exec`s `/sbin/init`) and typically reconfigures the network from
/// scratch, flushing the interface the shim configured — measured on alpine
/// as a ~20 ms window with no routes at all, about 100 ms after `Start`
/// would otherwise have returned. Waiting for the guest's init to settle is
/// what makes readiness mean "usable" rather than "the agent is alive"
/// (CORE-66).
///
/// The bridge address is taken as reported and never waited for: the shim
/// acquires it before the distro's init runs, so by the time that init has
/// settled it is either there or not coming (no bridge NIC, no lease).
fn readiness_addresses(
    info: &arcbox_connect::v1::SystemInfo,
    name: &str,
    attempt: u32,
) -> Option<GuestAddresses> {
    if info.distro_init_pending {
        tracing::trace!(
            "Machine '{name}' distro init still starting (attempt {attempt}); not ready",
        );
        return None;
    }
    let ip = select_routable_ip(&info.ip_addresses)?;
    let bridge_ip = Some(info.bridge_ip_address.clone()).filter(|ip| !ip.is_empty());
    Some(GuestAddresses { ip, bridge_ip })
}

/// One readiness attempt over the blocking (HV) transport: ping, protocol
/// check, then IP discovery. `Ok(None)` means "not ready yet, retry";
/// a protocol mismatch is fatal so stale agents fail machine start loudly
/// instead of misbehaving under proto field skew.
fn probe_ip_blocking(
    mut agent: crate::agent_client::AgentClient,
    name: &str,
    attempt: u32,
) -> Result<Option<GuestAddresses>> {
    let resp = match agent.ping_blocking() {
        Ok(resp) => resp,
        Err(e) => {
            tracing::trace!("Machine '{}' ping failed (attempt {attempt}): {e}", name);
            return Ok(None);
        }
    };
    crate::agent_client::AgentClient::check_agent_protocol(&resp)?;
    tracing::debug!(
        "Machine '{}' agent reachable (version: {}, attempt {})",
        name,
        resp.version,
        attempt,
    );
    match agent.get_system_info_blocking() {
        Ok(info) => Ok(readiness_addresses(&info, name, attempt)),
        Err(e) => {
            tracing::trace!(
                "Machine '{}' get_system_info failed (attempt {attempt}): {e}",
                name,
            );
            Ok(None)
        }
    }
}

/// One readiness attempt over the async (VZ) transport; same contract as
/// [`probe_ip_blocking`].
async fn probe_ip_async(
    mut agent: crate::agent_client::AgentClient,
    name: &str,
    attempt: u32,
) -> Result<Option<GuestAddresses>> {
    let resp = match agent.ping().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::trace!("Machine '{}' ping failed (attempt {attempt}): {e}", name);
            return Ok(None);
        }
    };
    crate::agent_client::AgentClient::check_agent_protocol(&resp)?;
    tracing::debug!(
        "Machine '{}' agent reachable (version: {}, attempt {})",
        name,
        resp.version,
        attempt,
    );
    match agent.get_system_info().await {
        Ok(info) => Ok(readiness_addresses(&info, name, attempt)),
        Err(e) => {
            tracing::trace!(
                "Machine '{}' get_system_info failed (attempt {attempt}): {e}",
                name,
            );
            Ok(None)
        }
    }
}

fn select_routable_ip(ips: &[String]) -> Option<String> {
    let mut ipv6_candidate = None;

    for ip in ips {
        let Ok(addr) = ip.parse::<IpAddr>() else {
            continue;
        };
        if addr.is_loopback() || addr.is_multicast() || addr.is_unspecified() {
            continue;
        }

        match addr {
            IpAddr::V4(v4) => return Some(v4.to_string()),
            IpAddr::V6(v6) => {
                if v6.is_unicast_link_local() {
                    continue;
                }
                if ipv6_candidate.is_none() {
                    ipv6_candidate = Some(v6.to_string());
                }
            }
        }
    }

    ipv6_candidate
}
