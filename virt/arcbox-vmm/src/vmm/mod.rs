//! Main VMM implementation.
//!
//! The VMM (Virtual Machine Monitor) orchestrates all components needed to run
//! a virtual machine: hypervisor, vCPUs, memory, and devices.
//!
//! Platform-specific logic lives in submodules:
//! - `darwin`: macOS (Virtualization.framework) managed execution
//! - `linux`: Linux (KVM) manual execution

#[cfg(target_os = "macos")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::device::DeviceManager;
use crate::error::{Result, VmmError};
use crate::event::EventLoop;
use crate::irq::IrqChip;
use crate::memory::MemoryManager;
#[cfg(target_os = "linux")]
use crate::vcpu::VcpuManager;

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "macos")]
pub(crate) mod darwin_hv;
#[cfg(target_os = "linux")]
mod linux;

mod config;
mod event_handlers;
mod helpers;
#[cfg(test)]
mod tests;

pub use config::{BlockDeviceConfig, SharedDirConfig, VmBackend, VmmConfig, VmmState};
use helpers::placeholder_vcpu_snapshots;

/// Virtual Machine Monitor.
///
/// Manages the complete lifecycle of a virtual machine including
/// vCPUs, memory, and devices.
///
/// # Example
///
/// ```ignore
/// use arcbox_vmm::{Vmm, VmmConfig};
/// use std::path::PathBuf;
///
/// let config = VmmConfig {
///     vcpu_count: 2,
///     memory_size: 1024 * 1024 * 1024, // 1GB
///     kernel_path: PathBuf::from("/path/to/vmlinux"),
///     kernel_cmdline: "console=ttyS0".to_string(),
///     guest_cid: Some(3),
///     ..Default::default()
/// };
///
/// let mut vmm = Vmm::new(config)?;
/// vmm.start()?;
/// ```
pub struct Vmm {
    /// Configuration.
    config: VmmConfig,
    /// Current state.
    state: VmmState,
    /// Running flag for graceful shutdown.
    running: Arc<AtomicBool>,
    /// vCPU manager (Linux only — KVM uses manual execution).
    #[cfg(target_os = "linux")]
    vcpu_manager: Option<VcpuManager>,
    /// Memory manager.
    memory_manager: Option<MemoryManager>,
    /// Device manager.
    device_manager: Option<DeviceManager>,
    /// IRQ chip (Arc for sharing with callback).
    irq_chip: Option<Arc<IrqChip>>,
    /// Event loop.
    event_loop: Option<EventLoop>,
    /// When true, Drop will skip calling into the hypervisor stop path.
    /// Used when the guest has already halted (e.g. ACPI shutdown) and the
    /// hypervisor teardown would crash. FDs and other resources are still freed.
    skip_hypervisor_stop: bool,
    /// Typed VM handle — Virtualization.framework managed VM.
    #[cfg(target_os = "macos")]
    darwin_vm: Option<arcbox_hypervisor::darwin::DarwinVm>,
    /// Typed VM handle — KVM VM (Arc for sharing with IRQ callback).
    #[cfg(target_os = "linux")]
    linux_vm: Option<Arc<std::sync::Mutex<arcbox_hypervisor::linux::KvmVm>>>,
    /// Cancellation token for the network datapath task (Darwin only).
    #[cfg(target_os = "macos")]
    net_cancel: Option<tokio_util::sync::CancellationToken>,
    /// VZ side network fd for `VZFileHandleNetworkDeviceAttachment` lifecycle.
    /// Kept open while the VM is running and closed on stop.
    #[cfg(target_os = "macos")]
    net_vz_fd: Option<OwnedFd>,
    /// Inbound listener manager for port forwarding (Darwin only).
    #[cfg(target_os = "macos")]
    inbound_listener_manager: Option<arcbox_net::darwin::inbound_relay::InboundListenerManager>,
    /// Shared DNS ownership and relative-name policy from NetworkManager.
    #[cfg(target_os = "macos")]
    shared_dns: Option<arcbox_net::dns::SharedDns>,
    /// Proxy the guest's egress is tunnelled through; `None` is direct.
    #[cfg(target_os = "macos")]
    proxy_env: Option<arcbox_fakeip::proxy_detect::ProxyEnvironment>,
    /// vmnet bridge interface for the bridge NIC (`vmnet` feature only).
    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    vmnet_bridge: Option<std::sync::Arc<arcbox_vmnet::Vmnet>>,
    /// Cancellation token for the vmnet relay task.
    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    vmnet_relay_cancel: Option<tokio_util::sync::CancellationToken>,
    /// VZ-side fd for the vmnet bridge NIC attachment.
    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    vmnet_bridge_fd: Option<OwnedFd>,
    /// Hypervisor.framework machine (`VmBackend::Hv` only).
    #[cfg(target_os = "macos")]
    hv: Option<darwin_hv::HvMachine>,
}

