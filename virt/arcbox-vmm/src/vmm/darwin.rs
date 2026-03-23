//! Darwin (macOS) platform-specific VMM implementation.
//!
//! Uses Apple's Virtualization.framework for managed VM execution.

use super::*;

use arcbox_hypervisor::darwin::DarwinVm;
use arcbox_hypervisor::traits::VirtualMachine;
use tokio_util::sync::CancellationToken;

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

        // Check if this is managed execution
        self.managed_execution = vm.is_managed_execution();
        tracing::info!("Using managed execution mode: {}", self.managed_execution);

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
        if self.config.networking {
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

        // For managed execution, we don't create vCPU threads
        // Instead, store the VM for lifecycle management
        if self.managed_execution {
            tracing::debug!("Managed execution: skipping vCPU thread creation");
            self.managed_vm = Some(Box::new(vm));
        } else {
            // This shouldn't happen on Darwin, but handle it anyway
            let vcpu_manager = VcpuManager::new(self.config.vcpu_count);
            // Note: Darwin vCPUs are placeholders, but we add them anyway
            self.vcpu_manager = Some(vcpu_manager);
        }

        Ok(())
    }

    /// Sets `O_NONBLOCK` and `FD_CLOEXEC` on a raw file descriptor.
    ///
    /// Socketpair fds are blocking and inheritable by default. Setting
    /// `CLOEXEC` prevents leaking them to child processes, and `NONBLOCK`
    /// is required for async I/O via tokio.
    fn set_nonblock_cloexec(fd: libc::c_int) -> Result<()> {
        // SAFETY: fcntl F_GETFD/F_SETFD/F_GETFL/F_SETFL are standard POSIX
        // operations on a valid fd.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 {
                return Err(VmmError::Device(format!(
                    "fcntl F_GETFD failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
                return Err(VmmError::Device(format!(
                    "fcntl F_SETFD FD_CLOEXEC failed: {}",
                    std::io::Error::last_os_error()
                )));
            }

            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags == -1 {
                return Err(VmmError::Device(format!(
                    "fcntl F_GETFL failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                return Err(VmmError::Device(format!(
                    "fcntl F_SETFL O_NONBLOCK failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }

    /// Creates the network device configuration for Darwin.
    ///
    /// Sets up a socketpair for `VZFileHandleNetworkDeviceAttachment` and a
    /// socket proxy that routes guest traffic through host OS sockets. No
    /// utun device or pf NAT is needed — this works in any network environment
    /// including VPNs.
    ///
    /// Returns a `VirtioDeviceConfig` with the VZ-side FDs embedded.
    fn create_network_device(&mut self) -> Result<VirtioDeviceConfig> {
        use arcbox_net::darwin::datapath_loop::NetworkDatapath;
        use arcbox_net::darwin::inbound_relay::InboundListenerManager;
        use arcbox_net::darwin::socket_proxy::SocketProxy;
        use arcbox_net::dhcp::{DhcpConfig, DhcpServer};
        use arcbox_net::dns::{DnsConfig, DnsForwarder};
        use std::net::Ipv4Addr;

        let gateway_ip = Ipv4Addr::new(10, 0, 2, 1);
        let guest_ip = Ipv4Addr::new(10, 0, 2, 2);
        let netmask = Ipv4Addr::new(255, 255, 255, 0);
        let gateway_mac: [u8; 6] = [0x02, 0xAB, 0xCD, 0x00, 0x00, 0x01];

        // 1. Create a SOCK_DGRAM socketpair for L2 Ethernet frame exchange.
        let mut fds: [libc::c_int; 2] = [0; 2];
        // SAFETY: socketpair with valid parameters.
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) };
        if ret != 0 {
            return Err(VmmError::Device(format!(
                "socketpair failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        // Prevent fd inheritance to child processes and enable non-blocking I/O.
        Self::set_nonblock_cloexec(fds[0])?;
        Self::set_nonblock_cloexec(fds[1])?;

        // fds[0] = VZ framework side (read guest tx, write guest rx)
        // fds[1] = host datapath side
        //
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let vz_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: fds are valid file descriptors returned by socketpair.
        let host_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Set a large socket buffer for the VZ side to avoid drops.
        // SAFETY: setsockopt with valid fd and parameters.
        let buf_size: libc::c_int = 2 * 1024 * 1024;
        unsafe {
            if libc::setsockopt(
                vz_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                tracing::warn!(
                    "setsockopt SO_SNDBUF failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::setsockopt(
                vz_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                tracing::warn!(
                    "setsockopt SO_RCVBUF failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        // 2. Cancellation token shared by all spawned network tasks.
        let cancel = CancellationToken::new();
        self.net_cancel = Some(cancel.clone());

        // 3. Create the socket proxy, reply channel, and inbound command channel.
        let (reply_tx, reply_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
        let socket_proxy =
            SocketProxy::new(gateway_ip, gateway_mac, guest_ip, reply_tx, cancel.clone());

        // Create the inbound listener manager for port forwarding.
        self.inbound_listener_manager = Some(InboundListenerManager::new(cmd_tx));

        tracing::info!(
            "Custom network stack: socket proxy (gateway={}, guest={})",
            gateway_ip,
            guest_ip,
        );

        // 4. Create the network stack components.
        let dhcp_config = DhcpConfig::new(gateway_ip, netmask)
            .with_pool_range(guest_ip, Ipv4Addr::new(10, 0, 2, 254))
            .with_dns_servers(vec![gateway_ip]);
        let dhcp_server = DhcpServer::new(dhcp_config);

        let dns_config = DnsConfig::new(gateway_ip);
        let dns_forwarder = if let Some(ref shared_table) = self.shared_dns_hosts {
            DnsForwarder::with_shared_hosts(dns_config, std::sync::Arc::clone(shared_table))
        } else {
            DnsForwarder::new(dns_config)
        };

        // 5. Build the datapath and spawn it on the tokio runtime.

        let datapath = NetworkDatapath::new(
            host_fd,
            socket_proxy,
            reply_rx,
            cmd_rx,
            dhcp_server,
            dns_forwarder,
            gateway_ip,
            guest_ip,
            gateway_mac,
            cancel,
        );

        let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
            VmmError::Device(format!(
                "tokio runtime not available for network datapath: {e}"
            ))
        })?;

        runtime.spawn(async move {
            if let Err(e) = datapath.run().await {
                tracing::error!("Network datapath exited with error: {}", e);
            }
        });

        tracing::info!("Network datapath task spawned");

        // 5. Keep ownership of the VZ-side fd for VM lifetime and pass the raw
        // fd into the hypervisor attachment config.
        let vz_raw_fd = vz_fd.as_raw_fd();
        self.net_vz_fd = Some(vz_fd);
        Ok(VirtioDeviceConfig::network_file_handle(vz_raw_fd))
    }

    /// Creates the bridge NIC via vmnet.framework and a socketpair relay.
    ///
    /// Returns a `VirtioDeviceConfig` with the VZ-side raw fd.
    #[cfg(feature = "vmnet")]
    fn create_vmnet_bridge_nic(&mut self) -> Result<VirtioDeviceConfig> {
        use arcbox_net::darwin::vmnet::{Vmnet, VmnetConfig};
        use arcbox_net::darwin::vmnet_relay::VmnetRelay;

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
        Self::set_nonblock_cloexec(fds[0])?;
        Self::set_nonblock_cloexec(fds[1])?;

        // SAFETY: fds are valid from socketpair.
        let vz_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: fds are valid from socketpair.
        let relay_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Set large buffers on the VZ side.
        let buf_size: libc::c_int = 2 * 1024 * 1024;
        // SAFETY: setsockopt with valid fd and parameters.
        unsafe {
            if libc::setsockopt(
                vz_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                tracing::warn!(
                    "setsockopt SO_SNDBUF (vmnet bridge) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
            if libc::setsockopt(
                vz_fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const buf_size).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                tracing::warn!(
                    "setsockopt SO_RCVBUF (vmnet bridge) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }

        // Spawn the relay task.
        let cancel = CancellationToken::new();
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

        // Store state for cleanup.
        let vz_raw_fd = vz_fd.as_raw_fd();
        self.vmnet_bridge = Some(vmnet);
        self.vmnet_relay_cancel = Some(cancel);
        self.vmnet_bridge_fd = Some(vz_fd);

        Ok(VirtioDeviceConfig::network_file_handle(vz_raw_fd))
    }

    /// Starts the managed VM.
    pub(super) fn start_managed_vm(&mut self) -> Result<()> {
        if let Some(ref mut managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_mut::<DarwinVm>() {
                vm.start().map_err(VmmError::Hypervisor)?;
            }
        }
        Ok(())
    }

    /// Pauses the managed VM.
    pub(super) fn pause_managed_vm(&mut self) -> Result<()> {
        if let Some(ref mut managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_mut::<DarwinVm>() {
                vm.pause().map_err(VmmError::Hypervisor)?;
            }
        }
        Ok(())
    }

    /// Resumes the managed VM.
    pub(super) fn resume_managed_vm(&mut self) -> Result<()> {
        if let Some(ref mut managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_mut::<DarwinVm>() {
                vm.resume().map_err(VmmError::Hypervisor)?;
            }
        }
        Ok(())
    }

    /// Stops the managed VM.
    pub(super) fn stop_managed_vm(&mut self) -> Result<()> {
        if let Some(ref mut managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_mut::<DarwinVm>() {
                vm.stop().map_err(VmmError::Hypervisor)?;
            }
        }
        Ok(())
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

    /// Requests graceful guest shutdown via ACPI and waits for it to stop.
    ///
    /// Returns `Ok(true)` if the VM stopped, `Ok(false)` on timeout or when
    /// graceful stop is unavailable.
    pub fn request_stop(&self, timeout: Duration) -> Result<bool> {
        if self.state != VmmState::Running {
            return Ok(false);
        }

        let vm = self
            .managed_vm
            .as_ref()
            .and_then(|managed_vm| managed_vm.downcast_ref::<DarwinVm>())
            .ok_or_else(|| VmmError::invalid_state("no managed DarwinVm".to_string()))?;

        vm.request_stop_and_wait(timeout)
            .map_err(VmmError::Hypervisor)
    }

    /// Connects to a vsock port on the guest VM.
    pub fn connect_vsock(&self, port: u32) -> Result<std::os::unix::io::RawFd> {
        if self.state != VmmState::Running {
            return Err(VmmError::invalid_state(format!(
                "cannot connect vsock: VMM is {:?}",
                self.state
            )));
        }

        if let Some(ref managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_ref::<DarwinVm>() {
                return vm.connect_vsock(port).map_err(VmmError::Hypervisor);
            }
        }

        Err(VmmError::invalid_state(
            "vsock not available in manual execution mode".to_string(),
        ))
    }

    /// Reads serial console output from a managed VM.
    pub fn read_console_output(&self) -> Result<String> {
        if let Some(ref managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_ref::<DarwinVm>() {
                return vm.read_console_output().map_err(VmmError::Hypervisor);
            }
        }

        Ok(String::new())
    }

    /// Sets the target memory size for the balloon device.
    ///
    /// The balloon device will inflate or deflate to reach the target:
    /// - **Smaller target**: Balloon inflates, reclaiming memory from guest
    /// - **Larger target**: Balloon deflates, returning memory to guest
    pub fn set_balloon_target(&self, target_bytes: u64) -> Result<()> {
        if self.state != VmmState::Running {
            return Err(VmmError::invalid_state(format!(
                "cannot set balloon target: VMM is {:?}",
                self.state
            )));
        }

        if let Some(ref managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_ref::<DarwinVm>() {
                return vm
                    .set_balloon_target_memory(target_bytes)
                    .map_err(VmmError::Hypervisor);
            }
        }

        Err(VmmError::invalid_state(
            "balloon not available in manual execution mode".to_string(),
        ))
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

        if let Some(ref managed_vm) = self.managed_vm {
            if let Some(vm) = managed_vm.downcast_ref::<DarwinVm>() {
                return vm.get_balloon_target_memory();
            }
        }

        0
    }

    /// Gets balloon statistics.
    ///
    /// Returns current balloon stats including target, current, and configured memory sizes.
    #[must_use]
    pub fn get_balloon_stats(&self) -> arcbox_hypervisor::BalloonStats {
        arcbox_hypervisor::BalloonStats {
            target_bytes: self.get_balloon_target(),
            current_bytes: 0, // macOS doesn't expose current balloon size
            configured_bytes: self.config.memory_size,
        }
    }

    /// Returns a mutable reference to the inbound listener manager.
    pub const fn inbound_listener_manager(
        &mut self,
    ) -> Option<&mut arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.as_mut()
    }

    /// Takes the inbound listener manager out of the VMM.
    ///
    /// After this call, the VMM no longer owns the manager. The caller is
    /// responsible for calling `stop_all()` on shutdown.
    pub const fn take_inbound_listener_manager(
        &mut self,
    ) -> Option<arcbox_net::darwin::inbound_relay::InboundListenerManager> {
        self.inbound_listener_manager.take()
    }

    /// Captures a VM snapshot context from the running Darwin VM.
    pub(super) fn capture_snapshot_darwin(
        &self,
    ) -> Option<Result<crate::snapshot::VmSnapshotContext>> {
        use arcbox_hypervisor::traits::{GuestMemory, VirtualMachine};

        let managed_vm = self.managed_vm.as_ref()?;
        let vm = managed_vm.downcast_ref::<DarwinVm>()?;

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

        let managed_vm = self.managed_vm.as_mut()?;
        let vm = managed_vm.downcast_mut::<DarwinVm>()?;

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
