//! Darwin (macOS) platform-specific VMM implementation.
//!
//! Uses Apple's Virtualization.framework for managed VM execution.

use super::*;

use super::darwin_net;

use crate::irq::{Gsi, IrqTriggerCallback};
use arcbox_hypervisor::VirtioDeviceConfig;
use arcbox_hypervisor::darwin::DarwinVm;
use arcbox_hypervisor::traits::VirtualMachine;

impl Vmm {
    /// Darwin-specific initialization using Virtualization.framework.
    pub(super) fn initialize_darwin(&mut self) -> Result<()> {
        use arcbox_hypervisor::VirtioDeviceConfig;
        use arcbox_hypervisor::darwin::DarwinHypervisor;
        use arcbox_hypervisor::traits::Hypervisor;

        let hypervisor = DarwinHypervisor::new()?;
        tracing::debug!("Platform capabilities: {:?}", hypervisor.capabilities());

        let vm_config = self.config.to_vm_config();
        let mut vm = hypervisor.create_vm(vm_config)?;

        tracing::info!(
            "Using managed execution mode: {}",
            vm.is_managed_execution()
        );

        // Add VirtioFS devices for shared directories
        for shared_dir in &self.config.shared_dirs {
            let device_config = VirtioDeviceConfig::filesystem(
                shared_dir.host_path.to_string_lossy(),
                &shared_dir.tag,
                shared_dir.read_only,
            );
            vm.add_virtio_device(device_config)?;
            tracing::info!(
                "Added VirtioFS share: {} -> {} (read_only: {})",
                shared_dir.tag,
                shared_dir.host_path.display(),
                shared_dir.read_only
            );
        }

        // Add Rosetta x86_64 translation share if enabled (best-effort).
        if self.config.enable_rosetta {
            if let Err(e) = vm.add_rosetta_share() {
                tracing::warn!(
                    "Rosetta share setup failed, continuing without x86_64 translation: {e}"
                );
            }
        }

        // Add block devices
        for block_dev in &self.config.block_devices {
            let device_config =
                VirtioDeviceConfig::block(block_dev.path.to_string_lossy(), block_dev.read_only);
            vm.add_virtio_device(device_config)?;
            tracing::info!(
                "Added block device: {} (read_only: {})",
                block_dev.path.display(),
                block_dev.read_only
            );
        }

        // Add networking if enabled.
        //
        // We try to set up a custom network stack using
        // VZFileHandleNetworkDeviceAttachment (socketpair) so that all
        // network traffic (ARP, DHCP, DNS, NAT) flows through our own
        // code. If any step fails, fall back to Apple's built-in NAT.
        if self.config.networking {
            match self.create_network_device() {
                Ok(net_config) => {
                    vm.add_virtio_device(net_config)?;
                }
                Err(e) => {
                    tracing::warn!(
                        "Custom network stack unavailable, falling back to Apple NAT: {}",
                        e
                    );
                    let net_config = VirtioDeviceConfig::network();
                    vm.add_virtio_device(net_config)?;
                    tracing::info!("Added network device with Apple NAT");
                }
            }
        }

        // Add a second NIC for host→container L3 routing.
        //
        // With the `vmnet` feature: create a vmnet.framework interface directly,
        // relay it through a socketpair, and use VZFileHandleNetworkDeviceAttachment.
        // Bridge discovery is instant (no FDB learning delay).
        //
        // Without: use VZNATNetworkDeviceAttachment (Apple creates bridge100,
        // requiring FDB scanning to discover it).
        //
        // ARCBOX_DIAG_DISABLE_BRIDGE_NIC skips the second NIC entirely — a
        // diagnostic knob for the ABX-423 freeze bisection (host→container
        // inbound routing is lost while set).
        let diag_disable_bridge = std::env::var_os("ARCBOX_DIAG_DISABLE_BRIDGE_NIC").is_some();
        if diag_disable_bridge {
            tracing::warn!("bridge NIC disabled via ARCBOX_DIAG_DISABLE_BRIDGE_NIC (diagnostic)");
        }
        if self.config.networking && !diag_disable_bridge {
            #[cfg(feature = "vmnet")]
            {
                match self.create_vmnet_bridge_nic() {
                    Ok(bridge_nic) => {
                        vm.add_virtio_device(bridge_nic)?;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "vmnet bridge NIC failed, falling back to VZNATNetworkDeviceAttachment: {e}"
                        );
                        let bridge_nic = match self.config.bridge_nic_mac.as_deref() {
                            Some(mac_address) => VirtioDeviceConfig::network_with_mac(mac_address),
                            None => VirtioDeviceConfig::network(),
                        };
                        vm.add_virtio_device(bridge_nic)?;
                    }
                }
            }

            #[cfg(not(feature = "vmnet"))]
            {
                let bridge_nic = match self.config.bridge_nic_mac.as_deref() {
                    Some(mac_address) => VirtioDeviceConfig::network_with_mac(mac_address),
                    None => VirtioDeviceConfig::network(),
                };
                vm.add_virtio_device(bridge_nic)?;
                tracing::info!(
                    mac_address = self.config.bridge_nic_mac.as_deref().unwrap_or("random"),
                    "Added bridge NIC (VZNATNetworkDeviceAttachment) for L3 routing"
                );
            }
        }

