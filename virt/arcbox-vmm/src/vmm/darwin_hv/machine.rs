//! The Hypervisor.framework machine and its lifecycle surface.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::device::DeviceManager;
use crate::error::{Result, VmmError};
use crate::event::EventLoop;
use crate::irq::IrqChip;
use crate::memory::MemoryManager;
use crate::vmm::VmmConfig;

use super::{CpuPower, HvGuestMem, HvVcpuIds, HvcBlkTable, VcpuThreadHandles};

/// A Hypervisor.framework machine: guest RAM, `HvVm`, GIC, device manager,
/// vCPU threads and the io workers that complete guest I/O.
///
/// Field order is the teardown contract on the panic path: Rust drops
/// fields top to bottom, so the device manager and DAX mappers release
/// their `hv_vm_unmap` handles, then the GIC, then the VM, then guest
/// memory. [`Self::stop`] runs the same order explicitly.
pub struct HvMachine {
    /// Configuration.
    pub(super) config: VmmConfig,
    /// Running flag for graceful shutdown.
    pub(super) running: Arc<AtomicBool>,
    /// Memory manager.
    pub(super) memory_manager: Option<MemoryManager>,
    /// Device manager.
    pub(super) device_manager: Option<DeviceManager>,
    /// IRQ chip (Arc for sharing with callback).
    pub(super) irq_chip: Option<Arc<IrqChip>>,
    /// Event loop.
    pub(super) event_loop: Option<EventLoop>,
    /// Cancellation token for the network datapath task (Darwin only).
    pub(super) net_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Inbound listener manager for port forwarding (Darwin only).
    pub(super) inbound_listener_manager:
        Option<arcbox_net::darwin::inbound_relay::InboundListenerManager>,
    /// Shared DNS ownership and relative-name policy from NetworkManager.
    pub(super) shared_dns: Option<arcbox_net::dns::SharedDns>,
    /// Proxy the guest's egress is tunnelled through; `None` is direct.
    pub(super) proxy_env: Option<arcbox_fakeip::proxy_detect::ProxyEnvironment>,
    /// Kernel entry address for the custom HV VMM path (stored during
    /// `initialize_darwin_hv`, consumed by `start_darwin_hv`).
    pub(super) hv_kernel_entry: Option<u64>,
    /// FDT load address for the custom HV VMM path.
    pub(super) hv_fdt_addr: Option<u64>,
    /// vCPU thread join handles for the custom HV VMM path.
    pub(super) hv_vcpu_threads: Vec<std::thread::JoinHandle<()>>,
    /// Completion emitted only by the guest's PSCI SYSTEM_OFF handler.
    pub(in crate::vmm) hv_poweroff_rx: Option<crossbeam_channel::Receiver<()>>,
    /// Retains the completion after the channel event has been consumed.
    pub(super) hv_poweroff_observed: bool,
    /// Shared vCPU thread handle registry for pause/resume (custom HV).
    pub(super) hv_vcpu_thread_handles: Option<VcpuThreadHandles>,
    /// Shared registry of Hypervisor.framework vCPU IDs (custom HV).
    /// Populated by each vCPU thread after it creates its `HvVcpu`. Used
    /// by `pause`/`stop` to target `hv_vcpus_exit` correctly on arm64.
    pub(super) hv_vcpu_ids: Option<HvVcpuIds>,
    /// Per-vCPU exit counters (custom HV). Sized by `start_darwin_hv`;
    /// kept after stop for post-mortem snapshots.
    pub(super) hv_vcpu_stats: Vec<std::sync::Arc<crate::vcpu_stats::VcpuStats>>,
    /// PSCI per-vCPU power registry (custom HV): power states plus the
    /// CPU_ON wake channels for secondary vCPUs.
    pub(super) hv_cpu_power: Option<CpuPower>,
    /// vmnet bridge interface for the bridge NIC (`vmnet` feature only).
    #[cfg(feature = "vmnet")]
    pub(super) vmnet_bridge: Option<std::sync::Arc<arcbox_vmnet::Vmnet>>,
    /// Cancellation token for the vmnet relay task.
    #[cfg(feature = "vmnet")]
    pub(super) vmnet_relay_cancel: Option<tokio_util::sync::CancellationToken>,
    /// Shared DeviceManager reference for HV backend (set during start_darwin_hv).
    /// Used by connect_vsock_hv to inject OP_REQUEST packets after VM starts.
    ///
    /// Declared before `hv_vm` so it drops first: `FsServer` inside the
    /// DeviceManager holds `Arc<HvDaxMapper>` handles that call
    /// `hv_vm_unmap` on drop. Those must complete before `hv_vm_destroy`.
    pub(super) hv_device_manager: Option<std::sync::Arc<DeviceManager>>,
    /// vsock-io worker thread handle (custom HV). Owns host→guest vsock
    /// injection; joined in `stop_darwin_hv` before guest memory drops.
    pub(super) hv_vsock_worker: Option<std::thread::JoinHandle<()>>,
    /// Debug-console RX worker thread handle (custom HV). Only present when
    /// `debug_console_socket` is configured; joined in `stop_darwin_hv`.
    pub(super) hv_console_worker: Option<std::thread::JoinHandle<()>>,
    /// HV-side network fd (NIC1). Paired with the NetworkDatapath fd.
    /// Kept alive so the socketpair stays open while the VM runs.
    pub(super) hv_net_fd: Option<OwnedFd>,
    /// HV-side bridge network fd (NIC2). Paired with the VmnetRelay fd.
    /// Kept alive so the vmnet socketpair stays open while the VM runs.
    pub(super) hv_bridge_net_fd: Option<OwnedFd>,
    /// Block device info captured during initialize for worker thread spawn.
    /// (DeviceId, raw_fd, blk_size, capacity_sectors, read_only, device_id_string, num_queues)
    pub(super) hv_blk_devices: Vec<(crate::device::DeviceId, i32, u32, u64, bool, String, u16)>,
    /// Block I/O worker thread handles for join on shutdown.
    pub(super) hv_blk_worker_threads: Vec<std::thread::JoinHandle<()>>,
    /// HVC fast path: the block devices by device index. Shared with vCPU threads.
    pub(super) hvc_blk_fds: HvcBlkTable,
    /// Per-VirtioFS-share DAX mappers (concrete type).
    ///
    /// One `Arc<HvDaxMapper>` per configured shared directory, in the
    /// same order as `config.shared_dirs`. The mapper itself is also
    /// handed to the `FsServer` as a `dyn DaxMapper` trait object —
    /// both Arcs point to the same underlying counters, so
    /// `dax_stats(share_idx)` reports live values as the guest issues
    /// `FUSE_SETUPMAPPING` / `FUSE_REMOVEMAPPING` requests.
    ///
    /// Declared before `hv_vm` so implicit Drop order is safe: any
    /// remaining `Arc<HvDaxMapper>` refs call `hv_vm_unmap` before
    /// `hv_vm_destroy` runs. `stop_darwin_hv` explicitly calls
    /// `drain_all` first, making implicit drop a no-op for the normal path.
    pub(super) hv_dax_mappers: Vec<std::sync::Arc<crate::dax::HvDaxMapper>>,
    /// HV backend balloon device handle (ABX-363).
    ///
    /// `None` until `initialize_darwin_hv` registers the device (only
    /// happens when `config.balloon` is true). Used by the HV dispatch
    /// path in `darwin.rs` for `set_balloon_target` / `get_balloon_stats`.
    pub(super) hv_balloon:
        Option<std::sync::Arc<std::sync::Mutex<arcbox_virtio::balloon::VirtioBalloon>>>,
    /// GICv3 handle (custom VMM path, macOS 15+).
    ///
    /// **Drop order — must be declared before `hv_vm`.**
    /// Rust drops struct fields in declaration order (top to bottom). The GIC
    /// interrupt controller holds internal references into the VM; its FFI
    /// destroy/release routines must run while the VM is still alive.
    /// Declaring `hv_gic` before `hv_vm` guarantees that on any exit path
    /// (both the normal `stop_darwin_hv` and the panic / implicit-drop path),
    /// the GIC is torn down before `hv_vm_destroy` is called.
    #[cfg(feature = "gic")]
    pub(super) hv_gic: Option<std::sync::Arc<arcbox_hv::Gic>>,
    /// Hypervisor.framework VM handle (custom VMM path).
    ///
    /// **Drop order — must be declared after `hv_gic`, `hv_dax_mappers`,
    /// and `hv_device_manager`.**
    /// Rust drops fields in declaration order (top to bottom), so fields
    /// declared above this one drop first. This ordering ensures:
    ///   1. `hv_device_manager` (FsServer → `Arc<HvDaxMapper>` → hv_vm_unmap)
    ///   2. `hv_dax_mappers` (remaining mapper refs → hv_vm_unmap)
    ///   3. `hv_gic` (GIC FFI teardown referencing the live VM)
    ///   4. `hv_vm` → `hv_vm_destroy`
    ///   5. `hv_guest_mem` (mmap'd pages — must outlive the VM)
    ///
    /// `stop_darwin_hv` also calls `drain_all` explicitly for the normal
    /// path; this declaration ordering is the safety net for panic paths.
    pub(super) hv_vm: Option<arcbox_hv::HvVm>,
    /// Guest memory backing (vm-memory mmap, custom VMM path).
    ///
    /// **Drop order — must be declared after `hv_vm`.**
    /// The mmap'd pages must remain accessible until after `hv_vm_destroy`
    /// releases all stage-2 mappings. Declaring this last ensures the host
    /// VA range is freed only after the VM no longer references it.
    pub(super) hv_guest_mem: Option<HvGuestMem>,
    /// Cooperative pause flag for the HV backend.
    ///
    /// When set to `true`, every vCPU thread parks itself after its next
    /// `vcpu.run()` return instead of re-entering guest execution. `resume`
    /// clears the flag and unparks the threads. Orthogonal to `running`
    /// (which is a terminal stop signal).
    pub(super) hv_paused: Arc<AtomicBool>,
    /// Set by a guest PSCI SYSTEM_RESET (as opposed to SYSTEM_OFF): the vCPU
    /// loop stops the VM and flags this so the lifecycle driver reboots the
    /// guest via [`Vmm::reboot`] instead of tearing the machine down. Cleared
    /// by `reboot`.
    pub(super) hv_reset_requested: Arc<AtomicBool>,
}