impl Vmm {
    /// Creates a new VMM with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid.
    pub fn new(config: VmmConfig) -> Result<Self> {
        // Validate configuration
        if config.vcpu_count == 0 {
            return Err(VmmError::config("vcpu_count must be > 0".to_string()));
        }

        if config.memory_size < 64 * 1024 * 1024 {
            return Err(VmmError::config("memory_size must be >= 64MB".to_string()));
        }

        if !config.kernel_path.as_os_str().is_empty() && !config.kernel_path.exists() {
            return Err(VmmError::config(format!(
                "kernel not found: {}",
                config.kernel_path.display()
            )));
        }
        if config.vsock && config.guest_cid.is_none() {
            return Err(VmmError::config(
                "guest_cid must be set when vsock is enabled".to_string(),
            ));
        }

        tracing::info!(
            "Creating VMM: vcpus={}, memory={}MB",
            config.vcpu_count,
            config.memory_size / (1024 * 1024)
        );

        let running = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "macos")]
        let hv = (config.backend == VmBackend::Hv)
            .then(|| darwin_hv::HvMachine::new(config.clone(), Arc::clone(&running)));

        Ok(Self {
            config,
            state: VmmState::Created,
            running,
            #[cfg(target_os = "linux")]
            vcpu_manager: None,
            memory_manager: None,
            device_manager: None,
            irq_chip: None,
            event_loop: None,
            skip_hypervisor_stop: false,
            #[cfg(target_os = "macos")]
            darwin_vm: None,
            #[cfg(target_os = "linux")]
            linux_vm: None,
            #[cfg(target_os = "macos")]
            net_cancel: None,
            #[cfg(target_os = "macos")]
            net_vz_fd: None,
            #[cfg(target_os = "macos")]
            inbound_listener_manager: None,
            #[cfg(target_os = "macos")]
            shared_dns: None,
            #[cfg(target_os = "macos")]
            proxy_env: None,
            #[cfg(all(target_os = "macos", feature = "vmnet"))]
            vmnet_bridge: None,
            #[cfg(all(target_os = "macos", feature = "vmnet"))]
            vmnet_relay_cancel: None,
            #[cfg(all(target_os = "macos", feature = "vmnet"))]
            vmnet_bridge_fd: None,
            #[cfg(target_os = "macos")]
            hv,
        })
    }

    /// Returns the current VMM state.
    #[must_use]
    pub const fn state(&self) -> VmmState {
        self.state
    }

    /// Returns whether the VMM is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Returns a clone of the running flag for external monitoring.
    #[must_use]
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    /// Sets the shared DNS context from the host-side `NetworkManager`.
    ///
    /// Call before `initialize()` so every datapath uses the host's current ownership
    /// and the configured suffix for relative names.
    #[cfg(target_os = "macos")]
    pub fn set_shared_dns(&mut self, dns: arcbox_net::dns::SharedDns) {
        match self.hv {
            Some(ref mut hv) => hv.set_shared_dns(dns),
            None => self.shared_dns = Some(dns),
        }
    }

    /// Tunnels the guest's TCP and UDP egress through `env`'s proxy.
    ///
    /// Must be called before `initialize()`. Without it every guest flow
    /// connects directly from the host.
    #[cfg(target_os = "macos")]
    pub fn set_proxy_env(&mut self, env: arcbox_fakeip::proxy_detect::ProxyEnvironment) {
        match self.hv {
            Some(ref mut hv) => hv.set_proxy_env(env),
            None => self.proxy_env = Some(env),
        }
    }

    /// Returns vmnet interface info for the bridge NIC, if available.
    ///
    /// Only populated when `vmnet` feature is enabled and interface started.
    #[cfg(all(target_os = "macos", feature = "vmnet"))]
    #[must_use]
    pub fn vmnet_interface_info(&self) -> Option<arcbox_vmnet::VmnetInterfaceInfo> {
        if let Some(ref hv) = self.hv {
            return hv.vmnet_interface_info();
        }
        self.vmnet_bridge
            .as_ref()
            .and_then(|v| v.interface_info().cloned())
    }

    /// Initializes the VMM components.
    ///
    /// This sets up the hypervisor, VM, memory, devices, and vCPUs.
    ///
    /// # Errors
    ///
    /// Returns an error if initialization fails.
    pub fn initialize(&mut self) -> Result<()> {
        if self.state != VmmState::Created {
            return Err(VmmError::invalid_state(format!(
                "cannot initialize from state {:?}",
                self.state
            )));
        }

        self.state = VmmState::Initializing;
        tracing::info!("Initializing VMM");

        // Platform-specific initialization
        #[cfg(target_os = "macos")]
        {
            tracing::info!("VM backend: {:?}", self.config.backend);
            match self.hv {
                Some(ref mut hv) => hv.initialize()?,
                None => self.initialize_darwin()?,
            }
        }

        #[cfg(target_os = "linux")]
        {
            self.initialize_linux()?;
        }

        tracing::info!("VMM initialized successfully");
        Ok(())
    }

    /// Starts the VMM.
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM cannot be started.
    pub fn start(&mut self) -> Result<()> {
        // Initialize if not already done
        if self.state == VmmState::Created {
            self.initialize()?;
        }

        if self.state != VmmState::Initializing && self.state != VmmState::Stopped {
            return Err(VmmError::invalid_state(format!(
                "cannot start from state {:?}",
                self.state
            )));
        }

        tracing::info!("Starting VMM");

        // Darwin: dispatch to the resolved backend.
        // Linux: start vCPU threads for manual execution.
        #[cfg(target_os = "macos")]
        {
            match self.hv {
                Some(ref mut hv) => hv.start()?,
                None => self.start_darwin_vm()?,
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(ref mut vcpu_manager) = self.vcpu_manager {
                vcpu_manager.start()?;
            }
        }

        // Start event loop
        if let Some(ref mut event_loop) = self.event_loop {
            event_loop.start()?;
        }

        self.running.store(true, Ordering::SeqCst);
        self.state = VmmState::Running;

        tracing::info!("VMM started");
        Ok(())
    }

    /// Pauses the VMM.
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM cannot be paused.
    pub fn pause(&mut self) -> Result<()> {
        if self.state != VmmState::Running {
            return Err(VmmError::invalid_state(format!(
                "cannot pause from state {:?}",
                self.state
            )));
        }

        tracing::info!("Pausing VMM");

        #[cfg(target_os = "macos")]
        {
            match self.hv {
                Some(ref hv) => hv.pause()?,
                None => self.pause_darwin_vm()?,
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(ref mut vcpu_manager) = self.vcpu_manager {
            vcpu_manager.pause()?;
        }

        self.state = VmmState::Paused;
        tracing::info!("VMM paused");
        Ok(())
    }

    /// Resumes a paused VMM.
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM cannot be resumed.
    pub fn resume(&mut self) -> Result<()> {
        if self.state != VmmState::Paused {
            return Err(VmmError::invalid_state(format!(
                "cannot resume from state {:?}",
                self.state
            )));
        }

        tracing::info!("Resuming VMM");

        #[cfg(target_os = "macos")]
        {
            match self.hv {
                Some(ref hv) => hv.resume()?,
                None => self.resume_darwin_vm()?,
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(ref mut vcpu_manager) = self.vcpu_manager {
            vcpu_manager.resume()?;
        }

        self.state = VmmState::Running;
        tracing::info!("VMM resumed");
        Ok(())
    }

    /// Stops the VMM.
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM cannot be stopped.
    pub fn stop(&mut self) -> Result<()> {
        if self.state == VmmState::Stopped {
            return Ok(());
        }

        tracing::info!("Stopping VMM");
        self.state = VmmState::Stopping;
        self.running.store(false, Ordering::SeqCst);

        // Stop event loop first
        if let Some(ref mut event_loop) = self.event_loop {
            event_loop.stop();
        }

        // Darwin: stop the resolved backend before canceling the network datapath.
        // Linux: stop vCPU threads.
        #[cfg(target_os = "macos")]
        {
            match self.hv {
                Some(ref mut hv) => hv.stop()?,
                None => self.stop_darwin_vm()?,
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(ref mut vcpu_manager) = self.vcpu_manager {
                vcpu_manager.stop()?;
            }
        }

        // Cancel custom file-handle network datapath after VM stop.
        #[cfg(target_os = "macos")]
        self.stop_network();

        self.state = VmmState::Stopped;
        tracing::info!("VMM stopped");
        Ok(())
    }

    /// Returns whether the guest requested a reboot (PSCI SYSTEM_RESET) rather
    /// than a power-off. The lifecycle driver polls this after the VM stops to
    /// choose between tearing the machine down and rebooting it via
    /// [`Vmm::reboot`]. Always `false` on non-macOS backends (only the HV path
    /// tracks the flag).
    #[must_use]
    pub fn reset_requested(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.hv
                .as_ref()
                .is_some_and(darwin_hv::HvMachine::reset_requested)
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Reboots the guest in the same process: full teardown followed by a fresh
    /// boot, re-creating the hypervisor VM and reloading the kernel/FDT. Clears
    /// the reset-requested flag. Used by the lifecycle driver when the guest
    /// issued PSCI SYSTEM_RESET.
    ///
    /// # Errors
    ///
    /// Returns an error if teardown or re-initialization fails.
    pub fn reboot(&mut self) -> Result<()> {
        tracing::info!("Rebooting VM: full teardown + fresh boot");
        #[cfg(target_os = "macos")]
        if let Some(ref hv) = self.hv {
            hv.clear_reset_requested();
        }
        self.stop()?;
        // `initialize()` requires the Created state; `stop()` left us Stopped
        // with every resource released. Reset the state so `start()` re-runs
        // initialization from a clean slate (re-creating the HvVm on HV).
        self.state = VmmState::Created;
        self.start()
    }

    /// Returns the configured memory size for this VM.
    ///
    /// This is the maximum memory the guest can use when the balloon is fully deflated.
    #[must_use]
    pub const fn configured_memory(&self) -> u64 {
        self.config.memory_size
    }

    /// Returns whether a balloon device is configured for this VM.
    #[must_use]
    pub const fn has_balloon(&self) -> bool {
        self.config.balloon
    }

    /// Sets the balloon's target memory size.
    ///
    /// Only the macOS backends carry a balloon device today (the real
    /// implementation lives in `darwin.rs`); elsewhere this reports the
    /// gap rather than pretending the target was applied. Callers drive
    /// it through the lifecycle balloon controller, whose
    /// `reclaim_capable` gate keeps it unreached in practice.
    ///
    /// # Errors
    ///
    /// Always, on platforms with no balloon backend.
    #[cfg(not(target_os = "macos"))]
    pub fn set_balloon_target(&self, _target_bytes: u64) -> Result<()> {
        Err(VmmError::Unsupported("balloon target control".to_string()))
    }

    /// Returns a snapshot of DAX counters for the given VirtioFS share
    /// index (0-based, in the order of `config.shared_dirs`). Only
    /// meaningful on the HV backend — the VZ path does not use
    /// `HvDaxMapper`. Returns `None` if the index is out of range or
    /// DAX mappers have not been initialized yet.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn dax_stats(&self, share_idx: usize) -> Option<crate::dax::DaxStats> {
        self.hv.as_ref()?.dax_stats(share_idx)
    }

    /// Captures a debug snapshot of the VM's virtio MMIO devices:
    /// per-queue kick counters, live avail/used ring indices, EVENT_IDX
    /// slots, and pending interrupt state.
    ///
    /// Custom-VMM backends only (HV on macOS, KVM on Linux) — returns an
    /// empty list under VZ, where the devices belong to
    /// Virtualization.framework.
    #[must_use]
    pub fn virtio_debug(&self) -> Vec<crate::device::DeviceDebug> {
        #[cfg(target_os = "macos")]
        if let Some(ref hv) = self.hv {
            return hv.virtio_debug();
        }
        self.device_manager
            .as_ref()
            .map(DeviceManager::virtio_debug)
            .unwrap_or_default()
    }

    /// Captures the full VM debug snapshot: virtio device/queue state
    /// plus per-vCPU exit counters and broadcast-wakeup totals.
    ///
    /// Custom-VMM backends only — empty under VZ.
    #[must_use]
    pub fn debug_snapshot(&self) -> crate::vcpu_stats::VmDebugSnapshot {
        crate::vcpu_stats::VmDebugSnapshot {
            devices: self.virtio_debug(),
            #[cfg(target_os = "macos")]
            vcpus: self
                .hv
                .as_ref()
                .map(darwin_hv::HvMachine::vcpu_snapshots)
                .unwrap_or_default(),
            #[cfg(not(target_os = "macos"))]
            vcpus: Vec::new(),
            kick_broadcasts: 0,
            unpark_broadcasts: 0,
        }
    }

    /// Captures a VM snapshot context from the running hypervisor VM.
    ///
    /// The returned context contains device state and full guest memory.
    /// vCPU register snapshots are currently placeholder values based on
    /// configured vCPU count.
    ///
    /// # Errors
    ///
    /// Returns an error if VM state is not snapshotable or VM handles are missing.
    pub fn capture_snapshot_context(&self) -> Result<crate::snapshot::VmSnapshotContext> {
        if self.state != VmmState::Running && self.state != VmmState::Paused {
            return Err(VmmError::invalid_state(format!(
                "cannot capture snapshot from state {:?}",
                self.state
            )));
        }

        #[cfg(target_os = "linux")]
        if let Some(result) = self.capture_snapshot_linux() {
            return result;
        }

        #[cfg(target_os = "macos")]
        {
            if matches!(self.config.backend, VmBackend::Hv) {
                return Err(VmmError::Unsupported(
                    "snapshot capture is not yet implemented for the HV backend".to_string(),
                ));
            }
            if let Some(result) = self.capture_snapshot_darwin() {
                return result;
            }
        }

        Err(VmmError::invalid_state(
            "hypervisor VM handle is unavailable for snapshot".to_string(),
        ))
    }

    /// Applies restored device + memory state to the running VM.
    ///
    /// # Errors
    ///
    /// Returns an error if restore cannot be applied.
    pub fn restore_from_snapshot_data(
        &mut self,
        restore_data: &crate::snapshot::VmRestoreData,
    ) -> Result<()> {
        if self.state != VmmState::Running && self.state != VmmState::Paused {
            return Err(VmmError::invalid_state(format!(
                "cannot restore snapshot from state {:?}",
                self.state
            )));
        }

        let expected_memory_len = usize::try_from(restore_data.memory_size()).map_err(|_| {
            VmmError::Memory(format!(
                "snapshot memory size {} does not fit in usize",
                restore_data.memory_size()
            ))
        })?;

        if restore_data.memory().len() != expected_memory_len {
            return Err(VmmError::Memory(format!(
                "snapshot memory length mismatch: expected {}, got {}",
                expected_memory_len,
                restore_data.memory().len()
            )));
        }

        #[cfg(target_os = "linux")]
        if let Some(result) = self.restore_snapshot_linux(restore_data) {
            return result;
        }

        #[cfg(target_os = "macos")]
        {
            if matches!(self.config.backend, VmBackend::Hv) {
                return Err(VmmError::Unsupported(
                    "snapshot restore is not yet implemented for the HV backend".to_string(),
                ));
            }
            if let Some(result) = self.restore_snapshot_darwin(restore_data) {
                return result;
            }
        }

        Err(VmmError::invalid_state(
            "hypervisor VM handle is unavailable for restore".to_string(),
        ))
    }

    /// Runs the VMM until it exits.
    ///
    /// This is the main event loop that blocks until the VM exits.
    ///
    /// # Errors
    ///
    /// Returns an error if the VMM encounters a fatal error.
    pub async fn run(&mut self) -> Result<()> {
        // Start if not already running
        if self.state != VmmState::Running {
            self.start()?;
        }

        tracing::info!("VMM running, waiting for exit");

        // Main event loop
        while self.is_running() {
            // Poll event loop
            if let Some(ref mut event_loop) = self.event_loop {
                if let Some(event) = event_loop.poll().await {
                    self.handle_event(event);
                }
            }

            // Small yield to prevent busy spinning
            tokio::task::yield_now().await;
        }

        tracing::info!("VMM exited");
        Ok(())
    }
}

impl Vmm {
    /// Marks this VMM to skip the Virtualization.framework stop path on drop.
    ///
    /// macOS-only: the VF stop call can crash when the guest has already
    /// halted via ACPI. FDs and network resources are still cleaned up.
    /// Must only be called after ACPI shutdown.
    ///
    /// Only the VZ backend honors the skip: on the custom HV backend the
    /// full stop path still runs on drop, because guest halt does not tear
    /// down the host side — the io workers (vsock, blk, net-rx, console)
    /// keep referencing guest RAM and must be joined before the mapping
    /// drops (ABX-415), and `stop_darwin_hv` is safe after a guest halt
    /// (the vCPU threads have already exited, so its cancel loop and joins
    /// return immediately).
    #[cfg(target_os = "macos")]
    pub fn set_skip_hypervisor_stop(&mut self) {
        self.skip_hypervisor_stop = true;
        self.mark_darwin_vm_skip_stop();
    }
}

impl Drop for Vmm {
    fn drop(&mut self) {
        if self.state != VmmState::Stopped && self.state != VmmState::Created {
            #[cfg(target_os = "macos")]
            let skip = self.skip_hypervisor_stop && self.config.backend == VmBackend::Vz;
            #[cfg(not(target_os = "macos"))]
            let skip = self.skip_hypervisor_stop;
            if skip {
                // The VZ framework stop path is unsafe (VF may crash when
                // the guest already halted). Only clean up network resources.
                #[cfg(target_os = "macos")]
                self.stop_network();
                self.state = VmmState::Stopped;
            } else {
                // Full teardown. On the HV backend this MUST run even when
                // `skip_hypervisor_stop` is set: dropping guest memory while
                // the io workers still run is a use-after-free that crashed
                // the daemon on every guest-initiated poweroff (ABX-415).
                let _ = self.stop();
            }
        }
    }
}