        // Add vsock if enabled
        if self.config.vsock {
            let vsock_config = VirtioDeviceConfig::vsock();
            vm.add_virtio_device(vsock_config)?;
            tracing::info!("Added vsock device");
        }

        // Add balloon device if enabled
        if self.config.balloon {
            let balloon_config = VirtioDeviceConfig::balloon();
            vm.add_virtio_device(balloon_config)?;
            tracing::info!("Added memory balloon device");
        }

        // Initialize memory manager
        let mut memory_manager = MemoryManager::new();
        memory_manager.initialize(self.config.memory_size)?;

        // Initialize device manager
        let device_manager = DeviceManager::new();

        // Initialize IRQ chip
        let irq_chip = Arc::new(IrqChip::new()?);

        // Set up IRQ callback for Darwin.
        // Virtualization.framework handles VirtIO interrupts internally,
        // so we set up a no-op callback that logs when IRQ is triggered.
        {
            let callback: IrqTriggerCallback = Box::new(|gsi: Gsi, level: bool| {
                // Darwin Virtualization.framework handles VirtIO interrupts internally.
                // For custom devices, interrupt injection is not supported.
                tracing::trace!(
                    "Darwin IRQ callback: gsi={}, level={} (handled by framework)",
                    gsi,
                    level
                );
                Ok(())
            });
            irq_chip.set_trigger_callback(Arc::new(callback));
            tracing::debug!("Darwin: IRQ callback configured (framework-managed)");
        }

        // Initialize event loop
        let event_loop = EventLoop::new()?;

        // Store managers
        self.memory_manager = Some(memory_manager);
        self.device_manager = Some(device_manager);
        self.irq_chip = Some(irq_chip);
        self.event_loop = Some(event_loop);

        // Darwin uses managed execution — store the typed VM handle directly.
        tracing::debug!("Managed execution: skipping vCPU thread creation");
        self.darwin_vm = Some(vm);