impl HvMachine {
    /// Creates an uninitialized machine for `config`. `running` is the
    /// owner's run flag; every thread the machine spawns watches it.
    pub(in crate::vmm) fn new(config: VmmConfig, running: Arc<AtomicBool>) -> Self {
        Self {
            config,
            running,
            memory_manager: None,
            device_manager: None,
            irq_chip: None,
            event_loop: None,
            net_cancel: None,
            inbound_listener_manager: None,
            shared_dns: None,
            proxy_env: None,
            hv_kernel_entry: None,
            hv_fdt_addr: None,
            hv_vcpu_threads: Vec::new(),
            hv_poweroff_rx: None,
            hv_poweroff_observed: false,
            hv_vcpu_thread_handles: None,
            hv_vcpu_ids: None,
            hv_vcpu_stats: Vec::new(),
            hv_cpu_power: None,
            #[cfg(feature = "vmnet")]
            vmnet_bridge: None,
            #[cfg(feature = "vmnet")]
            vmnet_relay_cancel: None,
            hv_device_manager: None,
            hv_vsock_worker: None,
            hv_console_worker: None,
            hv_net_fd: None,
            hv_bridge_net_fd: None,
            hv_blk_devices: Vec::new(),
            hv_blk_worker_threads: Vec::new(),
            hvc_blk_fds: Arc::new(Vec::new()),
            hv_dax_mappers: Vec::new(),
            hv_balloon: None,
            #[cfg(feature = "gic")]
            hv_gic: None,
            hv_vm: None,
            hv_guest_mem: None,
            hv_paused: Arc::new(AtomicBool::new(false)),
            hv_reset_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Sets the shared DNS policy. Must be called before [`Self::initialize`].
    pub(in crate::vmm) fn set_shared_dns(&mut self, dns: arcbox_net::dns::SharedDns) {
        self.shared_dns = Some(dns);
    }

    /// Sets the egress proxy. Must be called before [`Self::initialize`].
    pub(in crate::vmm) fn set_proxy_env(
        &mut self,
        env: arcbox_fakeip::proxy_detect::ProxyEnvironment,
    ) {
        self.proxy_env = Some(env);
    }

    /// Builds the VM, guest memory, devices and the boot image.
    pub(in crate::vmm) fn initialize(&mut self) -> Result<()> {
        self.initialize_darwin_hv()
    }

    /// Starts the vCPU threads and io workers, then the event loop.
    pub(in crate::vmm) fn start(&mut self) -> Result<()> {
        self.start_darwin_hv()?;
        if let Some(ref mut event_loop) = self.event_loop {
            event_loop.start()?;
        }
        Ok(())
    }

    /// Parks every vCPU thread.
    pub(in crate::vmm) fn pause(&self) -> Result<()> {
        self.pause_darwin_hv()
    }

    /// Unparks every vCPU thread.
    pub(in crate::vmm) fn resume(&self) -> Result<()> {
        self.resume_darwin_hv()
    }

    /// Stops the event loop, tears the machine down in the drop-order
    /// contract above, then cancels the network datapath.
    pub(in crate::vmm) fn stop(&mut self) -> Result<()> {
        if let Some(ref mut event_loop) = self.event_loop {
            event_loop.stop();
        }
        self.stop_darwin_hv()?;
        self.stop_network();
        Ok(())
    }

    /// Cancels the datapath and the vmnet relay and closes the HV-side
    /// bridge fd.
    fn stop_network(&mut self) {
        if let Some(cancel) = self.net_cancel.take() {
            cancel.cancel();
        }
        let _ = self.hv_bridge_net_fd.take();
        #[cfg(feature = "vmnet")]
        {
            if let Some(cancel) = self.vmnet_relay_cancel.take() {
                cancel.cancel();
            }
            if let Some(vmnet) = self.vmnet_bridge.take() {
                vmnet.stop();
            }
        }
    }

    /// Waits for the guest's PSCI SYSTEM_OFF. `running` is whether the
    /// owner considers the VM running; a completion already observed
    /// answers `true` regardless.
    pub(in crate::vmm) fn wait_for_poweroff(
        &mut self,
        timeout: Duration,
        running: bool,
    ) -> Result<bool> {
        if self.hv_poweroff_observed {
            return Ok(true);
        }
        if !running {
            return Ok(false);
        }
        let receiver = self
            .hv_poweroff_rx
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("HV poweroff receiver is not initialized"))?;
        match receiver.recv_timeout(timeout) {
            Ok(()) => {
                self.hv_poweroff_observed = true;
                Ok(true)
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Ok(false),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(VmmError::Vcpu(
                "HV vCPU threads exited without PSCI SYSTEM_OFF".into(),
            )),
        }
    }

    /// Whether the guest asked for PSCI SYSTEM_RESET.
    pub(in crate::vmm) fn reset_requested(&self) -> bool {
        self.hv_reset_requested.load(Ordering::SeqCst)
    }

    /// Clears the reset request before a reboot.
    pub(in crate::vmm) fn clear_reset_requested(&self) {
        self.hv_reset_requested.store(false, Ordering::SeqCst);
    }

    /// Opens a host-initiated vsock connection to `port` in the guest.
    pub(in crate::vmm) fn connect_vsock(&self, port: u32) -> Result<std::os::unix::io::RawFd> {
        self.connect_vsock_hv(port)
    }

    /// Sets the balloon target in bytes (4 KiB pages, saturating).
    pub(in crate::vmm) fn set_balloon_target(&self, target_bytes: u64) -> Result<()> {
        let balloon = self
            .hv_balloon
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("HV balloon not configured"))?;
        let pages = u32::try_from(target_bytes / 4096).unwrap_or(u32::MAX);
        let guard = balloon
            .lock()
            .map_err(|e| VmmError::Device(format!("balloon lock poisoned: {e}")))?;
        guard.set_num_pages(pages);
        Ok(())
    }

    /// Returns the balloon `(target, current)` in bytes; zeros without one.
    pub(in crate::vmm) fn balloon_bytes(&self) -> (u64, u64) {
        self.hv_balloon.as_ref().map_or((0, 0), |b| {
            b.lock().map_or((0, 0), |g| {
                (
                    u64::from(g.num_pages()) * 4096,
                    u64::from(g.actual()) * 4096,
                )
            })
        })
    }

    /// DAX counters for VirtioFS share `share_idx`.
    pub(in crate::vmm) fn dax_stats(&self, share_idx: usize) -> Option<crate::dax::DaxStats> {
        self.hv_dax_mappers.get(share_idx).map(|m| m.stats())
    }

    /// Virtio MMIO device and queue state.
    pub(in crate::vmm) fn virtio_debug(&self) -> Vec<crate::device::DeviceDebug> {
        self.hv_device_manager
            .as_deref()
            .or(self.device_manager.as_ref())
            .map(DeviceManager::virtio_debug)
            .unwrap_or_default()
    }

    /// Per-vCPU exit counters.
    pub(in crate::vmm) fn vcpu_snapshots(&self) -> Vec<crate::vcpu_stats::VcpuStatsSnapshot> {
        self.hv_vcpu_stats
            .iter()
            .enumerate()
            .map(|(i, stats)| stats.snapshot(i as u32))
            .collect()
    }

    /// vmnet interface info for the bridge NIC, once it has started.
    #[cfg(feature = "vmnet")]
    pub(in crate::vmm) fn vmnet_interface_info(&self) -> Option<arcbox_vmnet::VmnetInterfaceInfo> {
        self.vmnet_bridge
            .as_ref()
            .and_then(|v| v.interface_info().cloned())
    }

    /// The port-forwarding listener manager, while the machine owns it.
    pub(in crate::vmm) const fn inbound_listener_manager(
        &mut self,
    ) -> Option<&mut arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.as_mut()
    }

    /// Takes the port-forwarding listener manager.
    pub(in crate::vmm) const fn take_inbound_listener_manager(
        &mut self,
    ) -> Option<arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.take()
    }
}