        Ok(())
    }

    /// Creates the network device configuration for Darwin.
    ///
    /// Wires a socketpair to `VZFileHandleNetworkDeviceAttachment` and the
    /// host network stack ([`darwin_net::GuestNic`]). No utun device or pf
    /// NAT is needed — this works in any network environment including VPNs.
    ///
    /// Returns a `VirtioDeviceConfig` with the VZ-side FDs embedded.
    fn create_network_device(&mut self) -> Result<VirtioDeviceConfig> {
        // NOTE(MTU): the MTU the VZ device will negotiate (1500 by default,
        // ABX-423; 4000 under ARCBOX_DIAG_NET_MTU_4000 — see
        // arcbox-vz/src/device/network.rs). The datapath fragments guest-bound
        // UDP datagrams to it (ABX-428); guest-bound TCP segments are bounded
        // by the peer's advertised MSS (TcpBridge peer_mss).
        let net_mtu = arcbox_hypervisor::darwin::desired_network_mtu();
        let nic =
            darwin_net::GuestNic::new(net_mtu, self.shared_dns.as_ref(), self.proxy_env.clone())?;
        self.net_cancel = Some(nic.cancel);
        self.inbound_listener_manager = Some(nic.inbound);

        // Keep ownership of the VZ-side fd for VM lifetime and pass the raw
        // fd into the hypervisor attachment config.
        let vz_raw_fd = nic.guest_fd.as_raw_fd();
        self.net_vz_fd = Some(nic.guest_fd);
        Ok(VirtioDeviceConfig::network_file_handle(vz_raw_fd))
    }

    /// Creates the bridge NIC via vmnet.framework and a socketpair relay.
    ///
    /// Returns a `VirtioDeviceConfig` with the VZ-side raw fd.
    #[cfg(feature = "vmnet")]
    fn create_vmnet_bridge_nic(&mut self) -> Result<VirtioDeviceConfig> {
        use arcbox_vmnet::{Vmnet, VmnetConfig, VmnetRelay};
        use std::os::fd::FromRawFd;

        // Parse MAC from config.
        let config = if let Some(ref mac_str) = self.config.bridge_nic_mac {
            let mac = arcbox_net::darwin::parse_mac(mac_str)
                .map_err(|e| VmmError::Device(format!("invalid bridge NIC MAC: {e}")))?;
            VmnetConfig::shared().with_mac(mac)
        } else {
            VmnetConfig::shared()
        };

        let vmnet = std::sync::Arc::new(
            Vmnet::new(config).map_err(|e| VmmError::Device(format!("vmnet start failed: {e}")))?,
        );

        let info = vmnet
            .interface_info()
            .ok_or_else(|| VmmError::Device("vmnet interface_info unavailable".to_string()))?;

        tracing::info!(
            mac = arcbox_net::darwin::format_mac(&info.mac),
            mtu = info.mtu,
            max_packet_size = info.max_packet_size,
            "vmnet bridge NIC created"
        );

        // Create socketpair for the relay.
        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: socketpair with valid parameters.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "socketpair for vmnet bridge failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Prevent fd inheritance to child processes and enable non-blocking I/O.
        darwin_net::set_nonblock_cloexec(fds[0])?;
        darwin_net::set_nonblock_cloexec(fds[1])?;

        // SAFETY: fds are valid from socketpair.
        let vz_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: fds are valid from socketpair.
        let relay_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Size both ends (matching the primary NIC): the bridge guest NIC also
        // negotiates the enhanced MTU, so the relay end must be able to send a
        // full-size frame to the guest.
        darwin_net::set_socket_buffers(vz_fd.as_raw_fd());
        darwin_net::set_socket_buffers(relay_fd.as_raw_fd());

        // Spawn the relay task.
        let cancel = tokio_util::sync::CancellationToken::new();
        let relay = VmnetRelay::new(std::sync::Arc::clone(&vmnet), cancel.clone());

        let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
            VmmError::Device(format!("tokio runtime not available for vmnet relay: {e}"))
        })?;

        runtime.spawn(async move {
            if let Err(e) = relay.run(relay_fd).await {
                tracing::error!("vmnet relay exited with error: {e}");
            }
        });

        tracing::info!("vmnet relay task spawned");

        // Extract MAC string before moving vmnet (info borrows the Arc).
        let mac_str = arcbox_net::darwin::format_mac(&info.mac);

        // Store state for cleanup.
        let vz_raw_fd = vz_fd.as_raw_fd();
        self.vmnet_bridge = Some(vmnet);
        self.vmnet_relay_cancel = Some(cancel);
        self.vmnet_bridge_fd = Some(vz_fd);

        // Pass the vmnet MAC to the VZ-side NIC so bridge FDB lookups match.
        Ok(VirtioDeviceConfig::network_file_handle_with_mac(
            vz_raw_fd, mac_str,
        ))
    }

    /// Starts the Darwin VM.
    pub(super) fn start_darwin_vm(&mut self) -> Result<()> {
        if let Some(ref mut vm) = self.darwin_vm {
            vm.start().map_err(VmmError::Hypervisor)?;
        }
        Ok(())
    }

    /// Pauses the Darwin VM.
    pub(super) fn pause_darwin_vm(&mut self) -> Result<()> {
        if let Some(ref mut vm) = self.darwin_vm {
            vm.pause().map_err(VmmError::Hypervisor)?;
        }
        Ok(())
    }

    /// Resumes the Darwin VM.
    pub(super) fn resume_darwin_vm(&mut self) -> Result<()> {
        if let Some(ref mut vm) = self.darwin_vm {
            vm.resume().map_err(VmmError::Hypervisor)?;
        }
        Ok(())
    }

    /// Stops the Darwin VM.
    pub(super) fn stop_darwin_vm(&mut self) -> Result<()> {
        if let Some(ref mut vm) = self.darwin_vm {
            vm.stop().map_err(VmmError::Hypervisor)?;
        }
        Ok(())
    }

    /// Marks the inner `DarwinVm` to skip its `stop()` call on drop.
    pub(super) fn mark_darwin_vm_skip_stop(&mut self) {
        if let Some(ref mut vm) = self.darwin_vm {
            vm.set_skip_stop_on_drop();
        }
    }

    /// Cancels the custom file-handle network datapath and releases VZ-side fd.
    pub(super) fn stop_network(&mut self) {
        if let Some(cancel) = self.net_cancel.take() {
            cancel.cancel();
        }
        let _ = self.net_vz_fd.take();

        // Stop vmnet relay and bridge interface.
        #[cfg(feature = "vmnet")]
        {
            if let Some(cancel) = self.vmnet_relay_cancel.take() {
                cancel.cancel();
            }
            let _ = self.vmnet_bridge_fd.take();
            if let Some(vmnet) = self.vmnet_bridge.take() {
                vmnet.stop();
            }
        }
    }

    /// Waits for the guest VM to reach the Stopped state.
    ///
    /// The shutdown RPC initiates guest teardown. HV completion comes from
    /// PSCI SYSTEM_OFF, QEMU's from its guest-shutdown event; VZ completion
    /// comes from the framework's stopped state. Exclusive access prevents
    /// concurrent consumption of the HV completion.
    ///
    /// Returns `Ok(true)` if the VM stopped within `timeout`, `Ok(false)` on timeout.
    pub fn wait_for_stopped(&mut self, timeout: Duration) -> Result<bool> {
        let running = self.state == VmmState::Running;
        match self.machine {
            Machine::Vz => {}
            Machine::Hv(ref mut hv) => return hv.wait_for_poweroff(timeout, running),
            Machine::Qemu(ref mut qemu) => return qemu.wait_for_poweroff(timeout, running),
        }

        if self.state != VmmState::Running {
            return Ok(false);
        }

        let vm = self
            .darwin_vm
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("no DarwinVm".to_string()))?;

        vm.wait_for_stopped(timeout).map_err(VmmError::Hypervisor)
    }

    /// Connects to a vsock port on the guest VM.
    ///
    /// For the HV backend, this blocks until the vCPU thread has injected
    /// the OP_REQUEST into guest memory (up to 30s). After return, the guest
    /// will respond with RST or RESPONSE — the caller handles both via
    /// read() returning EOF (RST) or data (RESPONSE + subsequent OP_RW).
    ///
    /// For the VZ backend, the fd is immediately usable.
    pub fn connect_vsock(&self, port: u32) -> Result<std::os::unix::io::RawFd> {
        if self.state != VmmState::Running {
            return Err(VmmError::invalid_state(format!(
                "cannot connect vsock: VMM is {:?}",
                self.state
            )));
        }

        match self.machine {
            Machine::Vz => {}
            Machine::Hv(ref hv) => return hv.connect_vsock(port),
            Machine::Qemu(ref qemu) => return qemu.connect_vsock(port),
        }
        let vm = self
            .darwin_vm
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("no DarwinVm".to_string()))?;
        vm.connect_vsock(port).map_err(VmmError::Hypervisor)
    }

    /// Duplicates the host read ends of the guest's `hvc0` and `hvc1` for the
    /// engine's serial drain. `None` on the HV backend, whose console is a
    /// virtio device this VMM drains itself (`guest_console` target).
    pub fn dup_serial_readers(&self) -> Result<Option<arcbox_hypervisor::darwin::SerialReaders>> {
        match self.machine {
            Machine::Vz => self
                .darwin_vm
                .as_ref()
                .map(|vm| vm.dup_serial_readers().map_err(VmmError::Hypervisor))
                .transpose(),
            Machine::Hv(_) => Ok(None),
            Machine::Qemu(ref qemu) => qemu.dup_serial_readers().map(Some),
        }
    }

    /// Reads console output (hvc0) from the VM.
    pub fn read_console_output(&self) -> Result<String> {
        if let Some(ref vm) = self.darwin_vm {
            return vm.read_console_output().map_err(VmmError::Hypervisor);
        }

        Ok(String::new())
    }

    /// Reads agent log output (hvc1) from the VM.
    pub fn read_agent_log_output(&self) -> Result<String> {
        if let Some(ref vm) = self.darwin_vm {
            return vm.read_agent_log_output().map_err(VmmError::Hypervisor);
        }

        Ok(String::new())
    }

    /// Sets the target memory size for the balloon device.
    ///
    /// The balloon device will inflate or deflate to reach the target:
    /// - **Smaller target**: Balloon inflates, reclaiming memory from guest
    /// - **Larger target**: Balloon deflates, returning memory to guest
    ///
    /// Dispatches to VZ's `VZVirtioTraditionalMemoryBalloonDevice` on the VZ
    /// backend, or to the in-tree `arcbox-virtio-balloon` device on HV.
    pub fn set_balloon_target(&self, target_bytes: u64) -> Result<()> {
        if self.state != VmmState::Running {
            return Err(VmmError::invalid_state(format!(
                "cannot set balloon target: VMM is {:?}",
                self.state
            )));
        }

        match self.machine {
            Machine::Vz => {}
            Machine::Hv(ref hv) => return hv.set_balloon_target(target_bytes),
            Machine::Qemu(_) => {
                return Err(VmmError::Unsupported(
                    "the QEMU backend has no balloon".to_string(),
                ));
            }
        }
        let vm = self
            .darwin_vm
            .as_ref()
            .ok_or_else(|| VmmError::invalid_state("no DarwinVm".to_string()))?;
        vm.set_balloon_target_memory(target_bytes)
            .map_err(VmmError::Hypervisor)
    }

    /// Gets the current target memory size from the balloon device.
    ///
    /// Returns the target memory size in bytes, or 0 if no balloon is configured
    /// or the VM is not running.
    #[must_use]
    pub fn get_balloon_target(&self) -> u64 {
        if self.state != VmmState::Running {
            return 0;
        }

        match self.machine {
            Machine::Vz => self
                .darwin_vm
                .as_ref()
                .map_or(0, DarwinVm::get_balloon_target_memory),
            Machine::Hv(ref hv) => hv.balloon_bytes().0,
            Machine::Qemu(_) => 0,
        }
    }

    /// Gets balloon statistics.
    ///
    /// Returns current balloon stats including target, current, and configured memory sizes.
    #[must_use]
    pub fn get_balloon_stats(&self) -> arcbox_hypervisor::BalloonStats {
        let (target_bytes, current_bytes) = match self.machine {
            Machine::Vz => (self.get_balloon_target(), 0),
            Machine::Hv(ref hv) => hv.balloon_bytes(),
            Machine::Qemu(_) => (0, 0),
        };
        arcbox_hypervisor::BalloonStats {
            target_bytes,
            current_bytes,
            configured_bytes: self.config.memory_size,
        }
    }

    /// Returns a mutable reference to the inbound listener manager.
    pub const fn inbound_listener_manager(
        &mut self,
    ) -> Option<&mut arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        match self.machine {
            Machine::Vz => self.inbound_listener_manager.as_mut(),
            Machine::Hv(ref mut hv) => hv.inbound_listener_manager(),
            // No NIC yet.
            Machine::Qemu(_) => None,
        }
    }

    /// Takes the inbound listener manager out of the VMM.
    ///
    /// After this call, the VMM no longer owns the manager. The caller is
    /// responsible for calling `stop_all()` on shutdown.
    pub const fn take_inbound_listener_manager(
        &mut self,
    ) -> Option<arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        match self.machine {
            Machine::Vz => self.inbound_listener_manager.take(),
            Machine::Hv(ref mut hv) => hv.take_inbound_listener_manager(),
            Machine::Qemu(_) => None,
        }
    }

    /// Captures a VM snapshot context from the running Darwin VM.
    pub(super) fn capture_snapshot_darwin(
        &self,
    ) -> Option<Result<crate::snapshot::VmSnapshotContext>> {
        use arcbox_hypervisor::traits::{GuestMemory, VirtualMachine};

        let vm = self.darwin_vm.as_ref()?;

        let device_snapshots = match vm.snapshot_devices() {
            Ok(s) => s,
            Err(e) => return Some(Err(e.into())),
        };
        let memory_size = vm.memory().size();
        let memory_len = match usize::try_from(memory_size) {
            Ok(l) => l,
            Err(_) => {
                return Some(Err(VmmError::Memory(format!(
                    "guest memory size {} does not fit in usize",
                    memory_size
                ))));
            }
        };

        let mut memory = vec![0u8; memory_len];
        if let Err(e) = vm.memory().dump_all(&mut memory) {
            return Some(Err(e.into()));
        }

        let memory_len = memory.len();
        let memory_reader = Box::new(move |buf: &mut [u8]| {
            if buf.len() != memory_len {
                return Err(crate::snapshot::SnapshotError::Internal(format!(
                    "snapshot buffer size mismatch: expected {}, got {}",
                    memory_len,
                    buf.len()
                )));
            }
            buf.copy_from_slice(&memory);
            Ok(())
        });

        Some(Ok(crate::snapshot::VmSnapshotContext {
            vcpu_snapshots: placeholder_vcpu_snapshots(self.config.vcpu_count),
            device_snapshots,
            memory_size,
            memory_reader,
        }))
    }

    /// Restores snapshot data to the running Darwin VM.
    pub(super) fn restore_snapshot_darwin(
        &mut self,
        restore_data: &crate::snapshot::VmRestoreData,
    ) -> Option<Result<()>> {
        use arcbox_hypervisor::traits::{GuestMemory, VirtualMachine};

        let vm = self.darwin_vm.as_mut()?;

        if let Err(e) = vm.restore_devices(restore_data.device_snapshots()) {
            return Some(Err(e.into()));
        }
        if let Err(e) = vm.memory().write(
            arcbox_hypervisor::GuestAddress::new(0),
            restore_data.memory(),
        ) {
            return Some(Err(e.into()));
        }

        if restore_data
            .vcpu_snapshots()
            .iter()
            .any(|s| !s.is_placeholder())
        {
            return Some(Err(VmmError::invalid_state(
                "vCPU register restore is not yet supported; snapshot contains non-placeholder vCPU state".to_string(),
            )));
        }

        Some(Ok(()))
    }
}
